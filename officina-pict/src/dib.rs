//! Device-independent bitmaps (BMP files and the DIBs inside metafiles).
//! Decoded to straight-alpha RGBA, top row first.

use crate::{i32_at, u16_at, u32_at};

/// The most pixels in one bitmap.
pub const MAX_DIB_PIXELS: u64 = 64 << 20;

pub struct Dib {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// Stored top row first (most DIBs are stored bottom row first).
    pub top_down: bool,
}

struct Header {
    size: usize,
    width: u32,
    height: u32,
    top_down: bool,
    bpp: u16,
    compression: u32,
    colors: usize,
    /// Bytes per palette entry (3 in a core header).
    entry: usize,
    masks: Option<[u32; 4]>,
}

fn header(b: &[u8]) -> Option<Header> {
    let size = u32_at(b, 0) as usize;
    if size == 12 {
        let (w, h, bpp) = (u16_at(b, 4) as u32, u16_at(b, 6) as u32, u16_at(b, 10));
        return Some(Header { size, width: w, height: h, top_down: false, bpp, compression: 0, colors: if bpp <= 8 { 1 << bpp } else { 0 }, entry: 3, masks: None });
    }
    if !(40..=124).contains(&size) || b.len() < size {
        return None;
    }
    let (w, h) = (i32_at(b, 4), i32_at(b, 8));
    let (bpp, compression, used) = (u16_at(b, 14), u32_at(b, 16), u32_at(b, 32) as usize);
    if w <= 0 || h == 0 || h == i32::MIN {
        return None;
    }
    let colors = if bpp <= 8 { if used == 0 || used > 1 << bpp { 1 << bpp } else { used } } else { used.min(256) };
    let mut masks = None;
    let mut extra = 0;
    if compression == 3 || compression == 6 {
        if size >= 52 {
            masks = Some([u32_at(b, 40), u32_at(b, 44), u32_at(b, 48), if size >= 56 { u32_at(b, 52) } else { 0 }]);
        } else {
            extra = if compression == 6 { 16 } else { 12 };
            masks = Some([u32_at(b, 40), u32_at(b, 44), u32_at(b, 48), if compression == 6 { u32_at(b, 52) } else { 0 }]);
        }
    }
    Some(Header { size: size + extra, width: w as u32, height: h.unsigned_abs(), top_down: h < 0, bpp, compression, colors, entry: 4, masks })
}

/// A DIB's size from its header.
pub fn size(b: &[u8]) -> Option<(u32, u32)> {
    let h = header(b)?;
    (h.width > 0 && h.height > 0 && h.width <= 65535 && h.height <= 65535).then_some((h.width, h.height))
}

/// Where a packed DIB's pixels start (after the header and colour table).
pub fn bits_offset(b: &[u8]) -> Option<usize> {
    let h = header(b)?;
    Some(h.size + h.colors * h.entry)
}

/// A 1-bit DIB's bits, row by row from the top: (width, height, set).
pub fn mono_bits(bmi: &[u8], bits: &[u8]) -> Option<(u32, u32, Vec<bool>)> {
    let h = header(bmi)?;
    if h.bpp != 1 || h.compression != 0 || h.width > 256 || h.height > 256 {
        return None;
    }
    let (w, ht) = (h.width as usize, h.height as usize);
    let stride = w.div_ceil(32) * 4;
    let mut out = vec![false; w * ht];
    for y in 0..ht {
        let src = if h.top_down { y } else { ht - 1 - y };
        for x in 0..w {
            out[y * w + x] = bits.get(src * stride + x / 8).is_some_and(|b| (b >> (7 - x % 8)) & 1 != 0);
        }
    }
    Some((h.width, h.height, out))
}

/// A packed 1-bit DIB's bits.
pub fn mono_packed(b: &[u8]) -> Option<(u32, u32, Vec<bool>)> {
    mono_bits(b, b.get(bits_offset(b)?..)?)
}

/// A BMP file.
pub fn decode_file(b: &[u8]) -> Option<Dib> {
    if !b.starts_with(b"BM") {
        return None;
    }
    let dib = b.get(14..)?;
    let off = u32_at(b, 10) as usize;
    let bits = match off {
        o if o >= 14 + 12 && o < b.len() => &b[o..],
        _ => dib.get(bits_offset(dib)?..)?,
    };
    decode(dib, bits, false)
}

/// A packed DIB: header, colours, pixels.
pub fn decode_packed(b: &[u8], alpha: bool) -> Option<Dib> {
    decode(b, b.get(bits_offset(b)?..)?, alpha)
}

/// A DIB from its header and colour table (`bmi`) and its pixels. With
/// `alpha` a 32-bit bitmap's fourth byte is its (premultiplied) alpha;
/// otherwise it is opaque.
pub fn decode(bmi: &[u8], bits: &[u8], alpha: bool) -> Option<Dib> {
    decode_in(bmi, bits, alpha, Colors::Own)
}

