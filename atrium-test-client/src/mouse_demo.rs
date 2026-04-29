//! atrium-mouse-demo — connects to atrium-compositor, prints mouse
//! events as they stream in. Smoke-test for the
//! `COMP_INPUT_MOUSE_MOVE/BUTTON/SCROLL` path driven by the compositor's
//! native tablet reader.
//!
//! Usage (in the VM, with a compositor already running):
//!   atrium-mouse-demo
//!
//! Move the cursor / click / scroll in the QEMU window; events should
//! appear here in real time.

use fresco_socket::{Connection, Event};

fn main() -> std::io::Result<()> {
    let sock = std::env::var("ATRIUM_COMPOSITOR_SOCK")
        .unwrap_or_else(|_| "/tmp/atrium-compositor.sock".to_string());
    let mut conn = Connection::connect(&sock)?;
    eprintln!("atrium-mouse-demo: connected to {sock}");
    eprintln!("move / click / scroll inside the QEMU window — events stream below.");

    loop {
        let ev = conn.wait_event(None)?;
        let Some(ev) = ev else { continue };
        match ev {
            Event::MouseMove { x, y, .. } => {
                println!("move    x={x:7.1} y={y:7.1}");
            }
            Event::MouseButton { button, pressed, x, y, modifiers, .. } => {
                // HID Usage Page 0x09 button numbers.
                let name = match button {
                    1 => "primary",
                    2 => "secondary",
                    3 => "middle",
                    4 => "back",
                    5 => "forward",
                    other => { println!("btn   #{other} {} mods=0x{modifiers:02x} ({x:.1},{y:.1})",
                                       if pressed { "down" } else { "up" }); continue; }
                };
                println!("btn   {name:9} {} mods=0x{modifiers:02x} ({x:.1},{y:.1})",
                         if pressed { "down" } else { "up" });
            }
            Event::Scroll { dx, dy, .. } => {
                println!("scroll  dx={dx:+.1} dy={dy:+.1}");
            }
            _ => {}
        }
    }
}
