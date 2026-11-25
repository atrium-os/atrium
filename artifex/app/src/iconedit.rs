//! The icon editor's model (dev-tools.md §3.5, step 5, D16): an icon
//! editor, not a vector app. Rectangles, ellipses, lines and a pen on a
//! 24-unit grid (snapped to half units); fills (a colour, or a linear or
//! radial gradient between two) and strokes; Atrium's palette plus up to 8
//! colours of the icon's own; a flat list of shapes. No masks, boolean
//! operations, text, filters or effects.
//!
//! It writes plain filled paths (a stroke becomes a fill), the form the
//! dock draws, and refuses to write an icon past the dock's limits
//! (portcullis_toml::icon, mirrored here). The SVG carries the editor's own
//! description of its shapes in a comment, so an icon drawn here opens here
//! again; an SVG without one (drawn elsewhere) is not opened for editing.

/// The grid, in units.
pub const GRID: f32 = 24.0;
/// The dock's limits (portcullis_toml::icon): shapes, points, bytes.
pub const MAX_SHAPES: usize = 64;
pub const MAX_POINTS: usize = 8192;
pub const MAX_BYTES: usize = 32 * 1024;
/// The most colours an icon keeps besides the palette.
pub const MAX_OWN: usize = 8;
/// The most points the pen takes for one shape.
pub const MAX_PEN: usize = 64;
/// Stroke widths offered, in units.
pub const WIDTHS: [f32; 4] = [1.0, 1.5, 2.0, 3.0];

/// Atrium's palette (sRGB, opaque).
pub const PALETTE: [[u8; 4]; 12] = [
    [0xff, 0xff, 0xff, 0xff], [0x1c, 0x1c, 0x1e, 0xff], [0x8e, 0x8e, 0x93, 0xff], [0xd6, 0x45, 0x3d, 0xff],
    [0xe8, 0x83, 0x2a, 0xff], [0xf2, 0xc2, 0x3a, 0xff], [0x3f, 0xa3, 0x5b, 0xff], [0x2b, 0x9a, 0x9a, 0xff],
    [0x30, 0x7c, 0xd6, 0xff], [0x5b, 0x54, 0xc9, 0xff], [0x9b, 0x4f, 0xc2, 0xff], [0xd8, 0x5a, 0x8c, 0xff],
];

