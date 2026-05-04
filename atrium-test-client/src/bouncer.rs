//! atrium-rect-bouncer — multi-frame demo on the envelope-based wire.
//!
//! Migrated to `fresco-client` (M2.7b). Animates one rect bouncing in a
//! 1280×800 viewport by re-emitting `SCENE_NODE_SET` for the same
//! `node_id` each frame — last-write-wins semantics in `WindowSceneState`
//! make this a single-op-per-frame update instead of the legacy 5-blob
//! upload + SET_ROOT dance.
//!
//! ~30 fps; per frame = 1 SCENE_FRAME_BEGIN + 1 SCENE_NODE_SET +
//! 1 SCENE_FRAME_END = 3 messages, no waits.

use fresco_client::Connection;
use fresco_protocol::RectParams;

use std::time::{Duration, Instant};

const VIEW_W:    f32 = 1280.0;
const VIEW_H:    f32 = 800.0;
const RECT_W:    f32 = 220.0;
const RECT_H:    f32 = 160.0;
const TARGET_FPS: u64 = 30;

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

        /* Triangle-wave bounce; different periods for x and y. */
        let span_x = VIEW_W - RECT_W;
        let span_y = VIEW_H - RECT_H;
        let x = triangle(t * 0.4) * span_x;
        let y = triangle(t * 0.27 + 0.5) * span_y;

        conn.scene_frame_begin()?;
        conn.scene_node_rect(/*node_id=*/ 0, RectParams {
            x, y, w: RECT_W, h: RECT_H,
            r: 0.27, g: 0.67, b: 1.0, a: 1.0,
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

/// Triangle wave in [0..1], period = 1.0/freq seconds.
fn triangle(t: f32) -> f32 {
    let f = t.fract();
    if f < 0.5 { f * 2.0 } else { (1.0 - f) * 2.0 }
}
