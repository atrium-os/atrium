//! Rust port of libfresco's hello_rect.c — proves the FFI works.
//! Builds the same scene, expects an orange square in the host's
//! Fresco window.

use fresco_rs::{blob, flags, matrix_identity, node_type, Connection};

fn main() -> std::io::Result<()> {
    let conn = Connection::open()?;
    let disp = conn.display();
    println!("display: {}x{}", disp.width, disp.height);

    let verts: [f32; 12] = [
        -0.5, -0.5, 0.0,
         0.5, -0.5, 0.0,
         0.5,  0.5, 0.0,
        -0.5,  0.5, 0.0,
    ];
    let idx: [u16; 6] = [0, 1, 2, 0, 2, 3];

    let vert_h  = conn.cas_put(&blob::vertex_data(&verts))?;
    let idx_h   = conn.cas_put(&blob::index_data(&idx))?;
    let mesh_h  = conn.cas_put(&blob::mesh(4, 6, 0x0100, &vert_h, &idx_h))?;
    let mat_h   = conn.cas_put(&blob::material_solid(1.0, 0.5, 0.0, 1.0))?;
    let rend_h  = conn.cas_put(&blob::renderable(&mesh_h, &mat_h))?;

    // On-axis camera: same shape as the C example. Camera at (0,0,5),
    // looking down -z, no tilt; the camera-to-world matrix is just
    // a +5 z translation in the server's row-major convention.
    let cam_xform = [
        1.0, 0.0, 0.0, 0.0,
        0.0, 1.0, 0.0, 0.0,
        0.0, 0.0, 1.0, 0.0,
        0.0, 0.0, 5.0, 1.0,
    ];
    let cam_xform_h = conn.cas_put(&blob::transform(&cam_xform))?;
    let aspect = disp.width as f32 / disp.height as f32;
    let cam_h = conn.cas_put(&blob::camera(0.7853982, aspect, 0.1, 100.0, &cam_xform_h))?;
    conn.set_camera(&cam_h)?;

    println!("blobs uploaded:");
    println!("  verts {:02x}{:02x}..  idx {:02x}{:02x}..", vert_h[0], vert_h[1], idx_h[0], idx_h[1]);
    println!("  mesh  {:02x}{:02x}..  mat {:02x}{:02x}..  rend {:02x}{:02x}..",
        mesh_h[0], mesh_h[1], mat_h[0], mat_h[1], rend_h[0], rend_h[1]);

    let slot = 1u16;
    conn.slot_alloc(slot, node_type::FRESCO_NODE_RENDERABLE, flags::FRESCO_SLOT_FLAG_VISIBLE)?;
    conn.slot_set_xform_inline(slot, &matrix_identity())?;
    conn.slot_set_content(slot, &rend_h)?;
    conn.slot_set_root(slot)?;

    conn.frame_begin(0)?;
    conn.frame_end()?;

    println!("scene committed — sleeping 5 s. Look at the Fresco window!");
    std::thread::sleep(std::time::Duration::from_secs(5));
    println!("OK");
    Ok(())
}
