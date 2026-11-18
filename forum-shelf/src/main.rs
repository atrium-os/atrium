//! forum-shelf — subscribes to praecod and draws toasts.
//!
//! praecod: `praeco::client::socket()`. Runs as a
//! chrome surface; frescod's notification layer (Fresco thread) is where
//! it will sit above every app, clamped to its band.
//!
//! A click on a toast's action (tabula.md §5.3): the shelf has frescod
//! (`SHELF_FRESCOD_ACTIVATION`; by default the jail's
//! `/atrium/sockets/fresco-activation/`, else `/var/run/atrium/`) grant that press to the
//! posting app's uid, which only Forum's uid may (`FRESCOD_FORUM_UID`),
//! and hands it to praecod with the action. If the app isn't listening,
//! the shelf starts it through portcullisd (`app-launch`), in its
//! persona; praecod holds the action for it.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use aqueduct::{flag, Connection};
use forum_shelf::{Pressed, Shelf, ShelfView};
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

/// The person pressed an action on `n`: grant the press, tell praecod,
/// start the app if it isn't listening. Off the drawing thread.
fn act(praeco: &str, activation: &str, n: &proto::Notification, p: Pressed) {
    use fresco_protocol::activation::{purpose, Grant, GrantResult};
    let Some(press) = p.press else {
        eprintln!("forum-shelf: action {:?} on {}: no press with the click", p.action, p.id);
        return;
    };
    let token = match tabula::gate::Frescod::new(activation).grant(Grant { token: press, for_uid: n.uid, purpose: purpose::ACTION }) {
        GrantResult::Ok { token } => token,
        GrantResult::Refused { reason } => {
            eprintln!("forum-shelf: action {:?} on {}: frescod granted no press ({reason})", p.action, p.id);
            return;
        }
    };
    match praeco::client::act(std::path::Path::new(praeco), p.id, &p.action, token) {
        Ok(status::OK) => eprintln!("forum-shelf: action {:?} on {} for {}: delivered", p.action, p.id, n.app),
        Ok(status::NOT_RUNNING) => {
            eprintln!("forum-shelf: action {:?} on {} for {}: starting it", p.action, p.id, n.app);
            let stdio = [0, 2, 2];
            match forum_dock::launch::request_in(&n.app, n.persona.as_deref(), stdio) {
                forum_dock::launch::PreLaunch::Started(s) => {
                    let app = n.app.clone();
                    std::thread::spawn(move || eprintln!("forum-shelf: {app} ended: {:?}", s.wait()));
                }
                // Running but not listening: it gets the action when it does.
                forum_dock::launch::PreLaunch::AlreadyRunning { uid, .. } => eprintln!("forum-shelf: {} runs already (uid {uid})", n.app),
                other => eprintln!("forum-shelf: starting {}: {other:?}", n.app),
            }
        }
        Ok(st) => eprintln!("forum-shelf: praecod refused action {:?} on {} (status {st})", p.action, p.id),
        Err(e) => eprintln!("forum-shelf: praecod: {e}"),
    }
}

fn main() -> std::io::Result<()> {
    let path = praeco::client::socket().to_string_lossy().into_owned();
    // In its jail (`notification-shelf`), frescod's activation socket is
    // under /atrium/sockets; on the host, /var/run/atrium.
    let jailed = "/atrium/sockets/fresco-activation/fresco-activation.sock";
    let activation = std::env::var("SHELF_FRESCOD_ACTIVATION").unwrap_or_else(|_| {
        if std::path::Path::new(jailed).exists() { jailed.into() } else { "/var/run/atrium/fresco-activation.sock".into() }
    });
    let praeco_path = path.clone();
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
    let acted = Mutable::with_dirty(None, dirty.clone());
    let view = ShelfView { w: width.clone(), shelf: shelf.clone(), dismissed: dismissed.clone(), acted: acted.clone() };
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
                // praecod names the person a jailed shelf is for (its own
                // uid's user is the app's, not the person's).
                Event::Person { user } => s.user = user,
            }
            changed = true;
        }
        if let Some(p) = acted.get_cloned() {
            acted.set(None);
            if let Some(t) = s.toasts.iter().find(|t| t.n.id == p.id) {
                let (n, praeco, act_sock) = (t.n.clone(), praeco_path.clone(), activation.clone());
                std::thread::spawn(move || act(&praeco, &act_sock, &n, p.clone()));
                // Pressed: done with it.
                s.dismiss(t.n.id, now());
                changed = true;
            }
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
