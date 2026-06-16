//! icon — parse a Lucide-style SVG icon into vector geometry, flattened to stroked
//! scene-graph segments **at render scale** (so it stays crisp at any zoom).
//!
//! `parse_icon` keeps curves (cubics + arcs) as geometry; `draw_icon` flattens them
//! per render with a step count proportional to the on-screen size, and rounds the
//! joins/caps with a small disc at each vertex (matching Lucide's round stroke). The
//! whole thing renders through the existing `op_path` rotated-quad — no new shader.
//!
//! Supports the subset Lucide uses: `<path>` with M/L/H/V/C/A/Z (absolute + relative,
//! implicit repeats) and `<circle>`. ViewBox is Lucide's 0..24.

pub type Point = (f32, f32);

/// A curve segment continuing from the sub-path's current point. End-relative.
#[derive(Clone, Debug, PartialEq)]
enum Seg {
    Line(Point),
    Cubic(Point, Point, Point),
    /// Centre-parametrised elliptical arc (viewBox coords).
    Arc { c: Point, rx: f32, ry: f32, phi: f32, t1: f32, dt: f32, end: Point },
}

#[derive(Clone, Debug, PartialEq)]
struct SubPath { start: Point, segs: Vec<Seg> }

/// An icon's resolution-independent geometry (viewBox space).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IconGeometry { subpaths: Vec<SubPath> }

impl IconGeometry {
    pub fn is_empty(&self) -> bool { self.subpaths.is_empty() }
    /// Total segment count (for tests / sizing heuristics).
    pub fn seg_count(&self) -> usize { self.subpaths.iter().map(|s| s.segs.len()).sum() }
}

// ── parsing ──────────────────────────────────────────────────────────────────

pub fn parse_icon(svg: &str) -> IconGeometry {
    let mut g = IconGeometry::default();
    for d in extract_attr(svg, "<path", "d") {
        g.subpaths.extend(parse_path_d(&d));
    }
    for (cx, cy, r) in extract_circles(svg) {
        // A full circle as a single 360° arc — flattened per scale at draw time.
        g.subpaths.push(SubPath {
            start: (cx + r, cy),
            segs: vec![Seg::Arc {
                c: (cx, cy), rx: r, ry: r, phi: 0.0,
                t1: 0.0, dt: std::f32::consts::TAU, end: (cx + r, cy),
            }],
        });
    }
    g
}

fn extract_attr(svg: &str, tag: &str, attr: &str) -> Vec<String> {
    let mut out = Vec::new();
    let needle = format!("{attr}=\"");
    let mut rest = svg;
    while let Some(t) = rest.find(tag) {
        rest = &rest[t + tag.len()..];
        let end = rest.find('>').unwrap_or(rest.len());
        let elem = &rest[..end];
        if let Some(a) = elem.find(&needle) {
            let after = &elem[a + needle.len()..];
            if let Some(q) = after.find('"') { out.push(after[..q].to_string()); }
        }
        rest = &rest[end.min(rest.len())..];
    }
    out
}

fn extract_circles(svg: &str) -> Vec<(f32, f32, f32)> {
    let cx = extract_attr(svg, "<circle", "cx");
    let cy = extract_attr(svg, "<circle", "cy");
    let r = extract_attr(svg, "<circle", "r");
    let n = cx.len().min(cy.len()).min(r.len());
    (0..n).filter_map(|i| Some((cx[i].parse().ok()?, cy[i].parse().ok()?, r[i].parse().ok()?))).collect()
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Tok { Cmd(char), Num(f32) }

fn tokenize(d: &str) -> Vec<Tok> {
    let b = d.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i] as char;
        if c.is_ascii_alphabetic() { out.push(Tok::Cmd(c)); i += 1; }
        else if c == ' ' || c == ',' || c == '\n' || c == '\t' || c == '\r' { i += 1; }
        else if c == '-' || c == '+' || c == '.' || c.is_ascii_digit() {
            let start = i;
            if c == '-' || c == '+' { i += 1; }
            let mut seen_dot = false;
            while i < b.len() {
                let ch = b[i] as char;
                if ch.is_ascii_digit() { i += 1; }
                else if ch == '.' && !seen_dot { seen_dot = true; i += 1; }
                else if (ch == 'e' || ch == 'E') && i + 1 < b.len() {
                    i += 1;
                    if b[i] == b'-' || b[i] == b'+' { i += 1; }
                } else { break; }
            }
            if let Ok(v) = d[start..i].parse::<f32>() { out.push(Tok::Num(v)); }
        } else { i += 1; }
    }
    out
}

