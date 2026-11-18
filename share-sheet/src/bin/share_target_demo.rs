//! share-target-demo: an app that takes shares (share-sheet.md §4), the
//! way any app does, and the shape of the SDK's `service` template.
//!
//! Its manifest says what it takes:
//!
//! ```toml
//! [roles]
//! implements = ["share-target"]
//!
//! [roles.share-target]
//! mime = ["text/*", "image/*"]
//! ```
//!
//! limend starts it in a jail of its own when the person chooses it in the
//! share sheet, with its limend channel on stdin and an embed-only frescod
//! connection as the first thing on it. It opens one window (shown only in
//! the sharing app's sheet slot), waits for the whole item, shows it, and
//! answers `Accepted` (Keep) or `Declined` (Discard).
//!
//!   SHARE_TARGET_AUTO=accept|decline   answer as soon as the item is whole
//!
//! Logs `share-target-demo: got "TITLE" N formats MIME=SIZE...`,
//! `share-target-demo: text <first line>`, `share-target-demo: accepted`
//! or `declined`.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use limen::proto::ChildEvent;
use limen::share_target::{Incoming, Received, ToParent};
use pergola::geom::Rect;
use pergola::node::Node;
use pergola::theme::{font, size::em, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::window::StepResult;
use pergola::{App, Color, Mutable, SizeSpec, TextStyle, Window, WindowDesc};

const W: f32 = 560.0;
const H: f32 = 440.0;
/// Bytes of text it reads to show.
const SHOW: u64 = 4096;

#[derive(Clone, Default)]
struct Shown {
    lines: Vec<String>,
    whole: bool,
    /// Keep (true) or Discard (false), once pressed.
    answer: Option<bool>,
}

struct Target {
    shown: Mutable<Shown>,
}

fn style(size: f32, weight: Weight, color: Color) -> TextStyle {
    TextStyle { family: font::SANS.into(), size, weight, color }
}

impl View for Target {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;
        let s = self.shown.get_cloned();
        ctx.push(Node::Rect { rect: Rect::new(0.0, 0.0, W, H), fill: t.bg_elevated(), radius: 0.0 });
        ctx.add(Node::Text { rect: Rect::new(24.0, em(18.0), 0.0, em(24.0)), content: "Keep in Scraps (a demo target)".into(), style: style(type_size::LG, Weight::Semibold, t.text_primary()) });
        for (i, l) in s.lines.iter().take(10).enumerate() {
            ctx.add(Node::Text { rect: Rect::new(24.0, em(56.0 + i as f32 * 22.0), 0.0, em(20.0)), content: l.clone(), style: style(type_size::SM, Weight::Regular, t.text_primary()) });
        }
        if s.whole && s.answer.is_none() {
            for (x, label, keep) in [(W - 232.0, "Discard", false), (W - 120.0, "Keep", true)] {
                let b = ctx.push(Node::Rect { rect: Rect::new(x, H - em(52.0), 96.0, em(32.0)), fill: if keep { t.accent_fill() } else { t.bg_surface() }, radius: 6.0 });
                let shown = self.shown.clone();
                ctx.on_click(b, move || {
                    let mut s = shown.get_cloned();
                    if s.answer.is_none() {
                        s.answer = Some(keep);
                        shown.set(s);
                    }
                });
                ctx.add(Node::Text { rect: Rect::new(x + 22.0, H - em(46.0), 0.0, em(20.0)), content: label.into(), style: style(type_size::SM, Weight::Medium, if keep { t.text_on_accent() } else { t.text_primary() }) });
                ctx.pop();
            }
        }
        ctx.pop();
    }
}

fn die(why: impl std::fmt::Display) -> ! {
    eprintln!("share-target-demo: {why}");
    std::process::exit(1)
}

