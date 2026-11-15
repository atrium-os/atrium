//! ODF drawing outlines as the deck's paths: `draw:enhanced-geometry` (its
//! equations, modifiers and path commands), SVG path data (`svg:d`) and
//! point lists (`draw:points`).

use std::collections::HashMap;
use std::f64::consts::PI;

use officina_slides::model::{Path, PathCmd};

/// Bézier handle length for a quarter circle, as a fraction of the radius.
const K: f64 = 0.552_284_75;

/// Most path commands one outline may have.
const MAX_CMDS: usize = 20_000;

#[derive(Clone, Copy)]
enum C {
    M(f64, f64),
    L(f64, f64),
    Cu(f64, f64, f64, f64, f64, f64),
    Z,
}

/// A subpath list being drawn, in floating point.
#[derive(Default)]
struct Pen {
    paths: Vec<(Vec<C>, bool, bool)>,
    cur: Vec<C>,
    filled: bool,
    stroked: bool,
    at: (f64, f64),
    start: (f64, f64),
}

impl Pen {
    fn new() -> Pen {
        Pen { filled: true, stroked: true, ..Default::default() }
    }
    fn full(&self) -> bool {
        self.paths.iter().map(|p| p.0.len()).sum::<usize>() + self.cur.len() > MAX_CMDS
    }
    fn m(&mut self, x: f64, y: f64) {
        self.cur.push(C::M(x, y));
        self.at = (x, y);
        self.start = (x, y);
    }
    fn l(&mut self, x: f64, y: f64) {
        if self.cur.is_empty() {
            self.m(self.at.0, self.at.1);
        }
        self.cur.push(C::L(x, y));
        self.at = (x, y);
    }
    fn c(&mut self, a: f64, b: f64, c: f64, d: f64, x: f64, y: f64) {
        if self.cur.is_empty() {
            self.m(self.at.0, self.at.1);
        }
        self.cur.push(C::Cu(a, b, c, d, x, y));
        self.at = (x, y);
    }
    fn z(&mut self) {
        if !self.cur.is_empty() {
            self.cur.push(C::Z);
            self.at = self.start;
        }
    }
    /// Ends the subpath list drawn since the last `N`.
    fn n(&mut self) {
        if !self.cur.is_empty() {
            let cmds = std::mem::take(&mut self.cur);
            self.paths.push((cmds, self.filled, self.stroked));
        }
        self.filled = true;
        self.stroked = true;
    }
    /// An elliptical arc in screen angles (y down, radians), from `a0`
    /// sweeping `sweep`, joined to the current point (or moved to).
    fn arc(&mut self, cx: f64, cy: f64, rx: f64, ry: f64, a0: f64, sweep: f64, move_to: bool) {
        let p0 = (cx + rx * a0.cos(), cy + ry * a0.sin());
        if move_to || self.cur.is_empty() {
            self.m(p0.0, p0.1);
        } else {
            self.l(p0.0, p0.1);
        }
        let n = ((sweep.abs() / (PI / 2.0)).ceil() as usize).clamp(1, 8);
        let step = sweep / n as f64;
        let k = 4.0 / 3.0 * (step / 4.0).tan();
        let mut a = a0;
        for _ in 0..n {
            let b = a + step;
            let (ca, sa, cb, sb) = (a.cos(), a.sin(), b.cos(), b.sin());
            self.c(cx + rx * (ca - k * sa), cy + ry * (sa + k * ca), cx + rx * (cb + k * sb), cy + ry * (sb - k * cb), cx + rx * cb, cy + ry * sb);
            a = b;
        }
    }
    fn finish(mut self, w: f64, h: f64, ox: f64, oy: f64) -> Option<Vec<Path>> {
        self.n();
        if self.paths.is_empty() || !(w > 0.0 && h > 0.0) || !w.is_finite() || !h.is_finite() {
            return None;
        }
        // Whole numbers with room: at least 10 000 units on the long side.
        let f = (10_000.0 / w.max(h)).max(1.0);
        let q = |v: f64, o: f64| {
            let r = ((v - o) * f).round();
            if r.is_finite() {
                r.clamp(-1e12, 1e12) as i64
            } else {
                0
            }
        };
        let (pw, ph) = ((w * f).round() as i64, (h * f).round() as i64);
        let out = self
            .paths
            .into_iter()
            .map(|(cmds, filled, stroked)| Path {
                w: pw.max(1),
                h: ph.max(1),
                cmds: cmds
                    .into_iter()
                    .map(|c| match c {
                        C::M(x, y) => PathCmd::Move(q(x, ox), q(y, oy)),
                        C::L(x, y) => PathCmd::Line(q(x, ox), q(y, oy)),
                        C::Cu(a, b, c, d, x, y) => PathCmd::Cubic(q(a, ox), q(b, oy), q(c, ox), q(d, oy), q(x, ox), q(y, oy)),
                        C::Z => PathCmd::Close,
                    })
                    .collect(),
                filled,
                stroked,
            })
            .collect();
        Some(out)
    }
}