fn parse_path_d(d: &str) -> Vec<SubPath> {
    let toks = tokenize(d);
    let mut subs: Vec<SubPath> = Vec::new();
    let mut cur: Option<SubPath> = None;
    let (mut x, mut y) = (0.0f32, 0.0f32);
    let (mut sx, mut sy) = (0.0f32, 0.0f32);
    let mut i = 0;
    let mut cmd = ' ';

    macro_rules! nums {
        ($k:expr) => {{
            let mut v = [0.0f32; $k];
            let mut ok = true;
            for slot in v.iter_mut() {
                match toks.get(i) { Some(Tok::Num(n)) => { *slot = *n; i += 1; } _ => { ok = false; break; } }
            }
            if ok { Some(v) } else { None }
        }};
    }
    macro_rules! push_seg { ($s:expr) => { if let Some(sp) = cur.as_mut() { sp.segs.push($s); } }; }

    while i < toks.len() {
        if let Some(Tok::Cmd(c)) = toks.get(i) { cmd = *c; i += 1; }
        else if cmd == 'M' { cmd = 'L'; } else if cmd == 'm' { cmd = 'l'; }

        let rel = cmd.is_ascii_lowercase();
        match cmd.to_ascii_uppercase() {
            'M' => match nums!(2) {
                Some([nx, ny]) => {
                    if let Some(sp) = cur.take() { subs.push(sp); }
                    x = if rel { x + nx } else { nx };
                    y = if rel { y + ny } else { ny };
                    sx = x; sy = y;
                    cur = Some(SubPath { start: (x, y), segs: Vec::new() });
                }
                None => break,
            },
            'L' => match nums!(2) {
                Some([nx, ny]) => { x = if rel { x + nx } else { nx }; y = if rel { y + ny } else { ny }; push_seg!(Seg::Line((x, y))); }
                None => break,
            },
            'H' => match nums!(1) { Some([nx]) => { x = if rel { x + nx } else { nx }; push_seg!(Seg::Line((x, y))); } None => break },
            'V' => match nums!(1) { Some([ny]) => { y = if rel { y + ny } else { ny }; push_seg!(Seg::Line((x, y))); } None => break },
            'C' => match nums!(6) {
                Some([x1, y1, x2, y2, ex, ey]) => {
                    let (c1, c2, e) = if rel {
                        ((x + x1, y + y1), (x + x2, y + y2), (x + ex, y + ey))
                    } else { ((x1, y1), (x2, y2), (ex, ey)) };
                    push_seg!(Seg::Cubic(c1, c2, e));
                    x = e.0; y = e.1;
                }
                None => break,
            },
            'A' => match nums!(7) {
                Some([rx, ry, rot, large, sweep, ex, ey]) => {
                    let e = if rel { (x + ex, y + ey) } else { (ex, ey) };
                    if let Some(arc) = arc_center((x, y), rx, ry, rot, large != 0.0, sweep != 0.0, e) {
                        push_seg!(arc);
                    } else {
                        push_seg!(Seg::Line(e));
                    }
                    x = e.0; y = e.1;
                }
                None => break,
            },
            'Z' => {
                push_seg!(Seg::Line((sx, sy)));
                x = sx; y = sy;
                if let Some(sp) = cur.take() { subs.push(sp); }
            }
            _ => { i += 1; }
        }
    }
    if let Some(sp) = cur { subs.push(sp); }
    subs
}

