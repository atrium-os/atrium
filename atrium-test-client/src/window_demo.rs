//! atrium-window-demo — exercises bidirectional events on the socket.
//!
//! Sends CMD_CREATE_WINDOW, then uses `Connection::create_window` (which
//! internally calls `wait_event` until the matching WindowCreated arrives).
//! Verifies the demux machinery: the server's response Completion is
//! event-shaped (COMP_WINDOW_CREATED) and routes through the new
//! `pending_events` queue rather than the old direct-response path.
//!
//! Sleeps after, so a second `wait_event` would block indefinitely
//! waiting for async events (resize, close-requested) that today only
//! fire under WM-driven user input — which we don't have until step
//! 2(c.12) plumbs `/dev/usbhid`.

use fresco_socket::Connection;

fn main() -> std::io::Result<()> {
    let path = std::env::args().nth(1)
        .unwrap_or_else(|| "/tmp/frescod.sock".to_string());
    let mut conn = Connection::connect(&path)?;
    eprintln!("connected to {path}");

    let win = conn.create_window(800, 600, Some("hello-window"))?;
    eprintln!("server assigned window_id = {win}");

    // Hold socket open so the WM keeps the window alive (and so
    // any future resize/close event would land here).
    eprintln!("waiting for further events; ^C to exit");
    loop {
        match conn.wait_event(None)? {
            Some(ev) => eprintln!("event: {ev:?}"),
            None => break,
        }
    }
    Ok(())
}
