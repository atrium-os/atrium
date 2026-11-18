//! {{title}}: keeps the text people share to it (template: service).
//!
//! An app that provides a role (atrium.toml's `[roles] implements`). When
//! the person picks {{title}} in another app's share sheet, Limen starts
//! this program in a jail of its own, with no capabilities, its Limen
//! channel on standard input and a connection to the display that shows
//! its window only inside the sharing app's sheet. It gets one item,
//! shows it, and answers Keep (`Accepted`) or Discard (`Declined`).
//!
//! The item is hostile input, as any paste: text is shown and kept as
//! text, never parsed as anything else. To take pictures or documents,
//! add their types to `[roles.share-target] mime` and decode them in a
//! worker (template: viewer).
//!
//! It can't be started on its own: share something to it from another
//! app (the share-sheet crate's share-demo does) after `cargo atrium run`
//! has installed it.

use std::io::Write;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use limen::proto::ChildEvent;
use limen::share_target::{Incoming, Received, ToParent};
use pergola::geom::Rect;
use pergola::node::Node;
use pergola::theme::{font, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::widgets::Button;
use pergola::window::StepResult;
use pergola::{App, Color, Mutable, SizeSpec, TextStyle, Window, WindowDesc};

const W: f32 = 560.0;
const H: f32 = 400.0;
/// The most text it takes from one share.
const MAX_TEXT: u64 = 64 << 10;

#[derive(Clone, Default)]
struct Shown {
    title: String,
    text: String,
    /// The item is whole and not answered yet.
    asking: bool,
    /// Keep (true) or Discard (false), once pressed.
    answer: Option<bool>,
    note: String,
}

struct Target {
    shown: Mutable<Shown>,
}

fn text(ctx: &mut Ctx, y: f32, s: String, size: f32, weight: Weight) {
    ctx.add(Node::Text {
        rect: Rect::new(24.0, y, 0.0, 0.0),
        content: s,
        style: TextStyle { family: font::SANS.into(), size, weight, color: Color::rgba(0.1, 0.1, 0.12, 1.0) },
    });
}

impl View for Target {
    fn render(&self, ctx: &mut Ctx) {
        let s = self.shown.get_cloned();
        text(ctx, 20.0, format!("Keep in {{title}}: {}", s.title), type_size::LG, Weight::Semibold);
        for (i, line) in s.text.lines().take(10).enumerate() {
            text(ctx, 60.0 + 22.0 * i as f32, line.chars().take(80).collect(), type_size::SM, Weight::Regular);
        }
        text(ctx, H - 90.0, s.note.clone(), type_size::SM, Weight::Regular);
        if s.asking && s.answer.is_none() {
            for (x, label, keep) in [(W - 232.0, "Discard", false), (W - 120.0, "Keep", true)] {
                let shown = self.shown.clone();
                let b = if keep { Button::primary(label) } else { Button::secondary(label) };
                b.at(x, H - 56.0).width(96.0).on_click(move || {
                    let mut s = shown.get_cloned();
                    if s.answer.is_none() {
                        s.answer = Some(keep);
                        shown.set(s);
                    }
                }).render(ctx);
            }
        }
    }
}

fn die(why: impl std::fmt::Display) -> ! {
    eprintln!("{{name}}: {why}");
    std::process::exit(1)
}

/// The item's text, if it has some: at most MAX_TEXT bytes, bad UTF-8
/// replaced, control characters (but new lines) dropped.
fn text_of(r: &Received) -> Option<String> {
    let i = r.best(&["text/plain".to_string(), "text/*".to_string()])?;
    let b = r.bytes(i, MAX_TEXT).ok()?;
    Some(String::from_utf8_lossy(&b).chars().filter(|c| *c == '\n' || !c.is_control()).collect())
}

/// Keep it: appended to kept.txt in the app's own folder.
fn keep(title: &str, text: &str) -> std::io::Result<std::path::PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| std::io::Error::other("no home folder"))?;
    let path = std::path::Path::new(&home).join("kept.txt");
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(f, "## {title}\n{text}\n")?;
    Ok(path)
}

fn main() {
    let (mut child, fresco) = limen::client::Child::embedded().unwrap_or_else(|e| die(format_args!("no Limen channel (share something to it from another app): {e:?}")));
    let embed = loop {
        match child.poll(Duration::from_secs(30)) {
            Ok(Some(ChildEvent::Attach { embed, role, .. })) if role == "share-target" => break embed,
            Ok(Some(ChildEvent::Attach { role, .. })) => die(format_args!("asked to be {role}")),
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => die("Limen never attached it"),
        }
    };
    // The window shows only in the sharing app's sheet.
    let conn = fresco_client::Connection::wrap(fresco).unwrap_or_else(|e| die(format_args!("display: {e}")));
    let desc = WindowDesc {
        hints: fresco_protocol::WindowHints { embed_pending: true, server_decorations: Some(false), ..Default::default() },
        ..WindowDesc::new("{{title}}", SizeSpec::Fixed(W as u32, H as u32))
    };
    let mut win = Window::open_on(conn, &desc).unwrap_or_else(|e| die(format_args!("window: {e}")));
    let dirty = Arc::new(AtomicBool::new(true));
    let shown = Mutable::with_dirty(Shown { note: "Waiting for the item…".into(), ..Default::default() }, dirty.clone());
    let mut app = App::new_with_flag(Target { shown: shown.clone() }, dirty);
    let _ = win.paint(&mut app);
    let mut incoming = Incoming::default();
    let mut ready = false;
    let started = Instant::now();
    loop {
        match win.step(&mut app, Some(Duration::from_millis(30))) {
            Ok(StepResult::Closed) | Err(_) => break,
            _ => {}
        }
        // Tell Limen which window to put in the sheet, once the display
        // has named it.
        if !ready {
            if let Some(token) = win.embed_token() {
                ready = true;
                if child.ready(embed, token).is_err() {
                    die("Limen refused the window");
                }
            } else if started.elapsed() > Duration::from_secs(10) {
                die("no window token from the display");
            }
        }
        // The item: a Share, then a Part with a descriptor for each large
        // format. Anything out of shape is declined.
        let ok = match child.poll(Duration::from_millis(1)) {
            Ok(Some(ChildEvent::Message { bytes, .. })) => incoming.message(&bytes),
            Ok(Some(ChildEvent::MessageFd { bytes, .. })) => child.take_fd().is_some_and(|fd| incoming.message_fd(&bytes, fd)),
            Ok(Some(ChildEvent::Detaching { .. })) | Err(_) => break,
            _ => true,
        };
        if !ok {
            let _ = child.emit(embed, ToParent::Declined.encode());
        }
        if let Some(r) = incoming.take() {
            let (text, note) = match text_of(&r) {
                Some(t) => (t, String::new()),
                None => (String::new(), "There is no text in it to keep.".into()),
            };
            shown.set(Shown { title: r.item.title.clone(), asking: !text.is_empty(), text, note, answer: None });
        }
        let s = shown.get_cloned();
        if let Some(keep_it) = s.answer {
            let (m, note) = match keep_it.then(|| keep(&s.title, &s.text)) {
                Some(Ok(p)) => (ToParent::Accepted, format!("Kept in {}", p.display())),
                Some(Err(e)) => (ToParent::Declined, format!("It couldn't be kept: {e}")),
                None => (ToParent::Declined, "Discarded.".into()),
            };
            let _ = child.emit(embed, m.encode());
            shown.set(Shown { asking: false, answer: None, note, ..s });
        }
    }
}
