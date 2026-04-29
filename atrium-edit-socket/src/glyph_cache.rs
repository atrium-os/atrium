//! Per-glyph texture cache for atrium-edit-socket.
//!
//! At startup, shape each printable-ASCII character individually with
//! `fresco_text::shape_and_rasterize`, extract its sub-image as a
//! premultiplied RGBA texture, and upload it to the server. We hold
//! onto the resulting `(texture_hash, metrics)` per character so the
//! per-frame renderer just emits RenderItems referencing pre-uploaded
//! textures — no per-frame upload work.
//!
//! This is the per-glyph-texture path. A single shared atlas + per-
//! vertex-UV path will replace it once tiny_skia_backend grows
//! stride-20 vertex support.

use std::collections::HashMap;

use fresco_server::command::protocol::Hash256;
use fresco_socket::Connection;
use fresco_text::shape_and_rasterize;

/// Pixel metrics for one cached glyph. All values in pixels at the
/// chosen size — the renderer offsets each glyph by `bearing_x` /
/// `bearing_y` from its cell origin.
#[derive(Debug, Clone, Copy)]
pub struct GlyphMetrics {
    pub width:     u32,
    pub height:    u32,
    pub bearing_x: i32,
    pub bearing_y: i32,
}

/// Glyph entry — texture hash + metrics. Material hash references a
/// `material_textured` blob for that texture, ready to drop into a
/// renderable.
#[derive(Debug, Clone)]
pub struct CachedGlyph {
    pub material: Hash256,
    pub metrics:  GlyphMetrics,
}

pub struct GlyphCache {
    pub line_height: f32,
    pub baseline:    f32,
    pub cell_w:      f32,
    pub glyphs:      HashMap<char, CachedGlyph>,
}

impl GlyphCache {
    pub fn build(
        conn: &mut Connection,
        font_data: &[u8],
        size_px: f32,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Shape one space character to read the font's vertical metrics
        // and advance — we'll use those as the cell dimensions.
        let probe = shape_and_rasterize(font_data, "M", size_px)?;
        let line_height = probe.ascent + probe.descent + 2.0;
        let baseline    = probe.ascent;
        let cell_w      = probe.advance.max(1.0);

        let mut glyphs = HashMap::new();
        for byte in 0x21u8..=0x7e {
            let ch = byte as char;
            // Shape each glyph individually. shape_and_rasterize fails
            // on empty input but never on a single char.
            let atlas = match shape_and_rasterize(font_data, &ch.to_string(), size_px) {
                Ok(a)  => a,
                Err(_) => continue,
            };
            if atlas.glyphs.is_empty() { continue; }
            let q = atlas.glyphs[0];
            let u0 = (q.u0 * atlas.width  as f32).round() as u32;
            let v0 = (q.v0 * atlas.height as f32).round() as u32;
            let u1 = (q.u1 * atlas.width  as f32).round() as u32;
            let v1 = (q.v1 * atlas.height as f32).round() as u32;
            let w = u1.saturating_sub(u0);
            let h = v1.saturating_sub(v0);
            if w == 0 || h == 0 { continue; }

            let mut rgba = Vec::with_capacity((w * h * 4) as usize);
            for y in v0..v1 {
                let s = ((y * atlas.width + u0) * 4) as usize;
                let e = ((y * atlas.width + u1) * 4) as usize;
                rgba.extend_from_slice(&atlas.pixels[s..e]);
            }
            // Premultiply: (255, 255, 255, A) → (A, A, A, A).
            for px in rgba.chunks_exact_mut(4) {
                let a = px[3];
                px[0] = a; px[1] = a; px[2] = a;
            }

            let tex = conn.upload_texture(&rgba, w, h)?;
            let mat = conn.upload_blob(&fresco_socket::wire::material_textured(tex, [0xff; 4]))?;

            // bearing_x is relative to glyph cell origin. dx0 from
            // shape is the glyph's left edge relative to the baseline
            // cursor; we use that directly as bearing_x.
            // bearing_y is the distance from baseline UP to the glyph's top.
            // q.dy0 is the glyph's top relative to baseline (negative for ascenders).
            // bearing_y = -dy0 (positive when glyph is above baseline).
            glyphs.insert(ch, CachedGlyph {
                material: mat,
                metrics: GlyphMetrics {
                    width:     w,
                    height:    h,
                    bearing_x: q.dx0.round() as i32,
                    bearing_y: (-q.dy0).round() as i32,
                },
            });
        }

        eprintln!(
            "glyph cache: {} glyphs, line_height={:.1} baseline={:.1} cell_w={:.1}",
            glyphs.len(), line_height, baseline, cell_w,
        );
        Ok(Self { line_height, baseline, cell_w, glyphs })
    }

    pub fn lookup(&self, ch: char) -> Option<&CachedGlyph> {
        self.glyphs.get(&ch)
    }
}