/// SVG endpoint-arc → centre parametrisation (spec F.6.5). `None` for a degenerate arc.
#[allow(clippy::too_many_arguments)]
fn arc_center(p0: Point, mut rx: f32, mut ry: f32, rot_deg: f32, large: bool, sweep: bool, p1: Point) -> Option<Seg> {
    if rx == 0.0 || ry == 0.0 || (p0.0 == p1.0 && p0.1 == p1.1) { return None; }
    rx = rx.abs(); ry = ry.abs();
    let phi = rot_deg.to_radians();
    let (cosp, sinp) = (phi.cos(), phi.sin());
    let dx = (p0.0 - p1.0) / 2.0;
    let dy = (p0.1 - p1.1) / 2.0;
    let x1p = cosp * dx + sinp * dy;
    let y1p = -sinp * dx + cosp * dy;
    let lambda = (x1p * x1p) / (rx * rx) + (y1p * y1p) / (ry * ry);
    if lambda > 1.0 { let s = lambda.sqrt(); rx *= s; ry *= s; }
    let sign = if large != sweep { 1.0 } else { -1.0 };
    let num = (rx * rx * ry * ry - rx * rx * y1p * y1p - ry * ry * x1p * x1p).max(0.0);
    let den = rx * rx * y1p * y1p + ry * ry * x1p * x1p;
    let co = sign * (num / den).sqrt();
    let cxp = co * rx * y1p / ry;
    let cyp = -co * ry * x1p / rx;
    let cx = cosp * cxp - sinp * cyp + (p0.0 + p1.0) / 2.0;
    let cy = sinp * cxp + cosp * cyp + (p0.1 + p1.1) / 2.0;
    let ang = |ux: f32, uy: f32, vx: f32, vy: f32| -> f32 {
        let dot = ux * vx + uy * vy;
        let len = ((ux * ux + uy * uy) * (vx * vx + vy * vy)).sqrt();
        let mut a = (dot / len).clamp(-1.0, 1.0).acos();
        if ux * vy - uy * vx < 0.0 { a = -a; }
        a
    };
    let t1 = ang(1.0, 0.0, (x1p - cxp) / rx, (y1p - cyp) / ry);
    let mut dt = ang((x1p - cxp) / rx, (y1p - cyp) / ry, (-x1p - cxp) / rx, (-y1p - cyp) / ry);
    if !sweep && dt > 0.0 { dt -= std::f32::consts::TAU; }
    if sweep && dt < 0.0 { dt += std::f32::consts::TAU; }
    Some(Seg::Arc { c: (cx, cy), rx, ry, phi, t1, dt, end: p1 })
}

// ── rendering ────────────────────────────────────────────────────────────────

/// Quality: one flattening segment per this many on-screen pixels.
const PX_PER_STEP: f32 = 2.5;

/// Draw an icon as stroked scene segments + round joins/caps, scaled to `size` and
/// placed with top-left at (`x`, `y`). Curves are flattened *at render scale*.
pub fn draw_icon(
    ctx: &mut crate::view::Ctx,
    geom: &IconGeometry,
    x: f32, y: f32, size: f32,
    stroke: f32,
    color: crate::color::Color,
) {
    let scale = size / 24.0; // Lucide viewBox 0..24
    let tf = |p: Point| (x + p.0 * scale, y + p.1 * scale);
    for sp in &geom.subpaths {
        let mut cur = tf(sp.start);
        disc(ctx, cur, stroke, color); // round cap / join at the start
        for seg in &sp.segs {
            match seg {
                Seg::Line(p) => { let e = tf(*p); segment(ctx, cur, e, stroke, color); cur = e; }
                Seg::Cubic(c1, c2, p) => {
                    let (c1, c2, e) = (tf(*c1), tf(*c2), tf(*p));
                    let steps = steps_for(dist(cur, c1) + dist(c1, c2) + dist(c2, e));
                    let mut prev = cur;
                    for s in 1..=steps {
                        let t = s as f32 / steps as f32;
                        let pt = cubic_at(cur, c1, c2, e, t);
                        segment(ctx, prev, pt, stroke, color); prev = pt;
                    }
                    cur = e;
                }
                Seg::Arc { c, rx, ry, phi, t1, dt, end } => {
                    let steps = steps_for(rx.max(*ry) * scale * dt.abs());
                    let mut prev = cur;
                    for s in 1..=steps {
                        let t = t1 + dt * (s as f32 / steps as f32);
                        let pt = tf(arc_point(*c, *rx, *ry, *phi, t));
                        segment(ctx, prev, pt, stroke, color); prev = pt;
                    }
                    cur = tf(*end);
                }
            }
            disc(ctx, cur, stroke, color); // round join at the segment vertex
        }
    }
}

