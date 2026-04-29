//! Text shaping + rasterization for Fresco.
//!
//! Single entry point: [`shape_and_rasterize`] takes a font, a string,
//! and a pixel size, returns a glyph atlas (RGBA8 texture pixels +
//! per-glyph quad rectangles) ready to upload via libfresco's
//! `cas_put_texture` and a per-glyph quad mesh.
//!
//! No GTK/Pango/Cairo/freetype dependencies — pure Rust:
//!   - **rustybuzz**: HarfBuzz reimplementation in Rust (shaping)
//!   - **swash**: pure-Rust glyph outlines + rasterizer
//!   - **ttf-parser**: font file parsing
//!
//! This is the "BSD-native, no Linux baggage" text stack.
//!
//! Phase 1 scope (this crate version):
//!   - Single line of LTR text in one font/size
//!   - Simple shelf-packed atlas (sufficient for short strings)
//!   - 8-bit alpha glyphs uploaded as RGBA (R=G=B=255, A=coverage),
//!     so a `material_textured` with white tint renders white text;
//!     tint with a color to recolor.
//!
//! Out of scope here (lands when needed):
//!   - Multi-line layout / paragraph wrapping
//!   - BiDi / RTL
//!   - Sub-pixel positioning
//!   - Font fallback chains
//!   - Color emoji (COLR/CPAL or CBDT)

use std::error::Error;
use std::fmt;

/// One glyph's quad: position in pixel-space relative to the text
/// origin (baseline at y=0) and UV rect in the atlas.
#[derive(Debug, Clone, Copy)]
pub struct GlyphQuad {
    /// Destination rectangle (pixel-space). dx0/dy0 = top-left,
    /// dx1/dy1 = bottom-right. dy points down (text baseline at 0,
    /// ascenders negative).
    pub dx0: f32, pub dy0: f32,
    pub dx1: f32, pub dy1: f32,
    /// UV in [0..1] sampling the atlas.
    pub u0: f32,  pub v0: f32,
    pub u1: f32,  pub v1: f32,
}

#[derive(Debug)]
pub struct GlyphAtlas {
    pub width: u32,
    pub height: u32,
    /// RGBA8, row-major, top-to-bottom. R/G/B = 255 everywhere; A is
    /// the glyph alpha coverage. Suitable for `cas_put_texture`.
    pub pixels: Vec<u8>,
    /// One quad per shaped glyph, in order.
    pub glyphs: Vec<GlyphQuad>,
    /// Total advance width of the shaped run, in pixels.
    pub advance: f32,
    /// Font ascent (above baseline, positive) in pixels at this size.
    pub ascent: f32,
    /// Font descent (below baseline, positive) in pixels at this size.
    pub descent: f32,
}

#[derive(Debug)]
pub enum TextError {
    FontParse(String),
    Empty,
    AtlasOverflow,
}

impl fmt::Display for TextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TextError::FontParse(s) => write!(f, "font parse: {s}"),
            TextError::Empty        => write!(f, "empty input"),
            TextError::AtlasOverflow => write!(f, "atlas too small"),
        }
    }
}
impl Error for TextError {}

