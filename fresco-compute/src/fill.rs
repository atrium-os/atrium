//! Filled paths (`KIND_FILL`): lines, quadratic and cubic Béziers, nonzero
//! or even-odd, solid paint. Fresco never strokes: a client turns strokes,
//! dashes and markers into fills.
//!
//! # Coverage rule (normative; `raster.comp` and [`crate::reference`] follow it)
//!
//! 1. [`Frame::pack`](crate::Frame::pack) maps the points through the fill's
//!    affine transform, then flattens curves into lines by uniform
//!    subdivision in `t` with `n = ceil(sqrt(d / TOLERANCE))` pieces
//!    (at most [`MAX_CURVE_PIECES`]), where `d` is the control polygon's
//!    deviation: `|p0 - 2p1 + p2| / 4` for a quadratic,
//!    `3/4 * max(|p0 - 2p1 + p2|, |p1 - 2p2 + p3|)` for a cubic, in target
//!    pixels. Every subpath is closed. Executors only ever see lines.
//! 2. A pixel `(px, py)` accumulates the signed area
//!    `a = sum over lines of sign * integral over y in [py, py+1] of
//!    clamp(px + 1 - x(y), 0, 1) dy`, `sign` = +1 for a line going down
//!    (`y1 > y0`). For a closed path this is the integral of the winding
//!    number over the pixel square. Lines wholly left of a 16x16 tile are
//!    pre-summed per pixel row into the tile's backdrop; the rest are listed
//!    per tile and integrated in the kernel (`fill_line` there and here).
//! 3. Coverage is `clamp(|a|, 0, 1)` for nonzero; for even-odd `|a|` is taken
//!    modulo 2 and folded (`a > 1 ? 2 - a : a`). The area is exact wherever
//!    edges do not cross inside a pixel; where they do (a self-intersecting
//!    even-odd path's crossings) it is an approximation.
//! 4. The pixel centre must be inside the clip rect; the colour (linear RGB,
//!    straight alpha) gets alpha `color.a * coverage` and blends src-over
//!    like every other op.
//!
//! The executors run the same f32 operations in the same order, so Tier-2
//! and the GIR interpreter match the reference exactly; an executor that
//! fuses multiply-adds (RDNA4) may differ by one step in a channel.
//!
//! # Limits
//!
//! Points must be finite and within ±[`MAX_COORD`] in path space (before
//! the transform, which carries scroll and zoom). A path that flattens to
//! more than [`MAX_PATH_SEGMENTS`] lines, puts more than
//! [`MAX_TILE_SEGMENTS`] lines in one tile, or would take the frame past
//! [`MAX_FRAME_SEGMENTS`] lines or [`MAX_FRAME_INDICES`] list entries is
//! dropped (not drawn); a path wholly outside the target or its clip is
//! culled. The kernel re-checks every index it reads from the block, so a
//! hostile renderer's block only draws wrong pixels, in bounded work.
//!
//! # Block layout (in `texels`, at the op's `meta[1]`)
//!
//! | words | what |
//! |---|---|
//! | 0..8 | tile x0, tile y0, tiles wide, tiles high, first line word, lines, rule (0 nonzero, 1 even-odd), block words |
//! | 8.. | per tile, row major: first list word, list length, 16 backdrop floats (one per pixel row) |
//! | then | the lists: line indices |
//! | then | the lines: x0, y0, x1, y1 as f32 bits |

use crate::TILE;

pub const VERB_MOVE: u8 = 0;
pub const VERB_LINE: u8 = 1;
pub const VERB_QUAD: u8 = 2;
pub const VERB_CUBIC: u8 = 3;
pub const VERB_CLOSE: u8 = 4;

/// Points per verb.
pub fn verb_points(v: u8) -> Option<usize> {
    match v {
        VERB_MOVE | VERB_LINE => Some(1),
        VERB_QUAD => Some(2),
        VERB_CUBIC => Some(3),
        VERB_CLOSE => Some(0),
        _ => None,
    }
}

/// Bound on path-space coordinates.
pub const MAX_COORD: f32 = 32768.0;
/// Most lines one path may flatten to.
pub const MAX_PATH_SEGMENTS: usize = 65536;
/// Most lines all fills of a frame may flatten to.
pub const MAX_FRAME_SEGMENTS: usize = 512 * 1024;
/// Most lines one tile of one path may list (the kernel clamps to it too).
pub const MAX_TILE_SEGMENTS: u32 = 4096;
/// Most list entries (line, tile) per frame.
pub const MAX_FRAME_INDICES: usize = 4 << 20;
/// Flattening tolerance, target pixels.
pub const TOLERANCE: f32 = 0.25;
/// Most lines one curve flattens to.
pub const MAX_CURVE_PIECES: u32 = 1024;
/// Words per tile entry and in the block header.
pub const ENTRY_WORDS: u32 = 18;
pub const HEADER_WORDS: u32 = 8;
/// Largest block grid edge in tiles (`MAX_DIM / TILE`); the kernel refuses
/// bigger ones.
pub const MAX_GRID: u32 = crate::MAX_DIM / TILE;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FillRule {
    #[default]
    NonZero,
    EvenOdd,
}

