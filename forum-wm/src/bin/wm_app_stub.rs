//! wm-app-stub — a minimal Fresco client that creates one window and stays
//! connected. An ordinary-app stand-in so the WM's cross-app enumerate has real
//! surfaces to find in the in-VM F0 bring-up. It holds no window-management
//! capability (it never asks for one), so it models a normal application.
//!
//! Env: FRESCO_SOCKET (default /tmp/frescod.sock), APP_TITLE, APP_W, APP_H,
//!      APP_ROLE (document|panel|dialog|chrome|background; default document).

use fresco_client::Connection;
use fresco_protocol::{WindowHints, WmRole};

fn parse_role(s: &str) -> Option<WmRole> {
    Some(match s.to_ascii_lowercase().as_str() {
        "document" => WmRole::Document,
        "panel" => WmRole::Panel,
        "dialog" => WmRole::Dialog,
        "chrome" => WmRole::Chrome,
        "background" => WmRole::Background,
        _ => return None, // hud is Forum-reserved (refused server-side anyway)
    })
}

fn main() -> std::io::Result<()> {
    let sock = std::env::var("FRESCO_SOCKET").unwrap_or_else(|_| "/tmp/frescod.sock".into());
    let title = std::env::var("APP_TITLE").unwrap_or_else(|_| "app".into());
    let w: u32 = std::env::var("APP_W").ok().and_then(|s| s.parse().ok()).unwrap_or(400);
    let h: u32 = std::env::var("APP_H").ok().and_then(|s| s.parse().ok()).unwrap_or(300);
    let role = std::env::var("APP_ROLE").ok().and_then(|s| parse_role(&s));

    let mut conn = Connection::connect(&sock)?;
    let hints = WindowHints { role, ..Default::default() };
    let id = conn.window_create(w, h, title.clone(), hints)?;
    eprintln!("wm-app-stub: created window {id} ({title}, {w}x{h}, role={role:?}) on {sock}");

    // Stay connected (the compositor drops a client's windows on disconnect) and
    // log every async event we receive — so injected/real input is observable
    // end-to-end during interactive bring-up.
    loop {
        match conn.poll_event() {
            Ok(Some(ev)) => eprintln!("wm-app-stub[{title}] event: {ev:?}"),
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(e) => { eprintln!("wm-app-stub[{title}]: poll error: {e}"); break; }
        }
    }
    Ok(())
}
