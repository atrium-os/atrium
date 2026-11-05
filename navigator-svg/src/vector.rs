//! ★ SVG AS VECTOR SHAPES: the profile-checked tree as filled paths, one
//! to one with Fresco's `ATRIUM_CORE_FILL` (fresco-protocol `FillParams`;
//! the coverage rule is fresco-compute's `fill`).
//!
//! Fresco never strokes, so a stroke (with its dashes) is turned into the
//! filled outline it paints, here, by tiny-skia's stroker. A transform is
//! the fill's own affine. Paint is solid only, in sRGB with straight alpha
//! (the caller converts to Fresco's linear RGB); a gradient or pattern
//! keeps the bitmap path until Fresco's gradient paints land.
//!
//! What a fill cannot say exactly is refused, with the reason, and the
//! caller keeps the bitmap path for that SVG:
//! - a clip path, a mask or a filter (the profile already refuses filters);
//! - group opacity over more than one shape (overlapping shapes composite
//!   differently from each shape at that opacity), while group opacity over
//!   one shape is folded into its paint;
//! - a pattern paint; an image or text node (refused by the profile anyway).

use usvg::tiny_skia_path::{PathSegment, Path};

pub const VERB_MOVE: u8 = 0;
pub const VERB_LINE: u8 = 1;
pub const VERB_QUAD: u8 = 2;
pub const VERB_CUBIC: u8 = 3;
pub const VERB_CLOSE: u8 = 4;

/// sRGB, straight alpha.
pub type Rgba = [u8; 4];

/// A fill's paint. Gradient geometry is in PATH space (the fill's
/// transform applies to it, as to the points).
#[derive(Debug, Clone, PartialEq)]
pub enum Paint {
    Solid(Rgba),
    Linear { p0: [f32; 2], p1: [f32; 2], stops: Vec<(f32, Rgba)> },
    Radial { c: [f32; 2], r: f32, stops: Vec<(f32, Rgba)> },
}

pub const MAX_STOPS: usize = 16;

/// One filled path, in the SVG's own user space.
#[derive(Debug, Clone, PartialEq)]
pub struct Fill {
    pub verbs: Vec<u8>,
    /// Path space (before `transform`).
    pub points: Vec<[f32; 2]>,
    /// `[a, b, c, d, e, f]`: x' = a x + c y + e, y' = b x + d y + f, path
    /// space to the SVG's viewport (its size, `usvg::Tree::size`).
    pub transform: [f32; 6],
    pub even_odd: bool,
    pub paint: Paint,
    /// Multiplies the alpha: the paint's opacity and any group opacity
    /// folded in.
    pub opacity: f32,
    /// A clip rectangle in the SVG's viewport space `[x0, y0, x1, y1)`, from
    /// rectangular clip paths.
    pub clip: Option<[f32; 4]>,
}

fn segments(p: &Path, out_v: &mut Vec<u8>, out_p: &mut Vec<[f32; 2]>) {
    for s in p.segments() {
        match s {
            PathSegment::MoveTo(a) => { out_v.push(VERB_MOVE); out_p.push([a.x, a.y]) }
            PathSegment::LineTo(a) => { out_v.push(VERB_LINE); out_p.push([a.x, a.y]) }
            PathSegment::QuadTo(a, b) => { out_v.push(VERB_QUAD); out_p.extend([[a.x, a.y], [b.x, b.y]]) }
            PathSegment::CubicTo(a, b, c) => { out_v.push(VERB_CUBIC); out_p.extend([[a.x, a.y], [b.x, b.y], [c.x, c.y]]) }
            PathSegment::Close => out_v.push(VERB_CLOSE),
        }
    }
}

type Affine = [f32; 6];

fn mul(a: &Affine, b: &Affine) -> Affine {
    // a ∘ b: apply b first.
    [a[0] * b[0] + a[2] * b[1], a[1] * b[0] + a[3] * b[1], a[0] * b[2] + a[2] * b[3], a[1] * b[2] + a[3] * b[3],
     a[0] * b[4] + a[2] * b[5] + a[4], a[1] * b[4] + a[3] * b[5] + a[5]]
}
fn aff(t: usvg::Transform) -> Affine { [t.sx, t.ky, t.kx, t.sy, t.tx, t.ty] }
fn apply(t: &Affine, p: [f32; 2]) -> [f32; 2] { [t[0] * p[0] + t[2] * p[1] + t[4], t[1] * p[0] + t[3] * p[1] + t[5]] }
fn axis_aligned(t: &Affine) -> bool { t[1].abs() < 1e-6 && t[2].abs() < 1e-6 }