/// The angle (screen, radians) of a point on an ellipse.
fn angle_of(cx: f64, cy: f64, rx: f64, ry: f64, x: f64, y: f64) -> f64 {
    ((y - cy) / ry.max(1e-9)).atan2((x - cx) / rx.max(1e-9))
}

/// Evaluates an enhanced geometry's equations.
struct Calc<'a> {
    eqs: HashMap<&'a str, &'a str>,
    mods: Vec<f64>,
    memo: HashMap<String, f64>,
    depth: u32,
    /// View box left, top, width, height.
    vb: [f64; 4],
    logw: f64,
    logh: f64,
}

impl Calc<'_> {
    fn eq(&mut self, name: &str) -> f64 {
        if let Some(v) = self.memo.get(name) {
            return *v;
        }
        if self.depth > 64 {
            return 0.0;
        }
        let Some(f) = self.eqs.get(name).copied() else { return 0.0 };
        // A loop reads 0.
        self.memo.insert(name.to_string(), 0.0);
        self.depth += 1;
        let v = self.formula(f);
        self.depth -= 1;
        self.memo.insert(name.to_string(), v);
        v
    }

    fn formula(&mut self, f: &str) -> f64 {
        let b = f.as_bytes();
        let mut i = 0;
        let v = self.sum(b, &mut i);
        if v.is_finite() {
            v
        } else {
            0.0
        }
    }

    fn ws(b: &[u8], i: &mut usize) {
        while *i < b.len() && b[*i].is_ascii_whitespace() {
            *i += 1;
        }
    }

    fn sum(&mut self, b: &[u8], i: &mut usize) -> f64 {
        let mut v = self.product(b, i);
        loop {
            Self::ws(b, i);
            match b.get(*i) {
                Some(b'+') => {
                    *i += 1;
                    v += self.product(b, i);
                }
                Some(b'-') => {
                    *i += 1;
                    v -= self.product(b, i);
                }
                _ => return v,
            }
        }
    }

    fn product(&mut self, b: &[u8], i: &mut usize) -> f64 {
        let mut v = self.unary(b, i);
        loop {
            Self::ws(b, i);
            match b.get(*i) {
                Some(b'*') => {
                    *i += 1;
                    v *= self.unary(b, i);
                }
                Some(b'/') => {
                    *i += 1;
                    let d = self.unary(b, i);
                    v = if d == 0.0 { 0.0 } else { v / d };
                }
                _ => return v,
            }
        }
    }

    fn unary(&mut self, b: &[u8], i: &mut usize) -> f64 {
        Self::ws(b, i);
        match b.get(*i) {
            Some(b'-') => {
                *i += 1;
                -self.unary(b, i)
            }
            Some(b'+') => {
                *i += 1;
                self.unary(b, i)
            }
            _ => self.atom(b, i),
        }
    }

    fn atom(&mut self, b: &[u8], i: &mut usize) -> f64 {
        Self::ws(b, i);
        if self.depth > 200 {
            *i = b.len();
            return 0.0;
        }
        let Some(&c) = b.get(*i) else { return 0.0 };
        if c == b'(' {
            *i += 1;
            self.depth += 1;
            let v = self.sum(b, i);
            self.depth -= 1;
            Self::ws(b, i);
            if b.get(*i) == Some(&b')') {
                *i += 1;
            }
            return v;
        }
        if c.is_ascii_digit() || c == b'.' {
            let s = *i;
            while *i < b.len() && (b[*i].is_ascii_digit() || b[*i] == b'.' || ((b[*i] == b'e' || b[*i] == b'E') && *i + 1 < b.len() && (b[*i + 1].is_ascii_digit() || b[*i + 1] == b'-'))) {
                if b[*i] == b'e' || b[*i] == b'E' {
                    *i += 1;
                }
                *i += 1;
            }
            return std::str::from_utf8(&b[s..*i]).ok().and_then(|t| t.parse().ok()).unwrap_or(0.0);
        }
        if c == b'?' || c == b'$' {
            *i += 1;
            let s = *i;
            while *i < b.len() && b[*i].is_ascii_alphanumeric() {
                *i += 1;
            }
            let name = std::str::from_utf8(&b[s..*i]).unwrap_or("");
            return if c == b'?' {
                self.eq(name)
            } else {
                name.parse::<usize>().ok().and_then(|n| self.mods.get(n).copied()).unwrap_or(0.0)
            };
        }
        if c.is_ascii_alphabetic() {
            let s = *i;
            while *i < b.len() && (b[*i].is_ascii_alphanumeric() || b[*i] == b'-') {
                *i += 1;
            }
            let name = std::str::from_utf8(&b[s..*i]).unwrap_or("").to_string();
            Self::ws(b, i);
            if b.get(*i) == Some(&b'(') {
                *i += 1;
                let mut args = Vec::new();
                self.depth += 1;
                loop {
                    args.push(self.sum(b, i));
                    Self::ws(b, i);
                    match b.get(*i) {
                        Some(b',') => *i += 1,
                        Some(b')') => {
                            *i += 1;
                            break;
                        }
                        _ => break,
                    }
                    if args.len() > 8 {
                        break;
                    }
                }
                self.depth -= 1;
                let a = |n: usize| args.get(n).copied().unwrap_or(0.0);
                return match name.as_str() {
                    "abs" => a(0).abs(),
                    "sqrt" => a(0).max(0.0).sqrt(),
                    "sin" => a(0).sin(),
                    "cos" => a(0).cos(),
                    "tan" => a(0).tan(),
                    "atan" => a(0).atan(),
                    "atan2" => a(0).atan2(a(1)),
                    "min" => a(0).min(a(1)),
                    "max" => a(0).max(a(1)),
                    "if" => {
                        if a(0) > 0.0 {
                            a(1)
                        } else {
                            a(2)
                        }
                    }
                    _ => 0.0,
                };
            }
            let [l, t, w, h] = self.vb;
            return match name.as_str() {
                "pi" => PI,
                "left" => l,
                "top" => t,
                "right" => l + w,
                "bottom" => t + h,
                "width" => w,
                "height" => h,
                "logwidth" => self.logw,
                "logheight" => self.logh,
                "hasstroke" | "hasfill" => 1.0,
                _ => 0.0,
            };
        }
        *i += 1;
        0.0
    }

    fn value(&mut self, tok: &str) -> f64 {
        match tok.parse::<f64>() {
            Ok(v) if v.is_finite() => v,
            _ => self.formula(tok),
        }
    }
}

