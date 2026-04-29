//! Pre-rasterized monospace glyph cache for the printable ASCII range.
//!
//! Each char is rasterized once at startup with swash and packed into
//! a single atlas; per-char UVs and bearings are stored in a flat
//! array indexed by `(c as u32 - 0x20) as usize`. Non-ASCII chars
//! render as a placeholder.
//!
//! Monospace: every cell on screen has the same advance, taken from
//! the 'M' glyph's measured advance.

use std::collections::HashMap;

pub struct GlyphCache {
    pub atlas:    Vec<u8>,    // RGBA8, R=G=B=255, A=coverage
    pub atlas_w:  u32,
    pub atlas_h:  u32,
    pub line_height: f32,     // pixels per line (ascender→descender→leading)
    pub cell_w:   f32,        // monospace advance, pixels
    pub baseline: f32,        // baseline y from cell-top, pixels
    glyphs: HashMap<char, GlyphEntry>,
    fallback: Option<GlyphEntry>,
}

#[derive(Debug, Clone, Copy)]
pub struct GlyphEntry {
    pub u0: f32, pub v0: f32,
    pub u1: f32, pub v1: f32,
    /// Bearing from cell origin (top-left of cell at baseline) — used
    /// to position the glyph's bitmap correctly within the cell.
    pub bearing_x: i32,
    pub bearing_y: i32,
    pub width:  i32,
    pub height: i32,
}

impl GlyphCache {
    pub fn build(font_data: &[u8], pixel_size: f32) -> Result<Self, String> {
        let face = swash::FontRef::from_index(font_data, 0)
            .ok_or_else(|| "swash: not a TTF/OTF".to_string())?;
        let mut sctx = swash::scale::ScaleContext::new();
        let mut scaler = sctx.builder(face).size(pixel_size).hint(true).build();

        // Use ttf-parser to discover advances + metrics (charmap).
        let tface = ttf_parser::Face::parse(font_data, 0)
            .map_err(|e| format!("ttf-parser: {e:?}"))?;
        let upem = tface.units_per_em() as f32;
        let scale = pixel_size / upem;
        let line_height = (tface.ascender() as f32 - tface.descender() as f32 + tface.line_gap() as f32) * scale;
        let baseline    = tface.ascender() as f32 * scale;
        // Monospace cell width: M's advance.
        let m_gid = tface.glyph_index('M').map(|g| g.0).unwrap_or(0);
        let cell_w = tface.glyph_hor_advance(ttf_parser::GlyphId(m_gid))
            .unwrap_or((upem * 0.5) as u16) as f32 * scale;

        let atlas_w: u32 = 1024;
        let atlas_h: u32 = 1024;
        let mut atlas = vec![0u8; (atlas_w * atlas_h * 4) as usize];
        let mut shelf_x: u32 = 1;
        let mut shelf_y: u32 = 1;
        let mut shelf_h: u32 = 0;
        let mut glyphs = HashMap::with_capacity(96);

        // Rasterize printable ASCII (0x20..0x7e) plus a small "tofu" box
        // glyph for fallback.
        let chars = (0x20u32..=0x7e).filter_map(char::from_u32);
        for ch in chars {
            let gid = match tface.glyph_index(ch) {
                Some(g) => g.0,
                None => continue,
            };
            let img = swash::scale::Render::new(&[swash::scale::Source::Outline])
                .format(swash::zeno::Format::Alpha)
                .render(&mut scaler, swash::GlyphId::from(gid));
            let (gw, gh, bx, by) = match img.as_ref() {
                Some(im) => (im.placement.width, im.placement.height,
                             im.placement.left, im.placement.top),
                None => (0, 0, 0, 0),
            };
            if gw > 0 && gh > 0 {
                if shelf_x + gw + 1 > atlas_w {
                    shelf_x = 1;
                    shelf_y += shelf_h + 1;
                    shelf_h = 0;
                }
                if shelf_y + gh + 1 > atlas_h { return Err("atlas overflow".into()); }
                shelf_h = shelf_h.max(gh);
                let img = img.unwrap();
                for row in 0..gh {
                    for col in 0..gw {
                        let src = img.data[(row * gw + col) as usize];
                        let dst = ((shelf_y + row) * atlas_w + (shelf_x + col)) as usize * 4;
                        atlas[dst]     = 255;
                        atlas[dst + 1] = 255;
                        atlas[dst + 2] = 255;
                        atlas[dst + 3] = src;
                    }
                }
                glyphs.insert(ch, GlyphEntry {
                    u0: shelf_x as f32 / atlas_w as f32,
                    v0: shelf_y as f32 / atlas_h as f32,
                    u1: (shelf_x + gw) as f32 / atlas_w as f32,
                    v1: (shelf_y + gh) as f32 / atlas_h as f32,
                    bearing_x: bx, bearing_y: by,
                    width: gw as i32, height: gh as i32,
                });
                shelf_x += gw + 1;
            }
        }
        // Fallback = '?' if available.
        let fallback = glyphs.get(&'?').copied();

        Ok(Self {
            atlas, atlas_w, atlas_h,
            line_height, cell_w, baseline,
            glyphs, fallback,
        })
    }

    pub fn lookup(&self, c: char) -> Option<&GlyphEntry> {
        self.glyphs.get(&c).or(self.fallback.as_ref())
    }
}
