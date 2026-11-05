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
//! - a gradient or pattern paint; an image or text node (refused by the
//!   profile anyway).

use usvg::tiny_skia_path::{PathSegment, Path};

pub const VERB_MOVE: u8 = 0;
pub const VERB_LINE: u8 = 1;
pub const VERB_QUAD: u8 = 2;
pub const VERB_CUBIC: u8 = 3;
pub const VERB_CLOSE: u8 = 4;

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
    /// sRGB, straight alpha.
    pub rgba: [u8; 4],
    /// Multiplies the alpha: the paint's opacity and any group opacity
    /// folded in.
    pub opacity: f32,
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

fn color(p: &usvg::Paint) -> Result<[u8; 3], &'static str> {
    match p {
        usvg::Paint::Color(c) => Ok([c.red, c.green, c.blue]),
        usvg::Paint::LinearGradient(_) | usvg::Paint::RadialGradient(_) => Err("gradient paint"),
        usvg::Paint::Pattern(_) => Err("pattern paint"),
    }
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
    walk(tree.root(), 1.0, &mut out)?;
    Ok(out)
}

fn walk(g: &usvg::Group, opacity: f32, out: &mut Vec<Fill>) -> Result<(), &'static str> {
    if g.clip_path().is_some() { return Err("clip path") }
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
            usvg::Node::Group(c) => walk(c, opacity, out)?,
            usvg::Node::Path(p) => path(p, opacity, out)?,
            usvg::Node::Image(_) => return Err("embedded image"),
            usvg::Node::Text(_) => return Err("text"),
        }
    }
    Ok(())
}

fn path(p: &usvg::Path, opacity: f32, out: &mut Vec<Fill>) -> Result<(), &'static str> {
    if !p.is_visible() { return Ok(()) }
    let t = p.abs_transform();
    let transform = [t.sx, t.ky, t.kx, t.sy, t.tx, t.ty];
    let fill = |out: &mut Vec<Fill>| -> Result<(), &'static str> {
        let Some(f) = p.fill() else { return Ok(()) };
        let [r, g, b] = color(f.paint())?;
        let (mut verbs, mut points) = (vec![], vec![]);
        segments(p.data(), &mut verbs, &mut points);
        if verbs.is_empty() { return Ok(()) }
        out.push(Fill { verbs, points, transform, even_odd: matches!(f.rule(), usvg::FillRule::EvenOdd),
                        rgba: [r, g, b, 255], opacity: opacity * f.opacity().get() });
        Ok(())
    };
    let stroke = |out: &mut Vec<Fill>| -> Result<(), &'static str> {
        let Some(s) = p.stroke() else { return Ok(()) };
        let [r, g, b] = color(s.paint())?;
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
        out.push(Fill { verbs, points, transform, even_odd: false, rgba: [r, g, b, 255], opacity: opacity * s.opacity().get() });
        Ok(())
    };
    match p.paint_order() {
        usvg::PaintOrder::FillAndStroke => { fill(out)?; stroke(out)? }
        usvg::PaintOrder::StrokeAndFill => { stroke(out)?; fill(out)? }
    }
    Ok(())
}
