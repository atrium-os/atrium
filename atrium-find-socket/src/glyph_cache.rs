//! Atlas-based glyph cache for atrium-find-socket.
//!
//! Shapes each printable-ASCII character *individually* and shelf-
//! packs every glyph's coverage bitmap into one shared atlas. Each
//! per-glyph material references the atlas with the glyph's UV cell.
//!
//! The per-char approach (vs. shaping the whole ASCII range as one
//! string) avoids any ambiguity about input-char → output-glyph
//! ordering: rustybuzz can reorder, drop, or merge glyphs depending
//! on font features, and threading per-input-byte cluster info
//! through the API just to undo that reordering is more code than
//! shaping each char alone. Cost: ~94 shape calls, all sub-millisecond.

use std::collections::HashMap;

use fresco_server::command::protocol::Hash256;
use fresco_socket::Connection;
use fresco_text::shape_and_rasterize;

#[derive(Debug, Clone, Copy)]
pub struct GlyphMetrics {
    pub width:     u32,
    pub height:    u32,
    pub bearing_x: i32,
    pub bearing_y: i32,
}

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

const ATLAS_W: u32 = 512;
const ATLAS_H: u32 = 512;

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

        // Master atlas: RGBA, premultiplied alpha (A,A,A,A).
        let mut atlas = vec![0u8; (ATLAS_W * ATLAS_H * 4) as usize];
        // Shelf-pack cursor.
        let mut shelf_x: u32 = 1;
        let mut shelf_y: u32 = 1;
        let mut shelf_h: u32 = 0;

        // Per-char metrics + UV — collected, materials uploaded after
        // the atlas texture is uploaded once.
        let mut pending: Vec<(char, GlyphMetrics, [f32; 4])> = Vec::new();

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
            // Pixel-space bounding box of this glyph in its own atlas.
            let su0 = (q.u0 * a.width  as f32).round() as u32;
            let sv0 = (q.v0 * a.height as f32).round() as u32;
            let su1 = (q.u1 * a.width  as f32).round() as u32;
            let sv1 = (q.v1 * a.height as f32).round() as u32;
            let gw = su1.saturating_sub(su0);
            let gh = sv1.saturating_sub(sv0);
            if gw == 0 || gh == 0 { continue; }

            // Shelf-pack into master atlas (1 px gutter).
            if shelf_x + gw + 1 > ATLAS_W {
                shelf_x = 1;
                shelf_y += shelf_h + 1;
                shelf_h = 0;
            }
            if shelf_y + gh + 1 > ATLAS_H {
                return Err("master atlas overflow".into());
            }
            shelf_h = shelf_h.max(gh);

            // Copy + premultiply: source has (255, 255, 255, A); dest
            // gets (A, A, A, A).
            for row in 0..gh {
                for col in 0..gw {
                    let src_off = (((sv0 + row) * a.width + (su0 + col)) * 4) as usize;
                    let dst_off = (((shelf_y + row) * ATLAS_W + (shelf_x + col)) * 4) as usize;
                    let alpha = a.pixels[src_off + 3];
                    atlas[dst_off]     = alpha;
                    atlas[dst_off + 1] = alpha;
                    atlas[dst_off + 2] = alpha;
                    atlas[dst_off + 3] = alpha;
                }
            }

            let u0 = shelf_x as f32 / ATLAS_W as f32;
            let v0 = shelf_y as f32 / ATLAS_H as f32;
            let u1 = (shelf_x + gw) as f32 / ATLAS_W as f32;
            let v1 = (shelf_y + gh) as f32 / ATLAS_H as f32;
            shelf_x += gw + 1;

            pending.push((ch, GlyphMetrics {
                width:     gw,
                height:    gh,
                bearing_x: q.dx0.round() as i32,
                bearing_y: (-q.dy0).round() as i32,
            }, [u0, v0, u1, v1]));
        }

        let atlas_tex = conn.upload_texture(&atlas, ATLAS_W, ATLAS_H)?;
        eprintln!(
            "glyph atlas: {}x{} px, {} glyphs uploaded as 1 CAS texture",
            ATLAS_W, ATLAS_H, pending.len()
        );

        let mut glyphs = HashMap::new();
        for (ch, metrics, uv) in pending {
            let mat = conn.upload_blob(&fresco_socket::wire::material_textured_uv(
                atlas_tex,
                [0xff; 4],
                uv,
            ))?;
            glyphs.insert(ch, CachedGlyph { material: mat, metrics });
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
