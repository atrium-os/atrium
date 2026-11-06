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
//! | 8..18 | paint (0 solid, 1 linear, 2 radial), first stop word, stops, six gradient coefficients ([`gradient_coefficients`]), 0 |
//! | 8.. | per tile, row major: first list word, list length, 16 backdrop floats (one per pixel row) |
//! | then | the lists: line indices |
//! | then | the lines: x0, y0, x1, y1 as f32 bits |
//! | then | the gradient stops: offset, r, g, b, a ([`Gradient`]) |

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
pub const HEADER_WORDS: u32 = 18;
/// Most gradient stops.
pub const MAX_STOPS: u32 = 16;
/// Words per gradient stop: offset, then r, g, b, a (sRGB, straight).
pub const STOP_WORDS: u32 = 5;
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
    /// Linear RGB, straight alpha (opacity folded in). With a gradient,
    /// only the alpha is used: the opacity.
    pub color: [f32; 4],
    /// Target pixels `[x0, y0, x1, y1)`; `None`: the whole target.
    pub clip: Option<[f32; 4]>,
    pub gradient: Option<Gradient>,
}

/// A gradient's geometry, in path space (the fill's transform applies).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GradientShape {
    /// From `p0` (t = 0) to `p1` (t = 1).
    Linear { p0: [f32; 2], p1: [f32; 2] },
    /// t = distance from `center` / `radius`.
    Radial { center: [f32; 2], radius: f32 },
}

/// A gradient paint: CSS's default. Stops are `(offset, [r, g, b, a])`,
/// sRGB-encoded with straight alpha, offsets non-decreasing in [0, 1];
/// colours interpolate premultiplied in sRGB, `t` is clamped to the first
/// and last offsets (pad), and the result is quantized to 8-bit sRGB and
/// decoded through the LUT, so every executor gets the same linear value.
/// A zero-length or zero-radius gradient (or a singular transform) paints
/// the last stop.
#[derive(Clone, Debug, PartialEq)]
pub struct Gradient {
    pub shape: GradientShape,
    pub stops: Vec<(f32, [f32; 4])>,
}

/// Check a gradient: 1 to [`MAX_STOPS`] stops, finite, offsets
/// non-decreasing within [0, 1], a finite shape.
pub fn check_gradient(g: &Gradient) -> Result<(), &'static str> {
    if g.stops.is_empty() || g.stops.len() > MAX_STOPS as usize {
        return Err("gradient stop count");
    }
    let mut last = 0.0f32;
    for (o, c) in &g.stops {
        if !(o.is_finite() && *o >= last && *o <= 1.0) || !c.iter().all(|v| v.is_finite()) {
            return Err("gradient stops must be finite, in [0, 1] and non-decreasing");
        }
        last = *o;
    }
    let ok = match g.shape {
        GradientShape::Linear { p0, p1 } => p0.iter().chain(&p1).all(|v| v.is_finite()),
        GradientShape::Radial { center, radius } => center.iter().all(|v| v.is_finite()) && radius.is_finite(),
    };
    ok.then_some(()).ok_or("gradient shape not finite")
}

/// The gradient's per-pixel coefficients in target space: `t` (linear) or
/// `(u, v)` (radial, `t = |(u, v)|`) as affine functions of the pixel
/// centre: `u = m0 x + m1 y + m2`, `v = m3 x + m4 y + m5`.
pub fn gradient_coefficients(g: &Gradient, t: &[f32; 6]) -> [f32; 6] {
    let last = [0.0, 0.0, 1.0, 0.0, 0.0, 0.0];
    let [a, b, c, d, e, f] = t.map(f64::from);
    let det = a * d - b * c;
    if !(det.abs() > 1e-12) {
        return last;
    }
    // Path-space point as affine functions of target (X, Y):
    // px = pxx X + pxy Y + px0, py = pyx X + pyy Y + py0.
    let (pxx, pxy, px0) = (d / det, -c / det, (c * f - d * e) / det);
    let (pyx, pyy, py0) = (-b / det, a / det, (b * e - a * f) / det);
    let out = match g.shape {
        GradientShape::Linear { p0, p1 } => {
            let (x0, y0) = (p0[0] as f64, p0[1] as f64);
            let (dx, dy) = (p1[0] as f64 - x0, p1[1] as f64 - y0);
            let l2 = dx * dx + dy * dy;
            if !(l2 > 1e-12) {
                return last;
            }
            [
                (pxx * dx + pyx * dy) / l2,
                (pxy * dx + pyy * dy) / l2,
                ((px0 - x0) * dx + (py0 - y0) * dy) / l2,
                0.0,
                0.0,
                0.0,
            ]
        }
        GradientShape::Radial { center, radius } => {
            let r = radius as f64;
            if !(r > 1e-12) {
                return last;
            }
            let (cx, cy) = (center[0] as f64, center[1] as f64);
            [pxx / r, pxy / r, (px0 - cx) / r, pyx / r, pyy / r, (py0 - cy) / r]
        }
    };
    let out = out.map(|v| v as f32);
    if out.iter().all(|v| v.is_finite()) { out } else { last }
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

/// Why [`crate::Frame::pack_report`] did not draw a node (the values of
/// `fresco_protocol::fill_drop`). Only `OP_BUDGET` applies to nodes other
/// than fills.
pub mod drop {
    pub const TOO_MANY_LINES: u8 = 1;
    pub const TILE_TOO_DENSE: u8 = 2;
    pub const FRAME_BUDGET: u8 = 3;
    pub const BAD_PAINT: u8 = 4;
    pub const MALFORMED: u8 = 5;
    /// The frame passed [`crate::MAX_OPS`] ops after culling; the node and
    /// every one painted after it are not drawn.
    pub const OP_BUDGET: u8 = 6;
}

/// Paint kinds in a block's header word 8.
pub const PAINT_SOLID: u32 = 0;
pub const PAINT_LINEAR: u32 = 1;
pub const PAINT_RADIAL: u32 = 2;

/// What a fill packs to: its block (to append to `texels` at word `base`)
/// and its op's box and clip.
#[derive(Clone, Debug)]
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
    pack_paint(lines, rule, clip, None, width, height, base).ok()
}

