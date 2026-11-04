//! The F2 fuzz corpus (threat model §2.5.4): random scenes, packed, then
//! rewritten the way a hostile renderer could, keeping only the header
//! valid (`Frame::validate` accepts every frame made here). Shared by
//! tests/fuzz.rs (reference vs Tier-2 vs GIR) and gpusim's oracle tests
//! (RDNA4 code, buffers against unmapped pages).

use crate::*;

/// splitmix64: deterministic, no dependency.
pub struct Rng(pub u64);
impl Rng {
    pub fn next(&mut self) -> u64 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    pub fn below(&mut self, n: u32) -> u32 {
        (self.next() % n.max(1) as u64) as u32
    }
    pub fn chance(&mut self, pct: u32) -> bool {
        self.below(100) < pct
    }
    pub fn f(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }
    /// A hostile float: mostly in range, sometimes anything.
    pub fn wild(&mut self, lo: f32, hi: f32) -> f32 {
        match self.below(12) {
            0 => f32::NAN,
            1 => f32::INFINITY,
            2 => f32::NEG_INFINITY,
            3 => [1e30, -1e30, 3e9, -3e9, 2147483648.0, -2147483904.0][self.below(6) as usize],
            4 => f32::from_bits(self.next() as u32),
            5 => [0.0, -0.0, f32::MIN_POSITIVE, 1e-30][self.below(4) as usize],
            _ => self.f(lo, hi),
        }
    }
    /// A hostile u32: small, near a limit, or anything.
    pub fn word(&mut self, small: u32) -> u32 {
        match self.below(6) {
            0 => u32::MAX - self.below(4),
            1 => 0x8000_0000 - self.below(2),
            2 => self.next() as u32,
            _ => self.below(small),
        }
    }
    pub fn color(&mut self) -> [f32; 4] {
        [self.f(0.0, 1.0), self.f(0.0, 1.0), self.f(0.0, 1.0), if self.chance(50) { 1.0 } else { self.f(0.0, 1.0) }]
    }
}

/// A well-formed scene, as a renderer following the protocol would pack.
pub fn scene(r: &mut Rng, w: u32, h: u32) -> Scene {
    let (fw, fh) = (w as f32, h as f32);
    let mut s = Scene::default();
    for _ in 0..r.below(6) {
        s.rects.push(Rect {
            position: [r.f(-10.0, fw), r.f(-10.0, fh)],
            size: [r.f(0.0, fw), r.f(0.0, fh)],
            color: r.color(),
            radius: if r.chance(50) { 0.0 } else { r.f(-2.0, 20.0) },
        });
    }
    for _ in 0..r.below(4) {
        s.paths.push(Path {
            model: [r.f(0.0, fw), r.f(0.0, fh), r.f(0.0, fw), r.f(0.5, 6.0)],
            angle: r.f(-4.0, 4.0),
            color: r.color(),
        });
    }
    for _ in 0..r.below(4) {
        let (tw, th) = (1 + r.below(9), 1 + r.below(9));
        let format = if r.chance(50) { TexFormat::R8 } else { TexFormat::Rgba8Srgb };
        let data = (0..(tw * th) as usize * format.bytes_per_texel()).map(|_| r.next() as u8).collect();
        s.textures.push(Texture { width: tw, height: th, format, data });
    }
    let nt = s.textures.len() as u32;
    if nt > 0 {
        for _ in 0..r.below(3) {
            let quads = (0..1 + r.below(3)).map(|_| [r.f(-5.0, fw), r.f(-5.0, fh), r.f(0.5, fw), r.f(0.5, fh)]).collect();
            s.texture_batches.push(TextureBatch { texture: r.below(nt) as usize, quads });
        }
        for _ in 0..r.below(3) {
            let atlas = r.below(nt) as usize;
            let (aw, ah) = (s.textures[atlas].width as f32, s.textures[atlas].height as f32);
            let runs = (0..1 + r.below(2))
                .map(|_| GlyphRun {
                    origin: [r.f(0.0, fw), r.f(0.0, fh)],
                    color: r.color(),
                    glyphs: (0..1 + r.below(4))
                        .map(|_| Glyph {
                            d_offset: [r.f(-3.0, 20.0), r.f(-3.0, 3.0)],
                            atlas_uv: [r.f(0.0, aw), r.f(0.0, ah), r.f(0.5, aw), r.f(0.5, ah)],
                            bearing: [r.f(-1.0, 2.0), r.f(0.0, 8.0)],
                        })
                        .collect(),
                })
                .collect();
            s.glyph_batches.push(GlyphBatch { atlas, runs });
        }
    }
    s
}

