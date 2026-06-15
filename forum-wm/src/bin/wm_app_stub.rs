//! wm-app-stub — a minimal Fresco client that creates one window and stays
//! connected. An ordinary-app stand-in so the WM's cross-app enumerate has real
//! surfaces to find in the in-VM F0 bring-up. It holds no window-management
//! capability (it never asks for one), so it models a normal application.
//!
//! Env: FRESCO_SOCKET (default /tmp/frescod.sock), APP_TITLE, APP_W, APP_H.

use fresco_client::Connection;

fn main() -> std::io::Result<()> {
    let sock = std::env::var("FRESCO_SOCKET").unwrap_or_else(|_| "/tmp/frescod.sock".into());
    let title = std::env::var("APP_TITLE").unwrap_or_else(|_| "app".into());
    let w: u32 = std::env::var("APP_W").ok().and_then(|s| s.parse().ok()).unwrap_or(400);
    let h: u32 = std::env::var("APP_H").ok().and_then(|s| s.parse().ok()).unwrap_or(300);

    let mut conn = Connection::connect(&sock)?;
    let id = conn.window_create(w, h, title.clone(), Default::default())?;
    eprintln!("wm-app-stub: created window {id} ({title}, {w}x{h}) on {sock}");

    // Stay connected — the compositor drops a client's windows on disconnect.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
