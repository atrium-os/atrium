//! Vector shapes for [`crate::widgets::Canvas`]: a path, a fill and a
//! stroke, each turned into Fresco FILLs once (Fresco never strokes; the
//! stroke's outline, dashes applied, comes from tiny-skia's stroker, the one
//! the Navigator uses for SVG), and hit tested on the shape itself.

use std::cell::OnceCell;
use std::sync::Arc;

use fresco_protocol::{FillPaint, FillParams, GradientStop};

use crate::geom::{Point, Rect};
use crate::Color;

const MOVE: u8 = 0;
const LINE: u8 = 1;
const QUAD: u8 = 2;
const CUBIC: u8 = 3;
const CLOSE: u8 = 4;

/// A path in its own units: Fresco's verbs (move, line, quad, cubic,
/// close) and their points.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PathData {
    pub verbs: Vec<u8>,
    pub points: Vec<[f32; 2]>,
}

impl PathData {
    pub fn new() -> Self { Self::default() }
    pub fn move_to(mut self, x: f32, y: f32) -> Self { self.verbs.push(MOVE); self.points.push([x, y]); self }
    pub fn line_to(mut self, x: f32, y: f32) -> Self { self.verbs.push(LINE); self.points.push([x, y]); self }
    pub fn quad_to(mut self, x1: f32, y1: f32, x: f32, y: f32) -> Self {
        self.verbs.push(QUAD);
        self.points.extend([[x1, y1], [x, y]]);
        self
    }
    pub fn cubic_to(mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) -> Self {
        self.verbs.push(CUBIC);
        self.points.extend([[x1, y1], [x2, y2], [x, y]]);
        self
    }
    pub fn close(mut self) -> Self { self.verbs.push(CLOSE); self }

    pub fn rect(r: Rect) -> Self {
        Self::new().move_to(r.x(), r.y()).line_to(r.x() + r.w(), r.y()).line_to(r.x() + r.w(), r.y() + r.h()).line_to(r.x(), r.y() + r.h()).close()
    }

    pub fn rounded_rect(r: Rect, radius: f32) -> Self {
        let k = 0.552_284_8 * radius;
        let (x, y, w, h, q) = (r.x(), r.y(), r.w(), r.h(), radius.clamp(0.0, r.w().min(r.h()) / 2.0));
        if q == 0.0 {
            return Self::rect(r);
        }
        let k = k.min(q * 0.552_284_8);
        Self::new()
            .move_to(x + q, y).line_to(x + w - q, y)
            .cubic_to(x + w - q + k, y, x + w, y + q - k, x + w, y + q)
            .line_to(x + w, y + h - q)
            .cubic_to(x + w, y + h - q + k, x + w - q + k, y + h, x + w - q, y + h)
            .line_to(x + q, y + h)
            .cubic_to(x + q - k, y + h, x, y + h - q + k, x, y + h - q)
            .line_to(x, y + q)
            .cubic_to(x, y + q - k, x + q - k, y, x + q, y)
            .close()
    }

    pub fn ellipse(c: Point, rx: f32, ry: f32) -> Self {
        let (kx, ky) = (0.552_284_8 * rx, 0.552_284_8 * ry);
        let (x, y) = (c.x, c.y);
        Self::new()
            .move_to(x + rx, y)
            .cubic_to(x + rx, y + ky, x + kx, y + ry, x, y + ry)
            .cubic_to(x - kx, y + ry, x - rx, y + ky, x - rx, y)
            .cubic_to(x - rx, y - ky, x - kx, y - ry, x, y - ry)
            .cubic_to(x + kx, y - ry, x + rx, y - ky, x + rx, y)
            .close()
    }