/// One filled path.
#[derive(Clone, Debug, Default)]
pub struct Fill {
    pub verbs: Vec<u8>,
    pub points: Vec<[f32; 2]>,
    /// `[a, b, c, d, e, f]`: x' = a x + c y + e, y' = b x + d y + f.
    pub transform: [f32; 6],
    pub rule: FillRule,
    /// Linear RGB, straight alpha (opacity folded in).
    pub color: [f32; 4],
    /// Target pixels `[x0, y0, x1, y1)`; `None`: the whole target.
    pub clip: Option<[f32; 4]>,
}

pub const IDENTITY: [f32; 6] = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

/// Check a path's shape: known verbs, the right number of points, a move
/// first, every point finite and within ±[`MAX_COORD`]; the transform
/// finite. The broker and frescod both run it.
pub fn check(verbs: &[u8], points: &[[f32; 2]], transform: &[f32; 6]) -> Result<(), &'static str> {
    let mut need = 0usize;
    for (i, &v) in verbs.iter().enumerate() {
        let n = verb_points(v).ok_or("unknown verb")?;
        if i == 0 && v != VERB_MOVE {
            return Err("a path starts with a move");
        }
        need += n;
    }
    if need != points.len() {
        return Err("points do not match the verbs");
    }
    if points.iter().flatten().any(|c| !c.is_finite() || c.abs() > MAX_COORD) {
        return Err("point not finite or past the coordinate bound");
    }
    if transform.iter().any(|c| !c.is_finite()) {
        return Err("transform not finite");
    }
    Ok(())
}

fn xf(t: &[f32; 6], p: [f32; 2]) -> [f32; 2] {
    [t[0] * p[0] + t[2] * p[1] + t[4], t[1] * p[0] + t[3] * p[1] + t[5]]
}

fn pieces(d: f32) -> u32 {
    let n = (d / TOLERANCE).sqrt().ceil();
    if n >= MAX_CURVE_PIECES as f32 { MAX_CURVE_PIECES } else if n >= 1.0 { n as u32 } else { 1 }
}

fn len(v: [f32; 2]) -> f32 {
    (v[0] * v[0] + v[1] * v[1]).sqrt()
}

/// Flatten `f` into target-space lines `[x0, y0, x1, y1]`, closing every
/// subpath; horizontal lines are dropped (they cover nothing). `None` if
/// it is malformed or flattens past [`MAX_PATH_SEGMENTS`].
pub fn flatten(f: &Fill) -> Option<Vec<[f32; 4]>> {
    check(&f.verbs, &f.points, &f.transform).ok()?;
    let t = &f.transform;
    let mut out: Vec<[f32; 4]> = Vec::new();
    let mut pts = f.points.iter().map(|&p| xf(t, p));
    let (mut start, mut cur) = ([0.0f32; 2], [0.0f32; 2]);
    let line = |out: &mut Vec<[f32; 4]>, a: [f32; 2], b: [f32; 2]| {
        if a[1] != b[1] {
            out.push([a[0], a[1], b[0], b[1]]);
        }
    };
    for &v in &f.verbs {
        match v {
            VERB_MOVE => {
                line(&mut out, cur, start);
                start = pts.next()?;
                cur = start;
            }
            VERB_LINE => {
                let p = pts.next()?;
                line(&mut out, cur, p);
                cur = p;
            }
            VERB_QUAD => {
                let (p1, p2) = (pts.next()?, pts.next()?);
                let p0 = cur;
                let d = len([p0[0] - 2.0 * p1[0] + p2[0], p0[1] - 2.0 * p1[1] + p2[1]]) * 0.25;
                let n = pieces(d);
                let mut prev = p0;
                for i in 1..=n {
                    let s = i as f32 / n as f32;
                    let u = 1.0 - s;
                    let q = if i == n {
                        p2
                    } else {
                        [u * u * p0[0] + 2.0 * u * s * p1[0] + s * s * p2[0], u * u * p0[1] + 2.0 * u * s * p1[1] + s * s * p2[1]]
                    };
                    line(&mut out, prev, q);
                    prev = q;
                }
                cur = p2;
            }
            VERB_CUBIC => {
                let (p1, p2, p3) = (pts.next()?, pts.next()?, pts.next()?);
                let p0 = cur;
                let d1 = len([p0[0] - 2.0 * p1[0] + p2[0], p0[1] - 2.0 * p1[1] + p2[1]]);
                let d2 = len([p1[0] - 2.0 * p2[0] + p3[0], p1[1] - 2.0 * p2[1] + p3[1]]);
                let n = pieces(0.75 * d1.max(d2));
                let mut prev = p0;
                for i in 1..=n {
                    let s = i as f32 / n as f32;
                    let u = 1.0 - s;
                    let (a, b, c, e) = (u * u * u, 3.0 * u * u * s, 3.0 * u * s * s, s * s * s);
                    let q = if i == n {
                        p3
                    } else {
                        [a * p0[0] + b * p1[0] + c * p2[0] + e * p3[0], a * p0[1] + b * p1[1] + c * p2[1] + e * p3[1]]
                    };
                    line(&mut out, prev, q);
                    prev = q;
                }
                cur = p3;
            }
            _ => {
                // VERB_CLOSE
                line(&mut out, cur, start);
                cur = start;
            }
        }
        if out.len() > MAX_PATH_SEGMENTS {
            return None;
        }
    }
    line(&mut out, cur, start);
    (out.len() <= MAX_PATH_SEGMENTS && out.iter().flatten().all(|c| c.is_finite())).then_some(out)
}