/// Shape `text` with `font_data` at `pixel_size`, rasterize each glyph
/// into a packed atlas, and return the [GlyphAtlas].
pub fn shape_and_rasterize(
    font_data: &[u8],
    text: &str,
    pixel_size: f32,
) -> Result<GlyphAtlas, TextError> {
    if text.is_empty() {
        return Err(TextError::Empty);
    }

    // ── Shaping (rustybuzz) ──────────────────────────────────────
    let face = rustybuzz::Face::from_slice(font_data, 0)
        .ok_or_else(|| TextError::FontParse("rustybuzz: not a TTF/OTF".into()))?;
    let units_per_em = face.units_per_em() as f32;
    let scale = pixel_size / units_per_em;

    let ascent  = face.ascender()  as f32 * scale;
    let descent = (-face.descender()) as f32 * scale;

    let mut buf = rustybuzz::UnicodeBuffer::new();
    buf.push_str(text);
    let glyph_buf = rustybuzz::shape(&face, &[], buf);

    // ── Rasterization (swash) ────────────────────────────────────
    let swash_font = swash::FontRef::from_index(font_data, 0)
        .ok_or_else(|| TextError::FontParse("swash: not a TTF/OTF".into()))?;
    let mut scaler_ctx = swash::scale::ScaleContext::new();
    let mut scaler = scaler_ctx
        .builder(swash_font)
        .size(pixel_size)
        .hint(true)
        .build();

    // ── Atlas: shelf-pack each rasterized glyph ──────────────────
    let atlas_w: u32 = 512;
    let atlas_h: u32 = 512;
    let mut pixels = vec![0u8; (atlas_w * atlas_h * 4) as usize];
    let mut shelf_x: u32 = 1;   // 1-px gutter to avoid bleed
    let mut shelf_y: u32 = 1;
    let mut shelf_h: u32 = 0;
    let mut quads: Vec<GlyphQuad> = Vec::with_capacity(glyph_buf.len());

    let infos = glyph_buf.glyph_infos();
    let positions = glyph_buf.glyph_positions();
    let mut pen_x: f32 = 0.0;

    for (info, pos) in infos.iter().zip(positions.iter()) {
        let gid = info.glyph_id;

        // Render glyph to alpha bitmap via swash.
        let img = swash::scale::Render::new(&[swash::scale::Source::Outline])
            .format(swash::zeno::Format::Alpha)
            .render(&mut scaler, swash::GlyphId::from(gid as u16));

        let (gw, gh, glyph_left, glyph_top) = match img.as_ref() {
            Some(img) => (
                img.placement.width,
                img.placement.height,
                img.placement.left,
                img.placement.top,
            ),
            None => (0, 0, 0, 0),
        };

        // Place in atlas (shelf-pack).
        if gw > 0 && gh > 0 {
            if shelf_x + gw + 1 > atlas_w {
                shelf_x = 1;
                shelf_y += shelf_h + 1;
                shelf_h = 0;
            }
            if shelf_y + gh + 1 > atlas_h {
                return Err(TextError::AtlasOverflow);
            }
            shelf_h = shelf_h.max(gh);

            let img = img.unwrap();
            // Copy alpha bitmap rows into atlas as RGBA (R=G=B=255).
            for row in 0..gh {
                for col in 0..gw {
                    let src = img.data[(row * gw + col) as usize];
                    let dst = ((shelf_y + row) * atlas_w + (shelf_x + col)) as usize * 4;
                    pixels[dst]     = 255;
                    pixels[dst + 1] = 255;
                    pixels[dst + 2] = 255;
                    pixels[dst + 3] = src;
                }
            }

            // Build the destination quad: glyph's pen position offset
            // by its bearing (left/top from swash placement).
            let dx0 = pen_x + glyph_left as f32 + (pos.x_offset as f32 * scale);
            let dy0 = -(glyph_top as f32) - (pos.y_offset as f32 * scale);
            let dx1 = dx0 + gw as f32;
            let dy1 = dy0 + gh as f32;

            let u0 = shelf_x as f32 / atlas_w as f32;
            let v0 = shelf_y as f32 / atlas_h as f32;
            let u1 = (shelf_x + gw) as f32 / atlas_w as f32;
            let v1 = (shelf_y + gh) as f32 / atlas_h as f32;

            quads.push(GlyphQuad { dx0, dy0, dx1, dy1, u0, v0, u1, v1 });
            shelf_x += gw + 1;
        }

        // Advance the pen using rustybuzz's shaped advance.
        pen_x += pos.x_advance as f32 * scale;
    }

    Ok(GlyphAtlas {
        width: atlas_w, height: atlas_h, pixels,
        glyphs: quads, advance: pen_x, ascent, descent,
    })
}

/// Convert the atlas's per-glyph quads into a single mesh that, when
/// rendered with a `material_textured` referencing the atlas, draws
/// the whole text run. Output is `(verts, indices)` ready to feed
/// into `blob::vertex_data` + `blob::index_data` + `blob::mesh`.
///
///   `pixel_to_unit` scales pixel-space coordinates to your scene
///   units. e.g. for a 1024-wide window where 1 unit = full width,
///   pixel_to_unit = 1.0/512 (so font_px directly = scene units).
///   `origin_x`/`origin_y` translate the run within scene space.
///
///   Vertex layout: POSITION f32x3 + UV f32x2 (stride 20). Use mesh
///   flags 0x0500 with this output.
pub fn build_text_mesh(
    atlas: &GlyphAtlas,
    pixel_to_unit: f32,
    origin_x: f32,
    origin_y: f32,
) -> (Vec<f32>, Vec<u16>) {
    let mut verts = Vec::with_capacity(atlas.glyphs.len() * 20);
    let mut idx   = Vec::with_capacity(atlas.glyphs.len() * 6);
    let s = pixel_to_unit;
    for (i, g) in atlas.glyphs.iter().enumerate() {
        let base = i as u16 * 4;
        let x0 = origin_x + g.dx0 * s;
        let x1 = origin_x + g.dx1 * s;
        // Flip y because text-pixel-space has y down, scene has y up.
        let y0 = origin_y - g.dy1 * s;
        let y1 = origin_y - g.dy0 * s;
        // bottom-left, bottom-right, top-right, top-left
        verts.extend_from_slice(&[
            x0, y0, 0.0, g.u0, g.v1,
            x1, y0, 0.0, g.u1, g.v1,
            x1, y1, 0.0, g.u1, g.v0,
            x0, y1, 0.0, g.u0, g.v0,
        ]);
        idx.extend_from_slice(&[base, base+1, base+2, base, base+2, base+3]);
    }
    (verts, idx)
}
