//! share-demo: an app that shares (share-sheet.md §5), the way any app
//! does, for the VM gate and as the example.
//!
//! It uses Pergola's share button and `pergola::share::Sharing`, which do:
//!
//! 1. A press on Share: embed `share` (the system's share sheet) in a sheet
//!    slot and send it the press, a title and the types the app has.
//! 2. The sheet answers `Chosen` (or `Cancelled`, `Failed`): detach it and
//!    embed `share-target`, which limend starts from the person's choice.
//! 3. Send the item: text inline, a file (`SHARE_FILE`) by descriptor. The
//!    target answers `Accepted` or `Declined`.
//!
//! The app never learns which apps could take it, nor which one did.
//!
//!   SHARE_TEXT=...        [a line of notes] the text shared
//!   SHARE_FILE=path       also share this file by descriptor (else
//!                         `share-me.png` in the bundle, else a small
//!                         one it writes in its home)
//!   SHARE_FILE_MIME=type  [image/png] its type
//!   SHARE_UNASKED=1       open the sheet at start, without a press (the
//!                         gate's check that the sheet refuses it)
//!   SHARE_SECS=n          exit after n seconds
//!   FRESCO_SOCKET, LIMEN_SOCKET as usual.
//!
//! Logs `share-demo: sheet failed WHY`, `share-demo: chosen`,
//! `share-demo: cancelled`, `share-demo: accepted` / `declined`.

use std::os::fd::OwnedFd;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use limen::share_target::{Body, Format, Item};
use pergola::clipboard::Token;
use pergola::embed::Embeds;
use pergola::geom::Rect;
use pergola::node::Node;
use pergola::share::{Outcome, Sharing};
use pergola::theme::{font, size::em, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::window::StepResult;
use pergola::{App, Color, Mutable, SizeSpec, TextStyle, Window, WindowDesc};

/// The picture shared when nothing else is: a PNG signature and a mark.
const PICTURE: &[u8] = b"\x89PNG\r\n\x1a\n-a-picture-";

/// The sheet's slot, then the target's.
const SLOTS: (u64, u64) = (1, 2);

struct Demo {
    status: Mutable<String>,
    /// The press on Share, waiting for the main loop.
    pressed: Mutable<Option<Token>>,
    /// The slot to draw: the sheet's or the target's while a share is on.
    slot: Mutable<Option<u64>>,
    text: String,
    size: (f32, f32),
}

fn style(size: f32, weight: Weight, color: Color) -> TextStyle {
    TextStyle { family: font::SANS.into(), size, weight, color }
}

impl View for Demo {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;
        let (w, h) = self.size;
        ctx.push(Node::Rect { rect: Rect::new(0.0, 0.0, w, h), fill: t.bg_canvas(), radius: 0.0 });
        ctx.add(Node::Text { rect: Rect::new(24.0, em(14.0), 0.0, em(24.0)), content: "Notes (a demo app)".into(), style: style(type_size::LG, Weight::Semibold, t.text_primary()) });
        ctx.add(Node::Text { rect: Rect::new(24.0, em(56.0), 0.0, em(20.0)), content: self.text.clone(), style: style(type_size::SM, Weight::Regular, t.text_primary()) });
        ctx.add(Node::Text { rect: Rect::new(24.0, em(90.0), 0.0, em(20.0)), content: self.status.get_cloned(), style: style(type_size::SM, Weight::Regular, t.text_secondary()) });
        pergola::share::button(self.pressed.clone()).at(w - 120.0, em(12.0)).width(96.0).render(ctx);
        if let Some(key) = self.slot.get_cloned() {
            // The system places the sheet; this rect is only where the slot is.
            let id = ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, 1.0, 1.0), fill: Color::TRANSPARENT, radius: 0.0 });
            ctx.slot(id, key);
        }
        ctx.pop();
    }
}