/// An enhanced geometry's outline: `logw` × `logh` is the shape's size
/// (1/100 mm), used where the view box is empty.
pub fn enhanced(eg: &crate::xml::El, logw: f64, logh: f64) -> Option<Vec<Path>> {
    let path = eg.attr("draw:enhanced-path")?;
    let vb: Vec<f64> = eg.attr("svg:viewBox").unwrap_or("0 0 21600 21600").split([' ', ',']).filter(|s| !s.is_empty()).filter_map(|s| s.parse().ok()).collect();
    let mut vb = [vb.first().copied().unwrap_or(0.0), vb.get(1).copied().unwrap_or(0.0), vb.get(2).copied().unwrap_or(21600.0), vb.get(3).copied().unwrap_or(21600.0)];
    let (logw, logh) = (logw.max(1.0), logh.max(1.0));
    if vb[2] <= 0.0 || vb[3] <= 0.0 {
        vb = [0.0, 0.0, logw, logh];
    }
    let mut calc = Calc {
        eqs: eg.children("draw:equation").filter_map(|e| Some((e.attr("draw:name")?, e.attr("draw:formula")?))).collect(),
        mods: eg.attr("draw:modifiers").unwrap_or("").split([' ', ',']).filter(|s| !s.is_empty()).map(|s| s.parse().unwrap_or(0.0)).collect(),
        memo: HashMap::new(),
        depth: 0,
        vb,
        logw,
        logh,
    };
    // Tokens: command letters and parameters.
    let mut toks: Vec<&str> = Vec::new();
    for t in path.split([' ', ',', '\n', '\t']) {
        if !t.is_empty() {
            toks.push(t);
        }
    }
    let mut pen = Pen::new();
    let mut i = 0;
    let mut cmd = 'M';
    let mut quad_x = true;
    while i < toks.len() && !pen.full() {
        let t = toks[i];
        if t.len() == 1 && t.as_bytes()[0].is_ascii_alphabetic() {
            cmd = t.chars().next().unwrap_or('M');
            i += 1;
            match cmd {
                'Z' => {
                    pen.z();
                    continue;
                }
                'N' => {
                    pen.n();
                    continue;
                }
                'F' => {
                    pen.filled = false;
                    continue;
                }
                'S' => {
                    pen.stroked = false;
                    continue;
                }
                'X' => quad_x = true,
                'Y' => quad_x = false,
                'H' | 'I' | 'J' | 'K' => continue,
                _ => {}
            }
        }
        let need = match cmd {
            'M' | 'L' | 'X' | 'Y' => 2,
            'Q' => 4,
            'C' | 'T' | 'U' => 6,
            'A' | 'B' | 'V' | 'W' => 8,
            'G' => 4,
            _ => {
                i += 1;
                continue;
            }
        };
        if i + need > toks.len() || toks[i..i + need].iter().any(|t| t.len() == 1 && t.as_bytes()[0].is_ascii_alphabetic()) {
            i += 1;
            continue;
        }
        let v: Vec<f64> = toks[i..i + need].iter().map(|t| calc.value(t)).collect();
        i += need;
        match cmd {
            'M' => {
                pen.m(v[0], v[1]);
                cmd = 'L';
            }
            'L' => pen.l(v[0], v[1]),
            'C' => pen.c(v[0], v[1], v[2], v[3], v[4], v[5]),
            'Q' => {
                let (x0, y0) = pen.at;
                pen.c(x0 + 2.0 / 3.0 * (v[0] - x0), y0 + 2.0 / 3.0 * (v[1] - y0), v[2] + 2.0 / 3.0 * (v[0] - v[2]), v[3] + 2.0 / 3.0 * (v[1] - v[3]), v[2], v[3]);
            }
            'X' | 'Y' => {
                let (x0, y0) = pen.at;
                let (x1, y1) = (v[0], v[1]);
                if quad_x {
                    pen.c(x0 + K * (x1 - x0), y0, x1, y1 + K * (y0 - y1), x1, y1);
                } else {
                    pen.c(x0, y0 + K * (y1 - y0), x1 + K * (x0 - x1), y1, x1, y1);
                }
                quad_x = !quad_x;
            }
            'T' | 'U' => {
                // Centre, radii, start and end angles in degrees,
                // counter-clockwise as seen.
                let (t0, t1) = (v[4], v[5]);
                let mut d = (t1 - t0).rem_euclid(360.0);
                if d == 0.0 {
                    d = 360.0;
                }
                pen.arc(v[0], v[1], v[2], v[3], -t0.to_radians(), -d.to_radians(), cmd == 'U');
            }
            'A' | 'B' | 'V' | 'W' => {
                let (cx, cy) = ((v[0] + v[2]) / 2.0, (v[1] + v[3]) / 2.0);
                let (rx, ry) = (((v[2] - v[0]) / 2.0).abs(), ((v[3] - v[1]) / 2.0).abs());
                let a0 = angle_of(cx, cy, rx, ry, v[4], v[5]);
                let a1 = angle_of(cx, cy, rx, ry, v[6], v[7]);
                let clockwise = matches!(cmd, 'V' | 'W');
                let mut sweep = if clockwise { (a1 - a0).rem_euclid(2.0 * PI) } else { -(a0 - a1).rem_euclid(2.0 * PI) };
                if sweep == 0.0 {
                    sweep = if clockwise { 2.0 * PI } else { -2.0 * PI };
                }
                pen.arc(cx, cy, rx, ry, a0, sweep, matches!(cmd, 'B' | 'V'));
            }
            'G' => {
                let (wr, hr, st, sw) = (v[0], v[1], v[2].to_radians(), v[3].to_radians());
                let (x0, y0) = pen.at;
                let (cx, cy) = (x0 - wr * st.cos(), y0 - hr * st.sin());
                pen.arc(cx, cy, wr, hr, st, sw, false);
            }
            _ => {}
        }
    }
    pen.finish(vb[2], vb[3], vb[0], vb[1])
}

