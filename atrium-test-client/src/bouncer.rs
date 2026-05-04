//! atrium-rect-bouncer — multi-frame Fresco protocol demo.
//!
//! Animates a colored rect bouncing inside a 1280x800 viewport by
//! re-emitting the SceneGraph every frame. Each frame uploads a new
//! transform blob and a new scene_root that points at it; everything
//! else (vertex/index/mesh/material) is uploaded once and reused via
//! CAS dedup. SET_ROOT swaps the visible root atomically from the
//! render loop's POV.
//!
//! ~30 fps; the protocol round-trips per frame are: 1 transform
//! upload + 1 scene_node upload + 1 node_list upload + 1 scene_root
//! upload + SET_ROOT (no completion expected) = 5 messages, 4
//! waits-for-completion. Comfortable on a Unix socket.

use fresco_scene_server::command::protocol::{Hash256, NULL_HASH};
use fresco_socket::{wire, Connection};

use std::time::{Duration, Instant};

const VIEW_W: f32 = 1280.0;
const VIEW_H: f32 = 800.0;
const RECT_W: f32 = 220.0;
const RECT_H: f32 = 160.0;
const TARGET_FPS: u64 = 30;

fn build_root_with_transform(
    conn: &mut Connection,
    rect_mesh: Hash256,
    rect_mat: Hash256,
    sx: f32, sy: f32, tx: f32, ty: f32,
) -> std::io::Result<Hash256> {
    let xform = wire::affine_2d(sx, sy, tx, ty);
    let t  = conn.upload_blob(&wire::transform_matrix(&xform))?;
    let r  = conn.upload_blob(&wire::renderable(rect_mesh, rect_mat))?;
    let sn = conn.upload_blob(&wire::scene_node(t, r, NULL_HASH))?;
    let nl = conn.upload_blob(&wire::node_list(&[sn]))?;
    let sr = conn.upload_blob(&wire::scene_root(nl, NULL_HASH))?;
    Ok(sr)
}

fn main() -> std::io::Result<()> {
    let path = std::env::args().nth(1)
        .unwrap_or_else(|| "/tmp/frescod.sock".to_string());
    let mut conn = Connection::connect(&path)?;
    eprintln!("connected to {path}");

    // Upload static content once. CAS dedup means re-uploading these
    // each frame would be free, but explicit is clearer.
    let v = conn.upload_blob(&wire::vertex_data_xy(&[
        (0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0),
    ]))?;
    let i = conn.upload_blob(&wire::index_data_u16(&[0, 1, 2, 0, 2, 3]))?;
    let mesh = conn.upload_blob(&wire::mesh(4, 6, v, i))?;
    let mat = conn.upload_blob(&wire::solid_material([0x44, 0xaa, 0xff, 0xff]))?;

    eprintln!("static content uploaded; bouncing at {} fps", TARGET_FPS);

    let started = Instant::now();
    let frame_dur = Duration::from_nanos(1_000_000_000 / TARGET_FPS);
    let mut next = Instant::now() + frame_dur;
    let mut frame: u64 = 0;

    loop {
        let t = started.elapsed().as_secs_f32();

        // Bounce: triangle wave for x and y, different periods.
        let span_x = VIEW_W - RECT_W;
        let span_y = VIEW_H - RECT_H;
        let x = triangle(t * 0.4) * span_x;
        let y = triangle(t * 0.27 + 0.5) * span_y;

        let new_root = build_root_with_transform(
            &mut conn, mesh, mat,
            RECT_W, RECT_H, x, y,
        )?;
        conn.set_root(new_root)?;

        if frame % 30 == 0 {
            eprintln!("frame {frame}: rect at ({x:.0}, {y:.0})");
        }

        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        }
        next += frame_dur;
        frame += 1;
    }
}

/// Triangle wave in [0..1], period = 1.0/freq seconds.
fn triangle(t: f32) -> f32 {
    let f = t.fract();
    if f < 0.5 { f * 2.0 } else { (1.0 - f) * 2.0 }
}
