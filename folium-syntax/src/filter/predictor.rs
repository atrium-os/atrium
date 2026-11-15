//! Predictors for Flate and LZW (ISO 32000-2 §7.4.4.4): TIFF 2 and the
//! PNG filters 10–15.

use super::Capped;
use crate::object::Dict;
use crate::{Error, Result};

pub fn apply(data: Vec<u8>, p: Option<&Dict>, limit: usize) -> Result<Vec<u8>> {
    let Some(p) = p else { return Ok(data) };
    let pred = p.int("Predictor").unwrap_or(1);
    if pred <= 1 {
        return Ok(data);
    }
    let colors = p.int("Colors").unwrap_or(1).clamp(1, 32) as usize;
    let bpc = p.int("BitsPerComponent").unwrap_or(8);
    if !matches!(bpc, 1 | 2 | 4 | 8 | 16) {
        return Err(Error::Syntax { offset: 0, what: "a predictor's BitsPerComponent" });
    }
    let bpc = bpc as usize;
    let columns = p.int("Columns").unwrap_or(1).clamp(1, 1 << 24) as usize;
    let bpp = (colors * bpc).div_ceil(8).max(1);
    let row = (colors * bpc * columns).div_ceil(8);
    if pred == 2 {
        return Ok(tiff(data, colors, bpc, row));
    }
    let mut out = Capped::new(limit, data.len());
    let mut prev = vec![0u8; row];
    let mut cur = vec![0u8; row];
    for chunk in data.chunks(row + 1) {
        let kind = chunk[0];
        let src = &chunk[1..];
        cur[..src.len()].copy_from_slice(src);
        cur[src.len()..].fill(0);
        for i in 0..row {
            let a = if i >= bpp { cur[i - bpp] } else { 0 };
            let b = prev[i];
            let c = if i >= bpp { prev[i - bpp] } else { 0 };
            cur[i] = cur[i].wrapping_add(match kind {
                0 => 0,
                1 => a,
                2 => b,
                3 => ((a as u16 + b as u16) / 2) as u8,
                4 => paeth(a, b, c),
                _ => 0,
            });
        }
        out.extend(&cur[..src.len()])?;
        std::mem::swap(&mut prev, &mut cur);
    }
    Ok(out.v)
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = a as i16 + b as i16 - c as i16;
    let (pa, pb, pc) = ((p - a as i16).abs(), (p - b as i16).abs(), (p - c as i16).abs());
    if pa <= pb && pa <= pc { a } else if pb <= pc { b } else { c }
}

fn tiff(mut data: Vec<u8>, colors: usize, bpc: usize, row: usize) -> Vec<u8> {
    match bpc {
        8 => {
            for r in data.chunks_mut(row) {
                for i in colors..r.len() {
                    r[i] = r[i].wrapping_add(r[i - colors]);
                }
            }
        }
        16 => {
            for r in data.chunks_mut(row) {
                let n = r.len() / 2;
                for i in colors..n {
                    let a = u16::from_be_bytes([r[2 * (i - colors)], r[2 * (i - colors) + 1]]);
                    let b = u16::from_be_bytes([r[2 * i], r[2 * i + 1]]);
                    r[2 * i..2 * i + 2].copy_from_slice(&a.wrapping_add(b).to_be_bytes());
                }
            }
        }
        _ => {} // sub-byte TIFF prediction is vanishingly rare; left as is
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::Object;

    #[test]
    fn png_up_and_sub() {
        let mut d = Dict::new();
        d.insert("Predictor", Object::Int(12));
        d.insert("Columns", Object::Int(3));
        // Row 1 "None" 1 2 3, row 2 "Up" 1 1 1 -> 2 3 4, row 3 "Sub" 5 1 1 -> 5 6 7.
        let enc = vec![0, 1, 2, 3, 2, 1, 1, 1, 1, 5, 1, 1];
        assert_eq!(apply(enc, Some(&d), 99).unwrap(), vec![1, 2, 3, 2, 3, 4, 5, 6, 7]);
    }
}
