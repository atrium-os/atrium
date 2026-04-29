//! Two-window visual smoke test for Phase B1c compose path.
//!
//! Window 0 (the screen) draws an orange rect at the origin.
//! Window 1 is created at offset (200, 150) and draws a teal rect.
//! Both should be visible simultaneously: server's compose pass
//! merges window 1's render items onto the screen.

use fresco_rs::{blob, flags, matrix_identity, node_type, Connection};

fn quad_renderable(conn: &Connection, r: f32, g: f32, b: f32) -> std::io::Result<[u8; 32]> {
    let verts: [f32; 12] = [
        -0.5, -0.5, 0.0,
         0.5, -0.5, 0.0,
         0.5,  0.5, 0.0,
        -0.5,  0.5, 0.0,
    ];
    let idx: [u16; 6] = [0, 1, 2, 0, 2, 3];
    let v = conn.cas_put(&blob::vertex_data(&verts))?;
    let i = conn.cas_put(&blob::index_data(&idx))?;
    let m = conn.cas_put(&blob::mesh(4, 6, 0x0100, &v, &i))?;
    let mat = conn.cas_put(&blob::material_solid(r, g, b, 1.0))?;
    conn.cas_put(&blob::renderable(&m, &mat))
}

fn main() -> std::io::Result<()> {
    let conn = Connection::open()?;
    let disp = conn.display();
    println!("display: {}x{}", disp.width, disp.height);

    let aspect = disp.width as f32 / disp.height as f32;
    let cam_xform = [
        1.0, 0.0, 0.0, 0.0,
        0.0, 1.0, 0.0, 0.0,
        0.0, 0.0, 1.0, 0.0,
        0.0, 0.0, 5.0, 1.0,
    ];
    let cam_xform_h = conn.cas_put(&blob::transform(&cam_xform))?;
    let cam_h = conn.cas_put(&blob::camera(0.7853982, aspect, 0.1, 100.0, &cam_xform_h))?;

    // ── Window 1: orange rect, decorated, short title ──────────
    let w1 = conn.create_window(400, 300, Some("orange"))?;
    println!("created window {w1}");
    conn.window_set_pos(w1, 200.0, 200.0)?;
    let orange = quad_renderable(&conn, 1.0, 0.5, 0.0)?;
    conn.set_default_window(w1);
    conn.set_camera(&cam_h)?;
    conn.slot_alloc(1, node_type::FRESCO_NODE_RENDERABLE, flags::FRESCO_SLOT_FLAG_VISIBLE)?;
    conn.slot_set_xform_inline(1, &matrix_identity())?;
    conn.slot_set_content(1, &orange)?;
    conn.slot_set_root(1)?;
    conn.frame_begin(0)?;
    conn.frame_end()?;

    // ── Window 2: teal rect, decorated ─────────────────────────
    let w2 = conn.create_window(400, 300, Some("teal"))?;
    println!("created window {w2}");
    conn.window_set_pos(w2, 600.0, 350.0)?;

    let teal = quad_renderable(&conn, 0.0, 0.7, 0.7)?;
    conn.set_default_window(w2);
    conn.set_camera(&cam_h)?;
    conn.slot_alloc(1, node_type::FRESCO_NODE_RENDERABLE, flags::FRESCO_SLOT_FLAG_VISIBLE)?;
    let mut xform = matrix_identity();
    xform[12] = 1.5;
    xform[13] = -0.5;
    conn.slot_set_xform_inline(1, &xform)?;
    conn.slot_set_content(1, &teal)?;
    conn.slot_set_root(1)?;
    conn.frame_begin(0)?;
    conn.frame_end()?;

    println!("both windows committed — sleeping 30 s (drag the titlebar!)");
    std::thread::sleep(std::time::Duration::from_secs(30));

    conn.set_default_window(0);
    conn.destroy_window(w1)?;
    conn.destroy_window(w2)?;
    println!("OK");
    Ok(())
}
