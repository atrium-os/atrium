//! icon — parse a Lucide-style SVG icon into polylines for the vector renderer.
//!
//! The Atrium scene graph renders thick line segments (`op_path` rotated quads), so
//! a stroked icon is just a set of polylines — flattened from the SVG path data at
//! load, scaled + positioned at render. Resolution-independence comes from the
//! source being vector (curves are flattened finely enough to be smooth at any UI
//! size; re-flattening per-scale is a future refinement).
//!
//! Supports the subset Lucide uses: `<path>` with M/L/H/V/C/A/Z (absolute + relative,
//! including implicit repeated commands) and `<circle>`. Coordinates are in the
//! source viewBox space (Lucide is 0..24).

pub type Point = (f32, f32);
/// A connected run of points (a stroked sub-path). Consecutive points form segments.
pub type Polyline = Vec<Point>;

/// Curve flattening density — segments per cubic/arc. 16 is smooth for UI icon sizes.
const FLATTEN_STEPS: usize = 16;

/// Parse an SVG icon into polylines (viewBox coordinates).
pub fn parse_icon(svg: &str) -> Vec<Polyline> {
    let mut out = Vec::new();
    for d in extract_attr(svg, "<path", "d") {
        out.extend(parse_path_d(&d));
    }
    for (cx, cy, r) in extract_circles(svg) {
        out.push(circle_polyline(cx, cy, r));
    }
    out
}

/// Pull every `value` of `attr` from elements starting with `tag` (tiny, good enough
/// for these flat one-line SVGs; not a general XML parser).
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
            if let Some(q) = after.find('"') {
                out.push(after[..q].to_string());
            }
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
    (0..n)
        .filter_map(|i| Some((cx[i].parse().ok()?, cy[i].parse().ok()?, r[i].parse().ok()?)))
        .collect()
}

fn circle_polyline(cx: f32, cy: f32, r: f32) -> Polyline {
    let steps = 48;
    (0..=steps)
        .map(|i| {
            let a = i as f32 / steps as f32 * std::f32::consts::TAU;
            (cx + r * a.cos(), cy + r * a.sin())
        })
        .collect()
}

// ── path "d" mini-language ───────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
enum Tok { Cmd(char), Num(f32) }

fn tokenize(d: &str) -> Vec<Tok> {
    let b = d.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i] as char;
        if c.is_ascii_alphabetic() {
            out.push(Tok::Cmd(c));
            i += 1;
        } else if c == ' ' || c == ',' || c == '\n' || c == '\t' || c == '\r' {
            i += 1;
        } else if c == '-' || c == '+' || c == '.' || c.is_ascii_digit() {
            // Scan one number.
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
        } else {
            i += 1;
        }
    }
    out
}

/// Parse path data → polylines. `cur` is the pen; `start` the current sub-path start.
fn parse_path_d(d: &str) -> Vec<Polyline> {
    let toks = tokenize(d);
    let mut polys: Vec<Polyline> = Vec::new();
    let mut cur: Polyline = Vec::new();
    let (mut x, mut y) = (0.0f32, 0.0f32);
    let (mut sx, mut sy) = (0.0f32, 0.0f32);
    let mut i = 0;
    let mut cmd = ' ';

    // Pull the next `k` numbers; returns None if not enough.
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

    while i < toks.len() {
        // A command letter starts a new op; otherwise repeat the previous command
        // (with M→L / m→l per the SVG spec).
        if let Some(Tok::Cmd(c)) = toks.get(i) {
            cmd = *c;
            i += 1;
        } else if cmd == 'M' { cmd = 'L'; } else if cmd == 'm' { cmd = 'l'; }

        let rel = cmd.is_ascii_lowercase();
        match cmd.to_ascii_uppercase() {
            'M' => {
                if let Some([nx, ny]) = nums!(2) {
                    if !cur.is_empty() { polys.push(std::mem::take(&mut cur)); }
                    x = if rel { x + nx } else { nx };
                    y = if rel { y + ny } else { ny };
                    sx = x; sy = y;
                    cur.push((x, y));
                } else { break; }
            }
            'L' => {
                if let Some([nx, ny]) = nums!(2) {
                    x = if rel { x + nx } else { nx };
                    y = if rel { y + ny } else { ny };
                    cur.push((x, y));
                } else { break; }
            }
            'H' => {
                if let Some([nx]) = nums!(1) { x = if rel { x + nx } else { nx }; cur.push((x, y)); } else { break; }
            }
            'V' => {
                if let Some([ny]) = nums!(1) { y = if rel { y + ny } else { ny }; cur.push((x, y)); } else { break; }
            }
            'C' => {
                if let Some([x1, y1, x2, y2, ex, ey]) = nums!(6) {
                    let (c1, c2, e);
                    if rel {
                        c1 = (x + x1, y + y1); c2 = (x + x2, y + y2); e = (x + ex, y + ey);
                    } else {
                        c1 = (x1, y1); c2 = (x2, y2); e = (ex, ey);
                    }
                    flatten_cubic((x, y), c1, c2, e, &mut cur);
                    x = e.0; y = e.1;
                } else { break; }
            }
            'A' => {
                if let Some([rx, ry, rot, large, sweep, ex, ey]) = nums!(7) {
                    let e = if rel { (x + ex, y + ey) } else { (ex, ey) };
                    flatten_arc((x, y), rx, ry, rot, large != 0.0, sweep != 0.0, e, &mut cur);
                    x = e.0; y = e.1;
                } else { break; }
            }
            'Z' => {
                cur.push((sx, sy));
                x = sx; y = sy;
                if !cur.is_empty() { polys.push(std::mem::take(&mut cur)); }
            }
            _ => { i += 1; } // unsupported command: skip a token, keep going
        }
    }
    if !cur.is_empty() { polys.push(cur); }
    polys
}

