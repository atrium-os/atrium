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
//! In NSG everything is an integer (§4.1a): points in 1/64 of a path unit,
//! the transform's linear part in 1/65536 and its offset in 1/64 px, colour
//! as sRGB `rrggbbaa`, opacity 0-255. `fill_params` turns a node into what
//! frescod takes (colour to linear RGB, units to f32) and `validate` runs
//! the checks the broker runs before handing a node on.

use crate::U;

/// One filled path, as the scene carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Path {
    pub even_odd: bool,
    /// `[a, b, c, d]` in 1/65536, `[e, f]` in 1/64 px: x' = a x + c y + e,
    /// y' = b x + d y + f, from path units to the page.
    pub transform: [i64; 6],
    /// sRGB, straight alpha, `0xRRGGBBAA`.
    pub rgba: u32,
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
    fresco_protocol::FillParams {
        rule: u8::from(p.even_odd),
        transform: [lin(t[0]), lin(t[1]), lin(t[2]), lin(t[3]), off(t[4]), off(t[5])],
        paint: fresco_protocol::FillPaint::Solid {
            r: srgb_to_linear((p.rgba >> 24) as u8), g: srgb_to_linear((p.rgba >> 16) as u8),
            b: srgb_to_linear((p.rgba >> 8) as u8), a: (p.rgba & 0xff) as f32 / 255.0,
        },
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
        rule: fresco_compute::fill::FillRule::NonZero, color: [0.0; 4], clip: None };
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
        let p = Path {
            even_odd: f.even_odd,
            // Linear part: scene units per user unit are px*64 per unit, so
            // px per unit is m/64, kept in 1/65536.
            transform: [(m[0] / 64.0 * 65536.0).round() as i64, (m[1] / 64.0 * 65536.0).round() as i64,
                        (m[2] / 64.0 * 65536.0).round() as i64, (m[3] / 64.0 * 65536.0).round() as i64,
                        m[4].round() as i64, m[5].round() as i64],
            rgba: u32::from_be_bytes(f.rgba),
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
