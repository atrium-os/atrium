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
        if o.meta[0] == KIND_FILL {
            // Where its block sits does not change what it draws.
            oh = hash_words(oh, &[o.meta[0], o.meta[2], o.meta[3]]);
        } else {
            oh = hash_words(oh, &o.meta);
        }
        if o.meta[0] == KIND_TEXTURE || o.meta[0] == KIND_GLYPH {
            oh = mix(oh, tex_hash.get(o.meta[1] as usize).copied().unwrap_or(0));
        }
        if o.meta[0] == KIND_FILL {
            // Its whole block (header word 7 = its length): any change
            // dirties every tile the path touches.
            let lo = (o.meta[1] as usize).min(f.texels.len());
            let n = f.texels.get(lo + 7).copied().unwrap_or(0) as usize;
            let hi = lo.saturating_add(n.max(crate::fill::HEADER_WORDS as usize)).min(f.texels.len());
            // Hashed as if at word 0: a fill earlier in the frame growing or
            // shrinking moves this block, which redraws nothing.
            let mut block = f.texels[lo..hi].to_vec();
            crate::fill::unbase(&mut block, lo as u32);
            oh = hash_words(oh, &block);
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

/// A scrolled area of a window to move instead of drawing again
/// (fresco-scene-groups.md §4): between the frame in the buffer and the
/// next, the pixels in `area` move by `d`, except where something drawn
/// over the moving content (`over` in the next frame, `over_before` in the
/// one in the buffer) touches. Whole pixels throughout.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Shift {
    /// `[x0, y0, x1, y1)`.
    pub area: [u32; 4],
    pub d: [i32; 2],
    pub over: Vec<[u32; 4]>,
    pub over_before: Vec<[u32; 4]>,
}

fn cut(a: [u32; 4], b: [u32; 4]) -> Option<[u32; 4]> {
    let r = [a[0].max(b[0]), a[1].max(b[1]), a[2].min(b[2]), a[3].min(b[3])];
    (r[0] < r[2] && r[1] < r[3]).then_some(r)
}

/// `a` less `b`, as up to four rectangles.
fn minus(a: [u32; 4], b: [u32; 4]) -> Vec<[u32; 4]> {
    let Some(i) = cut(a, b) else { return vec![a] };
    [
        [a[0], a[1], a[2], i[1]],
        [a[0], i[3], a[2], a[3]],
        [a[0], i[1], i[0], i[3]],
        [i[2], i[1], a[2], i[3]],
    ]
    .into_iter()
    .filter(|r| r[0] < r[2] && r[1] < r[3])
    .collect()
}

/// What [`shift`] leaves to draw.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Shifted {
    /// Pixel rectangles `[x0, y0, x1, y1)` to raster (overlapping at
    /// most where the same pixels are drawn twice): each area's exposed
    /// strip, what is drawn over it, and the parts of its edge tiles
    /// outside it.
    pub pixels: Vec<[u32; 4]>,
    /// The box of every pixel moved or listed in `pixels`.
    pub changed: Option<[u32; 4]>,
}

/// One move of [`plan`]: the pixels of `dst` (`[x0, y0, x1, y1)`) take
/// the values the drawing had at `dst` less `d`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Move {
    pub dst: [u32; 4],
    pub d: [i32; 2],
}

impl Move {
    /// Where the pixels come from.
    pub fn src(&self) -> [u32; 4] {
        let [dx, dy] = self.d;
        let r = self.dst;
        [(r[0] as i64 - dx as i64) as u32, (r[1] as i64 - dy as i64) as u32, (r[2] as i64 - dx as i64) as u32, (r[3] as i64 - dy as i64) as u32]
    }
}