const MARK: &str = "artifex-icon v1 ";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tool {
    #[default]
    Select,
    Rect,
    Ellipse,
    Line,
    Pen,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Fill {
    #[default]
    None,
    Solid([u8; 4]),
    /// Top to bottom of the shape's box.
    Linear([u8; 4], [u8; 4]),
    /// From the box's centre out.
    Radial([u8; 4], [u8; 4]),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Rect { x: f32, y: f32, w: f32, h: f32, r: f32 },
    Ellipse { cx: f32, cy: f32, rx: f32, ry: f32 },
    Line { a: (f32, f32), b: (f32, f32) },
    /// The pen's points; closed, or an open line.
    Poly { pts: Vec<(f32, f32)>, closed: bool },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Shape {
    pub kind: Kind,
    pub fill: Fill,
    /// A colour and a width in units.
    pub stroke: Option<([u8; 4], f32)>,
}

/// One filled path, in units: subpaths of verbs (fresco's fill_verb) and points.
#[derive(Debug, Clone, PartialEq)]
pub struct Path {
    pub verbs: Vec<u8>,
    pub points: Vec<[f32; 2]>,
    pub even_odd: bool,
    pub paint: Fill,
    /// The shape's box, for its gradient.
    pub bbox: [f32; 4],
}

const MOVE: u8 = 0;
const LINE: u8 = 1;
const CUBIC: u8 = 3;
const CLOSE: u8 = 4;
const K: f32 = 0.552_284_8;

impl Shape {
    pub fn bbox(&self) -> [f32; 4] {
        match &self.kind {
            Kind::Rect { x, y, w, h, .. } => [*x, *y, x + w, y + h],
            Kind::Ellipse { cx, cy, rx, ry } => [cx - rx, cy - ry, cx + rx, cy + ry],
            Kind::Line { a, b } => [a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1)],
            Kind::Poly { pts, .. } => pts.iter().fold([f32::MAX, f32::MAX, f32::MIN, f32::MIN], |b, p| {
                [b[0].min(p.0), b[1].min(p.1), b[2].max(p.0), b[3].max(p.1)]
            }),
        }
    }

    /// Does the point fall on it (its box, grown by its stroke and a little)?
    pub fn hit(&self, x: f32, y: f32) -> bool {
        let b = self.bbox();
        let m = self.stroke.map(|s| s.1 / 2.0).unwrap_or(0.0) + 0.5;
        x >= b[0] - m && x <= b[2] + m && y >= b[1] - m && y <= b[3] + m
    }

    fn moved(&self, dx: f32, dy: f32) -> Shape {
        let mut s = self.clone();
        match &mut s.kind {
            Kind::Rect { x, y, .. } => { *x += dx; *y += dy; }
            Kind::Ellipse { cx, cy, .. } => { *cx += dx; *cy += dy; }
            Kind::Line { a, b } => { a.0 += dx; a.1 += dy; b.0 += dx; b.1 += dy; }
            Kind::Poly { pts, .. } => for p in pts { p.0 += dx; p.1 += dy; },
        }
        s
    }

    /// Its paths: the fill, then the stroke as a fill.
    pub fn paths(&self) -> Vec<Path> {
        let bbox = self.bbox();
        let mut out = Vec::new();
        let fillable = !matches!(self.kind, Kind::Line { .. });
        if fillable && self.fill != Fill::None {
            let mut p = Path { verbs: Vec::new(), points: Vec::new(), even_odd: false, paint: self.fill, bbox };
            match &self.kind {
                Kind::Rect { x, y, w, h, r } => rect(&mut p, *x, *y, *w, *h, *r, false),
                Kind::Ellipse { cx, cy, rx, ry } => ellipse(&mut p, *cx, *cy, *rx, *ry, false),
                Kind::Poly { pts, .. } if pts.len() >= 3 => poly(&mut p, pts),
                _ => {}
            }
            if !p.verbs.is_empty() {
                out.push(p);
            }
        }
        if let Some((c, w)) = self.stroke {
            let h = w / 2.0;
            let mut p = Path { verbs: Vec::new(), points: Vec::new(), even_odd: true, paint: Fill::Solid(c), bbox };
            match &self.kind {
                Kind::Rect { x, y, w: rw, h: rh, r } => {
                    rect(&mut p, x - h, y - h, rw + w, rh + w, if *r > 0.0 { r + h } else { 0.0 }, false);
                    if *rw > w && *rh > w {
                        rect(&mut p, x + h, y + h, rw - w, rh - w, (r - h).max(0.0), true);
                    }
                }
                Kind::Ellipse { cx, cy, rx, ry } => {
                    ellipse(&mut p, *cx, *cy, rx + h, ry + h, false);
                    if *rx > h && *ry > h {
                        ellipse(&mut p, *cx, *cy, rx - h, ry - h, true);
                    }
                }
                Kind::Line { a, b } => {
                    p.even_odd = false;
                    segment(&mut p, *a, *b, h);
                }
                Kind::Poly { pts, closed } => {
                    // Each segment and a round joint at each point, all
                    // turning the same way, under the non-zero rule.
                    p.even_odd = false;
                    let n = pts.len();
                    let segs = if *closed && n >= 3 { n } else { n.saturating_sub(1) };
                    for i in 0..segs {
                        segment(&mut p, pts[i], pts[(i + 1) % n], h);
                    }
                    for &q in pts {
                        disc(&mut p, q, h);
                    }
                }
            }
            if !p.verbs.is_empty() {
                out.push(p);
            }
        }
        out
    }
}

fn mv(p: &mut Path, x: f32, y: f32) { p.verbs.push(MOVE); p.points.push([x, y]); }
fn ln(p: &mut Path, x: f32, y: f32) { p.verbs.push(LINE); p.points.push([x, y]); }
fn cu(p: &mut Path, a: [f32; 2], b: [f32; 2], c: [f32; 2]) { p.verbs.push(CUBIC); p.points.extend([a, b, c]); }
fn close(p: &mut Path) { p.verbs.push(CLOSE); }

/// A rectangle with corners of radius `r`; `rev` runs it the other way.
fn rect(p: &mut Path, x: f32, y: f32, w: f32, h: f32, r: f32, rev: bool) {
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    if r <= 0.0 {
        let c = [(x, y), (x + w, y), (x + w, y + h), (x, y + h)];
        let order: Vec<(f32, f32)> = if rev { c.iter().rev().copied().collect() } else { c.to_vec() };
        mv(p, order[0].0, order[0].1);
        for q in &order[1..] {
            ln(p, q.0, q.1);
        }
        close(p);
        return;
    }
    let k = r * (1.0 - K);
    if !rev {
        mv(p, x + r, y);
        ln(p, x + w - r, y);
        cu(p, [x + w - k, y], [x + w, y + k], [x + w, y + r]);
        ln(p, x + w, y + h - r);
        cu(p, [x + w, y + h - k], [x + w - k, y + h], [x + w - r, y + h]);
        ln(p, x + r, y + h);
        cu(p, [x + k, y + h], [x, y + h - k], [x, y + h - r]);
        ln(p, x, y + r);
        cu(p, [x, y + k], [x + k, y], [x + r, y]);
    } else {
        mv(p, x + r, y);
        cu(p, [x + k, y], [x, y + k], [x, y + r]);
        ln(p, x, y + h - r);
        cu(p, [x, y + h - k], [x + k, y + h], [x + r, y + h]);
        ln(p, x + w - r, y + h);
        cu(p, [x + w - k, y + h], [x + w, y + h - k], [x + w, y + h - r]);
        ln(p, x + w, y + r);
        cu(p, [x + w, y + k], [x + w - k, y], [x + w - r, y]);
    }
    close(p);
}

fn ellipse(p: &mut Path, cx: f32, cy: f32, rx: f32, ry: f32, rev: bool) {
    let (kx, ky) = (rx * K, ry * K);
    let s = if rev { -1.0 } else { 1.0 };
    mv(p, cx + rx, cy);
    cu(p, [cx + rx, cy + s * ky], [cx + kx, cy + s * ry], [cx, cy + s * ry]);
    cu(p, [cx - kx, cy + s * ry], [cx - rx, cy + s * ky], [cx - rx, cy]);
    cu(p, [cx - rx, cy - s * ky], [cx - kx, cy - s * ry], [cx, cy - s * ry]);
    cu(p, [cx + kx, cy - s * ry], [cx + rx, cy - s * ky], [cx + rx, cy]);
    close(p);
}

fn poly(p: &mut Path, pts: &[(f32, f32)]) {
    mv(p, pts[0].0, pts[0].1);
    for q in &pts[1..] {
        ln(p, q.0, q.1);
    }
    close(p);
}

/// The band `h` either side of a to b, turning clockwise on screen.
fn segment(p: &mut Path, a: (f32, f32), b: (f32, f32), h: f32) {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1e-4 {
        return;
    }
    let (nx, ny) = (-dy / len * h, dx / len * h);
    let q = [(a.0 + nx, a.1 + ny), (b.0 + nx, b.1 + ny), (b.0 - nx, b.1 - ny), (a.0 - nx, a.1 - ny)];
    let area: f32 = (0..4).map(|i| { let (u, v) = (q[i], q[(i + 1) % 4]); u.0 * v.1 - v.0 * u.1 }).sum();
    let order: Vec<(f32, f32)> = if area > 0.0 { q.to_vec() } else { q.iter().rev().copied().collect() };
    mv(p, order[0].0, order[0].1);
    for v in &order[1..] {
        ln(p, v.0, v.1);
    }
    close(p);
}

/// A round joint: an octagon of radius `h`, turning clockwise on screen.
fn disc(p: &mut Path, c: (f32, f32), h: f32) {
    for i in 0..8 {
        let t = i as f32 * std::f32::consts::FRAC_PI_4;
        let (x, y) = (c.0 + h * t.cos(), c.1 + h * t.sin());
        if i == 0 { mv(p, x, y) } else { ln(p, x, y) }
    }
    close(p);
}

/// The dock's count of an icon: shapes (paths) and points.
pub fn counts(shapes: &[Shape]) -> (usize, usize) {
    let paths: Vec<Path> = shapes.iter().flat_map(Shape::paths).collect();
    (paths.len(), paths.iter().map(|p| p.points.len()).sum())
}

fn hex(c: [u8; 4]) -> String {
    format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
}

fn hex8(c: [u8; 4]) -> String {
    format!("#{:02x}{:02x}{:02x}{:02x}", c[0], c[1], c[2], c[3])
}

fn num(v: f32) -> String {
    let s = format!("{:.3}", v);
    let s = s.trim_end_matches('0').trim_end_matches('.').to_string();
    if s == "-0" { "0".into() } else { s }
}

fn d_of(p: &Path) -> String {
    let mut d = String::new();
    let mut i = 0;
    for &v in &p.verbs {
        match v {
            MOVE => { d.push_str(&format!("M{} {}", num(p.points[i][0]), num(p.points[i][1]))); i += 1; }
            LINE => { d.push_str(&format!("L{} {}", num(p.points[i][0]), num(p.points[i][1]))); i += 1; }
            CUBIC => {
                d.push_str(&format!("C{} {} {} {} {} {}", num(p.points[i][0]), num(p.points[i][1]), num(p.points[i + 1][0]),
                    num(p.points[i + 1][1]), num(p.points[i + 2][0]), num(p.points[i + 2][1])));
                i += 3;
            }
            _ => d.push('Z'),
        }
    }
    d
}

fn fill_json(f: &Fill) -> serde_json::Value {
    match f {
        Fill::None => serde_json::json!({"t": "none"}),
        Fill::Solid(c) => serde_json::json!({"t": "solid", "c": hex8(*c)}),
        Fill::Linear(a, b) => serde_json::json!({"t": "linear", "c": hex8(*a), "d": hex8(*b)}),
        Fill::Radial(a, b) => serde_json::json!({"t": "radial", "c": hex8(*a), "d": hex8(*b)}),
    }
}

fn shape_json(s: &Shape) -> serde_json::Value {
    let mut v = match &s.kind {
        Kind::Rect { x, y, w, h, r } => serde_json::json!({"k": "rect", "x": x, "y": y, "w": w, "h": h, "r": r}),
        Kind::Ellipse { cx, cy, rx, ry } => serde_json::json!({"k": "ellipse", "cx": cx, "cy": cy, "rx": rx, "ry": ry}),
        Kind::Line { a, b } => serde_json::json!({"k": "line", "p": [a.0, a.1, b.0, b.1]}),
        Kind::Poly { pts, closed } => serde_json::json!({"k": "poly", "closed": closed,
            "p": pts.iter().flat_map(|p| [p.0, p.1]).collect::<Vec<f32>>()}),
    };
    v["fill"] = fill_json(&s.fill);
    if let Some((c, w)) = s.stroke {
        v["stroke"] = serde_json::json!({"c": hex8(c), "w": w});
    }
    v
}

/// The icon as an SVG the dock draws, or why it can't be one.
pub fn to_svg(shapes: &[Shape]) -> Result<String, String> {
    if shapes.is_empty() {
        return Err("draw something first".into());
    }
    let paths: Vec<Path> = shapes.iter().flat_map(Shape::paths).collect();
    if paths.is_empty() {
        return Err("nothing shows: give a shape a fill or a stroke".into());
    }
    if paths.len() > MAX_SHAPES {
        return Err(format!("{} shapes; the dock draws {MAX_SHAPES}", paths.len()));
    }
    let points: usize = paths.iter().map(|p| p.points.len()).sum();
    if points > MAX_POINTS {
        return Err(format!("{points} points; the dock draws {MAX_POINTS}"));
    }
    let doc = serde_json::json!({"shapes": shapes.iter().map(shape_json).collect::<Vec<_>>()}).to_string();
    if doc.contains("--") {
        return Err("the icon's description can't go in a comment".into());
    }
    let mut defs = String::new();
    let mut body = String::new();
    for (i, p) in paths.iter().enumerate() {
        let fill = match p.paint {
            Fill::None => continue,
            Fill::Solid(c) => {
                let a = if c[3] < 255 { format!(" fill-opacity=\"{}\"", num(c[3] as f32 / 255.0)) } else { String::new() };
                format!("fill=\"{}\"{a}", hex(c))
            }
            Fill::Linear(a, b) | Fill::Radial(a, b) => {
                let [x0, y0, x1, y1] = p.bbox;
                let stops = format!("<stop offset=\"0\" stop-color=\"{}\"{}/><stop offset=\"1\" stop-color=\"{}\"{}/>",
                    hex(a), if a[3] < 255 { format!(" stop-opacity=\"{}\"", num(a[3] as f32 / 255.0)) } else { String::new() },
                    hex(b), if b[3] < 255 { format!(" stop-opacity=\"{}\"", num(b[3] as f32 / 255.0)) } else { String::new() });
                if matches!(p.paint, Fill::Linear(..)) {
                    let cx = (x0 + x1) / 2.0;
                    defs.push_str(&format!("<linearGradient id=\"g{i}\" gradientUnits=\"userSpaceOnUse\" x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\">{stops}</linearGradient>",
                        num(cx), num(y0), num(cx), num(y1)));
                } else {
                    let r = ((x1 - x0).max(y1 - y0) / 2.0).max(0.01);
                    defs.push_str(&format!("<radialGradient id=\"g{i}\" gradientUnits=\"userSpaceOnUse\" cx=\"{}\" cy=\"{}\" r=\"{}\">{stops}</radialGradient>",
                        num((x0 + x1) / 2.0), num((y0 + y1) / 2.0), num(r)));
                }
                format!("fill=\"url(#g{i})\"")
            }
        };
        let rule = if p.even_odd { " fill-rule=\"evenodd\"" } else { "" };
        body.push_str(&format!("<path d=\"{}\" {fill}{rule}/>\n", d_of(p)));
    }
    let defs = if defs.is_empty() { String::new() } else { format!("<defs>{defs}</defs>\n") };
    let svg = format!("<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 24 24\" width=\"24\" height=\"24\">\n<!-- {MARK}{doc} -->\n{defs}{body}</svg>\n");
    if svg.len() > MAX_BYTES {
        return Err(format!("{} KB; the dock takes {} KB", svg.len() / 1024, MAX_BYTES / 1024));
    }
    Ok(svg)
}

fn color(v: &serde_json::Value) -> Option<[u8; 4]> {
    let s = v.as_str()?.strip_prefix('#')?;
    if s.len() != 8 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let b = |i: usize| u8::from_str_radix(&s[i..i + 2], 16).ok();
    Some([b(0)?, b(2)?, b(4)?, b(6)?])
}

/// A coordinate or size, within reach of the grid.
fn unit(v: &serde_json::Value) -> Option<f32> {
    let f = v.as_f64()? as f32;
    (f.is_finite() && (-GRID..=2.0 * GRID).contains(&f)).then_some(f)
}

fn fill_of(v: Option<&serde_json::Value>) -> Option<Fill> {
    let Some(v) = v else { return Some(Fill::None) };
    Some(match v.get("t")?.as_str()? {
        "none" => Fill::None,
        "solid" => Fill::Solid(color(v.get("c")?)?),
        "linear" => Fill::Linear(color(v.get("c")?)?, color(v.get("d")?)?),
        "radial" => Fill::Radial(color(v.get("c")?)?, color(v.get("d")?)?),
        _ => return None,
    })
}

fn shape_of(v: &serde_json::Value) -> Option<Shape> {
    let g = |k: &str| v.get(k).and_then(unit);
    let kind = match v.get("k")?.as_str()? {
        "rect" => Kind::Rect { x: g("x")?, y: g("y")?, w: g("w")?.max(0.0), h: g("h")?.max(0.0), r: g("r")?.max(0.0) },
        "ellipse" => Kind::Ellipse { cx: g("cx")?, cy: g("cy")?, rx: g("rx")?.max(0.0), ry: g("ry")?.max(0.0) },
        "line" => {
            let p = v.get("p")?.as_array()?;
            if p.len() != 4 {
                return None;
            }
            Kind::Line { a: (unit(&p[0])?, unit(&p[1])?), b: (unit(&p[2])?, unit(&p[3])?) }
        }
        "poly" => {
            let p = v.get("p")?.as_array()?;
            if p.len() % 2 != 0 || p.len() < 4 || p.len() > 2 * MAX_PEN {
                return None;
            }
            let pts = p.chunks(2).map(|c| Some((unit(&c[0])?, unit(&c[1])?))).collect::<Option<Vec<_>>>()?;
            Kind::Poly { pts, closed: v.get("closed")?.as_bool()? }
        }
        _ => return None,
    };
    let stroke = match v.get("stroke") {
        None => None,
        Some(s) => {
            let w = s.get("w").and_then(unit)?;
            (w > 0.0 && w <= 8.0).then_some(())?;
            Some((color(s.get("c")?)?, w))
        }
    };
    Some(Shape { kind, fill: fill_of(v.get("fill"))?, stroke })
}

/// The shapes of an icon this editor wrote; None for an SVG drawn
/// elsewhere (or one whose description doesn't hold together).
pub fn from_svg(svg: &str) -> Option<Vec<Shape>> {
    if svg.len() > MAX_BYTES {
        return None;
    }
    let start = svg.find(&format!("<!-- {MARK}"))? + 5 + MARK.len();
    let end = start + svg[start..].find(" -->")?;
    let v: serde_json::Value = serde_json::from_str(&svg[start..end]).ok()?;
    let list = v.get("shapes")?.as_array()?;
    if list.is_empty() || list.len() > MAX_SHAPES {
        return None;
    }
    list.iter().map(shape_of).collect()
}

/// A colour a step lighter or darker.
pub fn shade(c: [u8; 4], lighter: bool) -> [u8; 4] {
    let f = |v: u8| -> u8 {
        let v = v as f32;
        (if lighter { v + (255.0 - v) * 0.25 } else { v * 0.75 }).round().clamp(0.0, 255.0) as u8
    };
    [f(c[0]), f(c[1]), f(c[2]), c[3]]
}

/// `shapes` as Fresco fills placed in the square `(x, y, side)`: the
/// canvas and the previews draw what the SVG says, through the same paths.
pub fn fills(shapes: &[Shape], x: f32, y: f32, side: f32) -> Vec<fresco_protocol::FillParams> {
    use fresco_protocol::{FillPaint, FillParams, GradientStop};
    // Fresco takes solid paint in linear light, gradient stops in sRGB.
    fn lin(c: u8) -> f32 {
        let c = c as f32 / 255.0;
        if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    }
    fn stop(o: f32, c: [u8; 4]) -> GradientStop {
        GradientStop { offset: o, r: c[0] as f32 / 255.0, g: c[1] as f32 / 255.0, b: c[2] as f32 / 255.0, a: c[3] as f32 / 255.0 }
    }
    let k = side / GRID;
    let mut out = Vec::new();
    for p in shapes.iter().flat_map(Shape::paths) {
        let [x0, y0, x1, y1] = p.bbox;
        let paint = match p.paint {
            Fill::None => continue,
            Fill::Solid(c) => FillPaint::Solid { r: lin(c[0]), g: lin(c[1]), b: lin(c[2]), a: c[3] as f32 / 255.0 },
            Fill::Linear(a, b) => {
                let cx = (x0 + x1) / 2.0;
                FillPaint::Linear { x0: cx, y0, x1: cx, y1, stops: vec![stop(0.0, a), stop(1.0, b)] }
            }
            Fill::Radial(a, b) => FillPaint::Radial { cx: (x0 + x1) / 2.0, cy: (y0 + y1) / 2.0,
                radius: ((x1 - x0).max(y1 - y0) / 2.0).max(0.01), stops: vec![stop(0.0, a), stop(1.0, b)] },
        };
        out.push(FillParams { rule: u8::from(p.even_odd), transform: [k, 0.0, 0.0, k, x, y], paint, opacity: 1.0,
            clip: Some([x, y, x + side, y + side]), verbs: p.verbs, points: p.points });
    }
    out
}

/// The editor: the shapes, what's chosen and what's being drawn.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Editor {
    /// The icon's file in the project (`assets/icons/<name>.svg`).
    pub rel: String,
    pub shapes: Vec<Shape>,
    pub tool: Tool,
    pub selected: Option<usize>,
    /// What a new shape gets.
    pub fill: Fill,
    pub stroke: Option<([u8; 4], f32)>,
    /// The icon's colours outside the palette.
    pub own: Vec<[u8; 4]>,
    /// A shape being drawn (not yet in `shapes`).
    pub drawing: Option<Shape>,
    /// Where a drag started, and the shape as it was (moving).
    drag: Option<((f32, f32), Option<Shape>)>,
    /// The pen's points so far.
    pub pen: Vec<(f32, f32)>,
    pub changed: bool,
    /// The last thing refused, said to the person.
    pub note: Option<String>,
    /// What a colour picked goes to: 0 the fill, 1 a gradient's second
    /// colour, 2 the stroke.
    pub target: u8,
}