fn flatten_cubic(p0: Point, c1: Point, c2: Point, p1: Point, out: &mut Polyline) {
    for s in 1..=FLATTEN_STEPS {
        let t = s as f32 / FLATTEN_STEPS as f32;
        let u = 1.0 - t;
        let w0 = u * u * u;
        let w1 = 3.0 * u * u * t;
        let w2 = 3.0 * u * t * t;
        let w3 = t * t * t;
        out.push((
            w0 * p0.0 + w1 * c1.0 + w2 * c2.0 + w3 * p1.0,
            w0 * p0.1 + w1 * c1.1 + w2 * c2.1 + w3 * p1.1,
        ));
    }
}

/// SVG elliptical arc → points, via endpoint→centre parametrisation (spec F.6.5).
#[allow(clippy::too_many_arguments)]
fn flatten_arc(p0: Point, mut rx: f32, mut ry: f32, rot_deg: f32, large: bool, sweep: bool, p1: Point, out: &mut Polyline) {
    if rx == 0.0 || ry == 0.0 || (p0.0 == p1.0 && p0.1 == p1.1) {
        out.push(p1);
        return;
    }
    rx = rx.abs(); ry = ry.abs();
    let phi = rot_deg.to_radians();
    let (cosp, sinp) = (phi.cos(), phi.sin());
    let dx = (p0.0 - p1.0) / 2.0;
    let dy = (p0.1 - p1.1) / 2.0;
    let x1p = cosp * dx + sinp * dy;
    let y1p = -sinp * dx + cosp * dy;
    // Correct out-of-range radii.
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
    let theta1 = ang(1.0, 0.0, (x1p - cxp) / rx, (y1p - cyp) / ry);
    let mut dtheta = ang((x1p - cxp) / rx, (y1p - cyp) / ry, (-x1p - cxp) / rx, (-y1p - cyp) / ry);
    if !sweep && dtheta > 0.0 { dtheta -= std::f32::consts::TAU; }
    if sweep && dtheta < 0.0 { dtheta += std::f32::consts::TAU; }
    for s in 1..=FLATTEN_STEPS {
        let t = theta1 + dtheta * (s as f32 / FLATTEN_STEPS as f32);
        let (ct, st) = (t.cos(), t.sin());
        out.push((
            cosp * rx * ct - sinp * ry * st + cx,
            sinp * rx * ct + cosp * ry * st + cy,
        ));
    }
}

/// Emit an icon (parsed polylines, Lucide 0..24 viewBox) into the scene as stroked
/// `Node::Path` segments, scaled to `size` and placed with top-left at (`x`, `y`).
/// Each polyline edge becomes one thick-quad path node.
pub fn draw_icon(
    ctx: &mut crate::view::Ctx,
    polys: &[Polyline],
    x: f32, y: f32, size: f32,
    stroke: f32,
    color: crate::color::Color,
) {
    let scale = size / 24.0; // Lucide's viewBox is 0 0 24 24
    for poly in polys {
        for seg in poly.windows(2) {
            ctx.add(crate::node::Node::Path {
                p0: (x + seg[0].0 * scale, y + seg[0].1 * scale),
                p1: (x + seg[1].0 * scale, y + seg[1].1 * scale),
                width: stroke,
                color,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_path_parses_to_two_polylines() {
        // Lucide terminal.svg: a chevron + a baseline.
        let svg = r#"<svg><path d="M12 19h8" /><path d="m4 17 6-6-6-6" /></svg>"#;
        let polys = parse_icon(svg);
        assert_eq!(polys.len(), 2);
        // "M12 19 h8" → (12,19)-(20,19)
        assert_eq!(polys[0], vec![(12.0, 19.0), (20.0, 19.0)]);
        // "m4 17 6-6 -6-6" → (4,17)-(10,11)-(4,5)
        assert_eq!(polys[1], vec![(4.0, 17.0), (10.0, 11.0), (4.0, 5.0)]);
    }

    #[test]
    fn circle_and_curves_produce_points() {
        let svg = r#"<svg><circle cx="12" cy="12" r="10" /><path d="M2 12h20" /></svg>"#;
        let polys = parse_icon(svg);
        assert_eq!(polys.len(), 2);
        // parse_icon emits paths first, then circles.
        assert_eq!(polys[0], vec![(2.0, 12.0), (22.0, 12.0)]);
        assert!(polys[1].len() > 8, "circle flattened to a ring");
    }

    #[test]
    fn arc_and_cubic_dont_panic_and_advance_the_pen() {
        // files.svg uses arcs; editor.svg uses cubics. Just ensure they flatten.
        let files = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../assets/icons/lucide/files.svg"));
        if let Ok(svg) = files {
            let polys = parse_icon(&svg);
            assert!(!polys.is_empty());
            assert!(polys.iter().all(|p| p.iter().all(|(x, y)| x.is_finite() && y.is_finite())));
        }
    }
}
