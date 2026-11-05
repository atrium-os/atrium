//! A Rust model of `bin.comp` + `raster.comp`, statement for statement. It is
//! the oracle the executors are checked against, and a plain CPU fallback.

use crate::{Frame, Op, KIND_PATH, KIND_RECT, KIND_TEXTURE, TILE};

/// Branch counters (feature `coverage`): what a corpus actually exercised.
pub mod coverage {
    /// The branches counted, by index.
    pub const NAMES: [&str; 22] = [
        "bin: empty bbox",
        "bin: bbox clipped to the target",
        "bin: bbox inside the target",
        "path: hit",
        "path: miss",
        "outside p0",
        "rect: sharp",
        "rect: rounded",
        "texture",
        "glyph",
        "unknown kind (drawn as a glyph)",
        "texture index past tex_count",
        "texture of zero or > MAX_TEX_DIM size",
        "texel past texel_words",
        "texel: rgba8",
        "texel: r8",
        "fill: outside its clip",
        "fill: block header past texel_words",
        "fill: tile outside its grid",
        "fill: list word past texel_words",
        "fill: line index past its lines",
        "fill: drawn",
    ];
    #[cfg(feature = "coverage")]
    static HITS: [std::sync::atomic::AtomicU64; NAMES.len()] =
        [const { std::sync::atomic::AtomicU64::new(0) }; NAMES.len()];