/// Culled: nothing of it is on the target or in its clip (not a drop).
pub const CULLED: u8 = 0;

/// [`pack`] with a gradient (`(gradient, fill transform)`).
pub fn pack_paint(
    lines: &[[f32; 4]],
    rule: FillRule,
    clip: Option<[f32; 4]>,
    paint: Option<(&Gradient, &[f32; 6])>,
    width: u32,
    height: u32,
    base: u32,
) -> Result<Packed, u8> {
    if lines.is_empty() {
        return Err(CULLED);
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
        return Err(CULLED);
    }
    let bx0 = x0.max(clip[0]).max(0.0).floor();
    let by0 = y0.max(clip[1]).max(0.0).floor();
    let bx1 = x1.min(clip[2]).min(width as f32).ceil();
    let by1 = y1.min(clip[3]).min(height as f32).ceil();
    if bx1 <= bx0 || by1 <= by0 {
        return Err(CULLED);
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
        return Err(drop::TILE_TOO_DENSE);
    }
    let entries = HEADER_WORDS as usize + n * ENTRY_WORDS as usize;
    let lines_at = entries + indices;
    let stops_at = lines_at + lines.len() * 4;
    let n_stops = paint.map_or(0, |(g, _)| g.stops.len());
    let words = stops_at + n_stops * STOP_WORDS as usize;
    let mut block = vec![0u32; words];
    let (kind, m) = match paint {
        None => (PAINT_SOLID, [0.0; 6]),
        Some((g, t)) => {
            let k = match g.shape {
                GradientShape::Linear { .. } => PAINT_LINEAR,
                GradientShape::Radial { .. } => PAINT_RADIAL,
            };
            (k, gradient_coefficients(g, t))
        }
    };
    block[..HEADER_WORDS as usize].copy_from_slice(&[
        tx0,
        ty0,
        tw,
        th,
        base + lines_at as u32,
        lines.len() as u32,
        (rule == FillRule::EvenOdd) as u32,
        words as u32,
        kind,
        base + stops_at as u32,
        n_stops as u32,
        m[0].to_bits(),
        m[1].to_bits(),
        m[2].to_bits(),
        m[3].to_bits(),
        m[4].to_bits(),
        m[5].to_bits(),
        0,
    ]);
    if let Some((g, _)) = paint {
        for (i, (o, c)) in g.stops.iter().enumerate() {
            let at = stops_at + i * STOP_WORDS as usize;
            block[at] = o.to_bits();
            for k in 0..4 {
                block[at + 1 + k] = c[k].to_bits();
            }
        }
    }
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
    Ok(Packed { block, bbox: [bx0, by0, bx1, by1], clip, lines: lines.len(), indices })
}