fn stops(s: &[usvg::Stop], opacity_spread: usvg::SpreadMethod) -> Result<Vec<(f32, Rgba)>, &'static str> {
    if !matches!(opacity_spread, usvg::SpreadMethod::Pad) { return Err("gradient spread other than pad") }
    if s.is_empty() || s.len() > MAX_STOPS { return Err("gradient stop count") }
    Ok(s.iter().map(|st| {
        let c = st.color();
        (st.offset().get(), [c.red, c.green, c.blue, (st.opacity().get() * 255.0).round() as u8])
    }).collect())
}

fn paint(p: &usvg::Paint) -> Result<Paint, &'static str> {
    match p {
        usvg::Paint::Color(c) => Ok(Paint::Solid([c.red, c.green, c.blue, 255])),
        usvg::Paint::LinearGradient(g) => {
            let t = aff(g.transform());
            let det = t[0] * t[3] - t[1] * t[2];
            if det.abs() < 1e-12 { return Err("gradient transform not invertible") }
            let (q0, q1) = ([g.x1(), g.y1()], [g.x2(), g.y2()]);
            let d = [q1[0] - q0[0], q1[1] - q0[1]];
            let dd = d[0] * d[0] + d[1] * d[1];
            if dd < 1e-12 { return Err("degenerate gradient") }
            // t(x) = (x - q0)·d / |d|² in gradient space; with x = T⁻¹ y it is
            // (y - T q0)·g with g = L⁻ᵀ d / |d|², L the linear part of T.
            // L = [[t0, t2], [t1, t3]], so L⁻ᵀ = [[t3, -t1], [-t2, t0]] / det.
            let gv = [(t[3] * d[0] - t[1] * d[1]) / det / dd, (-t[2] * d[0] + t[0] * d[1]) / det / dd];
            let gg = gv[0] * gv[0] + gv[1] * gv[1];
            let p0 = apply(&t, q0);
            let p1 = [p0[0] + gv[0] / gg, p0[1] + gv[1] / gg];
            Ok(Paint::Linear { p0, p1, stops: stops(g.stops(), g.spread_method())? })
        }
        usvg::Paint::RadialGradient(g) => {
            let t = aff(g.transform());
            // A similarity: equal column lengths, orthogonal columns.
            let (c0, c1) = ((t[0] * t[0] + t[1] * t[1]).sqrt(), (t[2] * t[2] + t[3] * t[3]).sqrt());
            if (c0 - c1).abs() > 1e-4 * c0.max(1e-6) || (t[0] * t[2] + t[1] * t[3]).abs() > 1e-4 * c0 * c1 {
                return Err("radial gradient under a non-uniform transform")
            }
            if (g.fx() - g.cx()).abs() > 1e-6 || (g.fy() - g.cy()).abs() > 1e-6 || g.fr().get() > 1e-6 {
                return Err("radial gradient with a focus")
            }
            Ok(Paint::Radial { c: apply(&t, [g.cx(), g.cy()]), r: g.r().get() * c0, stops: stops(g.stops(), g.spread_method())? })
        }
        usvg::Paint::Pattern(_) => Err("pattern paint"),
    }
}

/// The clip rectangle a clip path makes in the space of the element it
/// clips (`elem` maps that space to the viewport), or why not.
fn clip_rect(c: &usvg::ClipPath, elem: &Affine) -> Result<[f32; 4], &'static str> {
    if c.clip_path().is_some() { return Err("clip path") }
    let kids = c.root().children();
    let [usvg::Node::Path(p)] = kids else { return Err("clip path") };
    let total = mul(elem, &mul(&aff(c.transform()), &aff(p.abs_transform())));
    if !axis_aligned(&total) { return Err("clip path") }
    // The path must be exactly an axis-aligned rectangle: four corners.
    let mut pts = vec![];
    for seg in p.data().segments() {
        match seg {
            PathSegment::MoveTo(a) | PathSegment::LineTo(a) => pts.push([a.x, a.y]),
            PathSegment::Close => {}
            _ => return Err("clip path"),
        }
    }
    if pts.len() == 5 && (pts[4][0] - pts[0][0]).abs() < 1e-6 && (pts[4][1] - pts[0][1]).abs() < 1e-6 { pts.pop(); }
    if pts.len() != 4 { return Err("clip path") }
    let xs: Vec<f32> = pts.iter().map(|q| q[0]).collect();
    let ys: Vec<f32> = pts.iter().map(|q| q[1]).collect();
    let (x0, x1) = (xs.iter().cloned().fold(f32::MAX, f32::min), xs.iter().cloned().fold(f32::MIN, f32::max));
    let (y0, y1) = (ys.iter().cloned().fold(f32::MAX, f32::min), ys.iter().cloned().fold(f32::MIN, f32::max));
    if !pts.iter().all(|q| ((q[0] - x0).abs() < 1e-6 || (q[0] - x1).abs() < 1e-6) && ((q[1] - y0).abs() < 1e-6 || (q[1] - y1).abs() < 1e-6)) {
        return Err("clip path")
    }
    let a = apply(&total, [x0, y0]);
    let b = apply(&total, [x1, y1]);
    Ok([a[0].min(b[0]), a[1].min(b[1]), a[0].max(b[0]), a[1].max(b[1])])
}