/// What to show of an item, and what the log says.
fn describe(r: &Received) -> Vec<String> {
    let it = &r.item;
    let sizes: Vec<String> = it.formats.iter().enumerate().map(|(i, f)| format!("{}={}", f.mime, r.bytes(i, u64::MAX).map_or(0, |b| b.len()))).collect();
    eprintln!("share-target-demo: got {:?} {} formats {}", it.title, it.formats.len(), sizes.join(" "));
    let mut lines = vec![format!("“{}”", it.title)];
    lines.extend(sizes.iter().map(|s| format!("· {s} bytes")));
    // Text is shown as text, never parsed as anything else.
    if let Some(i) = r.best(&["text/plain".to_string()]) {
        let b = r.bytes(i, SHOW).unwrap_or_default();
        let text = String::from_utf8_lossy(&b);
        let first: String = text.lines().next().unwrap_or("").chars().filter(|c| !c.is_control()).take(80).collect();
        eprintln!("share-target-demo: text {first}");
        lines.push(String::new());
        lines.push(first);
    }
    lines
}

fn main() {
    let (mut child, fresco) = limen::client::Child::embedded().unwrap_or_else(|e| die(format_args!("no limend channel: {e:?}")));
    let embed = loop {
        match child.poll(Duration::from_secs(30)) {
            Ok(Some(ChildEvent::Attach { embed, role, parent_app, .. })) if role == "share-target" => {
                eprintln!("share-target-demo: a share from {parent_app}");
                break embed;
            }
            Ok(Some(ChildEvent::Attach { role, .. })) => die(format_args!("asked to be {role}")),
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => die("limend never attached us"),
        }
    };
    let conn = fresco_client::Connection::wrap(fresco).unwrap_or_else(|e| die(format_args!("frescod: {e}")));
    let desc = WindowDesc {
        hints: fresco_protocol::WindowHints { embed_pending: true, server_decorations: Some(false), ..Default::default() },
        ..WindowDesc::new("Keep in Scraps", SizeSpec::Fixed(W as u32, H as u32))
    };
    let mut win = Window::open_on(conn, &desc).unwrap_or_else(|e| die(format_args!("window: {e}")));
    let dirty = Arc::new(AtomicBool::new(true));
    let shown = Mutable::with_dirty(Shown { lines: vec!["Waiting for the item…".into()], ..Default::default() }, dirty.clone());
    let mut app = App::new_with_flag(Target { shown: shown.clone() }, dirty);
    let _ = win.paint(&mut app);
    let auto = std::env::var("SHARE_TARGET_AUTO").ok();
    let mut incoming = Incoming::default();
    let mut ready = false;
    let started = Instant::now();
    loop {
        match win.step(&mut app, Some(Duration::from_millis(30))) {
            Ok(StepResult::Closed) | Err(_) => break,
            _ => {}
        }
        if !ready {
            if let Some(token) = win.embed_token() {
                ready = true;
                if child.ready(embed, token).is_err() {
                    die("limend refused ready");
                }
            }
        }
        match child.poll(Duration::from_millis(1)) {
            Ok(Some(ChildEvent::Message { bytes, .. })) => {
                if !incoming.message(&bytes) {
                    eprintln!("share-target-demo: an odd message, declined");
                    let _ = child.emit(embed, ToParent::Declined.encode());
                }
            }
            Ok(Some(ChildEvent::MessageFd { bytes, .. })) => {
                let ok = child.take_fd().is_some_and(|fd| incoming.message_fd(&bytes, fd));
                if !ok {
                    eprintln!("share-target-demo: an odd part, declined");
                    let _ = child.emit(embed, ToParent::Declined.encode());
                }
            }
            Ok(Some(ChildEvent::Detaching { .. })) | Err(_) => break,
            _ => {}
        }
        if let Some(r) = incoming.take() {
            let lines = describe(&r);
            let answer = match auto.as_deref() {
                Some("accept") => Some(true),
                Some("decline") => Some(false),
                _ => None,
            };
            shown.set(Shown { lines, whole: true, answer });
        }
        if let Some(keep) = shown.get_cloned().answer {
            let m = if keep { ToParent::Accepted } else { ToParent::Declined };
            let _ = child.emit(embed, m.encode());
            eprintln!("share-target-demo: {}", if keep { "accepted" } else { "declined" });
            let mut s = shown.get_cloned();
            s.answer = None;
            s.whole = false;
            s.lines.push(if keep { "Kept.".into() } else { "Discarded.".into() });
            shown.set(s);
        }
        if !ready && started.elapsed() > Duration::from_secs(10) {
            die("no embed token from frescod");
        }
    }
}