/// The gradient's straight-alpha sRGB colour at `t`: `raster.comp`'s
/// `stop_color`, operation for operation. `stops` as packed.
pub fn gradient_at(stops: &[(f32, [f32; 4])], t: f32) -> [f32; 4] {
    let n = stops.len();
    let first = stops[0].0;
    let last = stops[n - 1].0;
    let t = if t >= first { if t <= last { t } else { last } } else { first };
    let mut i = 0;
    while i + 1 < n && stops[i + 1].0 < t {
        i += 1;
    }
    let (o0, c0) = stops[i];
    let (o1, c1) = if i + 1 < n { stops[i + 1] } else { stops[i] };
    let span = o1 - o0;
    let f = if span > 0.0 { (t - o0) / span } else { 1.0 };
    let pa = [c0[0] * c0[3], c0[1] * c0[3], c0[2] * c0[3]];
    let pb = [c1[0] * c1[3], c1[1] * c1[3], c1[2] * c1[3]];
    let a = c0[3] + f * (c1[3] - c0[3]);
    let mut out = [0.0f32; 4];
    for k in 0..3 {
        let p = pa[k] + f * (pb[k] - pa[k]);
        out[k] = if a > 0.0 { p / a } else { 0.0 };
    }
    out[3] = a;
    out
}

