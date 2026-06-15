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

/// The display socket frescod listens on. Overridable for tests / nonstandard runs.
fn socket_path() -> String {
    std::env::var("FRESCO_SOCKET").unwrap_or_else(|_| "/var/run/atrium/fresco.sock".into())
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
    let path = socket_path();
    eprintln!("forum-wm: connecting to frescod at {path}");
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

    // If a forum-ctl socket is configured, become the WM core daemon: serve the
    // chrome apps' intents (dock/bar/overview) over forum-ctl, each re-driving
    // Fresco through the same reconcile. Without it, this is the F0 one-shot.
    if let Ok(ctl) = std::env::var("FORUM_CTL_SOCKET") {
        serve_forum_ctl(&ctl, &mut wm, &mut io_conn)?;
    }
    Ok(())
}

/// Serve forum-ctl: accept chrome connections, carry out one intent each against
/// Fresco. Admission (default-deny) mirrors frescod's window-management gate:
///   1. production — getpeereid → app-registry → the owning user's `forum-control`
///      grant (the chrome apps hold it; ordinary apps don't).
///   2. dev override — FORUM_CTL_DEV set → accept a same-session peer (own uid),
///      for in-VM runs before a populated registry/policy.
fn serve_forum_ctl(path: &str, wm: &mut Wm, conn: &mut ClientConn) -> io::Result<()> {
    use std::os::unix::net::UnixListener;
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    let me = unsafe { libc::geteuid() };
    let dev = std::env::var("FORUM_CTL_DEV").is_ok();
    eprintln!("forum-wm: serving forum-ctl on {path}");
    for stream in listener.incoming() {
        let mut s = match stream { Ok(s) => s, Err(_) => continue };
        let admitted = peer_has_forum_control(&s) || (dev && peer_uid(&s) == Some(me));
        if !admitted {
            eprintln!("forum-wm: forum-ctl: rejecting peer (no forum-control grant)");
            // Drain the intent and answer with a clean error, so the chrome fails
            // with a reason instead of an abrupt EOF (symmetric with frescod's
            // IS_ERROR reply on a refused WM op).
            let _ = forum_ctl::read_frame(&mut s);
            if let Ok(bytes) = forum_ctl::encode(&forum_ctl::Reply::Err {
                message: "forbidden: forum-control capability required".into(),
            }) {
                let _ = forum_ctl::write_frame(&mut s, &bytes);
            }
            continue;
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

/// The connecting peer's uid (getpeereid over the UDS).
fn peer_uid(s: &std::os::unix::net::UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let (mut uid, mut gid) = (0u32, 0u32);
    let rc = unsafe { libc::getpeereid(s.as_raw_fd(), &mut uid, &mut gid) };
    (rc == 0).then_some(uid)
}

/// Whether the connecting chrome peer holds the `forum-control` capability:
/// getpeereid → app-registry (the app id the TCB registered for this uid) → the
/// owning user's policy grant. Fails closed (missing registry/policy → false).
fn peer_has_forum_control(s: &std::os::unix::net::UnixStream) -> bool {
    use std::os::fd::AsRawFd;
    let Ok(registry) = portcullis_peer::AppRegistry::load(portcullis_peer::DEFAULT_REGISTRY)
    else { return false };
    let Ok(peer) = portcullis_peer::resolve(s.as_raw_fd(), &registry) else { return false };
    let Some(app_id) = peer.app_id else { return false };
    let Ok(policy) = portcullis_policy::Policy::load(
        &portcullis_policy::Policy::user_path(&peer.user)) else { return false };
    policy.grants.get(&app_id)
        .and_then(|g| g.capabilities.forum_control)
        .unwrap_or(false)
}