    #[inline(always)]
    pub(crate) fn hit(_i: usize) {
        #[cfg(feature = "coverage")]
        HITS[_i].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// The counts so far (all zero without the feature).
    pub fn counts() -> [u64; NAMES.len()] {
        #[cfg(feature = "coverage")]
        return std::array::from_fn(|i| HITS[i].load(std::sync::atomic::Ordering::Relaxed));
        #[cfg(not(feature = "coverage"))]
        [0; NAMES.len()]
    }
}
use coverage::hit;

/// `bin.comp`: the tile masks.
pub fn bin(f: &Frame) -> Vec<u32> {
    let h = &f.header;
    let mut mask = vec![0u32; f.mask_words()];
    let (fw, fh) = (h.width as f32, h.height as f32);
    for i in 0..h.op_count as usize {
        let b = f.ops[i].p3;
        let x0 = clamp_(b[0], 0.0, fw) as i32;
        let y0 = clamp_(b[1], 0.0, fh) as i32;
        let x1 = clamp_(b[2], 0.0, fw) as i32;
        let y1 = clamp_(b[3], 0.0, fh) as i32;
        if x1 <= x0 || y1 <= y0 {
            hit(0);
            continue;
        }
        let clipped = !(b[0] >= 0.0 && b[1] >= 0.0 && b[2] <= fw && b[3] <= fh);
        hit(if clipped { 1 } else { 2 });
        let (tx0, ty0) = (x0 as u32 / TILE, y0 as u32 / TILE);
        let (tx1, ty1) = ((x1 - 1) as u32 / TILE, (y1 - 1) as u32 / TILE);
        let (word, bit) = (i / 32, 1u32 << (i % 32));
        for ty in ty0..=ty1 {
            for tx in tx0..=tx1 {
                mask[((ty * h.tiles_x + tx) * h.wpt) as usize + word] |= bit;
            }
        }
    }
    mask
}

// The kernels' NaN-defined min/max/clamp (common.glsl), not f32's.
fn min_(a: f32, b: f32) -> f32 {
    if a < b { a } else { b }
}
fn max_(a: f32, b: f32) -> f32 {
    if a > b { a } else { b }
}
fn clamp_(v: f32, lo: f32, hi: f32) -> f32 {
    if v >= lo { if v <= hi { v } else { hi } } else { lo }
}

/// raster.comp's `mix_`: a + t * (b - a).
fn mix(a: f32, b: f32, t: f32) -> f32 {
    a + t * (b - a)
}

fn mix4(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    [mix(a[0], b[0], t), mix(a[1], b[1], t), mix(a[2], b[2], t), mix(a[3], b[3], t)]
}

fn texel(f: &Frame, t: [u32; 4], x: i32, y: i32) -> [f32; 4] {
    // GLSL clamp(x, lo, hi) is min(max(x, lo), hi): no panic when hi < lo.
    let x = x.max(0).min(t[1] as i32 - 1);
    let y = y.max(0).min(t[2] as i32 - 1);
    let idx = (y as u32).wrapping_mul(t[1]).wrapping_add(x as u32);
    let dec = &f.lut[..256];
    let wi = t[0].wrapping_add(if t[3] == 0 { idx } else { idx / 4 });
    if wi >= f.header.texel_words {
        hit(13);
        return [0.0; 4];
    }
    if t[3] == 0 {
        hit(14);
        let w = f.texels[wi as usize];
        return [
            dec[(w & 255) as usize],
            dec[((w >> 8) & 255) as usize],
            dec[((w >> 16) & 255) as usize],
            (w >> 24) as f32 * (1.0 / 255.0),
        ];
    }
    hit(15);
    let w = f.texels[wi as usize];
    let v = ((w >> (8 * (idx % 4))) & 255) as f32 * (1.0 / 255.0);
    [v, 0.0, 0.0, 1.0]
}

fn sample_bilinear(f: &Frame, ti: u32, uv: [f32; 2]) -> [f32; 4] {
    if ti >= f.header.tex_count {
        hit(11);
        return [0.0; 4];
    }
    let t = f.tex[ti as usize];
    if t[1] == 0 || t[2] == 0 || t[1] > crate::MAX_TEX_DIM || t[2] > crate::MAX_TEX_DIM {
        hit(12);
        return [0.0; 4];
    }
    let tc = [uv[0] * t[1] as f32 - 0.5, uv[1] * t[2] as f32 - 0.5];
    let fl = [tc[0].floor(), tc[1].floor()];
    let fr = [tc[0] - fl[0], tc[1] - fl[1]];
    let x = clamp_(fl[0], -1.0, t[1] as f32) as i32;
    let y = clamp_(fl[1], -1.0, t[2] as f32) as i32;
    let a = mix4(texel(f, t, x, y), texel(f, t, x + 1, y), fr[0]);
    let b = mix4(texel(f, t, x, y + 1), texel(f, t, x + 1, y + 1), fr[0]);
    mix4(a, b, fr[1])
}

fn inside(r: [f32; 4], pc: [f32; 2]) -> bool {
    pc[0] >= r[0] && pc[0] < r[0] + r[2] && pc[1] >= r[1] && pc[1] < r[1] + r[3]
}

/// `raster.comp`'s `fill_shade`: a filled path's colour at `pc`.
fn fill_shade(f: &Frame, o: &Op, pc: [f32; 2]) -> [f32; 4] {
    use crate::fill::{coverage, fill_line, ENTRY_WORDS, HEADER_WORDS, MAX_GRID, MAX_TILE_SEGMENTS};
    let c = o.p0;
    if !(pc[0] >= c[0] && pc[0] < c[2] && pc[1] >= c[1] && pc[1] < c[3]) {
        hit(16);
        return [0.0; 4];
    }
    let tw_ = f.header.texel_words;
    let t = |i: u32| f.texels[i as usize];
    let b = o.meta[1];
    if b >= tw_ || tw_ - b < HEADER_WORDS {
        hit(17);
        return [0.0; 4];
    }
    let (tx0, ty0, gw, gh) = (t(b), t(b + 1), t(b + 2), t(b + 3));
    let (sbase, scount, rule) = (t(b + 4), t(b + 5), t(b + 6));
    let px = pc[0] - 0.5;
    let py = pc[1] - 0.5;
    let (x, y) = (px as u32, py as u32);
    let tx = (x / TILE).wrapping_sub(tx0);
    let ty = (y / TILE).wrapping_sub(ty0);
    if gw > MAX_GRID || gh > MAX_GRID || tx >= gw || ty >= gh || sbase > tw_ || scount > (tw_ - sbase) / 4 {
        hit(18);
        return [0.0; 4];
    }
    let idx = ty * gw + tx;
    if idx >= (tw_ - b - HEADER_WORDS) / ENTRY_WORDS {
        hit(18);
        return [0.0; 4];
    }
    let e = b + HEADER_WORDS + idx * ENTRY_WORDS;
    let ls = t(e);
    let mut lc = t(e + 1);
    if lc > MAX_TILE_SEGMENTS {
        lc = MAX_TILE_SEGMENTS;
    }
    let mut acc = f32::from_bits(t(e + 2 + y % TILE));
    if ls > tw_ || lc > tw_ - ls {
        hit(19);
        lc = 0;
    }
    for k in 0..lc {
        let si = t(ls + k);
        if si >= scount {
            hit(20);
            continue;
        }
        let w = sbase + si * 4;
        let s = [f32::from_bits(t(w)), f32::from_bits(t(w + 1)), f32::from_bits(t(w + 2)), f32::from_bits(t(w + 3))];
        acc += fill_line(s, px, py);
    }
    hit(21);
    let cov = coverage(acc, rule == 1);
    [o.p1[0], o.p1[1], o.p1[2], o.p1[3] * cov]
}

/// One op's straight-alpha colour at pixel centre `pc`.
pub fn shade(f: &Frame, o: &Op, pc: [f32; 2]) -> [f32; 4] {
    let kind = o.meta[0];
    if kind == crate::KIND_FILL {
        return fill_shade(f, o, pc);
    }
    if kind == KIND_PATH {
        let v = [pc[0] - o.p0[0], pc[1] - o.p0[1]];
        let lx = o.p2[0] * v[0] + o.p2[1] * v[1];
        let ly = o.p2[0] * v[1] - o.p2[1] * v[0];
        let (hl, hw) = (o.p0[2] * 0.5, o.p0[3] * 0.5);
        if lx >= -hl && lx < hl && ly >= -hw && ly < hw {
            hit(3);
            return o.p1;
        }
        hit(4);
        return [0.0; 4];
    }
    if !inside(o.p0, pc) {
        hit(5);
        return [0.0; 4];
    }
    if kind == KIND_RECT {
        let radius = o.p2[0];
        if radius <= 0.0 {
            hit(6);
            return o.p1;
        }
        hit(7);
        let half = [o.p0[2] * 0.5, o.p0[3] * 0.5];
        let r = min_(radius, min_(half[0], half[1]));
        let p = [(pc[0] - o.p0[0]) - half[0], (pc[1] - o.p0[1]) - half[1]];
        let q = [p[0].abs() - half[0] + r, p[1].abs() - half[1] + r];
        let qp = [max_(q[0], 0.0), max_(q[1], 0.0)];
        let lq = (qp[0] * qp[0] + qp[1] * qp[1]).sqrt();
        let d = min_(max_(q[0], q[1]), 0.0) + lq - r;
        let mut aa = 1.0;
        if q[0] > 0.0 && q[1] > 0.0 {
            aa = (q[0] + q[1]) / lq;
        }
        let alpha = clamp_(0.5 - d / aa, 0.0, 1.0);
        return [o.p1[0], o.p1[1], o.p1[2], o.p1[3] * alpha];
    }
    let t = [(pc[0] - o.p0[0]) / o.p0[2], (pc[1] - o.p0[1]) / o.p0[3]];
    if kind == KIND_TEXTURE {
        hit(8);
        return sample_bilinear(f, o.meta[1], t);
    }
    // KIND_GLYPH, and as the kernel does, any other kind.
    hit(if kind == crate::KIND_GLYPH { 9 } else { 10 });
    let uv = [mix(o.p2[0], o.p2[2], t[0]), mix(o.p2[1], o.p2[3], t[1])];
    let cov = sample_bilinear(f, o.meta[1], uv)[0];
    [o.p1[0], o.p1[1], o.p1[2], o.p1[3] * cov]
}

/// Linear to an sRGB byte through the LUT's thresholds.
pub fn encode(lut: &[f32], v: f32) -> u32 {
    let enc = &lut[256..];
    let mut lo = 0u32;
    let mut step = 128u32;
    while step > 0 {
        if lo + step <= 255 && v >= enc[(lo + step - 1) as usize] {
            lo += step;
        }
        step /= 2;
    }
    lo
}

/// `raster.comp` for one pixel.
pub fn pixel(f: &Frame, mask: &[u32], x: u32, y: u32) -> u32 {
    let h = &f.header;
    let pc = [x as f32 + 0.5, y as f32 + 0.5];
    let tile = (y / TILE) * h.tiles_x + x / TILE;
    let mut dst = [h.clear[0], h.clear[1], h.clear[2]];
    let mut da = h.clear[3];
    for w in 0..h.wpt {
        let mut bits = mask[(tile * h.wpt + w) as usize];
        while bits != 0 {
            let i = bits.trailing_zeros();
            bits &= bits - 1;
            let oi = w * 32 + i;
            if oi >= h.op_count {
                break;
            }
            let s = shade(f, &f.ops[oi as usize], pc);
            for c in 0..3 {
                dst[c] = s[c] * s[3] + dst[c] * (1.0 - s[3]);
            }
            da = s[3] + da * (1.0 - s[3]);
        }
    }
    let (r, g, b) = (encode(&f.lut, dst[0]), encode(&f.lut, dst[1]), encode(&f.lut, dst[2]));
    let a = (clamp_(da, 0.0, 1.0) * 255.0 + 0.5) as u32;
    b | (g << 8) | (r << 16) | (a << 24)
}

/// The alpha byte `raster.comp` writes at pixel (`x`, `y`), from the ops
/// whose box covers that pixel as `bin.comp` would bin them, without
/// binning the whole frame. For hit-testing: a pixel a window drew nothing
/// on has alpha 0 (on a transparent clear).
pub fn alpha_at(f: &Frame, x: u32, y: u32) -> u8 {
    let h = &f.header;
    if x >= h.width || y >= h.height {
        return 0;
    }
    let (fw, fh) = (h.width as f32, h.height as f32);
    let pc = [x as f32 + 0.5, y as f32 + 0.5];
    let mut da = h.clear[3];
    for o in &f.ops[..(h.op_count as usize).min(f.ops.len())] {
        let b = o.p3;
        let x0 = clamp_(b[0], 0.0, fw) as i32;
        let y0 = clamp_(b[1], 0.0, fh) as i32;
        let x1 = clamp_(b[2], 0.0, fw) as i32;
        let y1 = clamp_(b[3], 0.0, fh) as i32;
        if x1 <= x0 || y1 <= y0 {
            continue;
        }
        // bin.comp covers whole tiles; an op outside this pixel's tile is
        // not shaded here, and one inside it shades to 0 off its shape.
        let t = |v: u32| v / TILE;
        let (tx, ty) = (t(x), t(y));
        if tx < t(x0 as u32) || tx > t((x1 - 1) as u32) || ty < t(y0 as u32) || ty > t((y1 - 1) as u32) {
            continue;
        }
        let s = shade(f, o, pc);
        da = s[3] + da * (1.0 - s[3]);
    }
    (clamp_(da, 0.0, 1.0) * 255.0 + 0.5) as u8
}

/// Both kernels: the target, `pitch * height` BGRA8 words (padding 0).
pub fn render(f: &Frame) -> Vec<u32> {
    let mask = bin(f);
    let h = &f.header;
    let mut out = vec![0u32; f.target_words()];
    for y in 0..h.height {
        for x in 0..h.width {
            out[(y * h.pitch + x) as usize] = pixel(f, &mask, x, y);
        }
    }
    out
}
