//! PT (partial-update transport) demo — drives the in-app sub-rect
//! damage path end-to-end against a running `frescod-aqueduct`.
//!
//! Sequence:
//!   1. Connect, create a window, bind a full-window RGBA texture slot
//!      (whole upload), show it via a TEXTURE scene node, present.
//!      → frescod renders a FULL pass (new node).
//!   2. Update only a small sub-rectangle of the texture in place
//!      (`slot_update_region`) and present WITH a damage rect
//!      (`window_present_with_damage`). The TEXTURE node's params are
//!      unchanged, so frescod's per-node hash sees no scene delta — the
//!      ONLY damage is the app-declared rect. With PT.4 wired, frescod
//!      folds that into the frame's damage and takes a PARTIAL
//!      (scissored) pass; without it, the unchanged node would be
//!      fast-path-skipped.
//!
//! Validation signal: frescod's per-window profile log reports a
//! partial pass for the second present (look for `partial=` / the
//! partial-pct line). Run frescod-aqueduct with logging, then this
//! client; `FRESCOD_SOCK` selects the socket (default /tmp/frescod.sock).
//!
//! Env knobs: `PT_HOLD_MS` (how long to hold the socket open after the
//! damaged present so the compositor renders before EOF; default 1500).

use std::time::Duration;

use fresco_client::{Connection, DamageRect};
use fresco_protocol::{TextureParams, TextureFormat};

const WIN_W: u32 = 256;
const WIN_H: u32 = 256;
const SLOT: u32 = 1;
const NODE: u32 = 1;

/// Solid-colour RGBA8 surface, `w*h*4` bytes.
fn solid(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 4) as usize);
    for _ in 0..(w * h) { v.extend_from_slice(&rgba); }
    v
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sock = std::env::var("FRESCOD_SOCK")
        .unwrap_or_else(|_| "/tmp/frescod.sock".to_string());
    let hold_ms: u64 = std::env::var("PT_HOLD_MS")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(1500);

    let mut conn = Connection::connect(&sock)?;
    eprintln!("atrium-pt-demo: connected to {sock}");

    let win = conn.window_create(WIN_W, WIN_H, "pt-demo", Default::default())?;
    conn.set_default_window(win as u16);
    eprintln!("atrium-pt-demo: window {win} created ({WIN_W}x{WIN_H})");

    // ── Frame 1: whole-surface present (binds the slot + node). ──
    let base = solid(WIN_W, WIN_H, [40, 80, 160, 255]); // opaque blue-ish
    let hash = conn.upload_blob(&base)?;
    conn.slot_set_texture(SLOT, hash, WIN_W, WIN_H, TextureFormat::Rgba8UnormSrgb)?;
    conn.scene_frame_begin()?;
    conn.scene_node_texture(NODE, TextureParams {
        x: 0.0, y: 0.0, w: WIN_W as f32, h: WIN_H as f32, slot_id: SLOT,
    })?;
    conn.scene_frame_end()?;
    conn.window_present(win)?;
    eprintln!("atrium-pt-demo: frame 1 — whole-surface present (expect FULL pass)");

    std::thread::sleep(Duration::from_millis(300));

    // ── Frame 2: damaged sub-rect present. Update a 48×32 red patch at
    // (64, 96) in place; the TEXTURE node params are unchanged, so the
    // app-declared damage is the only dirty signal. ──
    let (dx, dy, dw, dh) = (64u32, 96u32, 48u32, 32u32);
    let patch = solid(dw, dh, [220, 30, 30, 255]); // opaque red
    conn.slot_update_region(SLOT, dx, dy, dw, dh, patch)?;
    conn.window_present_with_damage(win, DamageRect { x: dx, y: dy, w: dw, h: dh })?;
    eprintln!("atrium-pt-demo: frame 2 — damaged present ({dw}x{dh} @ {dx},{dy}) \
               (expect PARTIAL pass)");

    // Hold the socket so frescod renders frame 2 before we EOF.
    std::thread::sleep(Duration::from_millis(hold_ms));
    eprintln!("atrium-pt-demo: done");
    Ok(())
}