impl Editor {
    pub fn new(rel: &str) -> Editor {
        Editor { rel: rel.into(), fill: Fill::Solid(PALETTE[8]), ..Default::default() }
    }

    /// An editor on an icon this editor wrote.
    pub fn open(rel: &str, svg: &str) -> Option<Editor> {
        let shapes = from_svg(svg)?;
        let mut e = Editor::new(rel);
        for s in &shapes {
            let mut cs = Vec::new();
            match s.fill {
                Fill::Solid(c) => cs.push(c),
                Fill::Linear(a, b) | Fill::Radial(a, b) => cs.extend([a, b]),
                Fill::None => {}
            }
            cs.extend(s.stroke.map(|s| s.0));
            for c in cs {
                if !PALETTE.contains(&c) && !e.own.contains(&c) && e.own.len() < MAX_OWN {
                    e.own.push(c);
                }
            }
        }
        e.shapes = shapes;
        Some(e)
    }

    pub fn counts(&self) -> (usize, usize) {
        counts(&self.shapes)
    }

    /// Would adding `s` stay within the dock's limits?
    fn room_for(&mut self, s: &Shape) -> bool {
        let (n, pts) = counts(std::slice::from_ref(s));
        let (have, have_pts) = self.counts();
        if have + n > MAX_SHAPES || have_pts + pts > MAX_POINTS {
            self.note = Some(format!("The dock draws {MAX_SHAPES} shapes: this one doesn't fit"));
            return false;
        }
        true
    }

