//! Glyph cache for atrium-edit-socket on the M6.1 atrium-text bundle.
//!
//! One master atlas containing every printable-ASCII glyph, shelf-
//! packed at startup, uploaded as a single `TextureFormat::R8Unorm`
//! CAS blob bound to one slot. Each frame the renderer emits one
//! GLYPH_RUN scene node carrying GlyphInstance entries for every
//! visible character.
//!
//! Replaces the per-glyph TEXTURE-node pattern (94 RGBA blobs + 94
//! slots + 94 nodes per visible char) with one atlas + one slot +
//! one node per frame. ~30× wire-byte reduction on a typical screen
//! of editor text.

use std::collections::HashMap;

use fresco_client::Connection;
use fresco_protocol::{GlyphInstance, TextureFormat};
use fresco_text::shape_and_rasterize;

#[derive(Debug, Clone, Copy)]
pub struct GlyphMetrics {
    pub atlas_u:   u32,
    pub atlas_v:   u32,
    pub atlas_w:   u32,
    pub atlas_h:   u32,
    pub bearing_x: i32,
    pub bearing_y: i32,
}

pub struct GlyphCache {
    pub atlas_slot:   u32,
    pub atlas_width:  u32,
    pub atlas_height: u32,
    pub line_height:  f32,
    pub baseline:     f32,
    pub cell_w:       f32,
    pub glyphs:       HashMap<char, GlyphMetrics>,
}

const ATLAS_SLOT: u32 = 100;
const ATLAS_W:    u32 = 512;
const ATLAS_H:    u32 = 512;

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

        /* Master R8 atlas, shelf-packed. 1-px gutter; abort on
         * overflow so the dev catches under-sized atlases early. */
        let mut pixels = vec![0u8; (ATLAS_W * ATLAS_H) as usize];
        let mut shelf_x: u32 = 1;
        let mut shelf_y: u32 = 1;
        let mut shelf_h: u32 = 0;

        let mut glyphs = HashMap::new();

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

            if shelf_x + gw + 1 > ATLAS_W {
                shelf_x = 1;
                shelf_y += shelf_h + 1;
                shelf_h = 0;
            }
            if shelf_y + gh + 1 > ATLAS_H {
                return Err(format!(
                    "glyph atlas overflow at U+{:04X}; bump ATLAS_H or shrink font",
                    byte as u32).into());
            }
            shelf_h = shelf_h.max(gh);

            for row in 0..gh {
                for col in 0..gw {
                    let src_off = (((sv0 + row) * a.width + (su0 + col)) * 4) as usize;
                    let dst_off = ((shelf_y + row) * ATLAS_W + (shelf_x + col)) as usize;
                    pixels[dst_off] = a.pixels[src_off + 3];
                }
            }

            glyphs.insert(ch, GlyphMetrics {
                atlas_u:   shelf_x,
                atlas_v:   shelf_y,
                atlas_w:   gw,
                atlas_h:   gh,
                bearing_x: q.dx0.round() as i32,
                bearing_y: (-q.dy0).round() as i32,
            });
            shelf_x += gw + 1;
        }

        let hash = conn.upload_blob(&pixels)?;
        conn.slot_set_texture(ATLAS_SLOT, hash, ATLAS_W, ATLAS_H,
                              TextureFormat::R8Unorm)?;

        eprintln!(
            "glyph cache: {} glyphs in 1 atlas slot, line_height={:.1} \
             baseline={:.1} cell_w={:.1}",
            glyphs.len(), line_height, baseline, cell_w,
        );
        Ok(Self {
            atlas_slot: ATLAS_SLOT,
            atlas_width: ATLAS_W, atlas_height: ATLAS_H,
            line_height, baseline, cell_w, glyphs,
        })
    }

    pub fn lookup(&self, ch: char) -> Option<&GlyphMetrics> {
        self.glyphs.get(&ch)
    }

    /// Build a `GlyphInstance` placing `m` at column `col`, row `row`
    /// in a monospace grid where the run's origin is the top-left
    /// corner (origin.y = pad_y + baseline; the shader subtracts
    /// `bearing_y` so callers pass the row's baseline implicitly via
    /// the run origin).
    pub fn instance(m: &GlyphMetrics, col: usize, row: usize,
                    cell_w: f32, line_h: f32) -> GlyphInstance {
        GlyphInstance {
            dx: col as f32 * cell_w,
            dy: row as f32 * line_h,
            atlas_u: m.atlas_u,
            atlas_v: m.atlas_v,
            atlas_w: m.atlas_w,
            atlas_h: m.atlas_h,
            bearing_x: m.bearing_x as f32,
            bearing_y: m.bearing_y as f32,
        }
    }
}
