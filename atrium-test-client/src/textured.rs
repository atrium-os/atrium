//! atrium-textured — uploads a hand-built test pattern texture and
//! renders it as a textured quad. Verifies tiny_skia_backend's
//! Pattern shader path end-to-end.
//!
//! Builds a 256×256 RGBA8 checkerboard with a colorful gradient
//! overlay so we can tell from a single screenshot that texture
//! sampling, the per-vertex UVs, and tint defaults are all working.

use fresco_scene_server::command::protocol::NULL_HASH;
use fresco_socket::{wire, Connection};

const TEX_W: u32 = 256;
const TEX_H: u32 = 256;

fn main() -> std::io::Result<()> {
    let path = std::env::args().nth(1)
        .unwrap_or_else(|| "/tmp/frescod.sock".to_string());
    let mut conn = Connection::connect(&path)?;
    eprintln!("connected to {path}");

    // ── Build a 256×256 RGBA8 test pattern ──────────────────────
    // Diagonal gradient + checkerboard — busy enough to expose any
    // sampling / orientation bugs.
    let mut rgba = vec![0u8; (TEX_W * TEX_H * 4) as usize];
    for y in 0..TEX_H {
        for x in 0..TEX_W {
            let i = ((y * TEX_W + x) * 4) as usize;
            let cell = ((x / 32) ^ (y / 32)) & 1;
            let base = if cell == 0 { 60u8 } else { 200u8 };
            // Per-pixel tint: red along x, green along y.
            let r = ((x as u32 * 255) / TEX_W).min(255) as u8;
            let g = ((y as u32 * 255) / TEX_H).min(255) as u8;
            let b = base;
            // Premultiplied: A=255 means rgb stays as-is.
            rgba[i + 0] = r;
            rgba[i + 1] = g;
            rgba[i + 2] = b;
            rgba[i + 3] = 0xff;
        }
    }
    let tex_hash = conn.upload_texture(&rgba, TEX_W, TEX_H)?;
    eprintln!("uploaded texture {}x{}: {:02x}{:02x}..", TEX_W, TEX_H, tex_hash[0], tex_hash[1]);

    // ── Build a unit-rect mesh (textured material assumes UV (0..1)²
    // matches the rect's mesh-local coords). ────────────────────
    let v = conn.upload_blob(&wire::vertex_data_xy(&[
        (0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0),
    ]))?;
    let i = conn.upload_blob(&wire::index_data_u16(&[0, 1, 2, 0, 2, 3]))?;
    let mesh = conn.upload_blob(&wire::mesh(4, 6, v, i))?;

    // Textured material — no tint (0xFFFFFFFF).
    let mat = conn.upload_blob(&wire::material_textured(tex_hash, [0xff, 0xff, 0xff, 0xff]))?;

    // Place the textured rect at (200, 200) sized 600×600.
    let xform = wire::affine_2d(600.0, 600.0, 200.0, 200.0);
    let t  = conn.upload_blob(&wire::transform_matrix(&xform))?;
    let r  = conn.upload_blob(&wire::renderable(mesh, mat))?;
    let sn = conn.upload_blob(&wire::scene_node(t, r, NULL_HASH))?;
    let nl = conn.upload_blob(&wire::node_list(&[sn]))?;
    let sr = conn.upload_blob(&wire::scene_root(nl, NULL_HASH))?;
    conn.set_root(sr)?;
    eprintln!("textured rect set up; ^C to exit");
    std::thread::sleep(std::time::Duration::from_secs(3600));
    Ok(())
}