    fn add(&mut self, s: Shape) {
        if self.room_for(&s) {
            self.shapes.push(s);
            self.selected = Some(self.shapes.len() - 1);
            self.changed = true;
        }
    }

    pub fn set_tool(&mut self, t: Tool) {
        self.finish_pen();
        self.tool = t;
        self.drawing = None;
        self.drag = None;
    }

    /// A press, move or release on the canvas at (x, y) in units, snapped.
    pub fn pointer(&mut self, phase: u8, x: f32, y: f32) {
        let (x, y) = (x.clamp(0.0, GRID), y.clamp(0.0, GRID));
        self.note = None;
        match (self.tool, phase) {
            (Tool::Select, 0) => {
                self.selected = self.shapes.iter().rposition(|s| s.hit(x, y));
                self.drag = self.selected.map(|i| ((x, y), Some(self.shapes[i].clone())));
            }
            (Tool::Select, _) => {
                if let (Some(((x0, y0), Some(orig))), Some(i)) = (&self.drag, self.selected) {
                    let moved = orig.moved(x - x0, y - y0);
                    if self.shapes[i] != moved {
                        self.shapes[i] = moved;
                        self.changed = true;
                    }
                }
                if phase == 2 {
                    self.drag = None;
                }
            }
            (Tool::Pen, 0) => {
                // A press on the first point closes the shape.
                if self.pen.len() >= 3 && (self.pen[0].0 - x).abs() < 0.75 && (self.pen[0].1 - y).abs() < 0.75 {
                    self.close_pen(true);
                } else if self.pen.last() != Some(&(x, y)) {
                    if self.pen.len() >= MAX_PEN {
                        self.note = Some(format!("A pen shape takes {MAX_PEN} points"));
                    } else {
                        self.pen.push((x, y));
                    }
                }
            }
            (Tool::Pen, _) => {}
            (t, 0) => {
                self.drag = Some(((x, y), None));
                self.drawing = Some(self.shaped(t, (x, y), (x, y)));
            }
            (t, _) => {
                let Some(((x0, y0), _)) = self.drag else { return };
                let s = self.shaped(t, (x0, y0), (x, y));
                if phase == 2 {
                    self.drag = None;
                    self.drawing = None;
                    let b = s.bbox();
                    // A press without a drag draws nothing.
                    if b[2] - b[0] >= 0.5 || b[3] - b[1] >= 0.5 {
                        self.add(s);
                    }
                } else {
                    self.drawing = Some(s);
                }
            }
        }
    }

