//! share-sheet: Limen's `share` provider (docs/spec/share-sheet.md).
//!
//! Runs as `_share`, the uid frescod lets spend `SHARE` presses. Each sheet
//! an app opens gets a window of its own on its own thread, which frescod
//! keeps hidden until limend binds it into the app's sheet slot; there the
//! app can neither draw over it nor read it.
//!
//! Environment (defaults in brackets):
//!   FRESCO_SOCKET               [/tmp/frescod.sock]
//!   LIMEN_SOCKET                [/atrium/sockets/limen/limend.sock]
//!   SHARE_FRESCOD_ACTIVATION    [/var/run/atrium/fresco-activation.sock]
//!   SHARE_APP_REGISTRY          [the launch registry]
//!   SHARE_APPS                  [/var/lib/atrium/apps] installed manifests
//!   SHARE_AUTO_PICK             [unset] tests only: choose this app id as
//!                               soon as it is shown, as if pressed
//!   SHARE_SECS                  [unset] exit after this many seconds

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use limen::client::Child;
use limen::manifests::{Installed, Target};
use limen::proto::EmbedId;
use pergola::geom::Rect;
use pergola::node::Node;
use pergola::theme::{font, size::em, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::window::StepResult;
use pergola::{App, Color, Mutable, SizeSpec, TextStyle, Window, WindowDesc};
use share_sheet::{Event, Sheet};

fn env(k: &str, d: &str) -> String {
    std::env::var(k).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| d.to_string())
}

const W: f32 = 480.0;
const H: f32 = 400.0;

/// From a sheet's thread to the limend thread.
enum FromPane {
    Ready(EmbedId, [u8; 16]),
    Chose(EmbedId, String),
    Gone(EmbedId),
}

#[derive(Clone, Default)]
struct Shown {
    title: String,
    from_app: String,
    targets: Vec<Target>,
    /// What the person pressed: an app id, or "" for Cancel.
    pressed: Option<String>,
}

struct Pane {
    shown: Mutable<Shown>,
}

fn style(size: f32, weight: Weight, color: Color) -> TextStyle {
    TextStyle { family: font::SANS.into(), size, weight, color }
}

/// `s` cut to `max` characters, with an ellipsis.
fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() } else { s.chars().take(max - 1).collect::<String>() + "…" }
}

impl View for Pane {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;
        let s = self.shown.get_cloned();
        ctx.push(Node::Rect { rect: Rect::new(0.0, 0.0, W, H), fill: t.bg_elevated(), radius: 0.0 });
        ctx.add(Node::Text { rect: Rect::new(24.0, em(18.0), 0.0, em(24.0)), content: format!("Share “{}”", cut(&s.title, 40)), style: style(type_size::LG, Weight::Semibold, t.text_primary()) });
        // Who is sharing: the app's registry id, from limend, not from the app.
        ctx.add(Node::Text { rect: Rect::new(24.0, em(46.0), 0.0, em(18.0)), content: format!("From {}", cut(&s.from_app, 48)), style: style(type_size::SM, Weight::Regular, t.text_secondary()) });
        let mut y = em(80.0);
        if s.targets.is_empty() {
            ctx.add(Node::Text { rect: Rect::new(24.0, y, 0.0, em(20.0)), content: "No app on this computer takes this.".into(), style: style(type_size::SM, Weight::Regular, t.text_primary()) });
        }
        for target in s.targets.iter().take(6) {
            let row = ctx.push(Node::Rect { rect: Rect::new(16.0, y, W - 32.0, em(44.0)), fill: Color::TRANSPARENT, radius: 8.0 });
            if ctx.is_hovered(row) || ctx.is_pressed(row) {
                let c = if ctx.is_pressed(row) { t.state_pressed() } else { t.state_hover() };
                if let Some(Node::Rect { fill, .. }) = ctx.tree.get_mut(row) {
                    *fill = c;
                }
            }
            let (shown, app) = (self.shown.clone(), target.app.clone());
            ctx.on_click(row, move || {
                let mut s = shown.get_cloned();
                if s.pressed.is_none() {
                    s.pressed = Some(app.clone());
                    shown.set(s);
                }
            });
            ctx.add(Node::Text { rect: Rect::new(28.0, y + em(6.0), 0.0, em(20.0)), content: cut(&target.name, 40), style: style(type_size::MD, Weight::Medium, t.text_primary()) });
            ctx.add(Node::Text { rect: Rect::new(28.0, y + em(26.0), 0.0, em(16.0)), content: format!("{} · takes {}", cut(&target.app, 36), target.takes.join(", ")), style: style(type_size::XS, Weight::Regular, t.text_tertiary()) });
            ctx.pop();
            y += em(48.0);
        }
        let b = ctx.push(Node::Rect { rect: Rect::new(W - 120.0, H - em(52.0), 96.0, em(32.0)), fill: t.bg_surface(), radius: 6.0 });
        let shown = self.shown.clone();
        ctx.on_click(b, move || {
            let mut s = shown.get_cloned();
            if s.pressed.is_none() {
                s.pressed = Some(String::new());
                shown.set(s);
            }
        });
        ctx.add(Node::Text { rect: Rect::new(W - 98.0, H - em(46.0), 0.0, em(20.0)), content: "Cancel".into(), style: style(type_size::SM, Weight::Medium, t.text_primary()) });
        ctx.pop();
        ctx.pop();
    }
}

