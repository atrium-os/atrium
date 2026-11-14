//! scrinium-pick-demo: an app that asks the system picker for a file or a
//! folder, the way any app does (scrinium.md §4.1), and shows what came.
//! Used by the VM gate (scripts/vm-scrinium-check.sh), which reads its log.
//!
//!   PICK_KIND=file|folder  [file]
//!   PICK_EXT=txt,md        [any]
//!   PICK_WRITE=1           the app wants to change what it gets: it
//!                          appends a line to a picked file, or writes
//!                          `from-the-app.txt` into a picked folder
//!   PICK_SECS=n            exit after n seconds
//!   FRESCO_SOCKET, LIMEN_SOCKET as usual.
//!
//! Logs `pick-demo: file NAME SIZE <first line>`, `pick-demo: folder AT
//! <entries>`, `pick-demo: cancelled`, `pick-demo: failed WHY`.

use std::io::{Read, Seek, Write};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use limen::picker::{Filter, Kind, ToChild, ToParent};
use limen::proto::ParentEvent;
use pergola::embed::Embeds;
use pergola::geom::Rect;
use pergola::node::Node;
use pergola::theme::{font, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::window::StepResult;
use pergola::{App, Color, Mutable, SizeSpec, TextStyle, Window, WindowDesc};

const PICKER_SLOT: u64 = 1;

#[derive(Clone, Default)]
struct Shown {
    asking: bool,
    lines: Vec<String>,
}

struct Demo {
    shown: Mutable<Shown>,
    size: (f32, f32),
}

impl View for Demo {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;
        let s = self.shown.get_cloned();
        let (w, h) = self.size;
        // The whole window is pressable, so the gate can tell whether a
        // press reached the app while the picker's sheet held it.
        let bg = ctx.push(Node::Rect { rect: Rect::new(0.0, 0.0, w, h), fill: t.bg_canvas(), radius: 0.0 });
        ctx.on_click(bg, || eprintln!("pick-demo: the app got a press"));
        let st = |size, weight, color| TextStyle { family: font::SANS.into(), size, weight, color };
        ctx.add(Node::Text { rect: Rect::new(24.0, 14.0, 0.0, 24.0), content: "Notes (a demo app)".into(), style: st(type_size::LG, Weight::Semibold, t.text_primary()) });
        let what = if s.asking { "Asking the system picker. Only what you pick reaches this app." } else { "What the picker handed over:" };
        ctx.add(Node::Text { rect: Rect::new(24.0, 44.0, 0.0, 20.0), content: what.into(), style: st(type_size::SM, Weight::Regular, t.text_secondary()) });
        if s.asking {
            // The app's own content goes on as usual; the system places
            // the picker's sheet over it (scrinium.md §4.4), whatever rect
            // this slot has.
            for (i, l) in ["Monday: call the plumber", "Groceries: rice, lentils, coriander", "Read chapter 4"].iter().enumerate() {
                ctx.add(Node::Text { rect: Rect::new(24.0, 80.0 + i as f32 * 22.0, 0.0, 20.0), content: (*l).into(), style: st(type_size::SM, Weight::Regular, t.text_primary()) });
            }
            let id = ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, 1.0, 1.0), fill: Color::TRANSPARENT, radius: 0.0 });
            ctx.slot(id, PICKER_SLOT);
        } else {
            for (i, l) in s.lines.iter().enumerate() {
                ctx.add(Node::Text { rect: Rect::new(24.0, 80.0 + i as f32 * 22.0, 0.0, 20.0), content: l.clone(), style: st(type_size::SM, Weight::Regular, t.text_primary()) });
            }
        }
        ctx.pop();
    }
}

fn first_line(f: &mut std::fs::File) -> String {
    let mut s = String::new();
    let _ = f.take(4096).read_to_string(&mut s);
    s.lines().next().unwrap_or("").to_string()
}