/// [`shift`] without the pixels, for a drawing held elsewhere (the GPU
/// renderer's surface): the moves to make, every one reading the drawing
/// as it was before any of them, and what is left to draw; `held` gets
/// the new signatures as in [`shift`]. Sources and destinations lie
/// inside the window.
pub fn plan(width: u32, height: u32, sig: &[u64], held: &mut [u64], shifts: &[Shift]) -> (Vec<Move>, Shifted) {
    let tiles_x = width.div_ceil(TILE);
    let tiles_y = height.div_ceil(TILE);
    let n = (tiles_x * tiles_y) as usize;
    assert!(sig.len() == n && held.len() == n);
    let mut out = Shifted::default();
    let mut moves = Vec::new();
    let grow = |c: &mut Option<[u32; 4]>, r: [u32; 4]| {
        *c = Some(match *c {
            Some(m) => [m[0].min(r[0]), m[1].min(r[1]), m[2].max(r[2]), m[3].max(r[3])],
            None => r,
        });
    };
    let window = [0, 0, width, height];
    for s in shifts {
        let Some(a) = cut(s.area, window) else { continue };
        let [dx, dy] = s.d;
        if dx == 0 && dy == 0 {
            continue;
        }
        let moved = |r: [u32; 4]| -> Option<[u32; 4]> {
            let m = [r[0] as i64 + dx as i64, r[1] as i64 + dy as i64, r[2] as i64 + dx as i64, r[3] as i64 + dy as i64];
            let m = m.map(|v| v.clamp(0, u32::MAX as i64) as u32);
            cut(m, a)
        };
        // Destination pixels whose source is inside the area too.
        let copied = moved(a);
        if let Some(c) = copied {
            moves.push(Move { dst: c, d: s.d });
        }
        // The exposed strip.
        match copied {
            Some(c) => out.pixels.extend(minus(a, c)),
            None => out.pixels.push(a),
        }
        // What is drawn over the content, now and where it was copied from.
        out.pixels.extend(s.over.iter().filter_map(|o| cut(*o, a)));
        out.pixels.extend(s.over_before.iter().filter_map(|o| moved(*o)));
        // The tiles the area touches: what of them lies outside it.
        let t0 = [a[0] / TILE, a[1] / TILE, a[2].div_ceil(TILE), a[3].div_ceil(TILE)];
        let around = [t0[0] * TILE, t0[1] * TILE, (t0[2] * TILE).min(width), (t0[3] * TILE).min(height)];
        out.pixels.extend(minus(around, a));
        grow(&mut out.changed, around);
        for ty in t0[1]..t0[3] {
            for tx in t0[0]..t0[2] {
                let i = (ty * tiles_x + tx) as usize;
                held[i] = sig[i];
            }
        }
    }
    (moves, out)
}

/// Apply `shifts` to the drawing in `px` (`width` pixels a line) and to
/// `held`, its tiles' signatures, before a partial raster of the frame
/// whose signatures are `sig`: move each area's pixels, list the pixels
/// still to draw, and give every tile that touches an area its new
/// signature (once the listed pixels are drawn, it holds the new frame).
/// Tiles away from every area are left to their signatures.
pub fn shift(px: &mut [u32], width: u32, height: u32, sig: &[u64], held: &mut [u64], shifts: &[Shift]) -> Shifted {
    assert!(px.len() >= (width * height) as usize);
    let (moves, out) = plan(width, height, sig, held, shifts);
    // With more than one, every move reads the drawing as it was. Every
    // move happens before any raster, and a raster is always right, so
    // overlapping areas only cost pixels drawn twice.
    let before = (moves.len() > 1).then(|| px.to_vec());
    let w = width as usize;
    for m in &moves {
        let [x0, y0, x1, y1] = m.dst;
        let [sx, _, _, _] = m.src();
        let len = (x1 - x0) as usize;
        let rows: Box<dyn Iterator<Item = u32>> = if m.d[1] > 0 { Box::new((y0..y1).rev()) } else { Box::new(y0..y1) };
        for y in rows {
            let sy = (y as i64 - m.d[1] as i64) as usize;
            let d = y as usize * w + x0 as usize;
            let s = sy * w + sx as usize;
            match &before {
                Some(b) => px[d..d + len].copy_from_slice(&b[s..s + len]),
                None => px.copy_within(s..s + len, d),
            }
        }
    }
    out
}

/// The shift kernel's header (shaders/shift.comp): copy `size` words
/// wide and high from `src_off` (`src_pitch` a line, `src_words` in all)
/// to `dst_off` (`dst_pitch`, `dst_words`).
pub fn move_header(size: [u32; 2], src: (u32, u32, u32), dst: (u32, u32, u32)) -> [u32; 8] {
    [size[0], size[1], src.0, src.1, dst.0, dst.1, src.2, dst.2]
}

/// The tiles a frame `width` x `height` must raster after [`plan`]: those
/// whose signature differs from `held` and those `pixels` touch, as at
/// most `max` rectangles of tiles (see [`rects`]).
pub fn tiles_after_plan(width: u32, height: u32, sig: &[u64], held: &[u64], pixels: &[[u32; 4]], max: usize) -> Vec<[u32; 4]> {
    let (tx, ty) = (width.div_ceil(TILE), height.div_ceil(TILE));
    let mut d = dirty(sig, Some(held));
    for p in pixels {
        if p[0] >= p[2] || p[1] >= p[3] {
            continue;
        }
        for y in p[1] / TILE..p[3].div_ceil(TILE).min(ty) {
            for x in p[0] / TILE..p[2].div_ceil(TILE).min(tx) {
                d[(y * tx + x) as usize] = true;
            }
        }
    }
    rects(&d, tx, ty, max)
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
