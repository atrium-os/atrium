//! atrium-slot-demo — slot-graph animation on the envelope-based wire.
//!
//! Migrated to `fresco-client` (M2.7c). The legacy "slot graph" path
//! (CMD_SLOT_ALLOC_RENDERABLE / CMD_SLOT_SET_ROOT /
//! CMD_SLOT_SET_XFORM_INLINE) was a workaround for the legacy CAS-blob
//! protocol's per-frame upload cost — re-emit just the transform, not
//! the whole tree. The new fresco-protocol model doesn't need that:
//! repeating SCENE_NODE_SET on the same node_id is already a per-node
//! delta. So this demo is now identical in shape to atrium-rect-bouncer,
//! kept as a separate binary for parity with the legacy demo set.

use fresco_client::Connection;
use fresco_protocol::RectParams;

use std::time::{Duration, Instant};

const VIEW_W: f32 = 1280.0;
const VIEW_H: f32 = 800.0;
const RECT_W: f32 = 200.0;
const RECT_H: f32 = 140.0;
const TARGET_FPS: u64 = 30;

fn triangle(t: f32) -> f32 {
    let f = t.fract();
    if f < 0.5 { f * 2.0 } else { (1.0 - f) * 2.0 }
}

fn main() -> std::io::Result<()> {
    let path = std::env::args().nth(1)
        .unwrap_or_else(|| "/tmp/frescod.sock".to_string());
    let mut conn = Connection::connect(&path)?;
    eprintln!("connected to {path}; bouncing at {TARGET_FPS} fps");

    let started = Instant::now();
    let frame_dur = Duration::from_nanos(1_000_000_000 / TARGET_FPS);
    let mut next = Instant::now() + frame_dur;
    let mut frame: u64 = 0;
    loop {
        let t = started.elapsed().as_secs_f32();
        let span_x = VIEW_W - RECT_W;
        let span_y = VIEW_H - RECT_H;
        let x = triangle(t * 0.35) * span_x;
        let y = triangle(t * 0.21 + 0.5) * span_y;

        conn.scene_frame_begin()?;
        conn.scene_node_rect(1, RectParams {
            x, y, w: RECT_W, h: RECT_H,
            r: 1.0, g: 0.8, b: 0.2, a: 1.0,
        })?;
        conn.scene_frame_end()?;

        if frame % 30 == 0 {
            eprintln!("frame {frame}: rect at ({x:.0}, {y:.0})");
        }

        let now = Instant::now();
        if next > now { std::thread::sleep(next - now); }
        next += frame_dur;
        frame += 1;
    }
}
