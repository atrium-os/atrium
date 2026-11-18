//! {{title}}: the latest items of a JSON Feed (template: client).
//!
//! This process is the app's **core**: it holds atrium.toml's capabilities
//! (a window, and fetching through Nuntius). It has no network of its own
//! and never parses what it fetches: Nuntius hands it the response as a
//! descriptor, the `parse` worker reads it, and what comes back is a
//! checked `Feed`. Keep it that way as the app grows (`cargo atrium check`
//! and `cargo atrium fuzz` tell you when it doesn't).
//!
//! Change FEED to the feed you want, and list its site in atrium.toml's
//! `net-fetch-sites`.

use std::fs::File;
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use nuntius::{Deliver, Event, Open};
use pergola::geom::Rect;
use pergola::node::Node;
use pergola::theme::{font, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::widgets::Button;
use pergola::window::StepResult;
use pergola::{App, Color, Mutable, SizeSpec, TextStyle, Window, WindowDesc};

use {{crate}}::{Ask, Feed};

/// The feed to show: on a site in atrium.toml's `net-fetch-sites`.
const FEED: &str = "https://example.org/feed.json";

#[derive(Clone)]
enum Shown {
    Fetching,
    Feed(Feed),
    Failed(String),
}

struct Main {
    shown: Mutable<Shown>,
    refresh: mpsc::Sender<()>,
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
        let refresh = self.refresh.clone();
        Button::primary("Refresh").at(24.0, 20.0).width(110.0).on_click(move || { let _ = refresh.send(()); }).render(ctx);
        match self.shown.get_cloned() {
            Shown::Fetching => text(ctx, 24.0, 72.0, "Fetching…".into(), type_size::MD, Weight::Regular),
            Shown::Failed(why) => {
                text(ctx, 24.0, 72.0, "The feed can't be shown".into(), type_size::XL, Weight::Semibold);
                text(ctx, 24.0, 108.0, why, type_size::MD, Weight::Regular);
            }
            Shown::Feed(feed) => {
                text(ctx, 24.0, 72.0, feed.title, type_size::XL, Weight::Semibold);
                for (i, item) in feed.items.iter().take(20).enumerate() {
                    let y = 112.0 + 40.0 * i as f32;
                    text(ctx, 24.0, y, item.title.clone(), type_size::MD, Weight::Semibold);
                    text(ctx, 24.0, y + 18.0, item.url.clone(), type_size::SM, Weight::Regular);
                }
            }
        }
    }
}

/// One fetch through Nuntius: the response as a file, never read here.
fn fetch(client: &mut nuntius::Client, id: u32, url: &str) -> Result<File, String> {
    let mut o = Open::get(id, url);
    o.deliver = Deliver::Fd;
    client.open(o).map_err(|e| format!("Nuntius: {e}"))?;
    let (mut status, mut file) = (0, None);
    loop {
        match client.recv_timeout(Duration::from_secs(60)) {
            Ok(Some(Event::Head { status: s, .. })) => status = s,
            Ok(Some(Event::Fd { fd, .. })) => file = Some(File::from(fd)),
            Ok(Some(Event::Done { .. })) if status == 200 => return file.ok_or_else(|| "no body".to_string()),
            Ok(Some(Event::Done { .. })) => return Err(format!("the site answered {status}")),
            Ok(Some(Event::Error { code, .. })) => return Err(format!("fetching: {code:?}")),
            Ok(Some(_)) => {}
            Ok(None) => return Err("the fetch took too long".into()),
            Err(e) => return Err(format!("Nuntius: {e}")),
        }
    }
}

/// Fetch, parse in the worker, and send what to show; again for each
/// Refresh, until the app ends.
fn fetcher(tx: mpsc::Sender<Shown>, refresh: mpsc::Receiver<()>) {
    let workers = match atrium_worker::Workers::get() {
        Ok(w) => w,
        Err(e) => {
            let _ = tx.send(Shown::Failed(e.to_string()));
            return;
        }
    };
    let fetcher = match nuntius::launch::open() {
        Ok(f) => f,
        Err(e) => {
            let _ = tx.send(Shown::Failed(format!("no fetcher: {e}")));
            return;
        }
    };
    // `_alive` keeps the fetcher running for as long as this thread does.
    let (mut client, _alive) = fetcher.into_parts();
    let mut id = 0;
    loop {
        id += 1;
        let shown = match fetch(&mut client, id, FEED) {
            Ok(body) => match workers.call::<Ask, Feed>("parse", &Ask { max_items: 50 }, Some(body)) {
                Ok(feed) => Shown::Feed(feed),
                Err(e) => Shown::Failed(e.to_string()),
            },
            Err(e) => Shown::Failed(e),
        };
        if tx.send(shown).is_err() || refresh.recv().is_err() {
            return;
        }
        let _ = tx.send(Shown::Fetching);
    }
}

fn main() {
    let mut win = match Window::open(&WindowDesc::new("{{title}}", SizeSpec::Fixed(640, 960))) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("{{name}}: no display: {e}");
            std::process::exit(1);
        }
    };
    let (tx, rx) = mpsc::channel();
    let (refresh_tx, refresh_rx) = mpsc::channel();
    std::thread::spawn(move || fetcher(tx, refresh_rx));
    let dirty = Arc::new(AtomicBool::new(true));
    let shown = Mutable::with_dirty(Shown::Fetching, dirty.clone());
    let mut app = App::new_with_flag(Main { shown: shown.clone(), refresh: refresh_tx }, dirty);
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