/// What a fill packs to: its block (to append to `texels` at word `base`)
/// and its op's box and clip.
pub struct Packed {
    pub block: Vec<u32>,
    /// Pixel box `[x0, y0, x1, y1)` (op `p3`).
    pub bbox: [f32; 4],
    /// Clip `[x0, y0, x1, y1)` (op `p0`).
    pub clip: [f32; 4],
    pub lines: usize,
    pub indices: usize,
}

/// The part of line `s` with y in `[lo, hi]` (`s` not horizontal).
fn clip_y(s: [f32; 4], lo: f32, hi: f32) -> Option<[f32; 4]> {
    let (y0, y1) = (s[1].min(s[3]), s[1].max(s[3]));
    let (a, b) = (y0.max(lo), y1.min(hi));
    if b <= a {
        return None;
    }
    let dxdy = (s[2] - s[0]) / (s[3] - s[1]);
    let x = |y: f32| s[0] + (y - s[1]) * dxdy;
    // Keep the direction: from the end with the smaller t.
    Some(if s[3] > s[1] { [x(a), a, x(b), b] } else { [x(b), b, x(a), a] })
}

/// Tile `lines` for a `width` x `height` target: the block with its words
/// relative to `base` made absolute. `None` when culled or past a limit.
pub fn pack(lines: &[[f32; 4]], rule: FillRule, clip: Option<[f32; 4]>, width: u32, height: u32, base: u32) -> Option<Packed> {
    if lines.is_empty() {
        return None;
    }
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for s in lines {
        x0 = x0.min(s[0]).min(s[2]);
        y0 = y0.min(s[1]).min(s[3]);
        x1 = x1.max(s[0]).max(s[2]);
        y1 = y1.max(s[1]).max(s[3]);
    }
    let clip = clip.unwrap_or([0.0, 0.0, width as f32, height as f32]);
    if !clip.iter().all(|c| c.is_finite()) {
        return None;
    }
    let bx0 = x0.max(clip[0]).max(0.0).floor();
    let by0 = y0.max(clip[1]).max(0.0).floor();
    let bx1 = x1.min(clip[2]).min(width as f32).ceil();
    let by1 = y1.min(clip[3]).min(height as f32).ceil();
    if bx1 <= bx0 || by1 <= by0 {
        return None; // culled
    }
    let t = TILE as f32;
    let (tx0, ty0) = ((bx0 / t) as u32, (by0 / t) as u32);
    let (tx1, ty1) = (((bx1 - 1.0) / t) as u32, ((by1 - 1.0) / t) as u32);
    let (tw, th) = (tx1 - tx0 + 1, ty1 - ty0 + 1);
    let n = (tw * th) as usize;
    let mut lists: Vec<Vec<u32>> = vec![Vec::new(); n];
    // Backdrop deltas per tile, prefix-summed along each tile row below.
    let mut delta = vec![[0.0f32; 16]; n];
    for (i, s) in lines.iter().enumerate() {
        let (sy0, sy1) = (s[1].min(s[3]), s[1].max(s[3]));
        let band_lo = ((sy0 / t).floor().max(ty0 as f32)) as u32;
        let band_hi = ((sy1 / t).floor().min(ty1 as f32)) as u32;
        if sy1 <= ty0 as f32 * t || sy0 >= (ty1 + 1) as f32 * t {
            continue;
        }
        for ty in band_lo..=band_hi {
            let by = ty as f32 * t;
            let Some(p) = clip_y(*s, by, by + t) else { continue };
            let (pxmin, pxmax) = (p[0].min(p[2]), p[0].max(p[2]));
            let row = ((ty - ty0) * tw) as usize;
            // Listed in the tiles its x span touches.
            let c_lo = ((pxmin / t).floor().max(tx0 as f32)) as i64;
            let c_hi = ((pxmax / t).floor().min(tx1 as f32)) as i64;
            for c in c_lo.max(tx0 as i64)..=c_hi {
                lists[row + (c as u32 - tx0) as usize].push(i as u32);
            }
            // Every tile to its right gets its dy per pixel row.
            let first = ((pxmax / t).floor() + 1.0).max(tx0 as f32);
            if first <= tx1 as f32 {
                let d = &mut delta[row + (first as u32 - tx0) as usize];
                let sign = if s[3] > s[1] { 1.0 } else { -1.0 };
                for (r, v) in d.iter_mut().enumerate() {
                    let ry = by + r as f32;
                    let (a, b) = (p[1].min(p[3]).max(ry), p[1].max(p[3]).min(ry + 1.0));
                    if b > a {
                        *v += sign * (b - a);
                    }
                }
            }
        }
    }
    let indices: usize = lists.iter().map(Vec::len).sum();
    if lists.iter().any(|l| l.len() > MAX_TILE_SEGMENTS as usize) {
        return None;
    }
    let entries = HEADER_WORDS as usize + n * ENTRY_WORDS as usize;
    let lines_at = entries + indices;
    let words = lines_at + lines.len() * 4;
    let mut block = vec![0u32; words];
    block[..HEADER_WORDS as usize].copy_from_slice(&[
        tx0,
        ty0,
        tw,
        th,
        base + lines_at as u32,
        lines.len() as u32,
        (rule == FillRule::EvenOdd) as u32,
        words as u32,
    ]);
    let mut at = entries;
    for ty in 0..th {
        let mut acc = [0.0f32; 16];
        for tx in 0..tw {
            let k = (ty * tw + tx) as usize;
            for (a, d) in acc.iter_mut().zip(&delta[k]) {
                *a += d;
            }
            let e = HEADER_WORDS as usize + k * ENTRY_WORDS as usize;
            block[e] = base + at as u32;
            block[e + 1] = lists[k].len() as u32;
            for (r, a) in acc.iter().enumerate() {
                block[e + 2 + r] = a.to_bits();
            }
            block[at..at + lists[k].len()].copy_from_slice(&lists[k]);
            at += lists[k].len();
        }
    }
    for (i, s) in lines.iter().enumerate() {
        for (j, c) in s.iter().enumerate() {
            block[lines_at + i * 4 + j] = c.to_bits();
        }
    }
    Some(Packed { block, bbox: [bx0, by0, bx1, by1], clip, lines: lines.len(), indices })
}

