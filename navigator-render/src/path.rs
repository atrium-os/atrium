//! ★ VECTOR SHAPES: the NSG `path` node, one to one with Fresco's
//! `ATRIUM_CORE_FILL` (0x1004, fresco-protocol `FillParams`).
//!
//! An inline SVG the normalizer made standalone carries its text on its
//! replaced element (`atrium-svg`). The renderer turns it into filled paths
//! (navigator-svg `vector`: strokes and dashes made into fills, transforms
//! into each fill's affine) when ALL of it can be said that way and every
//! path passes Fresco's own checks at the painted scale; otherwise it keeps
//! the bitmap image node, and the reason is counted.
//!
//! In NSG everything is an integer (§4.1a): points and gradient geometry in
//! 1/64 of a path unit, the transform's linear part in 1/65536 and its
//! offset in 1/64 px, colours as sRGB `rrggbbaa`, stop offsets in 1/1024,
//! opacity 0-255, the clip rectangle in 1/64 px. `fill_params` turns a node into what
//! frescod takes (colour to linear RGB, units to f32) and `validate` runs
//! the checks the broker runs before handing a node on.

use crate::U;

/// A path's paint: sRGB, straight alpha, `0xRRGGBBAA`; gradient geometry
/// in 1/64 path unit (the transform applies to it), stops as (offset in
/// 1/1024, colour).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Paint {
    Solid(u32),
    Linear { p: [i64; 4], stops: Vec<(u32, u32)> },
    Radial { c: [i64; 3], stops: Vec<(u32, u32)> },
}

/// One filled path, as the scene carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Path {
    pub even_odd: bool,
    /// `[a, b, c, d]` in 1/65536, `[e, f]` in 1/64 px: x' = a x + c y + e,
    /// y' = b x + d y + f, from path units to the page.
    pub transform: [i64; 6],
    pub paint: Paint,
    /// The path's own clip rectangle (from a rectangular SVG clip path),
    /// page units `[x0, y0, x1, y1)` in 1/64 px.
    pub clip: Option<[i64; 4]>,
    /// 0-255, multiplying the alpha.
    pub opacity: u32,
    /// fresco-compute verbs: 0 move, 1 line, 2 quad, 3 cubic, 4 close.
    pub verbs: Vec<u8>,
    /// 1/64 path unit.
    pub points: Vec<(i64, i64)>,
}

/// Lines one window may hold in one frame (fresco-compute's
/// `MAX_FRAME_SEGMENTS`): the vector SVGs of one document stop taking the
/// vector path once theirs would pass half of it, so the page's own boxes
/// and text keep their share.
pub const DOCUMENT_SEGMENT_SHARE: usize = fresco_compute::fill::MAX_FRAME_SEGMENTS / 2;

fn srgb_to_linear(c: u8) -> f32 {
    let c = c as f32 / 255.0;
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

/// The node as frescod takes it, in window pixels (scene units / 64),
/// with `clip` in window pixels if any.
pub fn fill_params(p: &Path, clip: Option<[f32; 4]>) -> fresco_protocol::FillParams {
    let t = &p.transform;
    let lin = |v: i64| v as f32 / 65536.0;
    let off = |v: i64| v as f32 / 64.0;
    // ★ Colour spaces differ by paint: Fresco takes SOLID in linear RGB, but
    // gradient stops in sRGB (it interpolates them premultiplied in sRGB).
    let stops = |s: &[(u32, u32)]| -> Vec<fresco_protocol::GradientStop> {
        s.iter().map(|&(o, c)| fresco_protocol::GradientStop { offset: o as f32 / 1024.0,
            r: (c >> 24) as f32 / 255.0, g: ((c >> 16) & 0xff) as f32 / 255.0, b: ((c >> 8) & 0xff) as f32 / 255.0, a: (c & 0xff) as f32 / 255.0 }).collect()
    };
    let paint = match &p.paint {
        Paint::Solid(c) => fresco_protocol::FillPaint::Solid {
            r: srgb_to_linear((c >> 24) as u8), g: srgb_to_linear((c >> 16) as u8),
            b: srgb_to_linear((c >> 8) as u8), a: (c & 0xff) as f32 / 255.0 },
        Paint::Linear { p: q, stops: s } => fresco_protocol::FillPaint::Linear {
            x0: q[0] as f32 / 64.0, y0: q[1] as f32 / 64.0, x1: q[2] as f32 / 64.0, y1: q[3] as f32 / 64.0, stops: stops(s) },
        Paint::Radial { c, stops: s } => fresco_protocol::FillPaint::Radial {
            cx: c[0] as f32 / 64.0, cy: c[1] as f32 / 64.0, radius: c[2] as f32 / 64.0, stops: stops(s) },
    };
    // The path's own clip and the scene's, intersected.
    let own = p.clip.map(|k| [k[0] as f32 / 64.0, k[1] as f32 / 64.0, k[2] as f32 / 64.0, k[3] as f32 / 64.0]);
    let clip = match (own, clip) {
        (Some(a), Some(b)) => Some([a[0].max(b[0]), a[1].max(b[1]), a[2].min(b[2]), a[3].min(b[3])]),
        (a, b) => a.or(b),
    };
    fresco_protocol::FillParams {
        rule: u8::from(p.even_odd),
        transform: [lin(t[0]), lin(t[1]), lin(t[2]), lin(t[3]), off(t[4]), off(t[5])],
        paint,
        opacity: p.opacity as f32 / 255.0,
        clip,
        verbs: p.verbs.clone(),
        points: p.points.iter().map(|&(x, y)| [x as f32 / 64.0, y as f32 / 64.0]).collect(),
    }
}

/// ★ THE BROKER'S CHECK on a `path` node, before it goes to frescod:
/// fresco-protocol's own `FillParams::validate` (verbs, points, bounds,
/// finite values), the same function frescod runs.
pub fn validate(p: &Path) -> Result<(), &'static str> {
    fill_params(p, None).validate()
}