/// SVG path data in a view box.
pub fn svg_path(d: &str, vb: [f64; 4]) -> Option<Vec<Path>> {
    let b = d.as_bytes();
    let mut i = 0;
    let mut pen = Pen::new();
    let mut cmd = b'M';
    let mut last_ctrl: Option<(f64, f64)> = None;
    let mut last_quad: Option<(f64, f64)> = None;
    let num = |i: &mut usize| -> Option<f64> {
        while *i < b.len() && (b[*i].is_ascii_whitespace() || b[*i] == b',') {
            *i += 1;
        }
        let s = *i;
        if *i < b.len() && (b[*i] == b'-' || b[*i] == b'+') {
            *i += 1;
        }
        let mut dot = false;
        while *i < b.len() && (b[*i].is_ascii_digit() || (b[*i] == b'.' && !dot)) {
            dot |= b[*i] == b'.';
            *i += 1;
        }
        if *i < b.len() && (b[*i] == b'e' || b[*i] == b'E') {
            *i += 1;
            if *i < b.len() && (b[*i] == b'-' || b[*i] == b'+') {
                *i += 1;
            }
            while *i < b.len() && b[*i].is_ascii_digit() {
                *i += 1;
            }
        }
        std::str::from_utf8(&b[s..*i]).ok()?.parse::<f64>().ok().filter(|v| v.is_finite())
    };
    let flag = |i: &mut usize| -> Option<bool> {
        while *i < b.len() && (b[*i].is_ascii_whitespace() || b[*i] == b',') {
            *i += 1;
        }
        let c = *b.get(*i)?;
        *i += 1;
        match c {
            b'0' => Some(false),
            b'1' => Some(true),
            _ => None,
        }
    };
    loop {
        if pen.full() {
            break;
        }
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b',') {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        if b[i].is_ascii_alphabetic() {
            cmd = b[i];
            i += 1;
            if cmd == b'Z' || cmd == b'z' {
                pen.z();
                last_ctrl = None;
                last_quad = None;
                continue;
            }
        }
        let rel = cmd.is_ascii_lowercase();
        let (ox, oy) = if rel { pen.at } else { (0.0, 0.0) };
        let before = i;
        match cmd.to_ascii_uppercase() {
            b'M' => {
                let (Some(x), Some(y)) = (num(&mut i), num(&mut i)) else { break };
                pen.m(ox + x, oy + y);
                cmd = if rel { b'l' } else { b'L' };
                last_ctrl = None;
                last_quad = None;
            }
            b'L' => {
                let (Some(x), Some(y)) = (num(&mut i), num(&mut i)) else { break };
                pen.l(ox + x, oy + y);
                last_ctrl = None;
                last_quad = None;
            }
            b'H' => {
                let Some(x) = num(&mut i) else { break };
                pen.l(ox + x, pen.at.1);
                last_ctrl = None;
                last_quad = None;
            }
            b'V' => {
                let Some(y) = num(&mut i) else { break };
                pen.l(pen.at.0, oy + y);
                last_ctrl = None;
                last_quad = None;
            }
            b'C' => {
                let v: Vec<f64> = (0..6).filter_map(|_| num(&mut i)).collect();
                if v.len() < 6 {
                    break;
                }
                pen.c(ox + v[0], oy + v[1], ox + v[2], oy + v[3], ox + v[4], oy + v[5]);
                last_ctrl = Some((ox + v[2], oy + v[3]));
                last_quad = None;
            }
            b'S' => {
                let v: Vec<f64> = (0..4).filter_map(|_| num(&mut i)).collect();
                if v.len() < 4 {
                    break;
                }
                let (x0, y0) = pen.at;
                let c1 = last_ctrl.map_or((x0, y0), |(cx, cy)| (2.0 * x0 - cx, 2.0 * y0 - cy));
                pen.c(c1.0, c1.1, ox + v[0], oy + v[1], ox + v[2], oy + v[3]);
                last_ctrl = Some((ox + v[0], oy + v[1]));
                last_quad = None;
            }
            b'Q' | b'T' => {
                let (qx, qy, x, y) = if cmd.to_ascii_uppercase() == b'Q' {
                    let v: Vec<f64> = (0..4).filter_map(|_| num(&mut i)).collect();
                    if v.len() < 4 {
                        break;
                    }
                    (ox + v[0], oy + v[1], ox + v[2], oy + v[3])
                } else {
                    let (Some(x), Some(y)) = (num(&mut i), num(&mut i)) else { break };
                    let (x0, y0) = pen.at;
                    let q = last_quad.map_or((x0, y0), |(cx, cy)| (2.0 * x0 - cx, 2.0 * y0 - cy));
                    (q.0, q.1, ox + x, oy + y)
                };
                let (x0, y0) = pen.at;
                pen.c(x0 + 2.0 / 3.0 * (qx - x0), y0 + 2.0 / 3.0 * (qy - y0), x + 2.0 / 3.0 * (qx - x), y + 2.0 / 3.0 * (qy - y), x, y);
                last_quad = Some((qx, qy));
                last_ctrl = None;
            }
            b'A' => {
                let (Some(rx), Some(ry), Some(rot), Some(large), Some(sweep), Some(x), Some(y)) = (num(&mut i), num(&mut i), num(&mut i), flag(&mut i), flag(&mut i), num(&mut i), num(&mut i)) else { break };
                svg_arc(&mut pen, rx, ry, rot, large, sweep, ox + x, oy + y);
                last_ctrl = None;
                last_quad = None;
            }
            _ => {
                i += 1;
            }
        }
        if i == before {
            i += 1;
        }
    }
    pen.finish(vb[2], vb[3], vb[0], vb[1])
}