    fn shaped(&self, t: Tool, a: (f32, f32), b: (f32, f32)) -> Shape {
        let (x, y, w, h) = (a.0.min(b.0), a.1.min(b.1), (a.0 - b.0).abs(), (a.1 - b.1).abs());
        let kind = match t {
            Tool::Ellipse => Kind::Ellipse { cx: x + w / 2.0, cy: y + h / 2.0, rx: w / 2.0, ry: h / 2.0 },
            Tool::Line => Kind::Line { a, b },
            _ => Kind::Rect { x, y, w, h, r: 0.0 },
        };
        // A line is its stroke: one is given it if the style has none.
        let stroke = if t == Tool::Line {
            Some(self.stroke.unwrap_or(match self.fill { Fill::Solid(c) | Fill::Linear(c, _) | Fill::Radial(c, _) => (c, 2.0), Fill::None => (PALETTE[1], 2.0) }))
        } else {
            self.stroke
        };
        Shape { kind, fill: self.fill, stroke }
    }

    fn close_pen(&mut self, closed: bool) {
        let pts = std::mem::take(&mut self.pen);
        if pts.len() < 2 || (closed && pts.len() < 3) {
            return;
        }
        let closed = closed || (self.stroke.is_none() && pts.len() >= 3);
        let stroke = if !closed && self.stroke.is_none() { Some((PALETTE[1], 2.0)) } else { self.stroke };
        self.add(Shape { kind: Kind::Poly { pts, closed }, fill: if closed { self.fill } else { Fill::None }, stroke });
    }

