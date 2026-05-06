//! Single-font monospace glyph atlas for client-side text rendering.
//!
//! Builds one master `R8Unorm` atlas containing every printable-ASCII
//! glyph from a TrueType font, shelf-packs them with a 1-px gutter,
//! uploads the atlas as a single CAS blob bound to one slot, and
//! returns per-character metrics ready to feed into the atrium-text
//! glyph_run pipeline.
//!
//! Replaces the per-glyph TEXTURE pattern (94 RGBA blobs + 94 slots
//! + 94 nodes per visible char) with one atlas + one slot + one
//! GLYPH_RUN node per frame.
//!
//! Apps construct one `MonoAtlas` at startup, then per frame collect
//! `GlyphInstance`s via `instance(metrics, col, row, ...)` and emit
//! a single `GlyphRunParams`. Cursor / selection highlights stay as
//! `RectParams` nodes.

use std::collections::HashMap;
use std::io;

use fresco_protocol::{GlyphInstance, TextureFormat};
use fresco_text::shape_and_rasterize;

use crate::Connection;

#[derive(Debug, Clone, Copy)]
pub struct GlyphMetrics {
    pub atlas_u:   u32,
    pub atlas_v:   u32,
    pub atlas_w:   u32,
    pub atlas_h:   u32,
    pub bearing_x: i32,
    pub bearing_y: i32,
}

pub struct MonoAtlas {
    pub atlas_slot:   u32,
    pub atlas_width:  u32,
    pub atlas_height: u32,
    pub line_height:  f32,
    pub baseline:     f32,
    pub cell_w:       f32,
    pub glyphs:       HashMap<char, GlyphMetrics>,
}

const ATLAS_W: u32 = 512;
const ATLAS_H: u32 = 512;

impl MonoAtlas {
    /// Build an atlas containing every printable-ASCII glyph from
    /// `font_data` at `size_px`, upload it to the connection's CAS,
    /// and bind it to `atlas_slot`. Returns the metrics for the
    /// renderer to consult per frame.
    pub fn build(
        conn:       &mut Connection,
        font_data:  &[u8],
        size_px:    f32,
        atlas_slot: u32,
    ) -> io::Result<Self> {
        let probe = shape_and_rasterize(font_data, "M", size_px)
            .map_err(|e| io::Error::new(io::ErrorKind::Other,
                format!("shape probe: {e}")))?;
        let line_height = probe.ascent + probe.descent + 2.0;
        let baseline    = probe.ascent;
        let cell_w      = probe.advance.max(1.0);

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
                return Err(io::Error::new(io::ErrorKind::Other, format!(
                    "glyph atlas overflow at U+{:04X}; bump ATLAS_H or shrink font",
                    byte as u32)));
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
        conn.slot_set_texture(atlas_slot, hash, ATLAS_W, ATLAS_H,
                              TextureFormat::R8Unorm)?;

        log::info!(
            "MonoAtlas: {} glyphs in slot {atlas_slot}, line_height={:.1} \
             baseline={:.1} cell_w={:.1}",
            glyphs.len(), line_height, baseline, cell_w,
        );
        Ok(Self {
            atlas_slot,
            atlas_width: ATLAS_W, atlas_height: ATLAS_H,
            line_height, baseline, cell_w, glyphs,
        })
    }

    pub fn lookup(&self, ch: char) -> Option<&GlyphMetrics> {
        self.glyphs.get(&ch)
    }

    /// Build a `GlyphInstance` placing `m` at column `col`, row `row`
    /// in a monospace grid. The caller's `GlyphRunParams.x/y` is the
    /// run origin; the shader resolves `bearing_y` (baseline-to-top)
    /// against `origin.y`, so callers should set `y = pad_y + baseline`.
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