fn main() {
    let text = std::env::var("SHARE_TEXT").unwrap_or_else(|_| "Groceries: rice, lentils, coriander".into());
    // Without SHARE_FILE, a `share-me.png` in the app's own bundle (beside
    // `bin/`), else one it writes in its home: how the jailed gate, which
    // can't set the environment (and whose Run installs only `bin/`), gets a
    // picture to share.
    let file = std::env::var("SHARE_FILE").ok().or_else(|| {
        let p = std::env::current_exe().ok()?.parent()?.parent()?.join("share-me.png");
        p.exists().then(|| p.to_string_lossy().into_owned())
    }).or_else(|| {
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let p = home.join("share-me.png");
        std::fs::write(&p, PICTURE).map_err(|e| eprintln!("share-demo: {}: {e}", p.display())).ok()?;
        Some(p.to_string_lossy().into_owned())
    });
    let file_mime = std::env::var("SHARE_FILE_MIME").unwrap_or_else(|_| "image/png".into());
    let until = std::env::var("SHARE_SECS").ok().and_then(|s| s.parse::<u64>().ok()).map(|s| Instant::now() + Duration::from_secs(s));
    let mut unasked = std::env::var_os("SHARE_UNASKED").is_some();

    let mut item = Item::text(text.lines().next().unwrap_or("Notes"), &text);
    if let Some(p) = &file {
        let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        item.formats.insert(0, Format { mime: file_mime, body: Body::Fd { size } });
    }
    // A fresh descriptor for each share: a sent one is the target's.
    let files = || -> Vec<(u8, OwnedFd)> {
        file.iter().filter_map(|p| std::fs::File::open(p).map_err(|e| eprintln!("share-demo: {p}: {e}")).ok()).map(|f| (0u8, OwnedFd::from(f))).collect()
    };

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
    let status = Mutable::with_dirty("Press Share to send this note to another app.".to_string(), dirty.clone());
    let pressed = Mutable::with_dirty(None, dirty.clone());
    let slot = Mutable::with_dirty(None, dirty.clone());
    let mut app = App::new_with_flag(Demo { status: status.clone(), pressed: pressed.clone(), slot: slot.clone(), text: text.clone(), size: (win.width, win.height) }, dirty);
    let _ = win.paint(&mut app);
    let mut sharing: Option<Sharing> = None;
    loop {
        match win.step(&mut app, Some(Duration::from_millis(30))) {
            Ok(StepResult::Closed) | Err(_) => break,
            _ => {}
        }
        let token = win.embed_token();
        // A press on Share (or, for the gate, none at all).
        let press = pressed.get_cloned().or_else(|| (unasked && token.is_some()).then_some([0u8; 16]));
        if let (Some(press), Some(token), None) = (press, token, sharing.as_ref()) {
            unasked = false;
            pressed.set(None);
            match Sharing::begin(&embeds, token, press, item.clone(), files(), SLOTS) {
                Ok(s) => {
                    eprintln!("share-demo: asked for the share sheet");
                    status.set("Choose where to send it.".into());
                    sharing = Some(s);
                }
                Err(e) => eprintln!("share-demo: sheet failed no sheet ({e:?})"),
            }
        }
        let ev = embeds.borrow_mut().poll(Duration::from_millis(1));
        match ev {
            Ok(Some(ev)) => {
                if let Some(s) = sharing.as_mut() {
                    let was = s.slot();
                    let out = s.event(&embeds, &ev);
                    if was == Some(SLOTS.0) && s.slot() == Some(SLOTS.1) {
                        eprintln!("share-demo: chosen");
                        status.set("Sending…".into());
                    }
                    if let Some(out) = out {
                        let (log, shown) = match out {
                            Outcome::Shared => ("accepted".to_string(), "Shared."),
                            Outcome::NotShared if was == Some(SLOTS.0) => ("cancelled".to_string(), "Not shared."),
                            Outcome::NotShared => ("declined".to_string(), "Not shared."),
                            Outcome::Failed(why) => (format!("sheet failed {why}"), "Not shared."),
                        };
                        eprintln!("share-demo: {log}");
                        status.set(shown.into());
                        sharing = None;
                    }
                }
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("share-demo: limend ({e:?})");
                break;
            }
        }
        let now = sharing.as_ref().and_then(|s| s.slot());
        if slot.get_cloned() != now {
            slot.set(now);
        }
        if until.is_some_and(|u| Instant::now() >= u) {
            break;
        }
    }
}
