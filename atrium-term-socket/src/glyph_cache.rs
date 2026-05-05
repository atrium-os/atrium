//! Glyph cache for atrium-term-socket on the envelope+texture-op stack.
//!
//! Each printable-ASCII char gets:
//!   - a per-glyph RGBA bitmap (premultiplied (A,A,A,A))
//!   - a CAS hash (uploaded once via `Connection::upload_blob`)
//!   - a per-glyph slot id (`OP_SLOT_SET` once, with TextureDesc)
//!
//! The renderer references glyphs by slot id. This is wasteful per-
//! glyph (no atlas, no shared texture) but matches the simplest path
//! through the new texture op (`TextureParams { x, y, w, h, slot_id }`)
//! whose params don't carry per-glyph UV coordinates. A real atlas
//! would need either UV in `TextureParams` or a bundle op that takes
//! a sub-rect — both are M3+ design choices, not blockers here.
//!
//! Cost: ~94 small CAS uploads + 94 slot-set messages at startup. Done
//! once — the editor sends nothing glyph-related per keystroke beyond
//! the per-character TEXTURE node.

use std::collections::HashMap;

use fresco_client::Connection;
use fresco_protocol::TextureFormat;
use fresco_text::shape_and_rasterize;

#[derive(Debug, Clone, Copy)]
pub struct GlyphMetrics {
    pub width:     u32,
    pub height:    u32,
    pub bearing_x: i32,
    pub bearing_y: i32,
}

#[derive(Debug, Clone, Copy)]
pub struct CachedGlyph {
    pub slot_id: u32,
    pub metrics: GlyphMetrics,
}

pub struct GlyphCache {
    pub line_height: f32,
    pub baseline:    f32,
    pub cell_w:      f32,
    pub glyphs:      HashMap<char, CachedGlyph>,
}

/// First slot id used for glyphs. Apps can use lower ids for their
/// own per-window slots (e.g. background image, icon).
const GLYPH_SLOT_BASE: u32 = 100;

impl GlyphCache {
    pub fn build(
        conn: &mut Connection,
        font_data: &[u8],
        size_px: f32,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let probe = shape_and_rasterize(font_data, "M", size_px)?;
        let line_height = probe.ascent + probe.descent + 2.0;
        let baseline    = probe.ascent;
        let cell_w      = probe.advance.max(1.0);

        let mut glyphs = HashMap::new();
        let mut next_slot = GLYPH_SLOT_BASE;

        for byte in 0x21u8..=0x7e {
            let ch = byte as char;
            let a = match shape_and_rasterize(font_data, &ch.to_string(), size_px) {
                Ok(a)  => a,
                Err(_) => continue,
            };
            let q = match a.glyphs.first() {
                Some(q) => *q,
                None    => continue,
            };
            let su0 = (q.u0 * a.width  as f32).round() as u32;
            let sv0 = (q.v0 * a.height as f32).round() as u32;
            let su1 = (q.u1 * a.width  as f32).round() as u32;
            let sv1 = (q.v1 * a.height as f32).round() as u32;
            let gw = su1.saturating_sub(su0);
            let gh = sv1.saturating_sub(sv0);
            if gw == 0 || gh == 0 { continue; }

            /* Extract glyph bytes (RGBA), premultiply (A,A,A,A) so the
             * texture op's tint=white shader produces white-on-anything
             * coverage. */
            let mut rgba = vec![0u8; (gw * gh * 4) as usize];
            for row in 0..gh {
                for col in 0..gw {
                    let src_off = (((sv0 + row) * a.width + (su0 + col)) * 4) as usize;
                    let dst_off = ((row * gw + col) * 4) as usize;
                    let alpha = a.pixels[src_off + 3];
                    rgba[dst_off]     = alpha;
                    rgba[dst_off + 1] = alpha;
                    rgba[dst_off + 2] = alpha;
                    rgba[dst_off + 3] = alpha;
                }
            }

            let hash = conn.upload_blob(&rgba)?;
            let slot_id = next_slot;
            next_slot += 1;
            conn.slot_set_texture(
                slot_id, hash, gw, gh, TextureFormat::Rgba8UnormSrgb,
            )?;

            glyphs.insert(ch, CachedGlyph {
                slot_id,
                metrics: GlyphMetrics {
                    width:     gw,
                    height:    gh,
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
