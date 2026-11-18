//! share-demo: an app that shares (share-sheet.md §5), the way any app
//! does, for the VM gate and as the example.
//!
//! 1. A press on Share: the app embeds `share` (the system's share sheet)
//!    in a sheet slot and sends it the press, a title and the types it has.
//! 2. The sheet answers `Chosen` (or `Cancelled`, `Failed`): the app
//!    detaches it and embeds `share-target`, which limend starts from the
//!    person's choice.
//! 3. The app sends the item: text inline, a file (`SHARE_FILE`) by
//!    descriptor. The target answers `Accepted` or `Declined`.
//!
//! The app never learns which apps could take it, nor which one did.
//!
//!   SHARE_TEXT=...        [a line of notes] the text shared
//!   SHARE_FILE=path       also share this file by descriptor
//!   SHARE_FILE_MIME=type  [image/png] its type
//!   SHARE_UNASKED=1       open the sheet at start, without a press (the
//!                         gate's check that the sheet refuses it)
//!   SHARE_SECS=n          exit after n seconds
//!   FRESCO_SOCKET, LIMEN_SOCKET as usual.
//!
//! Logs `share-demo: sheet failed WHY`, `share-demo: chosen`,
//! `share-demo: cancelled`, `share-demo: accepted` / `declined`.

use std::os::fd::AsFd;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use limen::proto::{EmbedId, ParentEvent};
use limen::share_target::{Body, Format, Item};
use pergola::embed::Embeds;
use pergola::geom::Rect;
use pergola::node::Node;
use pergola::theme::{font, size::em, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::window::StepResult;
use pergola::{App, Color, Mutable, SizeSpec, TextStyle, Window, WindowDesc};

const SHEET_SLOT: u64 = 1;
const TARGET_SLOT: u64 = 2;

#[derive(Clone, Default)]
struct Shown {
    status: String,
    /// The press on Share, waiting for the main loop.
    press: Option<[u8; 16]>,
    /// A sheet or a target is up: its slot is drawn.
    slot: Option<u64>,
}

struct Demo {
    shown: Mutable<Shown>,
    text: String,
    size: (f32, f32),
}

fn style(size: f32, weight: Weight, color: Color) -> TextStyle {
    TextStyle { family: font::SANS.into(), size, weight, color }
}

impl View for Demo {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;
        let s = self.shown.get_cloned();
        let (w, h) = self.size;
        ctx.push(Node::Rect { rect: Rect::new(0.0, 0.0, w, h), fill: t.bg_canvas(), radius: 0.0 });
        ctx.add(Node::Text { rect: Rect::new(24.0, em(14.0), 0.0, em(24.0)), content: "Notes (a demo app)".into(), style: style(type_size::LG, Weight::Semibold, t.text_primary()) });
        ctx.add(Node::Text { rect: Rect::new(24.0, em(56.0), 0.0, em(20.0)), content: self.text.clone(), style: style(type_size::SM, Weight::Regular, t.text_primary()) });
        ctx.add(Node::Text { rect: Rect::new(24.0, em(90.0), 0.0, em(20.0)), content: s.status.clone(), style: style(type_size::SM, Weight::Regular, t.text_secondary()) });
        let b = ctx.push(Node::Rect { rect: Rect::new(w - 120.0, em(12.0), 96.0, em(32.0)), fill: t.accent_fill(), radius: 6.0 });
        let shown = self.shown.clone();
        ctx.on_click(b, move || {
            // The press that asked: frescod minted it for this window.
            if let Some(tok) = pergola::clipboard::current_activation() {
                let mut s = shown.get_cloned();
                if s.slot.is_none() {
                    s.press = Some(tok);
                    shown.set(s);
                }
            }
        });
        ctx.add(Node::Text { rect: Rect::new(w - 98.0, em(18.0), 0.0, em(20.0)), content: "Share".into(), style: style(type_size::SM, Weight::Medium, t.text_on_accent()) });
        ctx.pop();
        if let Some(key) = s.slot {
            // The system places the sheet; this rect is only where the slot is.
            let id = ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, 1.0, 1.0), fill: Color::TRANSPARENT, radius: 0.0 });
            ctx.slot(id, key);
        }
        ctx.pop();
    }
}

fn set_status(shown: &Mutable<Shown>, status: &str, slot: Option<u64>) {
    let mut s = shown.get_cloned();
    s.status = status.into();
    s.slot = slot;
    shown.set(s);
}

