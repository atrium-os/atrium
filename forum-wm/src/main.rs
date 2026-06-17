//! forum-wm — the Forum WM daemon (`docs/spec/forum.md`).
//!
//! Connects to frescod over the display socket (with the `window-management`
//! capability), then runs the reconcile loop the core defines:
//!
//!   enumerate the session's surfaces → arrange by role → declare the atomic
//!   layout → set per-surface rendering (gate the fully-occluded).
//!
//! This binary is the real I/O binding: `ClientConn` wraps a
//! `fresco_client::Connection` and implements the `FrescoConn` seam the pure
//! core (`lib.rs`) is written against. The placement/occlusion policy lives in
//! the lib and is unit-tested without a live frescod; here we just drive it.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use forum_wm::daemon::{FrescoConn, Wm};
use forum_wm::Screen;
use fresco_client::{Connection, Event};
use fresco_protocol::{WmDeclareLayoutPayload, WmRect, WmSetRenderingPayload, WmSurfaceInfo};

/// The WM's shared mutable core: the policy state + the live frescod connection.
/// Two threads touch it — the forum-ctl server (chrome intents) and the input
/// poller (focus-follows-click) — so they serialize on this lock. The single
/// connection is fine: every reader (`wait_event`/`poll_event`) and request
/// (`enumerate`/`declare`) happens under the lock, so they never race on the
/// stream, and the one subscriber is drained by the poller every tick.
type Core = Arc<Mutex<(Wm, ClientConn)>>;

/// The live frescod connection — the production implementation of the seam the
/// reconcile loop drives. Each method is one window-management protocol op.
struct ClientConn {
    conn: Connection,
}

impl FrescoConn for ClientConn {
    fn enumerate(&mut self) -> io::Result<Vec<WmSurfaceInfo>> {
        self.conn.wm_enumerate()
    }
    fn declare_layout(&mut self, layout: &WmDeclareLayoutPayload) -> io::Result<()> {
        self.conn.wm_declare_layout(layout)
    }
    fn set_rendering(&mut self, decisions: &[WmSetRenderingPayload]) -> io::Result<()> {
        // The protocol marks one surface at a time; the WM batches the set.
        for d in decisions {
            self.conn.wm_set_rendering(d)?;
        }
        Ok(())
    }
}


/// The output geometry. Until frescod exposes a mode query over the WM channel we
/// take it from the environment (FORUM_SCREEN="WxH"), defaulting to 1080p. The
/// chrome reservations (bar/dock) are the shell's, fixed here.
fn screen() -> Screen {
    let (w, h) = std::env::var("FORUM_SCREEN")
        .ok()
        .and_then(|s| {
            let (a, b) = s.split_once('x')?;
            Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
        })
        .unwrap_or((1920, 1080));
    Screen { rect: WmRect { x: 0, y: 0, w, h }, bar_h: 24, dock_h: 48 }
}

fn main() -> io::Result<()> {
    // The WM drives its cross-app ops over the DEDICATED window-management socket
    // (reachability = the window-management grant; frescod reads no policy), not
    // the shared client socket. See fresco_client::default_wm_socket_path.
    let path = fresco_client::default_wm_socket_path();
    eprintln!("forum-wm: connecting to frescod window-management socket at {}", path.display());
    let conn = Connection::connect(&path)?;
    // frescod now replies to a refused WM op with an IS_ERROR response, so a
    // denied enumerate surfaces a PermissionDenied immediately. This read timeout
    // is the secondary safety net for a genuinely unresponsive server (crash /
    // stall), so the reconcile can't block forever.
    conn.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    let mut io_conn = ClientConn { conn };

    let wm = Wm::new(screen());
    let layout = wm.reconcile(&mut io_conn)?;
    eprintln!(
        "forum-wm: declared layout — {} surface(s), focus={}",
        layout.slots.len(),
        layout.focus
    );

    if std::env::var("FORUM_WM_ONESHOT").is_ok() {
        // F0 behaviour: reconcile once and exit. No event loop, no daemon.
        return Ok(());
    }

    // Become the WM core daemon. Two concurrent drivers share the core:
    //
    //   - the INPUT POLLER reads frescod's event stream (the WM connection is a
    //     subscriber like any client) and applies focus-follows-click — a
    //     pointer press over an app surface moves keyboard focus there;
    //   - the FORUM-CTL SERVER answers the chrome apps' intents (dock/bar/
    //     overview) over forum-ctl, each re-driving Fresco through reconcile.
    //
    // Both re-derive the layout through the same `Wm`, so they must not run
    // concurrently against the one connection — `Core` (Arc<Mutex>) serializes
    // them. forum-ctl's path resolves to the canonical in-jail location (or
    // $FORUM_CTL_SOCKET / a dev fallback), so a jailed forum-wm serves the same
    // socket the jailed chrome apps reach through their shared
    // /atrium/sockets/forum-ctl/ mount.
    let core: Core = Arc::new(Mutex::new((wm, io_conn)));

    spawn_input_poller(Arc::clone(&core));

    let ctl = forum_ctl::default_socket_path();
    let ctl = ctl.to_string_lossy();
    eprintln!("forum-wm: serving forum-ctl at {ctl}");
    serve_forum_ctl(&ctl, &core)?;
    Ok(())
}

