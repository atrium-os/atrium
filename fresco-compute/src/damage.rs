//! Damage: which tiles of a frame differ from a frame already in a buffer.
//!
//! [`tile_signatures`] hashes, per 16x16 tile, the ops that touch it in draw
//! order (as `bin.comp` would list them), plus the texels each textured op
//! samples. Two frames with the same signature for a tile draw the same
//! pixels there, so an executor that remembers the signatures of what a
//! buffer holds re-rasters only the tiles whose signature changed.
//! [`rects`] groups the dirty tiles into a few rectangles to dispatch.

use crate::{Frame, KIND_FILL, KIND_GLYPH, KIND_TEXTURE, TILE};

const K: u64 = 0x9e37_79b9_7f4a_7c15;

fn mix(h: u64, v: u64) -> u64 {
    (h ^ v).wrapping_mul(K).rotate_left(29)
}

fn hash_words(mut h: u64, w: &[u32]) -> u64 {
    for c in w.chunks(2) {
        let v = c[0] as u64 | (c.get(1).copied().unwrap_or(0) as u64) << 32;
        h = mix(h, v);
    }
    h
}

/// One signature per tile, row major (`tiles_x * tiles_y`).
pub fn tile_signatures(f: &Frame) -> Vec<u64> {
    let h = &f.header;
    let base = hash_words(mix(0, h.width as u64 | (h.height as u64) << 32), &h.clear.map(f32::to_bits));
    let mut sig = vec![base; (h.tiles_x * h.tiles_y) as usize];
    // Texel content per texture, hashed once.
    let tex_hash: Vec<u64> = f
        .tex
        .iter()
        .map(|t| {
            let words = if t[3] == 0 { t[1] * t[2] } else { (t[1] * t[2]).div_ceil(4) };
            let lo = (t[0] as usize).min(f.texels.len());
            let hi = (lo + words as usize).min(f.texels.len());
            hash_words(hash_words(0, t), &f.texels[lo..hi])
        })
        .collect();
    for o in &f.ops[..h.op_count as usize] {
        // The same clamp as bin.comp: ops outside the target touch no tile.
        let b = o.p3;
        let x0 = (b[0] as i32).max(0);
        let y0 = (b[1] as i32).max(0);
        let x1 = (b[2] as i32).min(h.width as i32);
        let y1 = (b[3] as i32).min(h.height as i32);
        if x1 <= x0 || y1 <= y0 {
            continue;
        }
        let mut oh = 0u64;
        for v in [o.p0, o.p1, o.p2, o.p3] {
            oh = hash_words(oh, &v.map(f32::to_bits));
        }
        oh = hash_words(oh, &o.meta);
        if o.meta[0] == KIND_TEXTURE || o.meta[0] == KIND_GLYPH {
            oh = mix(oh, tex_hash.get(o.meta[1] as usize).copied().unwrap_or(0));
        }
        if o.meta[0] == KIND_FILL {
            // Its whole block (header word 7 = its length): any change
            // dirties every tile the path touches.
            let lo = (o.meta[1] as usize).min(f.texels.len());
            let n = f.texels.get(lo + 7).copied().unwrap_or(0) as usize;
            let hi = lo.saturating_add(n.max(crate::fill::HEADER_WORDS as usize)).min(f.texels.len());
            oh = hash_words(oh, &f.texels[lo..hi]);
        }
        let (tx0, ty0) = (x0 as u32 / TILE, y0 as u32 / TILE);
        let (tx1, ty1) = ((x1 - 1) as u32 / TILE, (y1 - 1) as u32 / TILE);
        for ty in ty0..=ty1 {
            for tx in tx0..=tx1 {
                let s = &mut sig[(ty * h.tiles_x + tx) as usize];
                *s = mix(*s, oh);
            }
        }
    }
    sig
}

/// Tile rectangles `[tx0, ty0, tx1, ty1)` covering exactly the dirty tiles:
/// runs of dirty tiles per row, merged with identical runs in the rows
/// below. If that gives more than `max` rectangles, their bounding box.
pub fn rects(dirty: &[bool], tiles_x: u32, tiles_y: u32, max: usize) -> Vec<[u32; 4]> {
    let mut open: Vec<[u32; 4]> = Vec::new(); // rects that reach the previous row
    let mut done: Vec<[u32; 4]> = Vec::new();
    for ty in 0..tiles_y {
        let mut runs = Vec::new();
        let mut tx = 0;
        while tx < tiles_x {
            if dirty[(ty * tiles_x + tx) as usize] {
                let s = tx;
                while tx < tiles_x && dirty[(ty * tiles_x + tx) as usize] {
                    tx += 1;
                }
                runs.push((s, tx));
            } else {
                tx += 1;
            }
        }
        let mut next = Vec::new();
        for (s, e) in runs {
            if let Some(i) = open.iter().position(|r| r[0] == s && r[2] == e) {
                let mut r = open.swap_remove(i);
                r[3] = ty + 1;
                next.push(r);
            } else {
                next.push([s, ty, e, ty + 1]);
            }
        }
        done.append(&mut open);
        open = next;
    }
    done.append(&mut open);
    if done.len() > max {
        let bb = done.iter().fold([u32::MAX, u32::MAX, 0, 0], |a, r| {
            [a[0].min(r[0]), a[1].min(r[1]), a[2].max(r[2]), a[3].max(r[3])]
        });
        return vec![bb];
    }
    done
}

/// The tiles whose signature differs from `prev` (all of them without one).
pub fn dirty(sig: &[u64], prev: Option<&[u64]>) -> Vec<bool> {
    match prev {
        Some(p) if p.len() == sig.len() => sig.iter().zip(p).map(|(a, b)| a != b).collect(),
        _ => vec![true; sig.len()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rects_cover_exactly_the_dirty_tiles() {
        // 6x4 tiles: an L and a separate block.
        let map = ["##....", "##..#.", "###.#.", "......"];
        let dirty: Vec<bool> = map.iter().flat_map(|r| r.chars().map(|c| c == '#')).collect();
        let rs = rects(&dirty, 6, 4, 16);
        let mut cover = vec![0u32; 24];
        for r in &rs {
            for ty in r[1]..r[3] {
                for tx in r[0]..r[2] {
                    cover[(ty * 6 + tx) as usize] += 1;
                }
            }
        }
        assert!(cover.iter().zip(&dirty).all(|(&c, &d)| c == d as u32), "{rs:?}");
        assert_eq!(rs.len(), 3, "{rs:?}");
        assert_eq!(rects(&dirty, 6, 4, 2), vec![[0, 0, 5, 3]]);
        assert!(rects(&[false; 24], 6, 4, 16).is_empty());
    }
}
