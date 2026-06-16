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

use forum_wm::daemon::{FrescoConn, Wm};
use forum_wm::Screen;
use fresco_client::Connection;
use fresco_protocol::{WmDeclareLayoutPayload, WmRect, WmSetRenderingPayload, WmSurfaceInfo};

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
    let path = fresco_client::default_socket_path();
    eprintln!("forum-wm: connecting to frescod at {}", path.display());
    let conn = Connection::connect(&path)?;
    // frescod now replies to a refused WM op with an IS_ERROR response, so a
    // denied enumerate surfaces a PermissionDenied immediately. This read timeout
    // is the secondary safety net for a genuinely unresponsive server (crash /
    // stall), so the reconcile can't block forever.
    conn.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    let mut io_conn = ClientConn { conn };

    let mut wm = Wm::new(screen());
    let layout = wm.reconcile(&mut io_conn)?;
    eprintln!(
        "forum-wm: declared layout — {} surface(s), focus={}",
        layout.slots.len(),
        layout.focus
    );

    // Become the WM core daemon: serve the chrome apps' intents (dock/bar/overview)
    // over forum-ctl, each re-driving Fresco through the same reconcile. The path
    // resolves to the canonical in-jail location (or $FORUM_CTL_SOCKET / a dev
    // fallback), so a jailed forum-wm serves the same socket the jailed chrome
    // apps connect to through their shared /atrium/sockets/forum-ctl/ mount. Set
    // FORUM_WM_ONESHOT=1 to keep the old F0 reconcile-once-and-exit behaviour.
    if std::env::var("FORUM_WM_ONESHOT").is_err() {
        let ctl = forum_ctl::default_socket_path();
        let ctl = ctl.to_string_lossy();
        eprintln!("forum-wm: serving forum-ctl at {ctl}");
        serve_forum_ctl(&ctl, &mut wm, &mut io_conn)?;
    }
    Ok(())
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
fn serve_forum_ctl(path: &str, wm: &mut Wm, conn: &mut ClientConn) -> io::Result<()> {
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
        let reply = forum_wm::control::handle_intent(wm, conn, intent);
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
