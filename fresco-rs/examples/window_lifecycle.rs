//! Phase B1 smoke test: drive the multi-window lifecycle ops.
//!
//! Issues CMD_CREATE_WINDOW twice, expects two distinct window_ids,
//! sets the title on each, then destroys them. No rendering yet —
//! just validates the protocol round-trip and that the server's
//! Compositor allocator is live.

use fresco_rs::Connection;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::open()?;
    let disp = conn.display();
    println!("display: {}x{}", disp.width, disp.height);

    let w1 = conn.create_window(800, 600, Some("first"))?;
    println!("created window 1: id={w1}");

    let w2 = conn.create_window(640, 480, Some("second"))?;
    println!("created window 2: id={w2}");

    if w1 == w2 {
        eprintln!("FAIL: server reused id {w1} for both windows");
        return Err("ID collision".into());
    }

    conn.window_set_title(w1, "renamed first window")?;
    println!("renamed window {w1}");

    conn.destroy_window(w1)?;
    conn.destroy_window(w2)?;
    println!("destroyed both windows");

    println!("PASS: window lifecycle round-trip works");
    Ok(())
}
