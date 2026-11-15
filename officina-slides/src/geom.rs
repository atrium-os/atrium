//! Outlines of OOXML's preset shapes (ECMA-376 Part 1 §20.1.10.56) that
//! decks use most, as paths in a shape's own box. A preset not here draws
//! as its box and is kept by name, so a round trip keeps it.

use crate::model::{Geom, Path, PathCmd};

/// Bézier handle length for a quarter circle, as a fraction of the radius.
const K: f64 = 0.552_284_75;

fn adj(geom_adj: &[(std::sync::Arc<str>, i64)], name: &str, default: i64) -> i64 {
    geom_adj.iter().find(|(n, _)| &**n == name).map_or(default, |(_, v)| *v)
}

fn poly(w: i64, h: i64, pts: &[(i64, i64)]) -> Path {
    let mut cmds = Vec::with_capacity(pts.len() + 1);
    for (i, &(x, y)) in pts.iter().enumerate() {
        cmds.push(if i == 0 { PathCmd::Move(x, y) } else { PathCmd::Line(x, y) });
    }
    cmds.push(PathCmd::Close);
    Path { w, h, cmds, filled: true, stroked: true }
}

/// An ellipse arc's quarter from angle `a0` to `a0 + 90°` (`a0` a multiple of 90).
fn quarter(cmds: &mut Vec<PathCmd>, cx: i64, cy: i64, rx: i64, ry: i64, q: u8) {
    let (kx, ky) = ((rx as f64 * K) as i64, (ry as f64 * K) as i64);
    // Clockwise in screen coordinates: right → bottom → left → top → right.
    let c = match q % 4 {
        0 => PathCmd::Cubic(cx + rx, cy + ky, cx + kx, cy + ry, cx, cy + ry),
        1 => PathCmd::Cubic(cx - kx, cy + ry, cx - rx, cy + ky, cx - rx, cy),
        2 => PathCmd::Cubic(cx - rx, cy - ky, cx - kx, cy - ry, cx, cy - ry),
        _ => PathCmd::Cubic(cx + kx, cy - ry, cx + rx, cy - ky, cx + rx, cy),
    };
    cmds.push(c);
}