/// The lines `p` flattens to at its painted transform, if it passes
/// fresco-compute's per-path and per-tile limits there; `None` if frescod
/// would drop it. (A path is placed at the origin for the tile check, so
/// where it sits on the page does not change the answer.)
pub fn lines_if_drawable(p: &Path) -> Option<usize> {
    let fp = fill_params(p, None);
    let mut t = fp.transform;
    let f0 = fresco_compute::fill::Fill { verbs: fp.verbs.clone(), points: fp.points.clone(), transform: t,
        rule: fresco_compute::fill::FillRule::NonZero, color: [0.0; 4], clip: None, gradient: Default::default() };
    let lines = fresco_compute::fill::flatten(&f0)?;
    if lines.is_empty() { return Some(0) }
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for s in &lines { x0 = x0.min(s[0]).min(s[2]); y0 = y0.min(s[1]).min(s[3]); x1 = x1.max(s[0]).max(s[2]); y1 = y1.max(s[1]).max(s[3]) }
    t[4] -= x0.floor(); t[5] -= y0.floor();
    let f = fresco_compute::fill::Fill { transform: t, ..f0 };
    let lines = fresco_compute::fill::flatten(&f)?;
    let (w, h) = ((x1 - x0).ceil() as u32 + 2, (y1 - y0).ceil() as u32 + 2);
    let rule = if p.even_odd { fresco_compute::fill::FillRule::EvenOdd } else { fresco_compute::fill::FillRule::NonZero };
    fresco_compute::fill::pack(&lines, rule, None, w.min(fresco_compute::MAX_DIM), h.min(fresco_compute::MAX_DIM), 0)?;
    Some(lines.len())
}

/// The SVG `text` drawn into the tile `(tx, ty, tw, th)` (scene units) as
/// paths, or why not. `budget` is what is left of this document's share of
/// lines; it is reduced by what these paths use.
pub fn from_svg(text: &str, tile: (U, U, U, U), budget: &mut usize) -> Result<Vec<Path>, &'static str> {
    let (tree, _) = navigator_svg::load(text.as_bytes()).map_err(|e| match e {
        navigator_svg::LoadError::Refused(why) => why,
        navigator_svg::LoadError::Corrupt(_) => "corrupt",
    })?;
    let fills = navigator_svg::vector::fills(&tree)?;
    let size = tree.size();
    let (tx, ty, tw, th) = tile;
    // The SVG's viewport onto the tile: scene units per user unit.
    let (sx, sy) = (tw as f64 / size.width() as f64, th as f64 / size.height() as f64);
    let mut out = Vec::with_capacity(fills.len());
    let mut used = 0usize;
    for f in fills {
        let a = &f.transform;
        // fit ∘ abs, from user units to scene units (1/64 px).
        let m = [sx * a[0] as f64, sy * a[1] as f64, sx * a[2] as f64, sy * a[3] as f64,
                 sx * a[4] as f64 + tx as f64, sy * a[5] as f64 + ty as f64];
        let q64 = |v: f32| (v as f64 * 64.0).round() as i64;
        let rgba = |c: [u8; 4]| u32::from_be_bytes(c);
        let st = |s: &[(f32, [u8; 4])]| s.iter().map(|&(o, c)| ((o.clamp(0.0, 1.0) * 1024.0).round() as u32, rgba(c))).collect::<Vec<_>>();
        let paint = match &f.paint {
            navigator_svg::vector::Paint::Solid(c) => Paint::Solid(rgba(*c)),
            navigator_svg::vector::Paint::Linear { p0, p1, stops } => Paint::Linear { p: [q64(p0[0]), q64(p0[1]), q64(p1[0]), q64(p1[1])], stops: st(stops) },
            navigator_svg::vector::Paint::Radial { c, r, stops } => Paint::Radial { c: [q64(c[0]), q64(c[1]), q64(*r)], stops: st(stops) },
        };
        // The clip rectangle, viewport units to page units (the fit is an
        // axis-aligned scale and offset).
        let clip = f.clip.map(|k| [(sx * k[0] as f64 + tx as f64).round() as i64, (sy * k[1] as f64 + ty as f64).round() as i64,
                                   (sx * k[2] as f64 + tx as f64).round() as i64, (sy * k[3] as f64 + ty as f64).round() as i64]);
        let p = Path {
            even_odd: f.even_odd,
            // Linear part: scene units per user unit are px*64 per unit, so
            // px per unit is m/64, kept in 1/65536.
            transform: [(m[0] / 64.0 * 65536.0).round() as i64, (m[1] / 64.0 * 65536.0).round() as i64,
                        (m[2] / 64.0 * 65536.0).round() as i64, (m[3] / 64.0 * 65536.0).round() as i64,
                        m[4].round() as i64, m[5].round() as i64],
            paint,
            clip,
            opacity: (f.opacity.clamp(0.0, 1.0) * 255.0).round() as u32,
            verbs: f.verbs,
            points: f.points.iter().map(|q| ((q[0] as f64 * 64.0).round() as i64, (q[1] as f64 * 64.0).round() as i64)).collect(),
        };
        validate(&p)?;
        used += lines_if_drawable(&p).ok_or("past Fresco's per-path or per-tile limit at the painted scale")?;
        if used > *budget { return Err("past the document's share of vector lines") }
        out.push(p);
    }
    *budget -= used;
    Ok(out)
}

