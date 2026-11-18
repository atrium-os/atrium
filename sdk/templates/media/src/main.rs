//! {{title}}: plays the audio and video you open with it (template: media).
//!
//! This process is the app's **core**: it holds atrium.toml's capabilities
//! (a window, the files the person opens in it, and the `media-player`
//! embed). It never reads a media file: each one goes, as a descriptor, to
//! the system's player, which Limen starts in a jail of its own and binds
//! into a slot of this window. The core draws the controls around it and
//! hears back only what a web page would (ready, playing, time, ended).
//!
//! To decode a format of your own instead, put the decoder in a worker
//! (template: viewer) and draw its frames here.

use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use limen::media_player::{Source, ToChild, ToParent};
use pergola::embed::{EmbedId, Embeds, ParentEvent};
use pergola::geom::Rect;
use pergola::node::Node;
use pergola::theme::{font, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::widgets::Button;
use pergola::window::StepResult;
use pergola::{App, Color, Mutable, SizeSpec, TextStyle, Window, WindowDesc};

/// The player's slot in this window.
const PLAYER: u64 = 1;
/// The control bar's height, under the player.
const BAR: f32 = 64.0;

/// What the controls ask of the player.
#[derive(Clone, Copy)]
enum Ask {
    Play,
    Pause,
}

struct Main {
    status: Mutable<String>,
    size: Mutable<(f32, f32)>,
    asks: mpsc::Sender<Ask>,
}

impl View for Main {
    fn render(&self, ctx: &mut Ctx) {
        let (w, h) = self.size.get();
        // The player draws here: Limen places its window over this slot.
        let slot = ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, w, (h - BAR).max(0.0)), fill: Color::rgba(0.0, 0.0, 0.0, 1.0), radius: 0.0 });
        ctx.slot(slot, PLAYER);
        let y = h - BAR + 14.0;
        let play = self.asks.clone();
        Button::primary("Play").at(16.0, y).width(90.0).on_click(move || { let _ = play.send(Ask::Play); }).render(ctx);
        let pause = self.asks.clone();
        Button::secondary("Pause").at(116.0, y).width(90.0).on_click(move || { let _ = pause.send(Ask::Pause); }).render(ctx);
        ctx.add(Node::Text {
            rect: Rect::new(226.0, y + 8.0, 0.0, 0.0),
            content: self.status.get_cloned(),
            style: TextStyle { family: font::SANS.into(), size: type_size::MD, weight: Weight::Regular, color: Color::rgba(0.1, 0.1, 0.12, 1.0) },
        });
    }
}

/// What the player said, in words.
fn said(m: ToParent) -> Option<String> {
    Some(match m {
        ToParent::Ready { duration_secs, width, height } if width > 0 => format!("{width}×{height}, {duration_secs:.0} s"),
        ToParent::Ready { duration_secs, .. } => format!("{duration_secs:.0} s"),
        ToParent::Playing => "Playing".into(),
        ToParent::Paused => "Paused".into(),
        ToParent::Ended => "Ended".into(),
        ToParent::Time { secs } => format!("{:.0}:{:02.0}", (secs / 60.0).floor(), secs % 60.0),
        ToParent::Error { code, .. } => format!("The player can't play this ({code})"),
        _ => return None,
    })
}

fn main() {
    let mut win = match Window::open(&WindowDesc::new("{{title}}", SizeSpec::Fixed(960, 600))) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("{{name}}: no display: {e}");
            std::process::exit(1);
        }
    };
    let embeds = match Embeds::connect() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{{name}}: Limen: {e:?} (does atrium.toml ask for embeds = [\"media-player\"]?)");
            std::process::exit(1);
        }
    };
    win.set_slot_binder(Embeds::binder(&embeds));

    // The files opened in the app, from its document channel.
    let (files, opened) = mpsc::channel();
    std::thread::spawn(move || {
        let Ok(Some(mut channel)) = atrium_open::Channel::connect() else { return };
        while let Ok(next) = channel.recv(None) {
            if let Some(o) = next {
                if files.send(o).is_err() {
                    return;
                }
            }
        }
    });

    let dirty = Arc::new(AtomicBool::new(true));
    let status = Mutable::with_dirty("Open a video or a song with {{title}} from Files.".to_string(), dirty.clone());
    let size = Mutable::with_dirty((win.width, win.height), dirty.clone());
    let (asks, asked) = mpsc::channel();
    let mut app = App::new_with_flag(Main { status: status.clone(), size: size.clone(), asks }, dirty);
    let mut player: Option<EmbedId> = None;
    loop {
        match win.step(&mut app, Some(Duration::from_millis(50))) {
            Ok(StepResult::Closed) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("{{name}}: {e}");
                std::process::exit(1);
            }
        }
        if size.get() != (win.width, win.height) {
            size.set((win.width, win.height));
        }
        // A file opened: start the player if there is none, and load it.
        while let Ok(o) = opened.try_recv() {
            let Some(token) = win.embed_token() else {
                status.set("The window isn't ready for the player yet; open it again".into());
                continue;
            };
            let mut e = embeds.borrow_mut();
            let id = match player {
                Some(id) => id,
                None => match e.request("media-player", token, PLAYER) {
                    Ok(id) => *player.insert(id),
                    Err(err) => {
                        status.set(format!("No player: {err:?}"));
                        continue;
                    }
                },
            };
            let load = ToChild::Load { source: Source::Fd, mime: o.mime.clone(), growing: false };
            use std::os::fd::AsFd;
            match e.send_fd(id, load.encode(), o.file.as_fd()).and_then(|_| e.send(id, ToChild::Play.encode())) {
                Ok(()) => status.set(o.name),
                Err(err) => status.set(format!("The player didn't take it: {err:?}")),
            }
        }
        while let Ok(a) = asked.try_recv() {
            if let Some(id) = player {
                let m = match a {
                    Ask::Play => ToChild::Play,
                    Ask::Pause => ToChild::Pause,
                };
                let _ = embeds.borrow_mut().send(id, m.encode());
            }
        }
        // What the player says; it is decoded with every bound checked.
        loop {
            let ev = embeds.borrow_mut().poll(Duration::ZERO);
            match ev {
                Ok(Some(ParentEvent::Message { bytes, .. })) => {
                    if let Some(s) = ToParent::decode(&bytes).and_then(said) {
                        status.set(s);
                    }
                }
                Ok(Some(ParentEvent::ChildLost { .. })) => {
                    player = None;
                    status.set("The player stopped; open the file again".into());
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(err) => {
                    eprintln!("{{name}}: Limen: {err:?}");
                    break;
                }
            }
        }
    }
}
