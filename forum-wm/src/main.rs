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

    let wm = Wm::new(screen());
    let layout = wm.reconcile(&mut io_conn)?;
    eprintln!(
        "forum-wm: declared layout — {} surface(s), focus={}",
        layout.slots.len(),
        layout.focus
    );

    // F0: a single reconcile pass proves the cap-gated client path end-to-end.
    // The event-driven loop (re-reconcile on surface-set / focus-intent changes)
    // lands once frescod emits WM change events on this channel.
    Ok(())
}