/// An SVG elliptical arc from the current point (SVG 1.1 F.6.5).
#[allow(clippy::too_many_arguments)]
fn svg_arc(pen: &mut Pen, rx: f64, ry: f64, rot: f64, large: bool, sweep: bool, x: f64, y: f64) {
    let (x1, y1) = pen.at;
    let (mut rx, mut ry) = (rx.abs(), ry.abs());
    if rx == 0.0 || ry == 0.0 || (x1 == x && y1 == y) {
        pen.l(x, y);
        return;
    }
    // A turned arc (`rot`) is drawn unturned: rare in slides.
    let _ = rot;
    let (dx, dy) = ((x1 - x) / 2.0, (y1 - y) / 2.0);
    let lam = dx * dx / (rx * rx) + dy * dy / (ry * ry);
    if lam > 1.0 {
        rx *= lam.sqrt();
        ry *= lam.sqrt();
    }
    let num = rx * rx * ry * ry - rx * rx * dy * dy - ry * ry * dx * dx;
    let den = rx * rx * dy * dy + ry * ry * dx * dx;
    let mut k = if den == 0.0 { 0.0 } else { (num / den).max(0.0).sqrt() };
    if large == sweep {
        k = -k;
    }
    let (cxp, cyp) = (k * rx * dy / ry, -k * ry * dx / rx);
    let (cx, cy) = (cxp + (x1 + x) / 2.0, cyp + (y1 + y) / 2.0);
    let a0 = ((dy - cyp) / ry).atan2((dx - cxp) / rx);
    let a1 = ((-dy - cyp) / ry).atan2((-dx - cxp) / rx);
    let mut d = a1 - a0;
    if sweep && d < 0.0 {
        d += 2.0 * PI;
    } else if !sweep && d > 0.0 {
        d -= 2.0 * PI;
    }
    pen.arc(cx, cy, rx, ry, a0, d, false);
}