/// The paint of `p` at window pixel centre `(x, y)` (window pixels), sRGB
/// straight alpha, before coverage and opacity: a solid colour, or the
/// gradient there (pad spread, stops interpolated premultiplied in sRGB, as
/// Fresco does). `None` outside the path's own clip. For the review
/// rasterizer and tests.
pub fn paint_at(p: &Path, x: f32, y: f32) -> Option<[f32; 4]> {
    let (cx, cy) = (x + 0.5, y + 0.5);
    if let Some(k) = p.clip {
        let k = [k[0] as f32 / 64.0, k[1] as f32 / 64.0, k[2] as f32 / 64.0, k[3] as f32 / 64.0];
        if cx < k[0] || cy < k[1] || cx >= k[2] || cy >= k[3] { return None }
    }
    let unpack = |c: u32| [(c >> 24) as f32 / 255.0, ((c >> 16) & 0xff) as f32 / 255.0, ((c >> 8) & 0xff) as f32 / 255.0, (c & 0xff) as f32 / 255.0];
    // Window pixel to path space: invert the transform.
    let to_path = || -> Option<(f32, f32)> {
        let m = &p.transform;
        let (a, b, c, d) = (m[0] as f32 / 65536.0, m[1] as f32 / 65536.0, m[2] as f32 / 65536.0, m[3] as f32 / 65536.0);
        let (e, f) = (m[4] as f32 / 64.0, m[5] as f32 / 64.0);
        let det = a * d - b * c;
        if det.abs() < 1e-12 { return None }
        let (ux, uy) = (cx - e, cy - f);
        Some(((d * ux - c * uy) / det, (-b * ux + a * uy) / det))
    };
    let (t, stops) = match &p.paint {
        Paint::Solid(c) => return Some(unpack(*c)),
        Paint::Linear { p: q, stops } if !stops.is_empty() => {
            let (px, py) = to_path()?;
            let (x0, y0, x1, y1) = (q[0] as f32 / 64.0, q[1] as f32 / 64.0, q[2] as f32 / 64.0, q[3] as f32 / 64.0);
            let (dx, dy) = (x1 - x0, y1 - y0);
            (((px - x0) * dx + (py - y0) * dy) / (dx * dx + dy * dy).max(1e-12), stops)
        }
        Paint::Radial { c: q, stops } if !stops.is_empty() => {
            let (px, py) = to_path()?;
            ((px - q[0] as f32 / 64.0).hypot(py - q[1] as f32 / 64.0) / (q[2] as f32 / 64.0).max(1e-12), stops)
        }
        _ => return None,
    };
    let t = t.clamp(0.0, 1.0);
    let stop = |i: usize| -> (f32, [f32; 4]) { (stops[i].0 as f32 / 1024.0, unpack(stops[i].1)) };
    let (first, last) = (stop(0), stop(stops.len() - 1));
    if t <= first.0 { return Some(first.1) }
    if t >= last.0 { return Some(last.1) }
    for i in 1..stops.len() {
        let (o1, c1) = stop(i);
        if t <= o1 {
            let (o0, c0) = stop(i - 1);
            let u = if o1 > o0 { (t - o0) / (o1 - o0) } else { 1.0 };
            // Premultiplied interpolation, then back to straight alpha.
            let a = c0[3] + (c1[3] - c0[3]) * u;
            let ch = |k: usize| { let v = c0[k] * c0[3] + (c1[k] * c1[3] - c0[k] * c0[3]) * u; if a > 0.0 { v / a } else { 0.0 } };
            return Some([ch(0), ch(1), ch(2), a]);
        }
    }
    Some(last.1)
}

/// The bitmap fallback for the vector SVG that path `i` belongs to:
/// (its first path, its path count, the image node to draw instead).
pub fn fallback_for(scene: &crate::Scene, i: usize) -> Option<&(usize, usize, crate::Image)> {
    scene.path_fallbacks.iter().find(|(first, n, _)| (*first..first + n).contains(&i))
}