    /// Finish the pen's shape as it is (open, or closed when it has no stroke).
    pub fn finish_pen(&mut self) {
        if !self.pen.is_empty() {
            self.close_pen(false);
        }
    }

    /// The style chosen: given to the selected shape, and to new ones.
    pub fn set_fill(&mut self, f: Fill) {
        self.fill = f;
        if let Some(s) = self.selected.and_then(|i| self.shapes.get_mut(i)) {
            if !matches!(s.kind, Kind::Line { .. }) && s.fill != f {
                s.fill = f;
                self.changed = true;
            }
        }
    }

    pub fn set_stroke(&mut self, st: Option<([u8; 4], f32)>) {
        self.stroke = st;
        if let Some(i) = self.selected {
            let line = matches!(self.shapes[i].kind, Kind::Line { .. });
            // A line keeps a stroke: it is nothing without one.
            if !(line && st.is_none()) && self.shapes[i].stroke != st {
                let mut s = self.shapes[i].clone();
                s.stroke = st;
                let old = std::mem::replace(&mut self.shapes[i], Shape { kind: Kind::Line { a: (0.0, 0.0), b: (0.0, 0.0) }, fill: Fill::None, stroke: None });
                if self.room_for(&s) {
                    self.shapes[i] = s;
                    self.changed = true;
                } else {
                    self.shapes[i] = old;
                }
            }
        }
    }

    /// A colour picked: the fill's (its first colour for a gradient), or
    /// with `second`, a gradient's second.
    pub fn pick(&mut self, c: [u8; 4], second: bool) {
        let f = match (self.fill, second) {
            (Fill::Linear(a, _), true) => Fill::Linear(a, c),
            (Fill::Radial(a, _), true) => Fill::Radial(a, c),
            (Fill::Linear(_, b), false) => Fill::Linear(c, b),
            (Fill::Radial(_, b), false) => Fill::Radial(c, b),
            _ => Fill::Solid(c),
        };
        self.set_fill(f);
    }

    /// The fill's colour a step lighter or darker, kept among the icon's own.
    pub fn shade(&mut self, lighter: bool) {
        let c = match self.fill {
            Fill::Solid(c) | Fill::Linear(c, _) | Fill::Radial(c, _) => c,
            Fill::None => return,
        };
        let n = shade(c, lighter);
        if !PALETTE.contains(&n) && !self.own.contains(&n) {
            if self.own.len() >= MAX_OWN {
                self.note = Some(format!("An icon keeps {MAX_OWN} colours of its own"));
                return;
            }
            self.own.push(n);
        }
        self.pick(n, false);
    }

