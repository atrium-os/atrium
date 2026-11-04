//! A Rust model of `bin.comp` + `raster.comp`, statement for statement. It is
//! the oracle the executors are checked against, and a plain CPU fallback.

use crate::{Frame, Op, KIND_GLYPH, KIND_PATH, KIND_RECT, KIND_TEXTURE, TILE};

/// `bin.comp`: the tile masks.
pub fn bin(f: &Frame) -> Vec<u32> {
    let h = &f.header;
    let mut mask = vec![0u32; f.mask_words()];
    for i in 0..h.op_count as usize {
        let b = f.ops[i].p3;
        let x0 = (b[0] as i32).max(0);
        let y0 = (b[1] as i32).max(0);
        let x1 = (b[2] as i32).min(h.width as i32);
        let y1 = (b[3] as i32).min(h.height as i32);
        if x1 <= x0 || y1 <= y0 {
            continue;
        }
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

fn mix(a: f32, b: f32, t: f32) -> f32 {
    a * (1.0 - t) + b * t
}

fn mix4(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    [mix(a[0], b[0], t), mix(a[1], b[1], t), mix(a[2], b[2], t), mix(a[3], b[3], t)]
}

fn texel(f: &Frame, t: [u32; 4], x: i32, y: i32) -> [f32; 4] {
    let x = x.clamp(0, t[1] as i32 - 1);
    let y = y.clamp(0, t[2] as i32 - 1);
    let idx = y as u32 * t[1] + x as u32;
    let dec = &f.lut[..256];
    if t[3] == 0 {
        let w = f.texels[(t[0] + idx) as usize];
        return [
            dec[(w & 255) as usize],
            dec[((w >> 8) & 255) as usize],
            dec[((w >> 16) & 255) as usize],
            (w >> 24) as f32 / 255.0,
        ];
    }
    let w = f.texels[(t[0] + idx / 4) as usize];
    let v = ((w >> (8 * (idx % 4))) & 255) as f32 / 255.0;
    [v, 0.0, 0.0, 1.0]
}

fn sample_bilinear(f: &Frame, ti: u32, uv: [f32; 2]) -> [f32; 4] {
    let t = f.tex[ti as usize];
    let tc = [uv[0] * t[1] as f32 - 0.5, uv[1] * t[2] as f32 - 0.5];
    let fl = [tc[0].floor(), tc[1].floor()];
    let fr = [tc[0] - fl[0], tc[1] - fl[1]];
    let (x, y) = (fl[0] as i32, fl[1] as i32);
    let a = mix4(texel(f, t, x, y), texel(f, t, x + 1, y), fr[0]);
    let b = mix4(texel(f, t, x, y + 1), texel(f, t, x + 1, y + 1), fr[0]);
    mix4(a, b, fr[1])
}

fn inside(r: [f32; 4], pc: [f32; 2]) -> bool {
    pc[0] >= r[0] && pc[0] < r[0] + r[2] && pc[1] >= r[1] && pc[1] < r[1] + r[3]
}

/// One op's straight-alpha colour at pixel centre `pc`.
pub fn shade(f: &Frame, o: &Op, pc: [f32; 2]) -> [f32; 4] {
    let kind = o.meta[0];
    if kind == KIND_PATH {
        let v = [pc[0] - o.p0[0], pc[1] - o.p0[1]];
        let lx = o.p2[0] * v[0] + o.p2[1] * v[1];
        let ly = o.p2[0] * v[1] - o.p2[1] * v[0];
        let (hl, hw) = (o.p0[2] * 0.5, o.p0[3] * 0.5);
        if lx >= -hl && lx < hl && ly >= -hw && ly < hw {
            return o.p1;
        }
        return [0.0; 4];
    }
    if !inside(o.p0, pc) {
        return [0.0; 4];
    }
    if kind == KIND_RECT {
        let radius = o.p2[0];
        if radius <= 0.0 {
            return o.p1;
        }
        let half = [o.p0[2] * 0.5, o.p0[3] * 0.5];
        let r = radius.min(half[0].min(half[1]));
        let p = [(pc[0] - o.p0[0]) - half[0], (pc[1] - o.p0[1]) - half[1]];
        let q = [p[0].abs() - half[0] + r, p[1].abs() - half[1] + r];
        let qp = [q[0].max(0.0), q[1].max(0.0)];
        let lq = (qp[0] * qp[0] + qp[1] * qp[1]).sqrt();
        let d = q[0].max(q[1]).min(0.0) + lq - r;
        let mut aa = 1.0;
        if q[0] > 0.0 && q[1] > 0.0 {
            aa = (q[0] + q[1]) / lq;
        }
        let alpha = (0.5 - d / aa).clamp(0.0, 1.0);
        return [o.p1[0], o.p1[1], o.p1[2], o.p1[3] * alpha];
    }
    let t = [(pc[0] - o.p0[0]) / o.p0[2], (pc[1] - o.p0[1]) / o.p0[3]];
    if kind == KIND_TEXTURE {
        return sample_bilinear(f, o.meta[1], t);
    }
    debug_assert_eq!(kind, KIND_GLYPH);
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
    for w in 0..h.wpt {
        let mut bits = mask[(tile * h.wpt + w) as usize];
        while bits != 0 {
            let i = bits.trailing_zeros();
            bits &= bits - 1;
            let s = shade(f, &f.ops[(w * 32 + i) as usize], pc);
            for c in 0..3 {
                dst[c] = s[c] * s[3] + dst[c] * (1.0 - s[3]);
            }
        }
    }
    let (r, g, b) = (encode(&f.lut, dst[0]), encode(&f.lut, dst[1]), encode(&f.lut, dst[2]));
    b | (g << 8) | (r << 16) | (255 << 24)
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