/// Rewrite what a hostile renderer controls, keeping the header valid.
pub fn corrupt(r: &mut Rng, f: &mut Frame) {
    let (fw, fh) = (f.header.width as f32, f.header.height as f32);
    let n_tex = f.tex.len() as u32;
    // Extra ops past the scene's, then rewrite some fields of any op.
    for _ in 0..r.below(4) {
        let mut o = f.ops[r.below(f.ops.len() as u32) as usize];
        o.meta[0] = r.below(4);
        f.ops.push(o);
    }
    for o in f.ops.iter_mut() {
        if !r.chance(40) {
            continue;
        }
        match r.below(6) {
            0 => o.meta[0] = if r.chance(70) { r.below(4) } else { r.word(8) },
            1 => o.meta[1] = r.word(n_tex + 2),
            2 => {
                let k = r.below(4) as usize;
                o.p3[k] = r.wild(-20.0, if k % 2 == 0 { fw + 20.0 } else { fh + 20.0 });
            }
            3 => o.p0[r.below(4) as usize] = r.wild(-20.0, fw + 20.0),
            4 => o.p2[r.below(4) as usize] = r.wild(-2.0, 2.0),
            _ => o.p1[r.below(4) as usize] = r.wild(-0.5, 1.5),
        }
    }
    let nw = f.texels.len() as u32;
    for t in f.tex.iter_mut() {
        if r.chance(30) {
            match r.below(4) {
                0 => t[0] = r.word(nw + 4),
                1 => t[1] = r.word(12),
                2 => t[2] = r.word(12),
                _ => t[3] = r.word(3),
            }
        }
    }
    if r.chance(20) {
        f.tex.push([r.word(nw + 4), r.word(12), r.word(12), r.word(2)]);
    }
    let op_count = if r.chance(70) { f.ops.len() as u32 } else { r.below(f.ops.len() as u32 + 1) };
    f.header.op_count = op_count;
    f.header.wpt = op_count.div_ceil(32);
    f.header.tex_count = r.below(f.tex.len() as u32 + 1);
    f.header.texel_words = if r.chance(70) { f.texels.len() as u32 } else { r.below(f.texels.len() as u32 + 1) };
    // Often make every buffer exactly as long as the header says, so an
    // access one past the declared end is one past the buffer.
    if r.chance(60) {
        f.ops.truncate((op_count as usize).max(1));
        f.tex.truncate((f.header.tex_count as usize).max(1));
        f.texels.truncate((f.header.texel_words as usize).max(1));
    }
}

/// One fuzz frame: a random scene, packed, then usually corrupted.
pub fn frame(r: &mut Rng) -> Frame {
    let w = 1 + r.below(56);
    let h = 1 + r.below(40);
    let pitch = w + if r.chance(50) { 0 } else { r.below(9) };
    let mut f = Frame::pack(&scene(r, w, h), w, h, pitch);
    if r.chance(85) {
        corrupt(r, &mut f);
    }
    f
}


/// One fuzz compose pass and its heap: surfaces of random sizes and
/// pixels, layers placed anywhere (partly or wholly off screen, scrolled,
/// clipped, empty), random opacities including NaN and out of range. The
/// layer list is trusted in production; this checks the kernel is exact and
/// in bounds for any list `Compose::validate` accepts.
pub fn compose_case(r: &mut Rng) -> (compose::Compose, Vec<u32>) {
    use compose::{Compose, Layer};
    let w = 1 + r.below(48);
    let h = 1 + r.below(40);
    let pitch = w + if r.chance(50) { 0 } else { r.below(5) };
    let mut heap: Vec<u32> = Vec::new();
    let mut surfaces = Vec::new();
    for _ in 0..1 + r.below(5) {
        let (sw, sh) = (r.below(30), r.below(30));
        let sp = sw + if r.chance(50) { 0 } else { r.below(4) };
        let off = heap.len() as u32;
        heap.extend((0..sp * sh).map(|_| r.next() as u32));
        surfaces.push([off, sp, sw, sh]);
    }
    if heap.is_empty() {
        heap.push(0);
    }
    let mut layers = Vec::new();
    for _ in 0..r.below(9) {
        let s = surfaces[r.below(surfaces.len() as u32) as usize];
        let (x, y) = (r.below(w + 20) as i32 - 10, r.below(h + 20) as i32 - 10);
        let opacity = match r.below(6) {
            0 => r.wild(0.0, 1.0),
            1 | 2 => r.f(0.0, 1.0),
            _ => 1.0,
        };
        let mut l = Layer::at(s, x, y, opacity);
        if r.chance(40) {
            // Clip to a random rect, possibly inverted or empty.
            l.rect = [l.rect[0] + r.below(8) as i32, l.rect[1] + r.below(8) as i32, l.rect[2] - r.below(8) as i32, l.rect[3] - r.below(8) as i32];
        }
        if r.chance(30) {
            // Scrolled: the surface's origin up/left of the rect.
            l.origin = [l.origin[0] - r.below(10) as i32, l.origin[1] - r.below(10) as i32];
        }
        layers.push(l);
    }
    let heap_words = if r.chance(80) { heap.len() as u32 } else { r.below(heap.len() as u32 + 1) };
    let mut c = Compose::new(&layers, w, h, pitch, heap_words);
    if c.validate(c.target_words(), heap.len()).is_err() {
        // heap_words cut a surface short: the whole heap, as the broker would.
        c.header.heap_words = heap.len() as u32;
    }
    (c, heap)
}
