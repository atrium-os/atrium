//! atrium-input-probe — a window that prints every input event it gets,
//! one line each, with whether a press carried an activation token. The
//! Tactus VM check (scripts/vm-tactus-check.sh) sends keys and pointer
//! moves through QEMU's USB devices and reads these lines; the gamepad
//! check (`GAMEPAD=1`) reads its pad lines, and the `pad-std` lines
//! pergola-gamepad derives from them (standard names).
//!
//! With `PROBE_PAD_CLAIM=1` it claims the first pad it is given
//! (fresco-capabilities.md §4) and prints `pad-claim` lines.
//!
//! `PROBE_COLOR=r,g,b` (0..1 each) fills the window with that colour
//! (the lock gate tells its windows apart in a screenshot).
//!
//! Usage: atrium-input-probe [fresco socket] [seconds]

use std::time::{Duration, Instant};

use fresco_client::{Connection, Event};
use fresco_protocol::{RectParams, WindowHints};

fn main() -> std::io::Result<()> {
    let path = std::env::args().nth(1).unwrap_or_else(|| "/atrium/sockets/fresco/fresco.sock".to_string());
    let secs: u64 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(300);
    let mut conn = Connection::connect(&path)?;
    let (w, h) = (1280, 800);
    let win = conn.window_create(w, h, "input-probe", WindowHints::default())?;
    conn.set_default_window(win as u16);
    conn.scene_frame_begin()?;
    let c: Vec<f32> = std::env::var("PROBE_COLOR").ok().map(|v| v.split(',').filter_map(|x| x.trim().parse().ok()).collect()).unwrap_or_default();
    let (r, g, b) = match c[..] { [r, g, b] => (r, g, b), _ => (0.1, 0.3, 0.2) };
    conn.scene_node_rect(1, RectParams { x: 0.0, y: 0.0, w: w as f32, h: h as f32, r, g, b, a: 1.0, radius: 0.0 })?;
    conn.scene_frame_end()?;
    println!("input-probe: window {win}");
    let end = Instant::now() + Duration::from_secs(secs);
    let mut pads = pergola_gamepad::Pads::new();
    let mut claim = std::env::var("PROBE_PAD_CLAIM").is_ok_and(|v| v == "1");
    let tok = |a: &[u8; 16]| if a.iter().any(|&b| b != 0) { "token" } else { "no-token" };
    while Instant::now() < end {
        let Some(ev) = conn.wait_event(Some(Duration::from_millis(500)))? else { continue };
        let std = match &ev {
            Event::GamepadConnect { device_id, vendor, product, buttons, axes, name, .. } => {
                pads.connect(*device_id, *vendor, *product, *buttons, *axes, name)
            }
            Event::GamepadDisconnect { device_id, .. } => pads.disconnect(*device_id),
            Event::GamepadButton { device_id, usage, pressed, .. } => pads.button(*device_id, *usage, *pressed),
            Event::GamepadAxis { device_id, usage, value, .. } => pads.axis(*device_id, *usage, *value),
            _ => Vec::new(),
        };
        match ev {
            Event::Key { window_id, hid_usage, pressed, modifiers, activation, repeat } => {
                let r = if repeat { " repeat" } else { "" };
                println!("key window={window_id} usage=0x{hid_usage:04x} pressed={} mods={modifiers} {}{r}", pressed as u8, tok(&activation))
            }
            Event::PointerButton { window_id, button, pressed, activation, .. } => {
                println!("button window={window_id} button={button} pressed={} {}", pressed as u8, tok(&activation))
            }
            Event::PointerMotion { window_id, x, y } => println!("motion window={window_id} x={x:.0} y={y:.0}"),
            Event::PointerScroll { window_id, dx, dy, phase } => println!("scroll window={window_id} dx={dx} dy={dy} phase={phase}"),
            Event::PointerMagnify { window_id, x, y, scale, phase } => println!("magnify window={window_id} x={x:.0} y={y:.0} scale={scale:.2} phase={phase}"),
            Event::GamepadConnect { window_id, device_id, vendor, product, buttons, axes, name } => {
                println!("pad-connect window={window_id} device={device_id} id={vendor:04x}:{product:04x} buttons={buttons} axes={axes} name={name:?}");
                if std::mem::take(&mut claim) {
                    conn.pad_claim(window_id, device_id, true)?;
                }
            }
            Event::GamepadClaim { window_id, device_id, held } => {
                println!("pad-claim window={window_id} device={device_id} held={}", held as u8)
            }
            Event::GamepadDisconnect { window_id, device_id } => println!("pad-disconnect window={window_id} device={device_id}"),
            Event::GamepadButton { window_id, device_id, usage, pressed } => {
                println!("pad-button window={window_id} device={device_id} usage=0x{usage:08x} pressed={}", pressed as u8)
            }
            Event::GamepadAxis { window_id, device_id, usage, value } => {
                println!("pad-axis window={window_id} device={device_id} usage=0x{usage:08x} value={value}")
            }
            _ => {}
        }
        for c in std {
            println!("pad-std {c:?}");
        }
        use std::io::Write;
        std::io::stdout().flush()?;
    }
    Ok(())
}