/// G(t), the integral of clamp(1 - t, 0, 1) (see the coverage rule).
pub fn g(t: f32) -> f32 {
    if t <= 0.0 {
        t
    } else if t >= 1.0 {
        0.5
    } else {
        t - 0.5 * t * t
    }
}

/// One line's signed area in pixel `(px, py)`: `raster.comp`'s `fill_line`,
/// operation for operation.
pub fn fill_line(s: [f32; 4], px: f32, py: f32) -> f32 {
    if s[3] == s[1] {
        return 0.0;
    }
    let (sy0, sy1) = if s[1] < s[3] { (s[1], s[3]) } else { (s[3], s[1]) };
    let (ylo, yhi) = (py, py + 1.0);
    if !(sy1 > ylo && sy0 < yhi) {
        return 0.0;
    }
    let dxdy = (s[2] - s[0]) / (s[3] - s[1]);
    let ya = if sy0 > ylo { sy0 } else { ylo };
    let yb = if sy1 < yhi { sy1 } else { yhi };
    let ta = (s[0] + (ya - s[1]) * dxdy) - px;
    let tb = (s[0] + (yb - s[1]) * dxdy) - px;
    let dy = yb - ya;
    let a = if (tb - ta).abs() < 1e-5 {
        let m = 1.0 - 0.5 * (ta + tb);
        if m >= 0.0 { if m <= 1.0 { m } else { 1.0 } } else { 0.0 }
    } else {
        (g(tb) - g(ta)) / (tb - ta)
    };
    let a = dy * a;
    if s[3] > s[1] { a } else { -a }
}

/// Coverage from the accumulated area.
pub fn coverage(acc: f32, even_odd: bool) -> f32 {
    let mut a = acc.abs();
    if even_odd {
        a = a - 2.0 * (a * 0.5).floor();
        if a > 1.0 {
            a = 2.0 - a;
        }
    }
    if a >= 0.0 { if a <= 1.0 { a } else { 1.0 } } else { 0.0 }
}
