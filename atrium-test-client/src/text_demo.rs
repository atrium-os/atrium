//! atrium-text-demo — first text on the FreeBSD-native stack, on the
//! envelope-based wire.
//!
//! Migrated to `fresco-client` (M2.7c). Same shaping + rasterization
//! via fresco-text (rustybuzz + swash); each glyph becomes an RGBA
//! upload, gets a per-glyph slot, and is rendered as one TEXTURE node
//! at its baseline position. Wasteful (one texture per glyph) but
//! proves the text-rendering chain end-to-end.
//!
//! Once atrium-text bundle ships (D3) with vector-outline rendering,
//! this collapses to per-string SCENE_NODE_SETs against the bundle's
//! glyph-run op. For now: explicit per-glyph slots + texture nodes.
//!
//! Premultiplication: fresco-text's atlas has (R=255,G=255,B=255,A=cov);
//! we rewrite to (A,A,A,A) so it lands premultiplied (white-opacity)
//! in the texture pipeline, matching what scene-graph TEXTURE nodes
//! expect.

use fresco_client::Connection;
use fresco_protocol::{TextureFormat, TextureParams};
use fresco_text::{shape_and_rasterize, GlyphAtlas};

const FONT_PATH:   &str = "/mnt/host/test-assets/DejaVuSansMono.ttf";
const TEXT:        &str = "Hello, FreeBSD!";
const SIZE_PX:     f32  = 64.0;
const BASELINE_X:  f32  = 80.0;
const BASELINE_Y:  f32  = 400.0;

/// Extract one glyph's RGBA bytes from the master atlas, premultiplied
/// (white * coverage). Returns `(rgba, w, h, dst_rect)`.
fn extract_glyph(atlas: &GlyphAtlas, idx: usize)
    -> Option<(Vec<u8>, u32, u32, (f32, f32, f32, f32))>
{
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
        "shaped {:?}: {} glyphs, atlas {}x{}, advance={:.1}",
        TEXT, atlas.glyphs.len(), atlas.width, atlas.height, atlas.advance,
    );

    /* Per-glyph: upload bytes → bind to a per-glyph slot → emit a
     * TEXTURE node at the glyph's destination rect. Slot ids start at
     * 100 to avoid collision with any app-level slots. */
    conn.scene_frame_begin()?;
    let mut emitted = 0u32;
    for (i, _g) in atlas.glyphs.iter().enumerate() {
        let Some((rgba, gw, gh, (dx0, dy0, dx1, dy1))) = extract_glyph(&atlas, i) else {
            continue;
        };
        let hash = conn.upload_blob(&rgba)?;
        let slot = 100 + i as u32;
        conn.slot_set_texture(slot, hash, gw, gh, TextureFormat::Rgba8UnormSrgb)?;
        conn.scene_node_texture(/*node_id=*/ slot, TextureParams {
            x: BASELINE_X + dx0,
            y: BASELINE_Y + dy0,
            w: dx1 - dx0,
            h: dy1 - dy0,
            slot_id: slot,
        })?;
        emitted += 1;
    }
    conn.scene_frame_end()?;
    eprintln!("emitted {emitted} glyph texture nodes — text should be visible");

    std::thread::sleep(std::time::Duration::from_secs(3600));
    Ok(())
}