/// One sheet's window, on its own thread.
fn pane(embed: EmbedId, fresco: String, first: Shown, rx: Receiver<()>, tx: Sender<FromPane>) {
    let desc = WindowDesc {
        hints: fresco_protocol::WindowHints { embed_pending: true, ..Default::default() },
        ..WindowDesc::new("Share", SizeSpec::Fixed(W as u32, H as u32))
    };
    let mut win = match fresco_client::Connection::connect(&fresco).and_then(|c| Window::open_on(c, &desc)) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("share-sheet: {embed}: window: {e}");
            let _ = tx.send(FromPane::Gone(embed));
            return;
        }
    };
    let auto = std::env::var("SHARE_AUTO_PICK").ok().filter(|a| first.targets.iter().any(|t| &t.app == a));
    let dirty = Arc::new(AtomicBool::new(true));
    let shown = Mutable::with_dirty(first, dirty.clone());
    let mut app = App::new_with_flag(Pane { shown: shown.clone() }, dirty);
    let _ = win.paint(&mut app);
    let mut ready = false;
    let mut sent = false;
    loop {
        match win.step(&mut app, Some(Duration::from_millis(30))) {
            Ok(StepResult::Closed) | Err(_) => break,
            _ => {}
        }
        if !ready {
            if let Some(token) = win.embed_token() {
                ready = true;
                let _ = tx.send(FromPane::Ready(embed, token));
                if let Some(a) = &auto {
                    eprintln!("share-sheet: {embed}: SHARE_AUTO_PICK {a}");
                    let mut s = shown.get_cloned();
                    s.pressed = Some(a.clone());
                    shown.set(s);
                }
            }
        }
        if !sent {
            if let Some(p) = shown.get_cloned().pressed {
                sent = true;
                let _ = tx.send(FromPane::Chose(embed, p));
            }
        }
        match rx.try_recv() {
            Ok(()) | Err(mpsc::TryRecvError::Disconnected) => break,
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }
    let _ = tx.send(FromPane::Gone(embed));
}

fn main() {
    let fresco = env("FRESCO_SOCKET", "/tmp/frescod.sock");
    let limen_sock = env("LIMEN_SOCKET", limen::client::DEFAULT_SOCKET);
    let act = env("SHARE_FRESCOD_ACTIVATION", "/var/run/atrium/fresco-activation.sock");
    let registry = env("SHARE_APP_REGISTRY", portcullis_peer::DEFAULT_REGISTRY);
    let apps = env("SHARE_APPS", limen::manifests::APPS_DIR);
    let until = std::env::var("SHARE_SECS").ok().and_then(|s| s.parse::<u64>().ok()).map(|s| Instant::now() + Duration::from_secs(s));
    loop {
        let child = match Child::provider(&limen_sock, "share") {
            Ok(c) => c,
            Err(e) => {
                eprintln!("share-sheet: limend at {limen_sock}: {e:?}; again in 5 s");
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
        };
        eprintln!("share-sheet: providing share");
        let mut sheet = Sheet::new(child, Box::new(tabula::gate::Frescod::new(&act)), Installed::at(&apps), share_sheet::registry_person(registry.clone()));
        let (tx, rx) = mpsc::channel::<FromPane>();
        let mut panes: HashMap<EmbedId, Sender<()>> = HashMap::new();
        loop {
            match sheet.poll(Duration::from_millis(20)) {
                Ok(Some(Event::Show { embed, title, from_app, targets })) => {
                    let (ptx, prx) = mpsc::channel();
                    panes.insert(embed, ptx);
                    let (f, t) = (fresco.clone(), tx.clone());
                    let first = Shown { title, from_app, targets, pressed: None };
                    std::thread::spawn(move || pane(embed, f, first, prx, t));
                }
                Ok(Some(Event::Close { embed })) => {
                    if let Some(p) = panes.remove(&embed) {
                        let _ = p.send(());
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    eprintln!("share-sheet: limend went ({e:?})");
                    break;
                }
            }
            while let Ok(m) = rx.try_recv() {
                match m {
                    FromPane::Ready(embed, token) => {
                        if let Err(e) = sheet.ready(embed, token) {
                            eprintln!("share-sheet: {embed}: ready: {e:?}");
                        }
                    }
                    FromPane::Chose(embed, app) if app.is_empty() => sheet.cancelled(embed),
                    FromPane::Chose(embed, app) => {
                        if let Err(why) = sheet.chose(embed, &app) {
                            eprintln!("share-sheet: {embed}: {why}");
                            sheet.cancelled(embed);
                        }
                    }
                    // The window went first: the app hears it was cancelled.
                    FromPane::Gone(embed) => {
                        panes.remove(&embed);
                        sheet.cancelled(embed);
                    }
                }
            }
            if until.is_some_and(|u| Instant::now() >= u) {
                return;
            }
        }
        for (_, p) in panes.drain() {
            let _ = p.send(());
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}