    /// Fill kinds: 0 none, 1 colour, 2 linear, 3 radial.
    pub fn fill_kind(&mut self, k: u8) {
        let (a, b) = match self.fill {
            Fill::Solid(c) => (c, shade(c, false)),
            Fill::Linear(a, b) | Fill::Radial(a, b) => (a, b),
            Fill::None => (PALETTE[8], shade(PALETTE[8], false)),
        };
        self.set_fill(match k {
            0 => Fill::None,
            2 => Fill::Linear(a, b),
            3 => Fill::Radial(a, b),
            _ => Fill::Solid(a),
        });
    }

    /// The colours offered: the palette, then the icon's own.
    pub fn colours(&self) -> Vec<[u8; 4]> {
        PALETTE.iter().chain(self.own.iter()).copied().collect()
    }

    /// The `i`th of [`Editor::colours`] given to what [`Editor::target`] names.
    /// A selected line's colour is its stroke's.
    pub fn colour(&mut self, i: usize) {
        let Some(c) = self.colours().get(i).copied() else { return };
        let line = self.selected.and_then(|i| self.shapes.get(i)).is_some_and(|s| matches!(s.kind, Kind::Line { .. }));
        match self.target {
            2 => self.set_stroke(Some((c, self.stroke.map_or(2.0, |s| s.1)))),
            _ if line => self.set_stroke(Some((c, self.stroke.map_or(2.0, |s| s.1)))),
            t => self.pick(c, t == 1),
        }
    }

    /// The stroke: 0 none, else the `w`th of [`WIDTHS`] (from 1).
    pub fn stroke_width(&mut self, w: u8) {
        let st = match WIDTHS.get(usize::from(w).wrapping_sub(1)) {
            Some(&width) => Some((self.stroke.map_or(PALETTE[1], |s| s.0), width)),
            None => None,
        };
        self.set_stroke(st);
    }

    pub fn select(&mut self, i: usize) {
        if i < self.shapes.len() {
            self.selected = Some(i);
            let s = &self.shapes[i];
            self.fill = if matches!(s.kind, Kind::Line { .. }) { self.fill } else { s.fill };
            self.stroke = s.stroke;
        }
    }

    /// Move the selected shape up (towards the front) or down the list.
    pub fn reorder(&mut self, front: bool) {
        let Some(i) = self.selected else { return };
        let j = if front { i + 1 } else { i.wrapping_sub(1) };
        if j < self.shapes.len() {
            self.shapes.swap(i, j);
            self.selected = Some(j);
            self.changed = true;
        }
    }