/// `draw:points` ("x,y x,y ...") as one path in its view box.
pub fn points(pts: &str, vb: [f64; 4], closed: bool) -> Option<Vec<Path>> {
    let mut pen = Pen::new();
    for (k, p) in pts.split_whitespace().take(MAX_CMDS).enumerate() {
        let Some((x, y)) = p.split_once(',') else { continue };
        let (Ok(x), Ok(y)) = (x.parse::<f64>(), y.parse::<f64>()) else { continue };
        if k == 0 {
            pen.m(x, y);
        } else {
            pen.l(x, y);
        }
    }
    if closed {
        pen.z();
    } else {
        pen.filled = false;
    }
    pen.finish(vb[2], vb[3], vb[0], vb[1])
}

/// A view box attribute's four numbers.
pub fn view_box(s: Option<&str>) -> Option<[f64; 4]> {
    let v: Vec<f64> = s?.split([' ', ',']).filter(|s| !s.is_empty()).filter_map(|s| s.parse().ok()).collect();
    (v.len() == 4 && v[2] > 0.0 && v[3] > 0.0 && v.iter().all(|x| x.is_finite())).then(|| [v[0], v[1], v[2], v[3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equations_and_paths() {
        let x = crate::xml::parse(br#"<draw:enhanced-geometry svg:viewBox="0 0 21600 21600" draw:modifiers="5400" draw:enhanced-path="M ?f0 0 L 21600 0 21600 21600 0 21600 Z N"><draw:equation draw:name="f0" draw:formula="$0 *2"/></draw:enhanced-geometry>"#).unwrap();
        let p = enhanced(&x, 1000.0, 1000.0).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].w, 21600);
        assert_eq!(p[0].cmds[0], PathCmd::Move(10800, 0));
        assert_eq!(p[0].cmds.len(), 5);
        let s = svg_path("M0 0 h10 v10 a5 5 0 0 1 -10 0 z", [0.0, 0.0, 10.0, 20.0]).unwrap();
        assert_eq!(s[0].w, 5000);
        assert!(matches!(s[0].cmds.last(), Some(PathCmd::Close)));
        let q = points("0,0 10,0 10,10", [0.0, 0.0, 10.0, 10.0], true).unwrap();
        assert_eq!(q[0].cmds.len(), 4);
        // A hostile equation loop ends.
        let x = crate::xml::parse(br#"<g draw:enhanced-path="M ?f0 ?f1 L 1 1"><draw:equation draw:name="f0" draw:formula="?f1+1"/><draw:equation draw:name="f1" draw:formula="?f0*((((((((1))))))))"/></g>"#).unwrap();
        assert!(enhanced(&x, 10.0, 10.0).is_some());
    }
}
