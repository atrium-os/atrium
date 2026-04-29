//! Phase B1b smoke test: slot ops carry a window_id in cmd.flags
//! and the server routes them to the right per-window slot table.
//!
//! 1. Create window 1
//! 2. Issue a CMD_SLOT_ALLOC with flags=window_id
//! 3. Server log should show "routed to window 1"
//!
//! Visible multi-window rendering is the next phase (B1c). Here we
//! only validate the protocol-level routing.

use fresco_rs::Connection;

const CMD_SLOT_ALLOC: u16 = 0x0110;
const NODE_RENDERABLE: u16 = 0x0005;
const SLOT_FLAG_VISIBLE: u32 = 0x01;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::open()?;
    let disp = conn.display();
    println!("display: {}x{}", disp.width, disp.height);

    let w = conn.create_window(800, 600, Some("routed"))?;
    println!("created window {w}");

    // Build a SLOT_ALLOC payload: slot_id(2) + node_type(2) + flags(4) +
    // xform_hash(32) + rend_hash(32) + child_count(2) + reserved(2) = 76 bytes
    let mut pld = [0u8; 76];
    pld[0..2].copy_from_slice(&7u16.to_le_bytes());                  // slot_id = 7
    pld[2..4].copy_from_slice(&NODE_RENDERABLE.to_le_bytes());
    pld[4..8].copy_from_slice(&SLOT_FLAG_VISIBLE.to_le_bytes());
    // hashes left zero, child_count=0

    // flags = window_id — server routes the slot op to window N
    conn.raw_submit(CMD_SLOT_ALLOC, w, 0, &pld)?;
    println!("submitted SLOT_ALLOC with flags={w}");

    // Brief sleep so server has time to log
    std::thread::sleep(std::time::Duration::from_millis(200));

    conn.destroy_window(w)?;
    println!("destroyed window {w}");
    println!("PASS: routing op submitted (check server log for 'routed to window {w}')");
    Ok(())
}