/// An sRGB channel in [0, 1] to linear through the LUT's 256 entries.
pub fn srgb_channel(lut: &[f32], c: f32) -> f32 {
    let v = if c >= 0.0 { if c <= 1.0 { c } else { 1.0 } } else { 0.0 };
    lut[(v * 255.0 + 0.5) as u32 as usize]
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


/// One fill for a `width` x `height` target, packed at word 0 (move it with
/// [`rebase`]), or why it is not drawn: `Err(CULLED)` or a [`drop`] reason.
/// Everything but the frame-wide budgets ([`MAX_FRAME_SEGMENTS`],
/// [`MAX_FRAME_INDICES`]), which [`crate::Frame::pack`] applies.
pub fn prepare(f: &Fill, width: u32, height: u32) -> Result<Packed, u8> {
    prepare_with(f, width, height, |f| flatten_local(f).map(std::sync::Arc::new))
}

/// [`prepare`], taking the fill's lines in its local space (see
/// [`flatten_local`]) from `local`.
fn prepare_with(
    f: &Fill,
    width: u32,
    height: u32,
    local: impl FnOnce(&Fill) -> Option<std::sync::Arc<Vec<[f32; 4]>>>,
) -> Result<Packed, u8> {
    if check(&f.verbs, &f.points, &f.transform).is_err() {
        return Err(drop::MALFORMED);
    }
    if f.gradient.as_ref().is_some_and(|g| check_gradient(g).is_err()) {
        return Err(drop::BAD_PAINT);
    }
    let local = local(f).ok_or(drop::TOO_MANY_LINES)?;
    let (e, g) = (f.transform[4], f.transform[5]);
    let lines: Vec<[f32; 4]> = local
        .iter()
        .map(|l| [l[0] + e, l[1] + g, l[2] + e, l[3] + g])
        .filter(|l| l[1] != l[3])
        .collect();
    if !lines.iter().flatten().all(|c| c.is_finite()) {
        return Err(drop::TOO_MANY_LINES);
    }
    let paint = f.gradient.as_ref().map(|g| (g, &f.transform));
    pack_paint(&lines, f.rule, f.clip, paint, width, height, 0)
}

/// The fill's lines with its transform's translation left out: what
/// [`prepare`] flattens, then moves by the translation. A fill that only
/// moves (a scrolled group's, fresco-scene-groups.md §4) keeps these, so
/// [`FillCache`] re-tiles it without flattening it again.
pub fn flatten_local(f: &Fill) -> Option<Vec<[f32; 4]>> {
    let t = f.transform;
    flatten(&Fill { transform: [t[0], t[1], t[2], t[3], 0.0, 0.0], verbs: f.verbs.clone(), points: f.points.clone(), gradient: None, clip: None, ..*f })
}

/// Move a block packed at word 0 to word `base`: its absolute words (the
/// line and stop starts, each tile's list start) gain `base`.
pub fn rebase(block: &mut [u32], base: u32) {
    if base == 0 || block.len() < HEADER_WORDS as usize {
        return;
    }
    block[4] += base;
    block[9] += base;
    let n = (block[2] * block[3]) as usize;
    for k in 0..n {
        block[HEADER_WORDS as usize + k * ENTRY_WORDS as usize] += base;
    }
}

/// [`rebase`] undone, for a block at word `base`: its absolute words made
/// relative (wrapping; a malformed block's words stay malformed, and a
/// grid bigger than the block is cut to it).
pub fn unbase(block: &mut [u32], base: u32) {
    if block.len() < HEADER_WORDS as usize {
        return;
    }
    block[4] = block[4].wrapping_sub(base);
    block[9] = block[9].wrapping_sub(base);
    let n = (block[2] as usize).saturating_mul(block[3] as usize);
    for k in 0..n {
        let Some(w) = block.get_mut(HEADER_WORDS as usize + k * ENTRY_WORDS as usize) else { break };
        *w = w.wrapping_sub(base);
    }
}

/// [`prepare`]'s results kept across frames, so a fill that did not change
/// (the colour and opacity aside, which are not in its block) is neither
/// flattened nor tiled again. Keyed by the fill's exact bits and the target
/// size, so a hit gives the very block a fresh pack would.
#[derive(Default)]
pub struct FillCache {
    map: std::collections::HashMap<Vec<u32>, (u64, std::sync::Arc<Result<Packed, u8>>)>,
    /// Lines in local space (see [`flatten_local`]), keyed by the path and
    /// the transform's linear part: a fill that moved is only re-tiled.
    lines: std::collections::HashMap<Vec<u32>, (u64, Option<std::sync::Arc<Vec<[f32; 4]>>>)>,
    tick: u64,
    words: usize,
}

/// Packs a cache entry lives without being used.
const CACHE_LIFE: u64 = 64;
/// Most block words a cache holds (64 MiB) before it starts over.
const CACHE_MAX_WORDS: usize = 16 << 20;

fn lines_key(f: &Fill) -> Vec<u32> {
    let mut k = Vec::with_capacity(8 + f.verbs.len() + f.points.len() * 2);
    k.extend([f.verbs.len() as u32, f.points.len() as u32]);
    k.extend(f.verbs.iter().map(|&v| v as u32));
    k.extend(f.points.iter().flat_map(|p| [p[0].to_bits(), p[1].to_bits()]));
    k.extend(f.transform[..4].iter().map(|v| v.to_bits()));
    k
}

fn cache_key(f: &Fill, width: u32, height: u32) -> Vec<u32> {
    let mut k = Vec::with_capacity(16 + f.verbs.len() + f.points.len() * 2);
    k.extend([width, height, (f.rule == FillRule::EvenOdd) as u32, f.verbs.len() as u32, f.points.len() as u32]);
    k.extend(f.verbs.iter().map(|&v| v as u32));
    k.extend(f.points.iter().flat_map(|p| [p[0].to_bits(), p[1].to_bits()]));
    k.extend(f.transform.iter().map(|v| v.to_bits()));
    match f.clip {
        None => k.push(0),
        Some(c) => {
            k.push(1);
            k.extend(c.iter().map(|v| v.to_bits()));
        }
    }
    match &f.gradient {
        None => k.push(0),
        Some(g) => {
            match g.shape {
                GradientShape::Linear { p0, p1 } => k.extend([1, p0[0].to_bits(), p0[1].to_bits(), p1[0].to_bits(), p1[1].to_bits()]),
                GradientShape::Radial { center, radius } => k.extend([2, center[0].to_bits(), center[1].to_bits(), radius.to_bits()]),
            }
            k.push(g.stops.len() as u32);
            for (o, c) in &g.stops {
                k.push(o.to_bits());
                k.extend(c.iter().map(|v| v.to_bits()));
            }
        }
    }
    k
}

impl FillCache {
    /// Start a pack: entries unused for a while go.
    pub fn begin(&mut self) {
        self.tick += 1;
        if self.words > CACHE_MAX_WORDS {
            self.map.clear();
            self.lines.clear();
            self.words = 0;
        } else if self.tick % 16 == 0 {
            let tick = self.tick;
            let mut freed = 0;
            self.map.retain(|_, (used, r)| {
                let keep = *used + CACHE_LIFE >= tick;
                if !keep {
                    freed += r.as_ref().as_ref().map_or(0, |p| p.block.len());
                }
                keep
            });
            self.lines.retain(|_, (used, l)| {
                let keep = *used + CACHE_LIFE >= tick;
                if !keep {
                    freed += l.as_ref().map_or(0, |l| l.len() * 4);
                }
                keep
            });
            self.words -= freed;
        }
    }

    /// [`prepare`], from the cache when it has it.
    pub fn get(&mut self, f: &Fill, width: u32, height: u32) -> std::sync::Arc<Result<Packed, u8>> {
        let key = cache_key(f, width, height);
        if let Some((used, r)) = self.map.get_mut(&key) {
            *used = self.tick;
            return r.clone();
        }
        let (tick, lines, words) = (self.tick, &mut self.lines, &mut self.words);
        let r = std::sync::Arc::new(prepare_with(f, width, height, |f| {
            let e = lines.entry(lines_key(f)).or_insert_with(|| {
                let l = flatten_local(f).map(std::sync::Arc::new);
                *words += l.as_ref().map_or(0, |l| l.len() * 4);
                (tick, l)
            });
            e.0 = tick;
            e.1.clone()
        }));
        self.words += r.as_ref().as_ref().map_or(0, |p| p.block.len());
        self.map.insert(key, (self.tick, r.clone()));
        r
    }

    /// Paths held flattened (in local space).
    pub fn flattened(&self) -> usize {
        self.lines.len()
    }

    /// Entries held.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}