fn main() {
    let text = std::env::var("SHARE_TEXT").unwrap_or_else(|_| "Groceries: rice, lentils, coriander".into());
    let file = std::env::var("SHARE_FILE").ok().and_then(|p| std::fs::File::open(&p).map_err(|e| eprintln!("share-demo: {p}: {e}")).ok());
    let file_mime = std::env::var("SHARE_FILE_MIME").unwrap_or_else(|_| "image/png".into());
    let until = std::env::var("SHARE_SECS").ok().and_then(|s| s.parse::<u64>().ok()).map(|s| Instant::now() + Duration::from_secs(s));
    let mut unasked = std::env::var_os("SHARE_UNASKED").is_some();

    let mut item = Item::text(text.lines().next().unwrap_or("Notes"), &text);
    if let Some(f) = &file {
        let size = f.metadata().map(|m| m.len()).unwrap_or(0);
        item.formats.insert(0, Format { mime: file_mime, body: Body::Fd { size } });
    }

    let mut win = Window::open(&WindowDesc::new("Notes", SizeSpec::Fixed(760, 540))).unwrap_or_else(|e| {
        eprintln!("share-demo: frescod: {e}");
        std::process::exit(1)
    });
    let embeds = Embeds::connect().unwrap_or_else(|e| {
        eprintln!("share-demo: limend: {e:?}");
        std::process::exit(1)
    });
    win.set_slot_binder(Embeds::binder(&embeds));
    let dirty = Arc::new(AtomicBool::new(true));
    let shown = Mutable::with_dirty(Shown { status: "Press Share to send this note to another app.".into(), ..Default::default() }, dirty.clone());
    let mut app = App::new_with_flag(Demo { shown: shown.clone(), text: text.clone(), size: (win.width, win.height) }, dirty);
    let _ = win.paint(&mut app);
    let mut sheet: Option<EmbedId> = None;
    let mut target: Option<EmbedId> = None;
    loop {
        match win.step(&mut app, Some(Duration::from_millis(30))) {
            Ok(StepResult::Closed) | Err(_) => break,
            _ => {}
        }
        let token = win.embed_token();
        // A press on Share (or, for the gate, none at all).
        let press = shown.get_cloned().press.or_else(|| (unasked && token.is_some()).then_some([0u8; 16]));
        if let (Some(press), Some(token)) = (press, token) {
            unasked = false;
            let mut s = shown.get_cloned();
            s.press = None;
            shown.set(s);
            match embeds.borrow_mut().request("share", token, SHEET_SLOT) {
                Ok(e) => {
                    let open = limen::share::Open { activation: press, title: item.title.clone(), mimes: item.mimes() };
                    let _ = embeds.borrow_mut().send(e, limen::share::ToChild::Open(open).encode());
                    sheet = Some(e);
                    set_status(&shown, "Choose where to send it.", Some(SHEET_SLOT));
                    eprintln!("share-demo: asked for the share sheet (embed {e})");
                }
                Err(e) => eprintln!("share-demo: sheet failed no sheet ({e:?})"),
            }
        }
        let ev = embeds.borrow_mut().poll(Duration::from_millis(1));
        match ev {
            Ok(Some(ParentEvent::Message { embed, bytes })) if Some(embed) == sheet => {
                let _ = embeds.borrow_mut().detach_embed(embed);
                sheet = None;
                match limen::share::ToParent::decode(&bytes) {
                    Some(limen::share::ToParent::Chosen) => {
                        eprintln!("share-demo: chosen");
                        let Some(token) = token else { continue };
                        match embeds.borrow_mut().request("share-target", token, TARGET_SLOT) {
                            Ok(t) => {
                                let mut e = embeds.borrow_mut();
                                let _ = e.send(t, limen::share_target::ToChild::Share(item.clone()).encode());
                                for i in item.fd_parts() {
                                    if let Some(f) = &file {
                                        if let Err(err) = e.send_fd(t, limen::share_target::ToChild::Part { index: i }.encode(), f.as_fd()) {
                                            eprintln!("share-demo: part {i} refused ({err:?})");
                                        }
                                    }
                                }
                                target = Some(t);
                                set_status(&shown, "Sending…", Some(TARGET_SLOT));
                            }
                            Err(err) => {
                                eprintln!("share-demo: target failed ({err:?})");
                                set_status(&shown, "That app couldn't be started.", None);
                            }
                        }
                    }
                    Some(limen::share::ToParent::Cancelled) => {
                        eprintln!("share-demo: cancelled");
                        set_status(&shown, "Not shared.", None);
                    }
                    Some(limen::share::ToParent::Failed { why }) => {
                        eprintln!("share-demo: sheet failed {why}");
                        set_status(&shown, "Not shared.", None);
                    }
                    None => eprintln!("share-demo: an odd message from the sheet"),
                }
            }
            Ok(Some(ParentEvent::Message { embed, bytes })) if Some(embed) == target => {
                let _ = embeds.borrow_mut().detach_embed(embed);
                target = None;
                match limen::share_target::ToParent::decode(&bytes) {
                    Some(limen::share_target::ToParent::Accepted) => {
                        eprintln!("share-demo: accepted");
                        set_status(&shown, "Shared.", None);
                    }
                    _ => {
                        eprintln!("share-demo: declined");
                        set_status(&shown, "Not shared.", None);
                    }
                }
            }
            Ok(Some(ParentEvent::ChildLost { embed })) => {
                eprintln!("share-demo: embed {embed} went");
                if Some(embed) == target || Some(embed) == sheet {
                    target = None;
                    sheet = None;
                    set_status(&shown, "Not shared.", None);
                }
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("share-demo: limend ({e:?})");
                break;
            }
        }
        if until.is_some_and(|u| Instant::now() >= u) {
            break;
        }
    }
}
