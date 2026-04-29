//! Real image viewer — decodes a PNG/JPG via the `image` crate and
//! displays it as a textured quad. First "real-world workflow" app
//! on Fresco-on-FreeBSD.
//!
//! Usage: image_viewer <path>
//!
//! Build: cargo build --release --example image_viewer --features image-decode

use fresco_rs::{blob, flags, matrix_identity, node_type, Connection};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1)
        .ok_or("usage: image_viewer <png-or-jpg-path>")?;

    let img = image::open(&path)?.to_rgba8();
    let (w, h) = img.dimensions();
    println!("loaded {}x{} from {path}", w, h);

    let conn = Connection::open()?;
    let disp = conn.display();
    println!("display: {}x{}", disp.width, disp.height);

    let tex_h = conn.cas_put_texture(w, h, img.as_raw())?;
    println!("texture uploaded: {:02x}{:02x}..", tex_h[0], tex_h[1]);

    // Quad sized to fit the image's aspect ratio inside [-0.7..0.7].
    let img_aspect = w as f32 / h as f32;
    let (qx, qy) = if img_aspect > 1.0 {
        (0.7, 0.7 / img_aspect)
    } else {
        (0.7 * img_aspect, 0.7)
    };
    let verts: [f32; 20] = [
        -qx, -qy, 0.0,  0.0, 1.0,
         qx, -qy, 0.0,  1.0, 1.0,
         qx,  qy, 0.0,  1.0, 0.0,
        -qx,  qy, 0.0,  0.0, 0.0,
    ];
    let idx: [u16; 6] = [0, 1, 2, 0, 2, 3];

    let vert_h = conn.cas_put(&blob::vertex_data(&verts))?;
    let idx_h  = conn.cas_put(&blob::index_data(&idx))?;
    let mesh_h = conn.cas_put(&blob::mesh(4, 6, 0x0500, &vert_h, &idx_h))?;
    let mat_h  = conn.cas_put(&blob::material_textured(&tex_h, 0xffffffff))?;
    let rend_h = conn.cas_put(&blob::renderable(&mesh_h, &mat_h))?;

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

    let slot = 1u16;
    conn.slot_alloc(slot, node_type::FRESCO_NODE_RENDERABLE, flags::FRESCO_SLOT_FLAG_VISIBLE)?;
    conn.slot_set_xform_inline(slot, &matrix_identity())?;
    conn.slot_set_content(slot, &rend_h)?;
    conn.slot_set_root(slot)?;

    conn.frame_begin(0)?;
    conn.frame_end()?;

    println!("displayed — sleeping 10s");
    std::thread::sleep(std::time::Duration::from_secs(10));
    Ok(())
}
