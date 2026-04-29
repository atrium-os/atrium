//! First textured quad on Fresco-on-FreeBSD.
//!
//! Builds a synthetic 256x256 checkerboard RGBA image, uploads it as
//! a NODE_TEXTURE (pixel data + header, two CAS blobs), creates a
//! NODE_MATERIAL_TEXTURED, and renders a quad with it.
//!
//! Validates the entire texture pipeline end-to-end without needing
//! an image-decoder dep. A real image_viewer using the `image` crate
//! is a trivial follow-on.

use fresco_rs::{blob, flags, matrix_identity, node_type, Connection};

fn make_checkerboard(size: u32, tile: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let on = ((x / tile) + (y / tile)) & 1 == 0;
            let (r, g, b) = if on { (255, 128, 0) } else { (32, 32, 32) };
            out.extend_from_slice(&[r, g, b, 255]);
        }
    }
    out
}

fn main() -> std::io::Result<()> {
    let conn = Connection::open()?;
    let disp = conn.display();
    println!("display: {}x{}", disp.width, disp.height);

    // 256x256 orange-and-grey checkerboard.
    const SIZE: u32 = 256;
    let pixels = make_checkerboard(SIZE, 32);

    // Vertices: POSITION+UV, stride 20. Quad in NDC, UVs flipped on Y
    // (Metal NDC y-up; image rows top-to-bottom).
    let verts: [f32; 20] = [
        -0.5, -0.5, 0.0,  0.0, 1.0,
         0.5, -0.5, 0.0,  1.0, 1.0,
         0.5,  0.5, 0.0,  1.0, 0.0,
        -0.5,  0.5, 0.0,  0.0, 0.0,
    ];
    let idx: [u16; 6] = [0, 1, 2, 0, 2, 3];

    // Standard scene blobs.
    let vert_h = conn.cas_put(&blob::vertex_data(&verts))?;
    let idx_h  = conn.cas_put(&blob::index_data(&idx))?;
    // Mesh flags 0x0500 = POSITION (0x0100) + UV0 (0x0400)  → stride 20.
    let mesh_h = conn.cas_put(&blob::mesh(4, 6, 0x0500, &vert_h, &idx_h))?;

    // Texture (auto-handles pixel-data + header upload).
    let tex_h = conn.cas_put_texture(SIZE, SIZE, &pixels)?;

    // Textured material — tint = white (no modulation).
    let mat_h  = conn.cas_put(&blob::material_textured(&tex_h, 0xffffffff))?;
    let rend_h = conn.cas_put(&blob::renderable(&mesh_h, &mat_h))?;

    // On-axis camera (same as hello_rect).
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
    println!("  pixels (in tex)  tex {:02x}{:02x}..  mat {:02x}{:02x}..  rend {:02x}{:02x}..",
        tex_h[0], tex_h[1], mat_h[0], mat_h[1], rend_h[0], rend_h[1]);

    // Slot tree — single visible slot at identity, content = renderable.
    let slot = 1u16;
    conn.slot_alloc(slot, node_type::FRESCO_NODE_RENDERABLE, flags::FRESCO_SLOT_FLAG_VISIBLE)?;
    conn.slot_set_xform_inline(slot, &matrix_identity())?;
    conn.slot_set_content(slot, &rend_h)?;
    conn.slot_set_root(slot)?;

    conn.frame_begin(0)?;
    conn.frame_end()?;

    println!("scene committed — sleeping 5 s. Look at the Fresco window for the orange/grey checkerboard.");
    std::thread::sleep(std::time::Duration::from_secs(5));
    Ok(())
}
