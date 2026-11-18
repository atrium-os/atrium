//! praeco-demo: a test app for notification actions (tabula.md §5.5).
//!
//! It listens for its actions first: one held for it (pressed while it
//! wasn't running) arrives at once, and it logs it and exits. Otherwise,
//! by the name it runs as:
//!
//! - `chat`: opens a window, posts "Ana: lunch on Friday?" with Archive
//!   and Reply, and waits up to five minutes. On Reply it asks praecod to
//!   bring its window forward (`praeco::client::raise`), as a mail app
//!   would for its compose window.
//! - `cover`: only opens a window and stays five minutes (to cover Chat's).
//! - anything else (`reminder`): posts and exits, so the press finds it
//!   stopped and Forum starts it.
//!
//! `--forge <id>` (tests): acts on notification `id` with a press frescod
//! never minted, as a subscriber would; praecod must refuse it.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use pergola::geom::Rect;
use pergola::node::Node;
use pergola::theme::{font, size::em, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::{App, Color, SizeSpec, StepResult, TextStyle, Window, WindowDesc};

struct Plain {
    name: String,
    fill: Color,
}

impl View for Plain {
    fn render(&self, ctx: &mut Ctx) {
        ctx.push(Node::Rect { rect: Rect::new(0.0, 0.0, 640.0, 400.0), fill: self.fill, radius: 0.0 });
        ctx.add(Node::Text { rect: Rect::new(24.0, em(24.0), 0.0, em(32.0)), content: self.name.clone(),
            style: TextStyle { family: font::SANS.into(), size: type_size::XL, weight: Weight::Semibold, color: Color::rgba(1.0, 1.0, 1.0, 1.0) } });
        ctx.pop();
    }
}

/// A window named `name` until `rx` says stop or `secs` pass.
fn window(name: &str, fill: Color, secs: u64, rx: mpsc::Receiver<()>) {
    let desc = WindowDesc::new(name, SizeSpec::Fixed(640, 400));
    let mut win = match Window::open(&desc) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("praeco-demo: window: {e}");
            return;
        }
    };
    let dirty = Arc::new(AtomicBool::new(true));
    let mut app = App::new_with_flag(Plain { name: name.into(), fill }, dirty);
    let _ = win.paint(&mut app);
    let until = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < until && rx.try_recv().is_err() {
        if matches!(win.step(&mut app, Some(Duration::from_millis(100))), Ok(StepResult::Closed) | Err(_)) {
            break;
        }
    }
}

fn main() {
    let sock = praeco::client::socket();
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--forge") {
        let id = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(0);
        match praeco::client::act(&sock, id, "archive", [9; 16]) {
            Ok(st) => println!("praeco-demo: forged act on {id}: status {st}"),
            Err(e) => println!("praeco-demo: forged act: {e}"),
        }
        return;
    }
    let name = args.first().map(PathBuf::from).and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned())).unwrap_or_default();
    if name == "cover" {
        let (_tx, rx) = mpsc::channel();
        eprintln!("praeco-demo: cover opens its window");
        window("Cover", Color::rgba(0.55, 0.2, 0.2, 1.0), 300, rx);
        return;
    }
    let mut ear = match praeco::client::Listener::connect(&sock) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("praeco-demo: listen: {e}");
            std::process::exit(1);
        }
    };
    if let Ok(Some(a)) = ear.next(Duration::from_secs(3)) {
        eprintln!("praeco-demo: {name} started for {:?} on {}", a.action, a.id);
        return;
    }
    let (stop, rx) = mpsc::channel();
    let shown = (name == "chat").then(|| std::thread::spawn(move || window("Chat", Color::rgba(0.15, 0.35, 0.65, 1.0), 300, rx)));
    if shown.is_some() {
        std::thread::sleep(Duration::from_secs(5));
    }
    let id = match praeco::client::post_with_actions(&sock, praeco::proto::urgency::HIGH, "Ana: lunch on Friday?", "The usual place at one?", &[("archive", "Archive"), ("reply", "Reply")]) {
        Ok(id) => {
            eprintln!("praeco-demo: {name} posted {id} with Archive and Reply");
            id
        }
        Err(e) => {
            eprintln!("praeco-demo: post: {e}");
            std::process::exit(1);
        }
    };
    // Asking before any press: refused.
    if let Err(e) = praeco::client::raise(&sock, id) {
        eprintln!("praeco-demo: {name} raise before a press: {e}");
    }
    if name != "chat" {
        eprintln!("praeco-demo: {name} exits");
        return;
    }
    let until = Instant::now() + Duration::from_secs(300);
    while Instant::now() < until {
        match ear.next(Duration::from_secs(5)) {
            Ok(Some(a)) => {
                eprintln!("praeco-demo: {name} heard {:?} on {}", a.action, a.id);
                if a.action == "reply" {
                    match praeco::client::raise(&sock, a.id) {
                        Ok(()) => eprintln!("praeco-demo: {name} raised its window for reply"),
                        Err(e) => eprintln!("praeco-demo: {name} raise: {e}"),
                    }
                }
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("praeco-demo: {name}: {e}");
                break;
            }
        }
    }
    let _ = stop.send(());
    if let Some(t) = shown {
        let _ = t.join();
    }
}
