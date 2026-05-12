//! `atrium-stress` — scene-complexity sweep against frescod-aqueduct.
//!
//! Emits N rects per frame at a target FPS. Each rect bounces with
//! a different phase + frequency so the renderer can't trivially
//! cache identical output. Used to characterize how the tier-1 SW
//! backend's `wait` phase scales with scene node count.
//!
//! Usage:
//!   atrium-stress [SOCK] [N_RECTS] [TARGET_FPS]
//!
//! Defaults: /tmp/frescod.sock, N=100, FPS=30.
//!
//! frescod-aqueduct should be run with FRESCOD_UNCAPPED=1 to measure
//! the renderer's actual ceiling rather than its frame-pacing cap.

use fresco_client::Connection;
use fresco_protocol::RectParams;

use std::time::{Duration, Instant};

const VIEW_W: f32 = 1280.0;
const VIEW_H: f32 = 800.0;
const RECT_W: f32 = 80.0;
const RECT_H: f32 = 60.0;

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let sock = args.next().unwrap_or_else(|| "/tmp/frescod.sock".to_string());
    let n: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(100);
    let fps: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(30);

    let mut conn = Connection::connect(&sock)?;
    eprintln!("atrium-stress: connected to {sock}; {n} rects @ {fps} fps");

    let started = Instant::now();
    let frame_dur = Duration::from_nanos(1_000_000_000 / fps);
    let mut next = Instant::now() + frame_dur;
    let mut frame: u64 = 0;

    let span_x = VIEW_W - RECT_W;
    let span_y = VIEW_H - RECT_H;

    loop {
        let t = started.elapsed().as_secs_f32();

        conn.scene_frame_begin()?;
        for i in 0..n {
            // Each rect has a distinct phase + frequency so the
            // scene differs per node and per frame.
            let phase_x = (i as f32) * 0.137;
            let phase_y = (i as f32) * 0.231;
            let freq_x  = 0.20 + (i % 7) as f32 * 0.03;
            let freq_y  = 0.18 + (i % 11) as f32 * 0.025;
            let x = triangle(t * freq_x + phase_x) * span_x;
            let y = triangle(t * freq_y + phase_y) * span_y;

            // Per-rect colour drift so the scene is visibly busy.
            let r = 0.5 + 0.5 * (t * 0.7 + i as f32 * 0.05).sin();
            let g = 0.5 + 0.5 * (t * 0.9 + i as f32 * 0.07).sin();
            let b = 0.5 + 0.5 * (t * 1.1 + i as f32 * 0.11).sin();

            conn.scene_node_rect(i, RectParams {
                x, y, w: RECT_W, h: RECT_H,
                r, g, b, a: 0.8,
            })?;
        }
        conn.scene_frame_end()?;

        frame += 1;
        if frame % (fps * 5) == 0 {
            eprintln!("atrium-stress: frame {frame}");
        }

        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        }
        next += frame_dur;
    }
}

/// Triangle wave in [0, 1] with period 1.
fn triangle(t: f32) -> f32 {
    let f = t - t.floor();
    if f < 0.5 { 2.0 * f } else { 2.0 - 2.0 * f }
}
