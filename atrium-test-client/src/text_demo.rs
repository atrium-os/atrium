//! atrium-text-demo — first text on the FreeBSD-native stack.
//!
//! Uses `fresco-text` (rustybuzz shaping + swash rasterization) to
//! produce a glyph atlas and per-glyph quads, then uploads each glyph
//! as its own small RGBA texture and renders it at the baseline.
//! Wasteful (one texture per glyph) but proves the text-rendering
//! chain end-to-end without per-vertex UV support in the backend.
//!
//! Once tiny_skia_backend grows per-vertex-UV support (step 2c.10),
//! a single shared atlas + UV-mapped glyph quads will replace this.
//!
//! The atlas's source layout is `(R=255, G=255, B=255, A=coverage)`.
//! We rewrite each per-glyph sub-image to `(A, A, A, A)` so it lands
//! premultiplied (white-opacity) in tiny-skia's Pixmap, which is what
//! the textured-material path expects.

use fresco_scene_server::command::protocol::{Hash256, NULL_HASH};
use fresco_socket::{wire, Connection};
use fresco_text::{shape_and_rasterize, GlyphAtlas};

const FONT_PATH: &str = "/mnt/host/test-assets/DejaVuSansMono.ttf";
const TEXT: &str = "Hello, FreeBSD!";
const SIZE_PX: f32 = 64.0;
const BASELINE_X: f32 = 80.0;
const BASELINE_Y: f32 = 400.0;

/// Extract one glyph's RGBA bytes from the master atlas, premultiplied
/// (white * coverage). Returns `(rgba, w, h, dst_rect)`.
fn extract_glyph(atlas: &GlyphAtlas, idx: usize) -> Option<(Vec<u8>, u32, u32, (f32, f32, f32, f32))> {
    let q = &atlas.glyphs[idx];
    let u0 = (q.u0 * atlas.width  as f32).round() as u32;
    let v0 = (q.v0 * atlas.height as f32).round() as u32;
    let u1 = (q.u1 * atlas.width  as f32).round() as u32;
    let v1 = (q.v1 * atlas.height as f32).round() as u32;
    let w = u1.saturating_sub(u0);
    let h = v1.saturating_sub(v0);
    if w == 0 || h == 0 { return None; }

    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in v0..v1 {
        let row_start = ((y * atlas.width + u0) * 4) as usize;
        let row_end   = ((y * atlas.width + u1) * 4) as usize;
        rgba.extend_from_slice(&atlas.pixels[row_start..row_end]);
    }
    for px in rgba.chunks_exact_mut(4) {
        let a = px[3];
        px[0] = a;
        px[1] = a;
        px[2] = a;
    }
    Some((rgba, w, h, (q.dx0, q.dy0, q.dx1, q.dy1)))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1)
        .unwrap_or_else(|| "/tmp/frescod.sock".to_string());
    let mut conn = Connection::connect(&path)?;
    eprintln!("connected to {path}");

    let font = std::fs::read(FONT_PATH)?;
    let atlas = shape_and_rasterize(&font, TEXT, SIZE_PX)?;
    eprintln!(
        "shaped {:?}: {} glyphs, atlas {}x{}, advance={:.1}, ascent={:.1} descent={:.1}",
        TEXT, atlas.glyphs.len(), atlas.width, atlas.height,
        atlas.advance, atlas.ascent, atlas.descent,
    );

    // Static unit-rect mesh — same one for every glyph.
    let v = conn.upload_blob(&wire::vertex_data_xy(&[
        (0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0),
    ]))?;
    let i = conn.upload_blob(&wire::index_data_u16(&[0, 1, 2, 0, 2, 3]))?;
    let mesh = conn.upload_blob(&wire::mesh(4, 6, v, i))?;

    let mut nodes: Vec<Hash256> = Vec::with_capacity(atlas.glyphs.len());

    for i in 0..atlas.glyphs.len() {
        let Some((rgba, gw, gh, (dx0, dy0, dx1, dy1))) = extract_glyph(&atlas, i) else {
            continue;
        };
        let tex_h = conn.upload_texture(&rgba, gw, gh)?;
        let mat   = conn.upload_blob(&wire::material_textured(tex_h, [0xff; 4]))?;

        let xform = wire::affine_2d(
            dx1 - dx0, dy1 - dy0,
            BASELINE_X + dx0, BASELINE_Y + dy0,
        );
        let t  = conn.upload_blob(&wire::transform_matrix(&xform))?;
        let r  = conn.upload_blob(&wire::renderable(mesh, mat))?;
        let sn = conn.upload_blob(&wire::scene_node(t, r, NULL_HASH))?;
        nodes.push(sn);
    }
    eprintln!("uploaded {} glyph render-nodes", nodes.len());

    let nl = conn.upload_blob(&wire::node_list(&nodes))?;
    let sr = conn.upload_blob(&wire::scene_root(nl, NULL_HASH))?;
    conn.set_root(sr)?;
    eprintln!("SET_ROOT — text should now be visible");

    std::thread::sleep(std::time::Duration::from_secs(3600));
    Ok(())
}
