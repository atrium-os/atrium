use crate::command::protocol::Hash256;
use crate::cas::store::CasStore;
use std::collections::HashMap;

const SEG_SIZE: usize = 28;

fn seg(seg_type: u8, points: &[f32]) -> [u8; SEG_SIZE] {
    let mut b = [0u8; SEG_SIZE];
    b[0] = seg_type;
    for (i, &f) in points.iter().enumerate() {
        b[4 + i * 4..8 + i * 4].copy_from_slice(&f.to_le_bytes());
    }
    b
}

struct GlyphBuilder {
    segments: Vec<[u8; SEG_SIZE]>,
    scale: f32,
    offset_x: f32,
    offset_y: f32,
}

impl GlyphBuilder {
    fn new(scale: f32, offset_x: f32, offset_y: f32) -> Self {
        Self { segments: Vec::new(), scale, offset_x, offset_y }
    }

    fn tx(&self, x: f32) -> f32 { x * self.scale + self.offset_x }
    fn ty(&self, y: f32) -> f32 { y * self.scale + self.offset_y }
}

impl ttf_parser::OutlineBuilder for GlyphBuilder {
    fn move_to(&mut self, x: f32, y: f32) {
        self.segments.push(seg(0, &[self.tx(x), self.ty(y)]));
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.segments.push(seg(1, &[self.tx(x), self.ty(y)]));
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.segments.push(seg(2, &[self.tx(x1), self.ty(y1), self.tx(x), self.ty(y)]));
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.segments.push(seg(3, &[
            self.tx(x1), self.ty(y1),
            self.tx(x2), self.ty(y2),
            self.tx(x), self.ty(y),
        ]));
    }

    fn close(&mut self) {
        self.segments.push(seg(5, &[]));
    }
}

pub struct FontData {
    data: Vec<u8>,
    glyph_cache: HashMap<(char, u32), (Hash256, Hash256)>, // (char, size_bits) → (path_data_hash, path_header_hash)
}

impl FontData {
    pub fn load(font_bytes: &[u8]) -> Option<Self> {
        ttf_parser::Face::parse(font_bytes, 0).ok()?;
        Some(Self {
            data: font_bytes.to_vec(),
            glyph_cache: HashMap::new(),
        })
    }

    pub fn glyph_path(
        &mut self,
        cas: &mut CasStore,
        ch: char,
        size: f32,
        x: f32,
        y: f32,
    ) -> Option<Hash256> {
        let size_bits = size.to_bits();
        let cache_key = (ch, size_bits);

        // check cache — same char + same size = same path data
        if let Some(&(pd_hash, _ph_hash)) = self.glyph_cache.get(&cache_key) {
            if cas.exists(&pd_hash) {
                // create a new PathHeader pointing to cached path_data but at new position
                // actually, position is handled by Transform, not path data
                // so we can reuse the entire PathHeader
                return Some(_ph_hash);
            }
        }

        let face = ttf_parser::Face::parse(&self.data, 0).ok()?;
        let glyph_id = face.glyph_index(ch)?;
        let units_per_em = face.units_per_em() as f32;
        let scale = size / units_per_em;

        let mut builder = GlyphBuilder::new(scale, 0.0, 0.0);
        face.outline_glyph(glyph_id, &mut builder)?;

        if builder.segments.is_empty() { return None; }

        // build path data blob
        let mut path_data = Vec::with_capacity(builder.segments.len() * SEG_SIZE);
        for s in &builder.segments {
            path_data.extend_from_slice(s);
        }
        let pd_hash = cas.store(&path_data);

        // build PathHeader (0x0D)
        let mut ph = [0u8; 128];
        ph[0] = 0x0D;
        ph[1] = 0x01; // FILL
        ph[12..16].copy_from_slice(&0.0005f32.to_le_bytes()); // tolerance
        ph[16..20].copy_from_slice(&(builder.segments.len() as u32).to_le_bytes());
        ph[20..22].copy_from_slice(&1u16.to_le_bytes()); // 1 subpath
        ph[32..64].copy_from_slice(&pd_hash);
        let ph_hash = cas.store(&ph);

        self.glyph_cache.insert(cache_key, (pd_hash, ph_hash));
        Some(ph_hash)
    }

    pub fn advance_width(&self, ch: char, size: f32) -> f32 {
        let face = match ttf_parser::Face::parse(&self.data, 0) {
            Ok(f) => f,
            Err(_) => return size * 0.5,
        };
        let glyph_id = match face.glyph_index(ch) {
            Some(id) => id,
            None => return size * 0.5,
        };
        let units_per_em = face.units_per_em() as f32;
        let scale = size / units_per_em;
        face.glyph_hor_advance(glyph_id)
            .map(|a| a as f32 * scale)
            .unwrap_or(size * 0.5)
    }

    pub fn layout_text(
        &mut self,
        cas: &mut CasStore,
        text: &str,
        size: f32,
        start_x: f32,
        start_y: f32,
    ) -> Vec<(Hash256, f32, f32)> {
        let mut x = start_x;
        let mut glyphs = Vec::new();

        for ch in text.chars() {
            if ch == ' ' {
                x += self.advance_width('n', size);
                continue;
            }
            if let Some(ph_hash) = self.glyph_path(cas, ch, size, x, start_y) {
                glyphs.push((ph_hash, x, start_y));
            }
            x += self.advance_width(ch, size);
        }

        glyphs
    }
}