    fn to_skia(&self) -> Option<tiny_skia_path::Path> {
        let mut b = tiny_skia_path::PathBuilder::new();
        let mut p = self.points.iter();
        for &v in &self.verbs {
            match v {
                MOVE => { let [x, y] = *p.next()?; b.move_to(x, y) }
                LINE => { let [x, y] = *p.next()?; b.line_to(x, y) }
                QUAD => { let ([a, c], [x, y]) = (*p.next()?, *p.next()?); b.quad_to(a, c, x, y) }
                CUBIC => { let ([a, c], [d, e], [x, y]) = (*p.next()?, *p.next()?, *p.next()?); b.cubic_to(a, c, d, e, x, y) }
                CLOSE => b.close(),
                _ => return None,
            }
        }
        b.finish()
    }

    fn from_skia(p: &tiny_skia_path::Path) -> Self {
        use tiny_skia_path::PathSegment as S;
        let mut d = Self::new();
        for s in p.segments() {
            match s {
                S::MoveTo(a) => { d.verbs.push(MOVE); d.points.push([a.x, a.y]) }
                S::LineTo(a) => { d.verbs.push(LINE); d.points.push([a.x, a.y]) }
                S::QuadTo(a, b) => { d.verbs.push(QUAD); d.points.extend([[a.x, a.y], [b.x, b.y]]) }
                S::CubicTo(a, b, c) => { d.verbs.push(CUBIC); d.points.extend([[a.x, a.y], [b.x, b.y], [c.x, c.y]]) }
                S::Close => d.verbs.push(CLOSE),
            }
        }
        d
    }

    /// The path as polygons (curves in 16 steps), for hit testing.
    fn polygons(&self) -> Vec<Vec<[f32; 2]>> {
        let mut out: Vec<Vec<[f32; 2]>> = vec![];
        let mut p = self.points.iter().copied();
        let mut at = [0.0f32; 2];
        for &v in &self.verbs {
            match v {
                MOVE => {
                    let Some(q) = p.next() else { break };
                    out.push(vec![q]);
                    at = q;
                }
                LINE => {
                    let Some(q) = p.next() else { break };
                    if let Some(poly) = out.last_mut() { poly.push(q) }
                    at = q;
                }
                QUAD | CUBIC => {
                    let n = if v == QUAD { 2 } else { 3 };
                    let pts: Vec<[f32; 2]> = p.by_ref().take(n).collect();
                    if pts.len() < n { break }
                    let poly = match out.last_mut() { Some(poly) => poly, None => break };
                    for i in 1..=16 {
                        let t = i as f32 / 16.0;
                        let u = 1.0 - t;
                        let q = if v == QUAD {
                            [u * u * at[0] + 2.0 * u * t * pts[0][0] + t * t * pts[1][0], u * u * at[1] + 2.0 * u * t * pts[0][1] + t * t * pts[1][1]]
                        } else {
                            let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
                            [a * at[0] + b * pts[0][0] + c * pts[1][0] + d * pts[2][0], a * at[1] + b * pts[0][1] + c * pts[1][1] + d * pts[2][1]]
                        };
                        poly.push(q);
                    }
                    at = *pts.last().unwrap();
                }
                _ => {}
            }
        }
        out
    }

    /// Whether `pt` is inside (non-zero or even-odd).
    pub fn contains(&self, pt: [f32; 2], even_odd: bool) -> bool {
        let mut winding = 0i32;
        for poly in self.polygons() {
            for i in 0..poly.len() {
                let (a, b) = (poly[i], poly[(i + 1) % poly.len()]);
                if (a[1] <= pt[1]) != (b[1] <= pt[1]) {
                    let x = a[0] + (pt[1] - a[1]) / (b[1] - a[1]) * (b[0] - a[0]);
                    if x > pt[0] {
                        winding += if b[1] > a[1] { 1 } else { -1 };
                    }
                }
            }
        }
        if even_odd { winding % 2 != 0 } else { winding != 0 }
    }
}

/// How a fill or stroke is painted. Gradient points are in path units.
#[derive(Debug, Clone, PartialEq)]
pub enum Paint {
    Solid(Color),
    Linear { from: Point, to: Point, stops: Vec<(f32, Color)> },
    Radial { center: Point, radius: f32, stops: Vec<(f32, Color)> },
}

fn to_srgb(c: f32) -> f32 {
    if c <= 0.003_130_8 { c * 12.92 } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 }
}