/// Where a bitmap's colours come from.
#[derive(Clone, Copy)]
pub enum Colors<'a> {
    /// Its colour table.
    Own,
    /// Its table holds 16-bit indices into the device's palette
    /// (DIB_PAL_COLORS).
    Indices(&'a [[u8; 4]]),
    /// It has no table: the device's palette.
    Logical(&'a [[u8; 4]]),
}

/// A packed DIB drawn with `usage` (0 RGB, 1 palette indices) against the
/// device's palette `pal`; one with no room for its colour table takes
/// the palette's colours.
pub fn decode_packed_in(b: &[u8], alpha: bool, usage: u32, pal: &[[u8; 4]]) -> Option<Dib> {
    let h = header(b)?;
    if usage == 1 {
        return decode_in(b, b.get(h.size + h.colors * 2..)?, alpha, Colors::Indices(pal));
    }
    let needed = if matches!(h.compression, 0 | 3 | 6) { (h.width as usize * h.bpp as usize).div_ceil(32) * 4 * h.height as usize } else { 0 };
    if h.bpp <= 8 && !pal.is_empty() && needed > 0 && b.len() < h.size + h.colors * h.entry + needed && b.len() >= h.size + needed {
        return decode_in(b, &b[h.size..], alpha, Colors::Logical(pal));
    }
    decode_packed(b, alpha)
}

pub fn decode_in(bmi: &[u8], bits: &[u8], alpha: bool, colors: Colors) -> Option<Dib> {
    let h = header(bmi)?;
    let (w, ht) = (h.width as usize, h.height as usize);
    if w == 0 || ht == 0 || w as u64 * ht as u64 > MAX_DIB_PIXELS || !matches!(h.bpp, 1 | 4 | 8 | 16 | 24 | 32) {
        return None;
    }
    let pal: Vec<[u8; 4]> = match colors {
        Colors::Own => (0..h.colors)
            .map(|i| {
                let o = h.size + i * h.entry;
                [*bmi.get(o + 2).unwrap_or(&0), *bmi.get(o + 1).unwrap_or(&0), *bmi.get(o).unwrap_or(&0), 255]
            })
            .collect(),
        Colors::Indices(p) => (0..h.colors).map(|i| p.get(u16_at(bmi, h.size + i * 2) as usize).copied().unwrap_or([0, 0, 0, 255])).collect(),
        Colors::Logical(p) => p.to_vec(),
    };
    let color = |i: usize| pal.get(i).copied().unwrap_or(if i == 0 { [0, 0, 0, 255] } else { [255, 255, 255, 255] });
    let mut out = vec![0u8; w * ht * 4];
    let row_of = |y: usize| if h.top_down { y } else { ht - 1 - y };
    match h.compression {
        1 | 2 if h.bpp == 8 || h.bpp == 4 => {
            // Run-length: absent pixels are left clear.
            let (mut x, mut y, mut p) = (0usize, 0usize, 0usize);
            let put = |x: usize, y: usize, i: usize, out: &mut Vec<u8>| {
                if x < w && y < ht {
                    let o = (row_of(y) * w + x) * 4;
                    out[o..o + 4].copy_from_slice(&color(i));
                }
            };
            let four = h.bpp == 4;
            while p + 1 < bits.len() && y < ht {
                let (n, v) = (bits[p] as usize, bits[p + 1]);
                p += 2;
                if n > 0 {
                    for k in 0..n {
                        let i = if four { if k % 2 == 0 { v >> 4 } else { v & 15 } } else { v };
                        put(x + k, y, i as usize, &mut out);
                    }
                    x += n;
                    continue;
                }
                match v {
                    0 => {
                        x = 0;
                        y += 1;
                    }
                    1 => break,
                    2 => {
                        x += *bits.get(p).unwrap_or(&0) as usize;
                        y += *bits.get(p + 1).unwrap_or(&0) as usize;
                        p += 2;
                    }
                    n => {
                        let n = n as usize;
                        for k in 0..n {
                            let i = if four {
                                let b = *bits.get(p + k / 2).unwrap_or(&0);
                                if k % 2 == 0 { b >> 4 } else { b & 15 }
                            } else {
                                *bits.get(p + k).unwrap_or(&0)
                            };
                            put(x + k, y, i as usize, &mut out);
                        }
                        x += n;
                        let used = if four { n.div_ceil(2) } else { n };
                        p += used + (used & 1);
                    }
                }
            }
        }
        0 | 3 | 6 => {
            let stride = (w * h.bpp as usize).div_ceil(32) * 4;
            // A short last row is drawn as far as it goes; no rows at all is damage.
            if bits.is_empty() {
                return None;
            }
            let masks = h.masks.unwrap_or(match h.bpp {
                16 => [0x7C00, 0x03E0, 0x001F, 0],
                _ => [0xFF_0000, 0xFF00, 0xFF, 0],
            });
            let chan = |v: u32, m: u32| -> u8 {
                if m == 0 {
                    return 0;
                }
                let s = m.trailing_zeros();
                let bits = (m >> s).count_ones();
                let x = (v & m) >> s;
                if bits >= 8 { (x >> (bits - 8)) as u8 } else { ((x * 255) / ((1 << bits) - 1)) as u8 }
            };
            let use_alpha = alpha && h.bpp == 32;
            for y in 0..ht {
                let Some(row) = bits.get(y * stride..) else { break };
                let o = row_of(y) * w * 4;
                for x in 0..w {
                    let px = match h.bpp {
                        1 => row.get(x / 8).map(|b| color(((b >> (7 - x % 8)) & 1) as usize)),
                        4 => row.get(x / 2).map(|b| color(if x % 2 == 0 { b >> 4 } else { b & 15 } as usize)),
                        8 => row.get(x).map(|&b| color(b as usize)),
                        16 => row.get(x * 2..x * 2 + 2).map(|s| {
                            let v = u16::from_le_bytes([s[0], s[1]]) as u32;
                            [chan(v, masks[0]), chan(v, masks[1]), chan(v, masks[2]), 255]
                        }),
                        24 => row.get(x * 3..x * 3 + 3).map(|s| [s[2], s[1], s[0], 255]),
                        _ => row.get(x * 4..x * 4 + 4).map(|s| {
                            let v = u32::from_le_bytes([s[0], s[1], s[2], s[3]]);
                            if h.masks.is_some() {
                                [chan(v, masks[0]), chan(v, masks[1]), chan(v, masks[2]), if use_alpha && masks[3] != 0 { chan(v, masks[3]) } else { 255 }]
                            } else if use_alpha {
                                // Premultiplied in the file.
                                let a = s[3];
                                let un = |c: u8| if a == 0 { 0 } else { ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8 };
                                [un(s[2]), un(s[1]), un(s[0]), a]
                            } else {
                                [s[2], s[1], s[0], 255]
                            }
                        }),
                    };
                    let Some(px) = px else { break };
                    out[o + x * 4..o + x * 4 + 4].copy_from_slice(&px);
                }
            }
        }
        _ => return None,
    }
    Some(Dib { width: h.width, height: h.height, rgba: out, top_down: h.top_down })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(w: i32, h: i32, bpp: u16, comp: u32, colors: &[[u8; 4]]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend(40u32.to_le_bytes());
        b.extend(w.to_le_bytes());
        b.extend(h.to_le_bytes());
        b.extend(1u16.to_le_bytes());
        b.extend(bpp.to_le_bytes());
        b.extend(comp.to_le_bytes());
        b.extend([0; 12]);
        b.extend((colors.len() as u32).to_le_bytes());
        b.extend(0u32.to_le_bytes());
        for c in colors {
            b.extend(c);
        }
        b
    }

    #[test]
    fn bitmaps_of_each_depth() {
        // 24-bit, bottom-up, 2x2: rows padded to 8 bytes.
        let bmi = info(2, 2, 24, 0, &[]);
        let bits = [0, 0, 255, 0, 255, 0, 0, 0, 255, 0, 0, 255, 255, 255, 0, 0];
        let d = decode(&bmi, &bits, false).unwrap();
        // The file's first row is the bottom one.
        assert_eq!(&d.rgba[0..8], &[0, 0, 255, 255, 255, 255, 255, 255]);
        assert_eq!(&d.rgba[8..16], &[255, 0, 0, 255, 0, 255, 0, 255]);
        // 1-bit, top-down, its colours from the table.
        let bmi = info(3, -1, 1, 0, &[[0, 0, 0, 0], [255, 0, 0, 0]]);
        let d = decode(&bmi, &[0b1010_0000, 0, 0, 0], false).unwrap();
        assert_eq!(d.rgba, vec![0, 0, 255, 255, 0, 0, 0, 255, 0, 0, 255, 255]);
        // 8-bit run-length.
        let bmi = info(4, 1, 8, 1, &[[0, 0, 0, 0], [0, 255, 0, 0]]);
        let d = decode(&bmi, &[3, 1, 0, 1], false).unwrap();
        assert_eq!(&d.rgba[..4], &[0, 255, 0, 255]);
        assert_eq!(d.rgba[15], 0, "a pixel the runs skip is clear");
        // Damage refused.
        assert!(decode(&info(0, 1, 24, 0, &[]), &[0; 4], false).is_none());
        assert!(decode(&info(1 << 20, 1 << 20, 24, 0, &[]), &[0; 4], false).is_none());
    }
}