fn intersect(a: Option<[f32; 4]>, b: [f32; 4]) -> [f32; 4] {
    match a { None => b, Some(a) => [a[0].max(b[0]), a[1].max(b[1]), a[2].min(b[2]).max(a[0].max(b[0])), a[3].min(b[3]).max(a[1].max(b[1]))] }
}

fn paths_under(g: &usvg::Group) -> usize {
    g.children().iter().map(|n| match n {
        usvg::Node::Group(c) => paths_under(c),
        usvg::Node::Path(p) => usize::from(p.fill().is_some()) + usize::from(p.stroke().is_some()),
        _ => 1,
    }).sum()
}

/// The fills that paint `tree`, in paint order, or why it cannot be drawn
/// as fills.
pub fn fills(tree: &usvg::Tree) -> Result<Vec<Fill>, &'static str> {
    let mut out = vec![];
    walk(tree.root(), 1.0, None, &mut out)?;
    Ok(out)
}

fn walk(g: &usvg::Group, opacity: f32, clip: Option<[f32; 4]>, out: &mut Vec<Fill>) -> Result<(), &'static str> {
    let mut clip = clip;
    if let Some(c) = g.clip_path() {
        let elem = aff(g.abs_transform());
        if !axis_aligned(&elem) { return Err("clip path") }
        clip = Some(intersect(clip, clip_rect(c, &elem)?));
    }
    if g.mask().is_some() { return Err("mask") }
    if !g.filters().is_empty() { return Err("filter") }
    let mut opacity = opacity;
    let o = g.opacity().get();
    if o < 1.0 {
        if paths_under(g) > 1 { return Err("group opacity over several shapes") }
        opacity *= o;
    }
    for n in g.children() {
        match n {
            usvg::Node::Group(c) => walk(c, opacity, clip, out)?,
            usvg::Node::Path(p) => path(p, opacity, clip, out)?,
            usvg::Node::Image(_) => return Err("embedded image"),
            usvg::Node::Text(_) => return Err("text"),
        }
    }
    Ok(())
}

fn path(p: &usvg::Path, opacity: f32, clip: Option<[f32; 4]>, out: &mut Vec<Fill>) -> Result<(), &'static str> {
    if !p.is_visible() { return Ok(()) }
    let t = p.abs_transform();
    let transform = [t.sx, t.ky, t.kx, t.sy, t.tx, t.ty];
    let fill = |out: &mut Vec<Fill>| -> Result<(), &'static str> {
        let Some(f) = p.fill() else { return Ok(()) };
        let paint = paint(f.paint())?;
        let (mut verbs, mut points) = (vec![], vec![]);
        segments(p.data(), &mut verbs, &mut points);
        if verbs.is_empty() { return Ok(()) }
        out.push(Fill { verbs, points, transform, even_odd: matches!(f.rule(), usvg::FillRule::EvenOdd),
                        paint, opacity: opacity * f.opacity().get(), clip });
        Ok(())
    };
    let stroke = |out: &mut Vec<Fill>| -> Result<(), &'static str> {
        let Some(s) = p.stroke() else { return Ok(()) };
        let paint = paint(s.paint())?;
        // ★ The outline the stroke paints, in path space: dashes applied,
        // then stroked. The resolution scale makes the outline's curves fine
        // enough for the largest scale the transform applies, times 4 so a
        // zoom does not show facets.
        let scale = (t.sx.hypot(t.ky)).max(t.kx.hypot(t.sy)).max(1e-3) * 4.0;
        let ts = s.to_tiny_skia();
        let source = match &ts.dash { Some(d) => p.data().dash(d, scale), None => Some(p.data().clone()) };
        let Some(outline) = source.and_then(|src| src.stroke(&usvg::tiny_skia_path::Stroke { dash: None, ..ts.clone() }, scale)) else { return Ok(()) };
        let (mut verbs, mut points) = (vec![], vec![]);
        segments(&outline, &mut verbs, &mut points);
        if verbs.is_empty() { return Ok(()) }
        out.push(Fill { verbs, points, transform, even_odd: false, paint, opacity: opacity * s.opacity().get(), clip });
        Ok(())
    };
    match p.paint_order() {
        usvg::PaintOrder::FillAndStroke => { fill(out)?; stroke(out)? }
        usvg::PaintOrder::StrokeAndFill => { stroke(out)?; fill(out)? }
    }
    Ok(())
}
