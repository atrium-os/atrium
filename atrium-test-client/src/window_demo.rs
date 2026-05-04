//! atrium-window-demo — bidirectional events on the envelope-based
//! Fresco socket protocol.
//!
//! Migrated to `fresco-client` (M2.7b canary). Replaces the legacy
//! 128-byte CMD_CREATE_WINDOW / COMP_WINDOW_CREATED dance with the
//! aqueduct-envelope OP_WINDOW_CREATE call that returns the
//! assigned window_id via the IS_RESPONSE flag.
//!
//! The async-event surface is unchanged conceptually — `wait_event()`
//! still returns the next server-pushed event; the wire format
//! underneath is now fresco-protocol's `EV_WINDOW_*` opcodes
//! (RESIZED / FOCUS_CHANGED / CLOSE_REQUESTED / DPI_CHANGED) instead
//! of the legacy COMP_INPUT_KEY / COMP_FOCUS_CHANGED.

use fresco_client::Connection;
use fresco_protocol::WindowHints;

fn main() -> std::io::Result<()> {
    let path = std::env::args().nth(1)
        .unwrap_or_else(|| "/tmp/frescod.sock".to_string());
    let mut conn = Connection::connect(&path)?;
    eprintln!("connected to {path}");

    let win = conn.window_create(800, 600, "hello-window", WindowHints::default())?;
    eprintln!("server assigned window_id = {win}");

    /* Hold socket open so the WM keeps the window alive (and so any
     * resize/close event would land here). */
    eprintln!("waiting for further events; ^C to exit");
    loop {
        match conn.wait_event(None)? {
            Some(ev) => eprintln!("event: {ev:?}"),
            None     => break,
        }
    }
    Ok(())
}
