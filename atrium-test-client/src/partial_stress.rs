//! `atrium-partial-stress` — intra-window damage-rect stress.
//!
//! Scenario tailored to exercise frescod-aqueduct's skip-hierarchy
//! level 3 (intra-window dirty rect / partial redraw):
//!
//! - Creates ONE 800×600 child window (not window 0; window 0's
//!   background takes the full clear path by design).
//! - Lays down 4 large static rects (corners) that never mutate.
//! - Adds a small 32×32 "cursor" rect that bounces around inside
//!   the window, mutating every frame.
//!
//! On each frame only the cursor's node hash changes. Its damage
//! rect (union of old+new bbox ≈ 64×64) covers ~0.85% of the
//! window — well below the 50% partial-redraw threshold. So
//! frescod-aqueduct should report ~100% "partial" passes after the
//! first full frame.
//!
//! Usage:
//!   atrium-partial-stress [SOCK]
//!
//! Defaults: /tmp/frescod.sock.

use fresco_client::Connection;
use fresco_protocol::{RectParams, WindowHints};

use std::time::{Duration, Instant};

const FPS: u64 = 30;
const WIN_W: u32 = 800;
const WIN_H: u32 = 600;
const CURSOR: f32 = 32.0;

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let sock = args.next().unwrap_or_else(|| "/tmp/frescod.sock".to_string());

    let mut conn = Connection::connect(&sock)?;
    eprintln!("connected to {sock}");

    let win = conn.window_create(
        WIN_W, WIN_H, "partial-stress", WindowHints::default(),
    )?;
    eprintln!("window id={win}");
    conn.set_default_window(win as u16);

    let started = Instant::now();
    let frame_dur = Duration::from_nanos(1_000_000_000 / FPS);
    let mut next = Instant::now() + frame_dur;
    let mut frame: u64 = 0;

    loop {
        let t = started.elapsed().as_secs_f32();
        conn.scene_frame_begin()?;

        // Four static corner rects (large, never change). Nodes 1..4.
        // These pin a big chunk of pixels as stable, so any change is
        // forced to be the cursor only.
        conn.scene_node_rect(1, RectParams {
            x:   0.0, y:   0.0, w: 200.0, h: 150.0,
            r: 0.20, g: 0.20, b: 0.40, a: 1.0,
        })?;
        conn.scene_node_rect(2, RectParams {
            x: 600.0, y:   0.0, w: 200.0, h: 150.0,
            r: 0.40, g: 0.20, b: 0.20, a: 1.0,
        })?;
        conn.scene_node_rect(3, RectParams {
            x:   0.0, y: 450.0, w: 200.0, h: 150.0,
            r: 0.20, g: 0.40, b: 0.20, a: 1.0,
        })?;
        conn.scene_node_rect(4, RectParams {
            x: 600.0, y: 450.0, w: 200.0, h: 150.0,
            r: 0.40, g: 0.40, b: 0.20, a: 1.0,
        })?;

        // Bouncing 32×32 cursor — node 0. Stays well inside the
        // window so its bbox + previous bbox both clip cleanly.
        let span_x = (WIN_W as f32) - CURSOR;
        let span_y = (WIN_H as f32) - CURSOR;
        let cx = (((t * 0.6).sin() * 0.5 + 0.5) * span_x).round();
        let cy = (((t * 0.43).cos() * 0.5 + 0.5) * span_y).round();
        conn.scene_node_rect(0, RectParams {
            x: cx, y: cy, w: CURSOR, h: CURSOR,
            r: 1.0, g: 1.0, b: 1.0, a: 1.0,
        })?;

        conn.scene_frame_end()?;

        frame += 1;
        if frame % (FPS * 5) == 0 {
            eprintln!("frame {frame}: cursor at ({cx:.0}, {cy:.0})");
        }

        let now = Instant::now();
        if next > now { std::thread::sleep(next - now); }
        next += frame_dur;
    }
}
