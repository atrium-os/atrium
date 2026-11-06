//! forum-shelf — subscribes to praecod and draws toasts.
//!
//! `PRAECO_SOCKET` (default `/var/run/atrium/praecod.sock`). Runs as a
//! chrome surface; frescod's notification layer (Fresco thread) is where
//! it will sit above every app, clamped to its band.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use aqueduct::{flag, Connection};
use forum_shelf::{Shelf, ShelfView};
use fresco_protocol::{WindowHints, WmRole};
use pergola::reactive::Mutable;
use pergola::{App, SizeSpec, StepResult, Window, WindowDesc};
use praeco::proto::{self, op, status, Event};

/// Subscribe and forward events until the connection ends; then retry.
fn listen(path: String, tx: mpsc::Sender<Event>) {
    loop {
        if let Ok(mut c) = Connection::connect(&path) {
            if c.send_message(proto::CLASS, op::SUBSCRIBE, flag::RESPONSE_EXPECTED, &[]).is_ok() {
                while let Ok(m) = c.recv_message() {
                    if m.flags & flag::IS_RESPONSE != 0 {
                        if m.payload.first() != Some(&status::OK) {
                            eprintln!("forum-shelf: praecod refused the subscription");
                            return;
                        }
                        continue;
                    }
                    if m.op == op::EV {
                        if let Ok(ev) = postcard::from_bytes::<Event>(&m.payload) {
                            if tx.send(ev).is_err() {
                                return;
                            }
                        }
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn main() -> std::io::Result<()> {
    let path = std::env::var("PRAECO_SOCKET").unwrap_or_else(|_| "/var/run/atrium/praecod.sock".into());
    let user = portcullis_peer::current_username().unwrap_or_default();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || listen(path, tx));

    // A notification-layer window (tabula.md §5.2): frescod keeps it above
    // the rest, clamps it into the top-right band and never gives it keys.
    // It needs this app's uid in FRESCOD_NOTIFY_UIDS, else it is a Document.
    let band = fresco_protocol::NOTIFICATION_BAND_W;
    let mut desc = WindowDesc::new("forum-shelf", SizeSpec::Fixed(band, 720));
    desc.hints = WindowHints { role: Some(WmRole::Notification), ..Default::default() };
    let mut window = Window::open(&desc)?;
    let dirty = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let shelf = Mutable::with_dirty(Shelf::new(user), dirty.clone());
    let dismissed = Mutable::with_dirty(None, dirty.clone());
    let width = Mutable::with_dirty(window.width, dirty.clone());
    let view = ShelfView { w: width.clone(), shelf: shelf.clone(), dismissed: dismissed.clone() };
    let mut app = App::new_with_flag(view, dirty);
    let t0 = Instant::now();
    let now = || t0.elapsed().as_secs_f64() * 1e3;
    window.paint(&mut app)?;
    loop {
        match window.step(&mut app, Some(Duration::from_millis(250)))? {
            StepResult::Closed => return Ok(()),
            StepResult::Event(fresco_client::Event::Resized { width: w, .. }) => width.set(w as f32),
            _ => {}
        }
        let mut s = shelf.get_cloned();
        let mut changed = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                Event::Posted(n) => s.posted(n, now()),
                Event::Withdrawn { id } => s.withdrawn(id, now()),
            }
            changed = true;
        }
        if let Some(id) = dismissed.get_cloned() {
            s.dismiss(id, now());
            dismissed.set(None);
            changed = true;
        }
        changed |= s.tick(now());
        if changed {
            shelf.set(s);
            window.paint(&mut app)?;
        }
    }
}