fn main() {
    let kind = if std::env::var("PICK_KIND").as_deref() == Ok("folder") { Kind::Folder } else { Kind::File };
    let write = std::env::var_os("PICK_WRITE").is_some();
    let extensions: Vec<String> = std::env::var("PICK_EXT").map(|s| s.split(',').filter(|e| !e.is_empty()).map(|e| e.to_ascii_lowercase()).collect()).unwrap_or_default();
    let filter = Filter { kind, extensions, multiple: false, write, title: if kind == Kind::Folder { "Choose a folder for your notes".into() } else { "Open a note".into() } };
    let until = std::env::var("PICK_SECS").ok().and_then(|s| s.parse::<u64>().ok()).map(|s| Instant::now() + Duration::from_secs(s));

    let mut win = match Window::open(&WindowDesc::new("Notes", SizeSpec::Fixed(760, 540))) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("pick-demo: frescod: {e}");
            std::process::exit(1);
        }
    };
    let embeds = match Embeds::connect() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("pick-demo: limend: {e:?}");
            std::process::exit(1);
        }
    };
    win.set_slot_binder(Embeds::binder(&embeds));
    let dirty = Arc::new(AtomicBool::new(true));
    let shown = Mutable::with_dirty(Shown { asking: true, lines: Vec::new() }, dirty.clone());
    let mut app = App::new_with_flag(Demo { shown: shown.clone(), size: (win.width, win.height) }, dirty);
    let _ = win.paint(&mut app);
    let mut embed = None;
    let mut lines = Vec::new();
    loop {
        match win.step(&mut app, Some(Duration::from_millis(30))) {
            Ok(StepResult::Closed) | Err(_) => break,
            _ => {}
        }
        if embed.is_none() {
            if let Some(token) = win.embed_token() {
                let asked = embeds.borrow_mut().request("picker", token, PICKER_SLOT);
                match asked {
                    Ok(e) => {
                        eprintln!("pick-demo: asked for a {kind:?} picker (embed {e})");
                        let _ = embeds.borrow_mut().send(e, ToChild::Open(filter.clone()).encode());
                        embed = Some(e);
                    }
                    Err(e) => {
                        eprintln!("pick-demo: failed no picker ({e:?})");
                        break;
                    }
                }
            }
        }
        let ev = embeds.borrow_mut().poll(Duration::from_millis(1));
        match ev {
            Ok(Some(ParentEvent::Attached { .. })) => eprintln!("pick-demo: the picker is in its slot"),
            Ok(Some(ParentEvent::MessageFd { bytes, .. })) => {
                let fd = embeds.borrow_mut().take_fd();
                match (ToParent::decode(&bytes), fd) {
                    (Some(ToParent::File { name, size, write: w }), Some(fd)) => {
                        let mut f = std::fs::File::from(fd);
                        let line = first_line(&mut f);
                        eprintln!("pick-demo: file {name} {size} {line}");
                        lines.push(format!("A file: “{name}”, {size} bytes. It begins: {line}"));
                        if w {
                            let r = f.seek(std::io::SeekFrom::End(0)).and_then(|_| f.write_all(b"added by the app\n"));
                            eprintln!("pick-demo: wrote to {name}: {}", if r.is_ok() { "ok".to_string() } else { format!("{r:?}") });
                        } else if f.write_all(b"x").is_ok() {
                            eprintln!("pick-demo: FAIL could write to a read-only pick");
                        }
                    }
                    other => eprintln!("pick-demo: failed odd message with a descriptor ({:?})", other.0),
                }
            }
            Ok(Some(ParentEvent::Message { bytes, .. })) => match ToParent::decode(&bytes) {
                Some(ToParent::Folder { at, name, read_only }) => {
                    let mut entries: Vec<String> = std::fs::read_dir(&at).map(|d| d.flatten().filter_map(|e| e.file_name().into_string().ok()).collect()).unwrap_or_else(|e| vec![format!("<{e}>")]);
                    entries.sort();
                    eprintln!("pick-demo: folder {at} {}", entries.join(","));
                    lines.push(format!("The folder “{name}”, at {at}{}: {}", if read_only { " (read-only)" } else { "" }, entries.join(", ")));
                    if !read_only {
                        let r = std::fs::write(std::path::Path::new(&at).join("from-the-app.txt"), "the app wrote this\n");
                        eprintln!("pick-demo: wrote into {at}: {}", if r.is_ok() { "ok".to_string() } else { format!("{r:?}") });
                    }
                }
                Some(ToParent::Done { files }) => {
                    eprintln!("pick-demo: done ({files} files)");
                    if let Some(e) = embed {
                        let _ = embeds.borrow_mut().detach_embed(e);
                    }
                    shown.set(Shown { asking: false, lines: lines.clone() });
                }
                Some(ToParent::Cancelled) => {
                    eprintln!("pick-demo: cancelled");
                    if let Some(e) = embed {
                        let _ = embeds.borrow_mut().detach_embed(e);
                    }
                    shown.set(Shown { asking: false, lines: vec!["You cancelled the pick.".into()] });
                }
                Some(ToParent::Failed { why }) => eprintln!("pick-demo: failed {why}"),
                _ => eprintln!("pick-demo: an odd message from the picker"),
            },
            Ok(Some(ParentEvent::ChildLost { .. })) => eprintln!("pick-demo: the picker went"),
            Ok(_) => {}
            Err(e) => {
                eprintln!("pick-demo: failed limend ({e:?})");
                break;
            }
        }
        if until.is_some_and(|u| Instant::now() >= u) {
            break;
        }
    }
}
