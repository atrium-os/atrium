//! atrium-slot-demo — exercises the slot-graph rendering path.
//!
//! Unlike `atrium-test-client` (which uses CMD_SET_ROOT + a full CAS
//! tree) and `atrium-rect-bouncer` (which re-uploads the tree each
//! frame), this app uses CMD_SLOT_* + CMD_FRAME_BEGIN/END — the path
//! that real apps like atrium-edit and atrium-term use. Every frame:
//!
//!   FRAME_BEGIN
//!   SLOT_SET_XFORM_INLINE 1 <matrix>   ← only the transform changes
//!   FRAME_END                           ← server traverses slots → render_list
//!
//! Static content (vertex/index/mesh/material/renderable) is uploaded
//! and slot 1 is allocated once at startup. After that, animation is
//! 3 commands per frame — much cheaper than rebuilding the scene tree.

use fresco_server::command::protocol::NULL_HASH;
use fresco_socket::{wire, Connection};

use std::time::{Duration, Instant};

const VIEW_W: f32 = 1280.0;
const VIEW_H: f32 = 800.0;
const RECT_W: f32 = 200.0;
const RECT_H: f32 = 140.0;
const TARGET_FPS: u64 = 30;

fn affine_at(x: f32, y: f32) -> [f32; 16] {
    wire::affine_2d(RECT_W, RECT_H, x, y)
}

fn triangle(t: f32) -> f32 {
    let f = t.fract();
    if f < 0.5 { f * 2.0 } else { (1.0 - f) * 2.0 }
}

fn main() -> std::io::Result<()> {
    let path = std::env::args().nth(1)
        .unwrap_or_else(|| "/tmp/atrium-compositor.sock".to_string());
    let mut conn = Connection::connect(&path)?;
    eprintln!("connected to {path}");

    // ── One-time uploads ──────────────────────────────────────────
    let v = conn.upload_blob(&wire::vertex_data_xy(&[
        (0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0),
    ]))?;
    let i = conn.upload_blob(&wire::index_data_u16(&[0, 1, 2, 0, 2, 3]))?;
    let mesh = conn.upload_blob(&wire::mesh(4, 6, v, i))?;
    let mat = conn.upload_blob(&wire::solid_material([0xff, 0xcc, 0x33, 0xff]))?;
    let renderable = conn.upload_blob(&wire::renderable(mesh, mat))?;

    // Initial transform.
    let xform0 = conn.upload_blob(&wire::transform_matrix(&affine_at(100.0, 100.0)))?;

    // ── Slot graph setup ─────────────────────────────────────────
    conn.frame_begin()?;
    conn.slot_alloc_renderable(/*slot_id=*/ 1, xform0, renderable)?;
    conn.slot_set_root(1)?;
    conn.frame_end()?;
    eprintln!("slot 1 set up; bouncing at {} fps", TARGET_FPS);

    let _ = NULL_HASH; // silence unused import on minimal builds

    // ── Per-frame: just update the transform ─────────────────────
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

        conn.frame_begin()?;
        conn.slot_set_xform_inline(1, &affine_at(x, y))?;
        conn.frame_end()?;

        if frame % 30 == 0 {
            eprintln!("frame {frame}: rect at ({x:.0}, {y:.0})");
        }

        let now = Instant::now();
        if next > now { std::thread::sleep(next - now); }
        next += frame_dur;
        frame += 1;
    }
}
