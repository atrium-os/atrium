//! {{title}}: a picture viewer (template: viewer).
//!
//! This process is the app's **core**: it holds atrium.toml's capabilities
//! (a window, and the files the person opens in it). It never reads those
//! files' bytes: each one goes to the `decode` worker as a descriptor, and
//! what comes back is a checked `Picture`. Keep it that way as the app
//! grows: anything that parses what came from outside goes in a worker
//! (`cargo atrium check` and `cargo atrium fuzz` tell you when it doesn't).
//!
//! Off Atrium, or in a workspace: `ATRIUM_WORKERS=local ATRIUM_APP_ROOT=.
//! cargo run -- picture.bmp` (after `cargo atrium run` has put the worker
//! in bin/).

use std::fs::File;
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use pergola::geom::Rect;
use pergola::node::{Image, Node};
use pergola::theme::{font, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::window::StepResult;
use pergola::{App, Color, Mutable, SizeSpec, TextStyle, Window, WindowDesc};

use {{crate}}::{Ask, Picture};

/// The window's size when it opens; Forum may give it another, and the
/// picture follows.
const W: f32 = 800.0;
const H: f32 = 600.0;

#[derive(Clone)]
enum Shown {
    Nothing,
    Picture(Image),
    Failed(String, String),
}

struct Main {
    shown: Mutable<Shown>,
    /// The window's size, in points.
    size: Mutable<(f32, f32)>,
}

fn text(ctx: &mut Ctx, y: f32, s: String, size: f32, weight: Weight) {
    ctx.add(Node::Text {
        rect: Rect::new(24.0, y, 0.0, 0.0),
        content: s,
        style: TextStyle { family: font::SANS.into(), size, weight, color: Color::rgba(0.1, 0.1, 0.12, 1.0) },
    });
}

impl View for Main {
    fn render(&self, ctx: &mut Ctx) {
        match self.shown.get_cloned() {
            Shown::Nothing => {
                text(ctx, 24.0, "No picture yet".into(), type_size::XL, Weight::Semibold);
                text(ctx, 64.0, "Open a BMP picture with {{title}} from Files.".into(), type_size::MD, Weight::Regular);
            }
            Shown::Failed(name, why) => {
                text(ctx, 24.0, format!("{name} can't be shown"), type_size::XL, Weight::Semibold);
                text(ctx, 64.0, why, type_size::MD, Weight::Regular);
            }
            Shown::Picture(image) => {
                // Fit it into the window, never larger than it is.
                let (ww, wh) = self.size.get();
                let (pw, ph) = (image.data().width as f32, image.data().height as f32);
                let s = (ww / pw).min(wh / ph).min(1.0);
                let (w, h) = (pw * s, ph * s);
                ctx.add(Node::Image { rect: Rect::new((ww - w) / 2.0, (wh - h) / 2.0, w, h), image });
            }
        }
    }
}

/// Ask the worker for one file's picture.
fn show(workers: &atrium_worker::Workers, name: String, file: File) -> Shown {
    match workers.call::<Ask, Picture>("decode", &Ask { max_side: 2048 }, Some(file)) {
        Ok(p) if p.is_whole() => Shown::Picture(Image::new(p.width, p.height, p.rgba)),
        Ok(_) => Shown::Failed(name, "the decoder's answer doesn't add up".into()),
        Err(e) => Shown::Failed(name, e.to_string()),
    }
}

/// The files opened in the app, one at a time, until the app ends.
fn opener(tx: mpsc::Sender<Shown>) {
    let workers = match atrium_worker::Workers::get() {
        Ok(w) => w,
        Err(e) => {
            let _ = tx.send(Shown::Failed("{{title}}".into(), e.to_string()));
            return;
        }
    };
    // A path on the command line, for development in a workspace.
    if let Some(p) = std::env::args().nth(1) {
        let shown = match File::open(&p) {
            Ok(f) => show(&workers, p, f),
            Err(e) => Shown::Failed(p, e.to_string()),
        };
        let _ = tx.send(shown);
    }
    let mut channel = match atrium_open::Channel::connect() {
        Ok(Some(c)) => c,
        Ok(None) => return,
        Err(e) => {
            eprintln!("{{name}}: the document channel: {e}");
            return;
        }
    };
    while let Ok(next) = channel.recv(None) {
        if let Some(o) = next {
            if tx.send(show(&workers, o.name, o.file)).is_err() {
                return;
            }
        }
    }
}

fn main() {
    let mut win = match Window::open(&WindowDesc::new("{{title}}", SizeSpec::Fixed(W as u32, H as u32))) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("{{name}}: no display: {e}");
            std::process::exit(1);
        }
    };
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || opener(tx));
    let dirty = Arc::new(AtomicBool::new(true));
    let shown = Mutable::with_dirty(Shown::Nothing, dirty.clone());
    let size = Mutable::with_dirty((win.width, win.height), dirty.clone());
    let mut app = App::new_with_flag(Main { shown: shown.clone(), size: size.clone() }, dirty);
    loop {
        match win.step(&mut app, Some(Duration::from_millis(100))) {
            Ok(StepResult::Closed) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("{{name}}: {e}");
                std::process::exit(1);
            }
        }
        while let Ok(s) = rx.try_recv() {
            shown.set(s);
        }
        if size.get() != (win.width, win.height) {
            size.set((win.width, win.height));
        }
    }
}