/// The input poller — focus-follows-click. frescod broadcasts every input event
/// to every connection (no ownership filter), and tags each pointer-button event
/// with the hit-test target (the surface under the cursor), so the WM learns
/// which surface the human clicked just by reading its own event stream.
///
/// A short non-blocking poll keeps the core lock free almost all the time: we
/// grab it, drain whatever events are queued (so a burst of motion events can't
/// back up the socket), note the last primary-button press, release the lock,
/// then act on that press under a fresh lock. The forum-ctl server therefore
/// sees the lock contended only for the brief drain + an actual focus change.
fn spawn_input_poller(core: Core) {
    std::thread::Builder::new()
        .name("forum-wm-input".into())
        .spawn(move || loop {
            // Drain the event backlog under one short lock; keep the last
            // primary press (a later click supersedes an earlier one this tick)
            // and note whether the surface set changed (a window appeared or
            // disappeared — the WM must re-place the survivors).
            let mut clicked: Option<u32> = None;
            let mut surfaces_changed = false;
            {
                let mut g = core.lock().unwrap();
                loop {
                    match g.1.conn.poll_event() {
                        Ok(Some(Event::PointerButton {
                            window_id, pressed: true, button: 1, ..
                        })) if window_id != 0 => clicked = Some(window_id),
                        Ok(Some(Event::WindowCreated { .. }))
                        | Ok(Some(Event::WindowDestroyed { .. })) => surfaces_changed = true,
                        Ok(Some(_)) => {}        // other event — ignore
                        Ok(None) => break,       // backlog drained
                        Err(e) => {
                            eprintln!("forum-wm: input poll error: {e}");
                            break;
                        }
                    }
                }
            }

            // A surface came or went → re-derive the whole layout so the new
            // window is placed (or the gone one's slot reclaimed). Do this
            // before applying any click, so focus-follows-click sees the
            // up-to-date surface set.
            if surfaces_changed {
                let mut g = core.lock().unwrap();
                let (wm, conn) = { let c = &mut *g; (&mut c.0, &mut c.1) };
                match wm.reconcile(conn) {
                    Ok(l) => eprintln!(
                        "forum-wm: reconciled on surface change — {} surface(s), focus={}",
                        l.slots.len(), l.focus,
                    ),
                    Err(e) => eprintln!("forum-wm: reconcile error: {e}"),
                }
            }

            if let Some(w) = clicked {
                let mut g = core.lock().unwrap();
                let (wm, conn) = { let c = &mut *g; (&mut c.0, &mut c.1) };
                match wm.focus_click(conn, w) {
                    Ok(Some(f)) => eprintln!("forum-wm: focus-follows-click → surface {f}"),
                    Ok(None) => {}               // click didn't change focus
                    Err(e) => eprintln!("forum-wm: focus_click error: {e}"),
                }
            }

            std::thread::sleep(Duration::from_millis(16)); // ~60 Hz, lock released
        })
        .expect("spawn forum-wm input poller");
}

/// Serve forum-ctl: accept chrome connections, carry out one intent each against
/// Fresco.
///
/// ADMISSION = REACHABILITY (object-capability; minimal trust). We do NOT read
/// the owner's policy or the registry to authorize a peer. `forum-control` is
/// enforced by the TCB at the JAIL BOUNDARY: a chrome app can only see + connect
/// to this socket because Portcullis mounted `/atrium/sockets/forum-ctl/` into
/// its jail (`apply_forum_control`, gated by the manifest cap + the owner's
/// launch-time policy grant). An app without the grant has no mount → cannot
/// reach us. So a connection on this socket IS the capability — possession of the
/// channel, not a lookup. forum-wm re-checking a policy would only duplicate (and
/// widen the trust of) a decision the TCB already made + enforced. See docs/spec/
/// portcullis.md §9.0. (Dev `--no-prompt` launches skip the launch-time policy
/// gate — an explicit operator trust decision, not a hole here.)
fn serve_forum_ctl(path: &str, core: &Core) -> io::Result<()> {
    use std::os::unix::net::UnixListener;
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    eprintln!("forum-wm: serving forum-ctl on {path}");
    for stream in listener.incoming() {
        let mut s = match stream { Ok(s) => s, Err(_) => continue };
        if let Some(uid) = peer_uid(&s) {
            eprintln!("forum-wm: forum-ctl: chrome connected (uid {uid})");
        }
        let intent = match forum_ctl::read_frame(&mut s)
            .ok()
            .and_then(|b| forum_ctl::decode::<forum_ctl::Intent>(&b).ok())
        {
            Some(i) => i,
            None => continue,
        };
        // Carry out the intent under the core lock — serialized against the
        // input poller's focus-follows-click so the two never drive the one
        // connection at once.
        let reply = {
            let mut g = core.lock().unwrap();
            let (wm, conn) = { let c = &mut *g; (&mut c.0, &mut c.1) };
            forum_wm::control::handle_intent(wm, conn, intent)
        };
        if let Ok(bytes) = forum_ctl::encode(&reply) {
            let _ = forum_ctl::write_frame(&mut s, &bytes);
        }
    }
    Ok(())
}

/// The connecting peer's uid (getpeereid over the UDS) — for the log line only;
/// admission does not depend on it (see `serve_forum_ctl`).
fn peer_uid(s: &std::os::unix::net::UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let (mut uid, mut gid) = (0u32, 0u32);
    let rc = unsafe { libc::getpeereid(s.as_raw_fd(), &mut uid, &mut gid) };
    (rc == 0).then_some(uid)
}
