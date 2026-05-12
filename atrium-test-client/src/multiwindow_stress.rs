//! `atrium-multiwindow-stress` — multi-window perf scenario.
//!
//! Opens N windows. Updates exactly ONE of them per frame (the
//! "active" window cycles slowly). The other N-1 windows hold a
//! static rendered scene.
//!
//! Used to verify frescod-aqueduct's per-window dirty tracking
//! actually skips rasterisation for un-dirty windows under
//! multi-window load. Per-frame work should be: 1 window's
//! rasterisation + N textured-rect composite ops, NOT N windows'
//! worth of rasterisation.
//!
//! Usage:
//!   atrium-multiwindow-stress [SOCK] [N_WINDOWS] [SWITCH_HZ]
//!
//! Defaults: /tmp/frescod.sock, N=5, SWITCH_HZ=1 (active window
//! cycles once per second).

use fresco_client::Connection;
use fresco_protocol::{RectParams, WindowHints};

use std::time::{Duration, Instant};

const FPS: u64 = 30;

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let sock = args.next().unwrap_or_else(|| "/tmp/frescod.sock".to_string());
    let n: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(5);
    let switch_hz: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(1);

    let mut conn = Connection::connect(&sock)?;
    eprintln!("connected to {sock}");
    eprintln!("opening {n} windows; active rotates at {switch_hz} Hz");

    // Open N windows in a horizontal strip. Each window's local
    // content area becomes its private surface.
    let win_w: u32 = 200;
    let win_h: u32 = 150;
    let gap: u32 = 20;
    let mut windows: Vec<u32> = Vec::with_capacity(n as usize);
    for i in 0..n {
        let w = conn.window_create(win_w, win_h,
            &format!("window-{i}"), WindowHints::default())?;
        eprintln!("  window {i}: id={w}");
        windows.push(w);
    }

    let started = Instant::now();
    let frame_dur = Duration::from_nanos(1_000_000_000 / FPS);
    let mut next = Instant::now() + frame_dur;
    let mut frame: u64 = 0;

    loop {
        let t = started.elapsed().as_secs_f32();
        let active_idx = ((t * switch_hz as f32) as usize) % n as usize;

        // Drive update into the active window only. The other
        // windows' scene state is unchanged — frescod-aqueduct's
        // per-window dirty tracker should skip rasterising them.
        let _ = gap;
        conn.set_default_window(windows[active_idx] as u16);
        conn.scene_frame_begin()?;
        let phase = (t * 2.0).sin().abs();
        conn.scene_node_rect(0, RectParams {
            x: 10.0 + phase * 50.0,
            y: 10.0 + phase * 30.0,
            w: 150.0, h: 100.0,
            r: 0.5 + 0.5 * (t * 3.0).sin(),
            g: 0.5 + 0.5 * (t * 4.0).sin(),
            b: 0.5 + 0.5 * (t * 5.0).sin(),
            a: 1.0,
        })?;
        conn.scene_frame_end()?;

        frame += 1;
        if frame % (FPS * 5) == 0 {
            eprintln!("frame {frame}: active window = {active_idx}");
        }

        let now = Instant::now();
        if next > now { std::thread::sleep(next - now); }
        next += frame_dur;
    }
}