    pub fn delete(&mut self) {
        if let Some(i) = self.selected.filter(|&i| i < self.shapes.len()) {
            self.shapes.remove(i);
            self.selected = None;
            self.changed = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drag(e: &mut Editor, a: (f32, f32), b: (f32, f32)) {
        e.pointer(0, a.0, a.1);
        e.pointer(1, (a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0);
        e.pointer(2, b.0, b.1);
    }

    #[test]
    fn draws_moves_styles_and_orders() {
        let mut e = Editor::new("assets/icons/app.svg");
        e.set_tool(Tool::Rect);
        drag(&mut e, (2.0, 2.0), (22.0, 22.0));
        assert_eq!(e.shapes[0].kind, Kind::Rect { x: 2.0, y: 2.0, w: 20.0, h: 20.0, r: 0.0 });
        e.set_tool(Tool::Ellipse);
        e.pick(PALETTE[0], false);
        drag(&mut e, (7.0, 7.0), (17.0, 17.0));
        assert_eq!(e.shapes[1].kind, Kind::Ellipse { cx: 12.0, cy: 12.0, rx: 5.0, ry: 5.0 });
        assert_eq!(e.shapes[1].fill, Fill::Solid(PALETTE[0]));
        // A press without a drag draws nothing.
        e.pointer(0, 3.0, 3.0);
        e.pointer(2, 3.0, 3.0);
        assert_eq!(e.shapes.len(), 2);
        // Select the ellipse (on top), move it by 1.
        e.set_tool(Tool::Select);
        drag(&mut e, (12.0, 12.0), (13.0, 12.0));
        assert_eq!(e.selected, Some(1));
        assert_eq!(e.shapes[1].kind, Kind::Ellipse { cx: 13.0, cy: 12.0, rx: 5.0, ry: 5.0 });
        // A stroke and a radial gradient on it; then it goes behind.
        e.set_stroke(Some((PALETTE[1], 1.5)));
        e.fill_kind(3);
        assert!(matches!(e.shapes[1].fill, Fill::Radial(..)) && e.shapes[1].stroke == Some((PALETTE[1], 1.5)));
        e.reorder(false);
        assert_eq!((e.selected, matches!(e.shapes[0].kind, Kind::Ellipse { .. })), (Some(0), true));
        assert_eq!(e.counts().0, 3, "a rectangle, and an ellipse's fill and its stroke");
        e.delete();
        assert_eq!(e.shapes.len(), 1);
    }

    #[test]
    fn the_pen_closes_on_its_first_point() {
        let mut e = Editor::new("x");
        e.set_tool(Tool::Pen);
        for p in [(4.0, 20.0), (12.0, 4.0), (20.0, 20.0), (4.0, 20.0)] {
            e.pointer(0, p.0, p.1);
            e.pointer(2, p.0, p.1);
        }
        assert_eq!(e.shapes.len(), 1);
        assert!(matches!(&e.shapes[0].kind, Kind::Poly { pts, closed: true } if pts.len() == 3));
        // An open line when finished early with a stroke.
        e.set_stroke(Some((PALETTE[3], 2.0)));
        e.selected = None;
        for p in [(2.0, 2.0), (6.0, 2.0)] {
            e.pointer(0, p.0, p.1);
        }
        e.set_tool(Tool::Select);
        assert!(matches!(&e.shapes[1].kind, Kind::Poly { closed: false, .. }));
        assert_eq!(e.shapes[1].fill, Fill::None);
    }

    #[test]
    fn writes_what_it_reads_back() {
        let mut e = Editor::new("x");
        e.set_tool(Tool::Rect);
        e.fill_kind(2);
        drag(&mut e, (2.0, 2.0), (22.0, 22.0));
        e.set_tool(Tool::Line);
        e.set_stroke(Some(([0x12, 0x34, 0x56, 0x80], 2.0)));
        drag(&mut e, (6.0, 18.0), (18.0, 6.0));
        let svg = to_svg(&e.shapes).unwrap();
        assert!(svg.contains("<linearGradient") && svg.contains("fill-opacity=\"0.502\""), "{svg}");
        let back = Editor::open("x", &svg).unwrap();
        assert_eq!(back.shapes, e.shapes);
        // The gradient's second stop is a shade of a palette colour, so it's the icon's own.
        assert_eq!(back.own, vec![shade(PALETTE[8], false), [0x12, 0x34, 0x56, 0x80]]);
        // Drawn elsewhere, or a description that doesn't hold together.
        assert!(from_svg("<svg xmlns=\"http://www.w3.org/2000/svg\"><path d=\"M0 0L1 1Z\"/></svg>").is_none());
        assert!(from_svg(&svg.replace("\"x\":2", "\"x\":1e30")).is_none());
        assert!(from_svg(&svg.replace("\"k\":\"rect\"", "\"k\":\"text\"")).is_none());
    }

    #[test]
    fn the_dock_limits_hold() {
        let mut e = Editor::new("x");
        e.set_tool(Tool::Rect);
        for i in 0..70 {
            let x = (i % 10) as f32 * 2.0;
            let y = (i / 10) as f32 * 3.0;
            drag(&mut e, (x, y), (x + 1.5, y + 2.0));
        }
        assert_eq!(e.counts().0, MAX_SHAPES);
        assert!(e.note.as_deref().unwrap_or("").contains("64"));
        assert!(to_svg(&e.shapes).is_ok());
        let mut too_many = e.shapes.clone();
        too_many.push(too_many[0].clone());
        assert!(to_svg(&too_many).unwrap_err().contains("64"));
        assert!(to_svg(&[]).is_err());
    }

    #[test]
    fn own_colours_are_bounded() {
        let mut e = Editor::new("x");
        e.pick([0x10, 0x80, 0x40, 0xff], false);
        for _ in 0..20 {
            e.shade(true);
        }
        assert!(e.own.len() <= MAX_OWN);
        assert!(e.note.is_some() || e.own.len() < MAX_OWN);
    }

    #[test]
    fn the_dock_draws_what_it_writes() {
        let mut e = Editor::new("x");
        e.set_tool(Tool::Rect);
        e.fill_kind(1);
        drag(&mut e, (2.0, 2.0), (22.0, 22.0));
        e.set_tool(Tool::Ellipse);
        e.fill_kind(3);
        e.set_stroke(Some((PALETTE[1], 1.5)));
        drag(&mut e, (6.0, 6.0), (18.0, 18.0));
        e.set_tool(Tool::Line);
        drag(&mut e, (4.0, 20.0), (20.0, 4.0));
        e.set_stroke(Some((PALETTE[2], 2.0)));
        e.set_tool(Tool::Pen);
        for p in [(3.0, 3.0), (9.0, 5.0), (5.0, 9.0)] {
            e.pointer(0, p.0, p.1);
        }
        e.set_tool(Tool::Select);
        let svg = to_svg(&e.shapes).unwrap();
        let r = atrium_icon::check(svg.as_bytes());
        assert!(r.ok(), "{r:?}\n{svg}");
        let (paths, points) = e.counts();
        assert_eq!(r.shapes, paths, "{svg}");
        assert_eq!(r.points, points, "{svg}");
    }

    #[test]
    fn colours_go_where_aimed() {
        let mut e = Editor::new("x");
        e.set_tool(Tool::Rect);
        drag(&mut e, (2.0, 2.0), (12.0, 12.0));
        e.colour(3);
        assert_eq!(e.shapes[0].fill, Fill::Solid(PALETTE[3]));
        e.fill_kind(2);
        e.target = 1;
        e.colour(5);
        assert_eq!(e.shapes[0].fill, Fill::Linear(PALETTE[3], PALETTE[5]));
        e.stroke_width(4);
        assert_eq!(e.shapes[0].stroke, Some((PALETTE[1], 3.0)));
        e.target = 2;
        e.colour(0);
        assert_eq!(e.shapes[0].stroke, Some((PALETTE[0], 3.0)));
        e.stroke_width(0);
        assert_eq!(e.shapes[0].stroke, None);
        // Past the palette and the icon's own: nothing.
        e.colour(99);
        assert_eq!(e.shapes[0].stroke, None);
        // A line's colour is its stroke's, whatever is aimed at.
        e.set_tool(Tool::Line);
        e.target = 0;
        drag(&mut e, (2.0, 20.0), (20.0, 20.0));
        e.colour(6);
        assert_eq!(e.shapes[1].stroke.map(|s| s.0), Some(PALETTE[6]));
        // The canvas draws every path that paints.
        let f = fills(&e.shapes, 10.0, 20.0, 240.0);
        assert_eq!(f.len(), e.shapes.iter().flat_map(Shape::paths).filter(|p| p.paint != Fill::None).count());
        assert!(f.iter().all(|f| f.paint.validate().is_ok() && f.transform == [10.0, 0.0, 0.0, 10.0, 10.0, 20.0]));
    }
}