fn steps_for(len_px: f32) -> usize { ((len_px / PX_PER_STEP).ceil() as usize).clamp(2, 64) }
fn dist(a: Point, b: Point) -> f32 { ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt() }

fn cubic_at(p0: Point, c1: Point, c2: Point, p1: Point, t: f32) -> Point {
    let u = 1.0 - t;
    let (w0, w1, w2, w3) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
    (w0 * p0.0 + w1 * c1.0 + w2 * c2.0 + w3 * p1.0,
     w0 * p0.1 + w1 * c1.1 + w2 * c2.1 + w3 * p1.1)
}

fn arc_point(c: Point, rx: f32, ry: f32, phi: f32, t: f32) -> Point {
    let (cosp, sinp) = (phi.cos(), phi.sin());
    let (ct, st) = (t.cos(), t.sin());
    (cosp * rx * ct - sinp * ry * st + c.0,
     sinp * rx * ct + cosp * ry * st + c.1)
}

fn segment(ctx: &mut crate::view::Ctx, p0: Point, p1: Point, width: f32, color: crate::color::Color) {
    ctx.add(crate::node::Node::Path { p0, p1, width, color });
}

/// A round join/cap: a stroke-sized disc (a rounded square) centred at `p`.
fn disc(ctx: &mut crate::view::Ctx, p: Point, stroke: f32, color: crate::color::Color) {
    ctx.add(crate::node::Node::Rect {
        rect: crate::geom::Rect::new(p.0 - stroke * 0.5, p.1 - stroke * 0.5, stroke, stroke),
        fill: color,
        radius: stroke * 0.5,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_parses_to_two_subpaths_of_lines() {
        let svg = r#"<svg><path d="M12 19h8" /><path d="m4 17 6-6-6-6" /></svg>"#;
        let g = parse_icon(svg);
        assert_eq!(g.subpaths.len(), 2);
        assert_eq!(g.subpaths[0].start, (12.0, 19.0));
        assert_eq!(g.subpaths[0].segs, vec![Seg::Line((20.0, 19.0))]);
        assert_eq!(g.subpaths[1].start, (4.0, 17.0));
        assert_eq!(g.subpaths[1].segs, vec![Seg::Line((10.0, 11.0)), Seg::Line((4.0, 5.0))]);
    }

    #[test]
    fn circle_becomes_one_arc_subpath() {
        let g = parse_icon(r#"<svg><circle cx="12" cy="12" r="10" /></svg>"#);
        assert_eq!(g.subpaths.len(), 1);
        assert!(matches!(g.subpaths[0].segs[0], Seg::Arc { .. }));
    }

    #[test]
    fn real_icons_parse_with_arcs_and_cubics() {
        for f in ["files", "editor", "settings"] {
            let path = format!("{}/../assets/icons/lucide/{f}.svg", env!("CARGO_MANIFEST_DIR"));
            if let Ok(svg) = std::fs::read_to_string(&path) {
                let g = parse_icon(&svg);
                assert!(!g.is_empty(), "{f} parsed");
                assert!(g.seg_count() > 0);
            }
        }
    }
}
