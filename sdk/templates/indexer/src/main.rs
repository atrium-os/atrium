//! {{title}}: the most common words in the text files you open
//! (template: indexer).
//!
//! This process is the app's **core**: it holds atrium.toml's capabilities
//! (a window, and the files the person opens in it). It reads each file's
//! text but never parses it: the text goes to the `index` worker, kept for
//! the app's run, and what comes back is a checked `Top`. Anything that
//! parses what came from outside belongs in a worker (`cargo atrium check`
//! and `cargo atrium fuzz` tell you when it doesn't).
//!
//! Off Atrium, or in a workspace: `ATRIUM_WORKERS=local ATRIUM_APP_ROOT=.
//! cargo run -- notes.txt` (after `cargo atrium run` has put the worker in
//! bin/).

use std::fs::File;
use std::io::Read;
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use pergola::geom::Rect;
use pergola::node::Node;
use pergola::theme::{font, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::window::StepResult;
use pergola::{App, Color, Mutable, SizeSpec, TextStyle, Window, WindowDesc};

use {{crate}}::{Add, Top, MAX_TEXT};

#[derive(Clone)]
enum Shown {
    Nothing,
    Top(Top),
    Failed(String),
}

struct Main {
    shown: Mutable<Shown>,
}

fn text(ctx: &mut Ctx, x: f32, y: f32, s: String, size: f32, weight: Weight) {
    ctx.add(Node::Text {
        rect: Rect::new(x, y, 0.0, 0.0),
        content: s,
        style: TextStyle { family: font::SANS.into(), size, weight, color: Color::rgba(0.1, 0.1, 0.12, 1.0) },
    });
}

impl View for Main {
    fn render(&self, ctx: &mut Ctx) {
        match self.shown.get_cloned() {
            Shown::Nothing => {
                text(ctx, 24.0, 24.0, "No text yet".into(), type_size::XL, Weight::Semibold);
                text(ctx, 24.0, 64.0, "Open text files with {{title}} from Files.".into(), type_size::MD, Weight::Regular);
            }
            Shown::Failed(why) => {
                text(ctx, 24.0, 24.0, "Something went wrong".into(), type_size::XL, Weight::Semibold);
                text(ctx, 24.0, 64.0, why, type_size::MD, Weight::Regular);
            }
            Shown::Top(top) => {
                text(ctx, 24.0, 24.0, format!("{} words so far", top.total), type_size::XL, Weight::Semibold);
                for (x, title, list) in [(24.0, "This file", &top.file), (324.0, "All files", &top.words)] {
                    text(ctx, x, 64.0, title.into(), type_size::MD, Weight::Semibold);
                    for (i, c) in list.iter().enumerate() {
                        let y = 92.0 + 22.0 * i as f32;
                        text(ctx, x, y, c.word.clone(), type_size::MD, Weight::Regular);
                        text(ctx, x + 200.0, y, c.count.to_string(), type_size::MD, Weight::Regular);
                    }
                }
            }
        }
    }
}

/// One file's text, at most MAX_TEXT bytes of it.
fn read(file: File) -> Result<String, String> {
    let mut bytes = Vec::new();
    file.take(MAX_TEXT as u64).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    // Text that isn't UTF-8 has its bad bytes replaced: no parsing here.
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// The files opened in the app, into one worker kept for the whole run.
fn indexer(tx: mpsc::Sender<Shown>) {
    let session = atrium_worker::Workers::get().and_then(|w| w.session("index"));
    let mut session = match session {
        Ok(s) => s,
        Err(e) => {
            let _ = tx.send(Shown::Failed(e.to_string()));
            return;
        }
    };
    let mut add = |file: File| -> Shown {
        let r = read(file).and_then(|text| session.call::<Add, Top>(&Add { text }, None).map_err(|e| e.to_string()));
        match r {
            Ok(top) => Shown::Top(top),
            Err(e) => Shown::Failed(e),
        }
    };
    // Paths on the command line, for development in a workspace.
    for p in std::env::args().skip(1) {
        let shown = match File::open(&p) {
            Ok(f) => add(f),
            Err(e) => Shown::Failed(format!("{p}: {e}")),
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
            if tx.send(add(o.file)).is_err() {
                return;
            }
        }
    }
}

fn main() {
    let mut win = match Window::open(&WindowDesc::new("{{title}}", SizeSpec::Fixed(620, 560))) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("{{name}}: no display: {e}");
            std::process::exit(1);
        }
    };
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || indexer(tx));
    let dirty = Arc::new(AtomicBool::new(true));
    let shown = Mutable::with_dirty(Shown::Nothing, dirty.clone());
    let mut app = App::new_with_flag(Main { shown: shown.clone() }, dirty);
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
    }
}
