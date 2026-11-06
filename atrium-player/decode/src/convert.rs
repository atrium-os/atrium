//! From a decoder's planar picture to what Fresco's video layers take
//! (fresco-video.md §3): `NV12` for 8-bit, `P010` for 10- and 12-bit.
//! 4:2:2 and 4:4:4 are averaged down to 4:2:0, and 4:0:0 gets neutral
//! chroma. Safe code: every index is bounds-checked, and a destination
//! that's too small is an error before anything is written.
use crate::{Error, Layout, Picture, Result};

/// One plane of a destination buffer.
pub struct PlaneMut<'a> {
    pub data: &'a mut [u8],
    pub stride: usize,
}

/// Bytes an NV12 (`bytes_per_sample` 1) or P010 (2) frame needs, with
/// tightly packed rows: (luma, chroma).
pub fn layer_sizes(width: u32, height: u32, bytes_per_sample: usize) -> (usize, usize) {
    let (w, h) = (width as usize, height as usize);
    let cw = w.div_ceil(2) * 2;
    (w * h * bytes_per_sample, cw * h.div_ceil(2) * bytes_per_sample)
}

fn sample(p: &Picture, plane: usize, x: usize, y: usize) -> u16 {
    let s = &p.planes[plane][y * p.strides[plane]..];
    if p.bit_depth > 8 {
        u16::from_le_bytes([s[2 * x], s[2 * x + 1]])
    } else {
        s[x] as u16
    }
}

/// Chroma sample at 4:2:0 position (x, y), averaging what the layout has there.
fn chroma420(p: &Picture, plane: usize, x: usize, y: usize) -> u16 {
    let (cw, ch) = p.layout.chroma_size(p.width, p.height);
    let (cw, ch) = (cw as usize, ch as usize);
    match p.layout {
        Layout::I420 => sample(p, plane, x, y),
        Layout::I422 => {
            let (a, b) = (2 * y, (2 * y + 1).min(ch - 1));
            (sample(p, plane, x, a) + sample(p, plane, x, b)).div_ceil(2)
        }
        Layout::I444 => {
            let (x0, x1) = (2 * x, (2 * x + 1).min(cw - 1));
            let (y0, y1) = (2 * y, (2 * y + 1).min(ch - 1));
            let t = sample(p, plane, x0, y0) as u32 + sample(p, plane, x1, y0) as u32 + sample(p, plane, x0, y1) as u32 + sample(p, plane, x1, y1) as u32;
            ((t + 2) / 4) as u16
        }
        Layout::I400 => 1 << (p.bit_depth - 1),
    }
}

fn check(dst: &PlaneMut, row_bytes: usize, rows: usize) -> Result<()> {
    if dst.stride < row_bytes || rows > 0 && dst.data.len() < dst.stride * (rows - 1) + row_bytes {
        return Err(Error::TooBig);
    }
    Ok(())
}

/// Write `p` as NV12. Pictures over 8 bits are rounded down to 8.
pub fn to_nv12(p: &Picture, y: &mut PlaneMut, uv: &mut PlaneMut) -> Result<()> {
    let (w, h) = (p.width as usize, p.height as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    check(y, w, h)?;
    check(uv, cw * 2, ch)?;
    let shift = p.bit_depth - 8;
    let round = if shift > 0 { 1u16 << (shift - 1) } else { 0 };
    let to8 = |v: u16| ((v + round) >> shift).min(255) as u8;
    for r in 0..h {
        let dst = &mut y.data[r * y.stride..r * y.stride + w];
        if shift == 0 {
            dst.copy_from_slice(&p.planes[0][r * p.strides[0]..r * p.strides[0] + w]);
        } else {
            let src = &p.planes[0][r * p.strides[0]..r * p.strides[0] + 2 * w];
            for (d, s) in dst.iter_mut().zip(src.chunks_exact(2)) {
                *d = to8(u16::from_le_bytes([s[0], s[1]]));
            }
        }
    }
    for r in 0..ch {
        let dst = &mut uv.data[r * uv.stride..r * uv.stride + 2 * cw];
        if p.layout == Layout::I420 && shift == 0 {
            let u = &p.planes[1][r * p.strides[1]..r * p.strides[1] + cw];
            let v = &p.planes[2][r * p.strides[2]..r * p.strides[2] + cw];
            for ((d, &a), &b) in dst.chunks_exact_mut(2).zip(u).zip(v) {
                d[0] = a;
                d[1] = b;
            }
        } else {
            for x in 0..cw {
                dst[2 * x] = to8(chroma420(p, 1, x, r));
                dst[2 * x + 1] = to8(chroma420(p, 2, x, r));
            }
        }
    }
    Ok(())
}

/// Write `p` as P010: u16 little-endian with the value in the top bits.
/// 8-bit pictures are widened.
pub fn to_p010(p: &Picture, y: &mut PlaneMut, uv: &mut PlaneMut) -> Result<()> {
    let (w, h) = (p.width as usize, p.height as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    check(y, 2 * w, h)?;
    check(uv, 4 * cw, ch)?;
    let shift = 16 - p.bit_depth as u32;
    let put = |d: &mut [u8], v: u16| d.copy_from_slice(&(v << shift).to_le_bytes());
    for r in 0..h {
        let dst = &mut y.data[r * y.stride..r * y.stride + 2 * w];
        for (x, d) in dst.chunks_exact_mut(2).enumerate() {
            put(d, sample(p, 0, x, r));
        }
    }
    for r in 0..ch {
        let dst = &mut uv.data[r * uv.stride..r * uv.stride + 4 * cw];
        for x in 0..cw {
            put(&mut dst[4 * x..4 * x + 2], chroma420(p, 1, x, r));
            put(&mut dst[4 * x + 2..4 * x + 4], chroma420(p, 2, x, r));
        }
    }
    Ok(())
}

/// The picture's planes packed with no padding, as ffmpeg's `framemd5`
/// hashes them (yuv420p, or yuv420p10le for deeper pictures).
pub fn packed_planar(p: &Picture) -> Vec<u8> {
    let bps = if p.bit_depth > 8 { 2 } else { 1 };
    let (cw, ch) = p.layout.chroma_size(p.width, p.height);
    let mut out = Vec::new();
    let planes = if p.layout == Layout::I400 { 1 } else { 3 };
    for i in 0..planes {
        let (w, h) = if i == 0 { (p.width, p.height) } else { (cw, ch) };
        for r in 0..h as usize {
            let s = r * p.strides[i];
            out.extend_from_slice(&p.planes[i][s..s + w as usize * bps]);
        }
    }
    out
}