/// The paths of a geometry in a `w` × `h` box, and whether it was drawn as
/// itself (`false`: an unknown preset drawn as its box).
pub fn paths(g: &Geom, w: i64, h: i64) -> (Vec<Path>, bool) {
    let (name, a) = match g {
        Geom::Custom(p) => return (p.clone(), true),
        Geom::Preset { name, adj } => (&**name, adj.as_slice()),
    };
    let ss = w.min(h);
    let p = match name {
        "rect" => poly(w, h, &[(0, 0), (w, 0), (w, h), (0, h)]),
        "roundRect" => {
            let r = ss * adj(a, "adj", 16_667).clamp(0, 50_000) / 100_000;
            Path { w, h, cmds: round_rect(w, h, r), filled: true, stroked: true }
        }
        "ellipse" => {
            let (cx, cy, rx, ry) = (w / 2, h / 2, w / 2, h / 2);
            let mut c = vec![PathCmd::Move(cx + rx, cy)];
            for q in 0..4 {
                quarter(&mut c, cx, cy, rx, ry, q);
            }
            c.push(PathCmd::Close);
            Path { w, h, cmds: c, filled: true, stroked: true }
        }
        "triangle" => {
            let x = w * adj(a, "adj", 50_000).clamp(0, 100_000) / 100_000;
            poly(w, h, &[(x, 0), (w, h), (0, h)])
        }
        "rtTriangle" => poly(w, h, &[(0, 0), (w, h), (0, h)]),
        "diamond" => poly(w, h, &[(w / 2, 0), (w, h / 2), (w / 2, h), (0, h / 2)]),
        "parallelogram" => {
            let x = ss * adj(a, "adj", 25_000).clamp(0, 100_000 * w / ss.max(1)) / 100_000;
            poly(w, h, &[(x, 0), (w, 0), (w - x, h), (0, h)])
        }
        "trapezoid" => {
            let x = ss * adj(a, "adj", 25_000).clamp(0, 50_000 * w / ss.max(1)) / 100_000;
            poly(w, h, &[(x, 0), (w - x, 0), (w, h), (0, h)])
        }
        "pentagon" => regular(w, h, 5),
        "hexagon" => {
            let x = ss * adj(a, "adj", 25_000).clamp(0, 50_000 * w / ss.max(1)) / 100_000;
            poly(w, h, &[(x, 0), (w - x, 0), (w, h / 2), (w - x, h), (x, h), (0, h / 2)])
        }
        "octagon" => {
            let x = ss * adj(a, "adj", 29_289).clamp(0, 50_000) / 100_000;
            poly(w, h, &[(x, 0), (w - x, 0), (w, x), (w, h - x), (w - x, h), (x, h), (0, h - x), (0, x)])
        }
        "star5" => star(w, h, 5, 0.381_966),
        "plus" => {
            let x = ss * adj(a, "adj", 25_000).clamp(0, 50_000) / 100_000;
            poly(w, h, &[(x, 0), (w - x, 0), (w - x, x), (w, x), (w, h - x), (w - x, h - x), (w - x, h), (x, h), (x, h - x), (0, h - x), (0, x), (x, x)])
        }
        "rightArrow" | "leftArrow" | "upArrow" | "downArrow" => {
            let body = adj(a, "adj1", 50_000).clamp(0, 100_000);
            let head = adj(a, "adj2", 50_000).clamp(0, 100_000);
            // Drawn pointing right in a (len × thick) box, then turned.
            let (len, thick) = if matches!(name, "rightArrow" | "leftArrow") { (w, h) } else { (h, w) };
            let hl = (ss * head / 100_000).min(len);
            let t0 = thick * (100_000 - body) / 200_000;
            let t1 = thick - t0;
            let pts = [(0, t0), (len - hl, t0), (len - hl, 0), (len, thick / 2), (len - hl, thick), (len - hl, t1), (0, t1)];
            let pts: Vec<(i64, i64)> = match name {
                "rightArrow" => pts.to_vec(),
                "leftArrow" => pts.iter().map(|&(x, y)| (len - x, y)).collect(),
                "downArrow" => pts.iter().map(|&(x, y)| (y, x)).collect(),
                _ => pts.iter().map(|&(x, y)| (y, len - x)).collect(),
            };
            poly(w, h, &pts)
        }
        "chevron" | "homePlate" => {
            let x = ss * adj(a, "adj", 50_000).clamp(0, 100_000 * w / ss.max(1)) / 100_000;
            if name == "chevron" {
                poly(w, h, &[(0, 0), (w - x, 0), (w, h / 2), (w - x, h), (0, h), (x, h / 2)])
            } else {
                poly(w, h, &[(0, 0), (w - x, 0), (w, h / 2), (w - x, h), (0, h)])
            }
        }
        "line" | "straightConnector1" => Path { w, h, cmds: vec![PathCmd::Move(0, 0), PathCmd::Line(w, h)], filled: false, stroked: true },
        _ => return (vec![poly(w, h, &[(0, 0), (w, 0), (w, h), (0, h)])], false),
    };
    (vec![p], true)
}

/// A rectangle with corners of radius `r`, clockwise from the top edge.
fn round_rect(w: i64, h: i64, r: i64) -> Vec<PathCmd> {
    let k = (r as f64 * K) as i64;
    vec![
        PathCmd::Move(r, 0),
        PathCmd::Line(w - r, 0),
        PathCmd::Cubic(w - r + k, 0, w, r - k, w, r),
        PathCmd::Line(w, h - r),
        PathCmd::Cubic(w, h - r + k, w - r + k, h, w - r, h),
        PathCmd::Line(r, h),
        PathCmd::Cubic(r - k, h, 0, h - r + k, 0, h - r),
        PathCmd::Line(0, r),
        PathCmd::Cubic(0, r - k, r - k, 0, r, 0),
        PathCmd::Close,
    ]
}

fn regular(w: i64, h: i64, n: usize) -> Path {
    let pts: Vec<(i64, i64)> = (0..n)
        .map(|i| {
            let t = -std::f64::consts::FRAC_PI_2 + i as f64 * std::f64::consts::TAU / n as f64;
            ((w as f64 / 2.0 * (1.0 + t.cos())) as i64, (h as f64 / 2.0 * (1.0 + t.sin())) as i64)
        })
        .collect();
    poly(w, h, &pts)
}

fn star(w: i64, h: i64, n: usize, inner: f64) -> Path {
    let pts: Vec<(i64, i64)> = (0..2 * n)
        .map(|i| {
            let r = if i % 2 == 0 { 1.0 } else { inner };
            let t = -std::f64::consts::FRAC_PI_2 + i as f64 * std::f64::consts::PI / n as f64;
            ((w as f64 / 2.0 * (1.0 + r * t.cos())) as i64, (h as f64 / 2.0 * (1.0 + r * t.sin())) as i64)
        })
        .collect();
    poly(w, h, &pts)
}