impl Paint {
    fn wire(&self) -> FillPaint {
        // Solid is linear on the wire; gradient stops are sRGB (Fresco
        // interpolates them in sRGB, as CSS does).
        let stops = |s: &[(f32, Color)]| s.iter().map(|(o, c)| GradientStop { offset: *o, r: to_srgb(c.r), g: to_srgb(c.g), b: to_srgb(c.b), a: c.a }).collect();
        match self {
            Paint::Solid(c) => FillPaint::Solid { r: c.r, g: c.g, b: c.b, a: c.a },
            Paint::Linear { from, to, stops: s } => FillPaint::Linear { x0: from.x, y0: from.y, x1: to.x, y1: to.y, stops: stops(s) },
            Paint::Radial { center, radius, stops: s } => FillPaint::Radial { cx: center.x, cy: center.y, radius: *radius, stops: stops(s) },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Cap { #[default] Butt, Round, Square }

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Join { #[default] Miter, Round, Bevel }

#[derive(Debug, Clone, PartialEq)]
pub struct Stroke {
    pub paint: Paint,
    pub width: f32,
    pub cap: Cap,
    pub join: Join,
    pub miter_limit: f32,
    /// Dash lengths (on, off, …) and the offset into them.
    pub dash: Option<(Vec<f32>, f32)>,
}

impl Stroke {
    pub fn new(paint: Paint, width: f32) -> Self {
        Self { paint, width, cap: Cap::Butt, join: Join::Miter, miter_limit: 4.0, dash: None }
    }
}

/// A shape on a canvas: a path filled and/or stroked, under its own
/// transform (`[a, b, c, d, e, f]`, path units to canvas px: rotation and
/// skew welcome).
#[derive(Debug, Clone)]
pub struct Shape {
    pub path: Arc<PathData>,
    pub fill: Option<Paint>,
    pub even_odd: bool,
    pub stroke: Option<Stroke>,
    pub transform: [f32; 6],
    pub opacity: f32,
    /// The stroke's outline, made on first use.
    outline: OnceCell<Option<PathData>>,
}

pub const IDENTITY: [f32; 6] = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

impl Shape {
    pub fn new(path: PathData) -> Self {
        Self { path: Arc::new(path), fill: None, even_odd: false, stroke: None, transform: IDENTITY, opacity: 1.0, outline: OnceCell::new() }
    }
    pub fn fill(mut self, p: Paint) -> Self { self.fill = Some(p); self }
    pub fn even_odd(mut self) -> Self { self.even_odd = true; self }
    pub fn stroke(mut self, s: Stroke) -> Self { self.stroke = Some(s); self.outline = OnceCell::new(); self }
    pub fn transform(mut self, t: [f32; 6]) -> Self { self.transform = t; self }
    pub fn translate(mut self, x: f32, y: f32) -> Self { self.transform[4] += x; self.transform[5] += y; self }
    pub fn opacity(mut self, o: f32) -> Self { self.opacity = o; self }

    fn outline(&self) -> Option<&PathData> {
        self.outline.get_or_init(|| {
            let s = self.stroke.as_ref()?;
            let t = self.transform;
            // Fine enough for the transform's largest scale, times 4 so a
            // zoom shows no facets (as the Navigator's SVG strokes).
            let scale = (t[0].hypot(t[1])).max(t[2].hypot(t[3])).max(1e-3) * 4.0;
            let src = self.path.to_skia()?;
            let src = match &s.dash {
                Some((a, off)) => src.dash(&tiny_skia_path::StrokeDash::new(a.clone(), *off)?, scale)?,
                None => src,
            };
            let st = tiny_skia_path::Stroke {
                width: s.width,
                miter_limit: s.miter_limit,
                line_cap: match s.cap { Cap::Butt => tiny_skia_path::LineCap::Butt, Cap::Round => tiny_skia_path::LineCap::Round, Cap::Square => tiny_skia_path::LineCap::Square },
                line_join: match s.join { Join::Miter => tiny_skia_path::LineJoin::Miter, Join::Round => tiny_skia_path::LineJoin::Round, Join::Bevel => tiny_skia_path::LineJoin::Bevel },
                dash: None,
            };
            Some(PathData::from_skia(&src.stroke(&st, scale)?))
        }).as_ref()
    }

    /// The FILLs that draw it, fill first, in canvas px.
    pub fn fills(&self) -> Vec<FillParams> {
        let mut out = vec![];
        let wire = |d: &PathData, even_odd: bool, paint: &Paint| FillParams {
            rule: u8::from(even_odd),
            transform: self.transform,
            paint: paint.wire(),
            opacity: self.opacity,
            clip: None,
            verbs: d.verbs.clone(),
            points: d.points.clone(),
        };
        if let (Some(p), false) = (&self.fill, self.path.verbs.is_empty()) {
            out.push(wire(&self.path, self.even_odd, p));
        }
        if let (Some(s), Some(o)) = (&self.stroke, self.outline()) {
            if !o.verbs.is_empty() {
                out.push(wire(o, false, &s.paint));
            }
        }
        out
    }

    /// Whether `p` (canvas px) lands on what it paints: its fill, or its
    /// stroke's outline.
    pub fn hit(&self, p: Point) -> bool {
        let [a, b, c, d, e, f] = self.transform;
        let det = a * d - b * c;
        if det.abs() < 1e-12 {
            return false;
        }
        let (x, y) = (p.x - e, p.y - f);
        let q = [(d * x - c * y) / det, (-b * x + a * y) / det];
        (self.fill.is_some() && self.path.contains(q, self.even_odd))
            || self.outline().is_some_and(|o| o.contains(q, false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dashed_stroke_becomes_its_outline_and_is_hit_only_on_ink() {
        let line = PathData::new().move_to(0.0, 0.0).line_to(100.0, 0.0);
        let s = Shape::new(line).stroke(Stroke { dash: Some((vec![10.0, 10.0], 0.0)), ..Stroke::new(Paint::Solid(Color::rgba(0.0, 0.0, 0.0, 1.0)), 4.0) });
        let f = s.fills();
        assert_eq!(f.len(), 1, "no fill, one stroke");
        assert!(f[0].verbs.iter().filter(|&&v| v == MOVE).count() >= 5, "five dashes");
        assert!(f[0].validate().is_ok());
        assert!(s.hit(Point::new(5.0, 1.0)), "on a dash");
        assert!(!s.hit(Point::new(15.0, 1.0)), "in a gap");
        assert!(!s.hit(Point::new(5.0, 3.0)), "past the stroke's half width");
    }

    #[test]
    fn a_rotated_ellipse_is_hit_inside_its_own_outline() {
        let (s, c) = (45f32.to_radians().sin(), 45f32.to_radians().cos());
        let e = Shape::new(PathData::ellipse(Point::new(0.0, 0.0), 50.0, 10.0))
            .fill(Paint::Solid(Color::rgba(1.0, 0.0, 0.0, 1.0)))
            .transform([c, s, -s, c, 100.0, 100.0]);
        assert!(e.hit(Point::new(100.0 + 30.0 * c, 100.0 + 30.0 * s)), "along the rotated long axis");
        assert!(!e.hit(Point::new(130.0, 100.0)), "where the unrotated one would be");
        let ring = Shape::new(PathData::ellipse(Point::new(0.0, 0.0), 20.0, 20.0)).fill(Paint::Solid(Color::rgba(0.0, 0.0, 1.0, 1.0)));
        let mut two = (*ring.path).clone();
        let inner = PathData::ellipse(Point::new(0.0, 0.0), 10.0, 10.0);
        two.verbs.extend(&inner.verbs);
        two.points.extend(&inner.points);
        let donut = Shape::new(two).fill(Paint::Solid(Color::rgba(0.0, 0.0, 1.0, 1.0))).even_odd();
        assert!(!donut.hit(Point::new(0.0, 0.0)), "the hole");
        assert!(donut.hit(Point::new(15.0, 0.0)), "the ring");
    }
}
