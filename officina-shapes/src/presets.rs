//! OOXML's preset shapes from their own definitions (ECMA-376 Part 1
//! §20.1.9, `presetShapeDefinitions.xml`), kept compact in `presets.txt`
//! (see `tools/gen_presets.py`): adjust values, guides worked out by the
//! shape-guide formulas, then paths of moves, lines, arcs and Béziers.
//!
//! LICENCE: the definitions are ECMA-376's (royalty-free), as Apache POI
//! ships them under Apache-2.0; the evaluator is ours.
//!
//! The same evaluator serves custom geometry with guides (`a:custGeom`).

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use crate::{Path, PathCmd};

/// A formula's operands: a guide by number, or a number.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Arg {
    Var(u16),
    Num(f64),
}

/// A shape-guide formula (§20.1.9.11).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Op {
    MulDiv,
    AddSub,
    AddDiv,
    IfElse,
    Abs,
    At2,
    Cat2,
    Cos,
    Max,
    Min,
    Mod,
    Pin,
    Sat2,
    Sin,
    Sqrt,
    Tan,
    Val,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Formula {
    pub op: Op,
    pub args: [Arg; 3],
}

/// A path command; points are operands.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Cmd {
    Move(Arg, Arg),
    Line(Arg, Arg),
    Arc(Arg, Arg, Arg, Arg),
    Quad(Arg, Arg, Arg, Arg),
    Cubic(Arg, Arg, Arg, Arg, Arg, Arg),
    Close,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PathDef {
    /// Its own coordinate size (0: the shape's).
    pub w: f64,
    pub h: f64,
    pub filled: bool,
    pub stroked: bool,
    pub cmds: Vec<Cmd>,
}

/// A geometry: adjust values with their defaults, guides, paths. Variables
/// are numbered: the built-ins first, then the adjust values, then guides.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Def {
    pub names: Vec<Arc<str>>,
    pub adj: Vec<(u16, Formula)>,
    pub guides: Vec<(u16, Formula)>,
    pub paths: Vec<PathDef>,
    /// Where text goes: left, top, right, bottom (none: the whole box).
    pub rect: Option<[Arg; 4]>,
}

/// The built-in variables (§20.1.9.11), in order: their values depend on
/// the shape's size.
const BUILTINS: [&str; 42] = [
    "w", "h", "l", "t", "r", "b", "hc", "vc", "ss", "ls", "wd2", "wd3", "wd4", "wd5", "wd6", "wd8", "wd10", "wd12", "wd32", "hd2", "hd3", "hd4", "hd5", "hd6", "hd8", "hd10", "ssd2", "ssd4", "ssd6", "ssd8", "ssd16", "ssd32", "cd2", "cd4", "cd8", "3cd4", "3cd8", "5cd8", "7cd8", "ssd3", "hd12", "wd16",
];

fn builtins(w: f64, h: f64) -> [f64; 42] {
    let (ss, ls) = (w.min(h), w.max(h));
    [
        w, h, 0.0, 0.0, w, h, w / 2.0, h / 2.0, ss, ls, w / 2.0, w / 3.0, w / 4.0, w / 5.0, w / 6.0, w / 8.0, w / 10.0, w / 12.0, w / 32.0, h / 2.0, h / 3.0, h / 4.0, h / 5.0, h / 6.0, h / 8.0, h / 10.0, ss / 2.0, ss / 4.0, ss / 6.0, ss / 8.0, ss / 16.0, ss / 32.0, 10_800_000.0, 5_400_000.0, 2_700_000.0, 16_200_000.0, 8_100_000.0, 13_500_000.0, 18_900_000.0, ss / 3.0, h / 12.0, w / 16.0,
    ]
}

/// The most guides and path commands one geometry may have.
pub const MAX_GUIDES: usize = 1024;
pub const MAX_CMDS: usize = 4096;

/// Builds a definition: names resolve to variables as they are defined.
#[derive(Default)]
pub struct Builder {
    def: Def,
    index: HashMap<Arc<str>, u16>,
}

impl Builder {
    pub fn new() -> Builder {
        let mut b = Builder::default();
        for n in BUILTINS {
            b.var(n);
        }
        b
    }

    fn var(&mut self, name: &str) -> u16 {
        if let Some(&i) = self.index.get(name) {
            return i;
        }
        let i = self.def.names.len() as u16;
        let n: Arc<str> = name.into();
        self.def.names.push(n.clone());
        self.index.insert(n, i);
        i
    }

    /// An operand: a number, or a variable defined so far (an unknown name
    /// reads 0, as a new variable).
    pub fn arg(&mut self, s: &str) -> Arg {
        match s.parse::<f64>() {
            Ok(v) if v.is_finite() => Arg::Num(v),
            _ => Arg::Var(self.var(s)),
        }
    }

    /// `fmla` (`"*/ w adj 100000"`) into a formula.
    pub fn formula(&mut self, fmla: &str) -> Option<Formula> {
        let mut it = fmla.split_ascii_whitespace();
        let op = match it.next()? {
            "*/" => Op::MulDiv,
            "+-" => Op::AddSub,
            "+/" => Op::AddDiv,
            "?:" => Op::IfElse,
            "abs" => Op::Abs,
            "at2" => Op::At2,
            "cat2" => Op::Cat2,
            "cos" => Op::Cos,
            "max" => Op::Max,
            "min" => Op::Min,
            "mod" => Op::Mod,
            "pin" => Op::Pin,
            "sat2" => Op::Sat2,
            "sin" => Op::Sin,
            "sqrt" => Op::Sqrt,
            "tan" => Op::Tan,
            "val" => Op::Val,
            _ => return None,
        };
        let mut args = [Arg::Num(0.0); 3];
        for a in args.iter_mut() {
            match it.next() {
                Some(s) => *a = self.arg(s),
                None => break,
            }
        }
        Some(Formula { op, args })
    }

    pub fn adj(&mut self, name: &str, fmla: &str) {
        if self.def.adj.len() + self.def.guides.len() >= MAX_GUIDES {
            return;
        }
        if let Some(f) = self.formula(fmla) {
            let v = self.var(name);
            self.def.adj.push((v, f));
        }
    }

    pub fn guide(&mut self, name: &str, fmla: &str) {
        if self.def.adj.len() + self.def.guides.len() >= MAX_GUIDES {
            return;
        }
        if let Some(f) = self.formula(fmla) {
            let v = self.var(name);
            self.def.guides.push((v, f));
        }
    }

    pub fn path(&mut self, w: f64, h: f64, filled: bool, stroked: bool) {
        self.def.paths.push(PathDef { w, h, filled, stroked, cmds: Vec::new() });
    }

    pub fn cmd(&mut self, c: Cmd) {
        let total: usize = self.def.paths.iter().map(|p| p.cmds.len()).sum();
        if total >= MAX_CMDS {
            return;
        }
        if self.def.paths.is_empty() {
            self.path(0.0, 0.0, true, true);
        }
        self.def.paths.last_mut().expect("a path").cmds.push(c);
    }

    /// The text rectangle, from names or numbers.
    pub fn rect(&mut self, l: &str, t: &str, r: &str, b: &str) {
        let r = [self.arg(l), self.arg(t), self.arg(r), self.arg(b)];
        self.def.rect = Some(r);
    }

    pub fn finish(self) -> Def {
        self.def
    }
}

/// Angles are 60,000ths of a degree.
fn rad(a: f64) -> f64 {
    (a / 60_000.0).to_radians()
}

fn eval(f: &Formula, v: &[f64]) -> f64 {
    let a = |i: usize| match f.args[i] {
        Arg::Num(x) => x,
        Arg::Var(k) => v.get(k as usize).copied().unwrap_or(0.0),
    };
    let r = match f.op {
        Op::MulDiv => {
            let z = a(2);
            if z == 0.0 { 0.0 } else { a(0) * a(1) / z }
        }
        Op::AddSub => a(0) + a(1) - a(2),
        Op::AddDiv => {
            let z = a(2);
            if z == 0.0 { 0.0 } else { (a(0) + a(1)) / z }
        }
        Op::IfElse => if a(0) > 0.0 { a(1) } else { a(2) },
        Op::Abs => a(0).abs(),
        Op::At2 => a(1).atan2(a(0)).to_degrees() * 60_000.0,
        Op::Cat2 => a(0) * a(2).atan2(a(1)).cos(),
        Op::Cos => a(0) * rad(a(1)).cos(),
        Op::Max => a(0).max(a(1)),
        Op::Min => a(0).min(a(1)),
        Op::Mod => (a(0) * a(0) + a(1) * a(1) + a(2) * a(2)).sqrt(),
        Op::Pin => {
            let (x, y, z) = (a(0), a(1), a(2));
            if y < x { x } else if y > z { z } else { y }
        }
        Op::Sat2 => a(0) * a(2).atan2(a(1)).sin(),
        Op::Sin => a(0) * rad(a(1)).sin(),
        Op::Sqrt => a(0).max(0.0).sqrt(),
        Op::Tan => a(0) * rad(a(1)).tan(),
        Op::Val => a(0),
    };
    if r.is_finite() { r.clamp(-1e12, 1e12) } else { 0.0 }
}

impl Def {
    /// Every variable's value in a `w` × `h` box.
    fn values(&self, wf: f64, hf: f64, adj: &[(Arc<str>, i64)]) -> Vec<f64> {
        let mut v = vec![0.0; self.names.len()];
        v[..BUILTINS.len()].copy_from_slice(&builtins(wf, hf));
        for (k, f) in &self.adj {
            let name = &self.names[*k as usize];
            v[*k as usize] = match adj.iter().find(|(n, _)| n == name) {
                Some((_, x)) => *x as f64,
                None => eval(f, &v),
            };
        }
        for (k, f) in &self.guides {
            v[*k as usize] = eval(f, &v);
        }
        v
    }

    /// Where its text goes in a `w` × `h` box: left, top, right, bottom
    /// (the whole box when the definition names none).
    pub fn text_rect(&self, w: i64, h: i64, adj: &[(Arc<str>, i64)]) -> [i64; 4] {
        let Some(r) = self.rect else { return [0, 0, w, h] };
        let v = self.values(w.max(0) as f64, h.max(0) as f64, adj);
        let get = |a: Arg| match a {
            Arg::Num(x) => x,
            Arg::Var(k) => v.get(k as usize).copied().unwrap_or(0.0),
        };
        let c = |x: f64| x.round().clamp(-1e12, 1e12) as i64;
        let [l, t, rr, b] = r.map(|a| c(get(a)));
        if rr > l && b > t { [l, t, rr, b] } else { [0, 0, w, h] }
    }

    /// The paths in a `w` × `h` box; `adj` overrides adjust values by name.
    pub fn paths(&self, w: i64, h: i64, adj: &[(Arc<str>, i64)]) -> Vec<Path> {
        let (wf, hf) = (w.max(0) as f64, h.max(0) as f64);
        let v = self.values(wf, hf, adj);
        let get = |a: Arg| match a {
            Arg::Num(x) => x,
            Arg::Var(k) => v.get(k as usize).copied().unwrap_or(0.0),
        };
        let mut out = Vec::with_capacity(self.paths.len());
        for p in &self.paths {
            // A path of its own size is scaled to the shape's.
            let (sx, sy) = (if p.w > 0.0 { wf / p.w } else { 1.0 }, if p.h > 0.0 { hf / p.h } else { 1.0 });
            let q = |x: f64, y: f64| ((x * sx).round().clamp(-1e12, 1e12) as i64, (y * sy).round().clamp(-1e12, 1e12) as i64);
            let mut cmds = Vec::with_capacity(p.cmds.len() + 4);
            let (mut cur, mut start) = ((0.0, 0.0), (0.0, 0.0));
            for c in &p.cmds {
                match *c {
                    Cmd::Move(x, y) => {
                        cur = (get(x), get(y));
                        start = cur;
                        let (a, b) = q(cur.0, cur.1);
                        cmds.push(PathCmd::Move(a, b));
                    }
                    Cmd::Line(x, y) => {
                        cur = (get(x), get(y));
                        let (a, b) = q(cur.0, cur.1);
                        cmds.push(PathCmd::Line(a, b));
                    }
                    Cmd::Quad(x1, y1, x, y) => {
                        let (c1, e) = ((get(x1), get(y1)), (get(x), get(y)));
                        let p1 = (cur.0 + 2.0 / 3.0 * (c1.0 - cur.0), cur.1 + 2.0 / 3.0 * (c1.1 - cur.1));
                        let p2 = (e.0 + 2.0 / 3.0 * (c1.0 - e.0), e.1 + 2.0 / 3.0 * (c1.1 - e.1));
                        let (a, b, c) = (q(p1.0, p1.1), q(p2.0, p2.1), q(e.0, e.1));
                        cmds.push(PathCmd::Cubic(a.0, a.1, b.0, b.1, c.0, c.1));
                        cur = e;
                    }
                    Cmd::Cubic(x1, y1, x2, y2, x, y) => {
                        let (a, b, c) = (q(get(x1), get(y1)), q(get(x2), get(y2)), q(get(x), get(y)));
                        cmds.push(PathCmd::Cubic(a.0, a.1, b.0, b.1, c.0, c.1));
                        cur = (get(x), get(y));
                    }
                    Cmd::Arc(wr, hr, st, sw) => {
                        cur = arc(&mut cmds, cur, get(wr), get(hr), get(st), get(sw), &q);
                    }
                    Cmd::Close => {
                        cmds.push(PathCmd::Close);
                        cur = start;
                    }
                }
            }
            out.push(Path { w, h, cmds, filled: p.filled, stroked: p.stroked });
        }
        out
    }
}

/// An `arcTo` from `cur`: the ellipse of radii `wr`, `hr` through `cur` at
/// angle `st`, swept by `sw` (60,000ths of a degree, clockwise on screen),
/// as Béziers of at most a quarter turn. The angles are as seen, so they
/// are turned into the ellipse's own parameter first. Returns the end.
fn arc(cmds: &mut Vec<PathCmd>, cur: (f64, f64), wr: f64, hr: f64, st: f64, sw: f64, q: &dyn Fn(f64, f64) -> (i64, i64)) -> (f64, f64) {
    let (wr, hr) = (wr.abs(), hr.abs());
    if wr == 0.0 || hr == 0.0 || sw == 0.0 {
        return cur;
    }
    let param = |a: f64| {
        let t = rad(a);
        (wr * t.sin()).atan2(hr * t.cos())
    };
    // At most one whole turn.
    let sw = sw.clamp(-21_600_000.0, 21_600_000.0);
    let t0 = param(st);
    // The end's parameter, past the start the way it sweeps.
    let mut t1 = param(st + sw);
    if sw > 0.0 {
        while t1 <= t0 {
            t1 += std::f64::consts::TAU;
        }
    } else {
        while t1 >= t0 {
            t1 -= std::f64::consts::TAU;
        }
    }
    let (cx, cy) = (cur.0 - wr * t0.cos(), cur.1 - hr * t0.sin());
    let total = t1 - t0;
    let n = ((total.abs() / std::f64::consts::FRAC_PI_2).ceil() as usize).clamp(1, 8);
    let d = total / n as f64;
    let k = 4.0 / 3.0 * (d / 4.0).tan();
    let mut a = t0;
    let mut end = cur;
    for _ in 0..n {
        let b = a + d;
        let (p0, p3) = ((cx + wr * a.cos(), cy + hr * a.sin()), (cx + wr * b.cos(), cy + hr * b.sin()));
        let p1 = (p0.0 - k * wr * a.sin(), p0.1 + k * hr * a.cos());
        let p2 = (p3.0 + k * wr * b.sin(), p3.1 - k * hr * b.cos());
        let (x1, x2, x3) = (q(p1.0, p1.1), q(p2.0, p2.1), q(p3.0, p3.1));
        cmds.push(PathCmd::Cubic(x1.0, x1.1, x2.0, x2.1, x3.0, x3.1));
        end = p3;
        a = b;
    }
    end
}

/// Every preset's definition, parsed once.
fn all() -> &'static HashMap<&'static str, Def> {
    static ALL: OnceLock<HashMap<&'static str, Def>> = OnceLock::new();
    ALL.get_or_init(|| {
        let mut m = HashMap::new();
        let mut cur: Option<(&'static str, Builder)> = None;
        for line in include_str!("presets.txt").lines() {
            let mut it = line.splitn(2, ' ');
            let (tag, rest) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
            if let Some(name) = tag.strip_prefix('=') {
                if let Some((n, b)) = cur.take() {
                    m.insert(n, b.finish());
                }
                cur = Some((name, Builder::new()));
                continue;
            }
            let Some((_, b)) = cur.as_mut() else { continue };
            let f: Vec<&str> = rest.split_ascii_whitespace().collect();
            let arg = |b: &mut Builder, i: usize| b.arg(f.get(i).copied().unwrap_or("0"));
            match tag {
                "a" | "g" => {
                    if let Some((name, fmla)) = rest.split_once(' ') {
                        if tag == "a" { b.adj(name, fmla) } else { b.guide(name, fmla) }
                    }
                }
                "r" => b.rect(f.first().copied().unwrap_or("l"), f.get(1).copied().unwrap_or("t"), f.get(2).copied().unwrap_or("r"), f.get(3).copied().unwrap_or("b")),
                "p" => {
                    let num = |i: usize| f.get(i).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
                    b.path(num(0), num(1), f.get(2) != Some(&"-"), f.get(3) != Some(&"0"));
                }
                "M" => {
                    let c = Cmd::Move(arg(b, 0), arg(b, 1));
                    b.cmd(c)
                }
                "L" => {
                    let c = Cmd::Line(arg(b, 0), arg(b, 1));
                    b.cmd(c)
                }
                "A" => {
                    let c = Cmd::Arc(arg(b, 0), arg(b, 1), arg(b, 2), arg(b, 3));
                    b.cmd(c)
                }
                "Q" => {
                    let c = Cmd::Quad(arg(b, 0), arg(b, 1), arg(b, 2), arg(b, 3));
                    b.cmd(c)
                }
                "C" => {
                    let c = Cmd::Cubic(arg(b, 0), arg(b, 1), arg(b, 2), arg(b, 3), arg(b, 4), arg(b, 5));
                    b.cmd(c)
                }
                "Z" => b.cmd(Cmd::Close),
                _ => {}
            }
        }
        if let Some((n, b)) = cur.take() {
            m.insert(n, b.finish());
        }
        m
    })
}

/// A preset's definition by name.
pub fn preset(name: &str) -> Option<&'static Def> {
    all().get(name)
}

/// Every preset's name.
pub fn names() -> impl Iterator<Item = &'static str> {
    all().keys().copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_rectangles_follow_the_definitions() {
        // An ellipse's text sits in the square inside it (about 14.6 % in).
        let [l, t, r, b] = preset("ellipse").unwrap().text_rect(10_000, 4_000, &[]);
        assert!((l - 1_464).abs() <= 2 && (t - 586).abs() <= 2 && (r - 8_536).abs() <= 2 && (b - 3_414).abs() <= 2, "{l} {t} {r} {b}");
        // A right arrow's in its body: the middle half of the height.
        let [_, t, _, b] = preset("rightArrow").unwrap().text_rect(10_000, 4_000, &[]);
        assert_eq!((t, b), (1_000, 3_000));
        assert_eq!(preset("rect").unwrap().text_rect(500, 300, &[]), [0, 0, 500, 300]);
    }

    fn bounds(p: &[Path]) -> (i64, i64, i64, i64) {
        let mut b = (i64::MAX, i64::MAX, i64::MIN, i64::MIN);
        for path in p {
            for c in &path.cmds {
                let pts: Vec<(i64, i64)> = match *c {
                    PathCmd::Move(x, y) | PathCmd::Line(x, y) => vec![(x, y)],
                    PathCmd::Cubic(_, _, _, _, x, y) => vec![(x, y)],
                    PathCmd::Close => vec![],
                };
                for (x, y) in pts {
                    b = (b.0.min(x), b.1.min(y), b.2.max(x), b.3.max(y));
                }
            }
        }
        b
    }

    #[test]
    fn every_preset_draws_inside_its_box() {
        let n = names().count();
        assert!(n >= 180, "{n} presets");
        for name in names() {
            let d = preset(name).unwrap();
            let p = d.paths(400_000, 300_000, &[]);
            assert!(!p.is_empty() && p.iter().any(|x| !x.cmds.is_empty()), "{name} draws");
            let (l, t, r, b) = bounds(&p);
            // Callouts, connectors and a few others reach out of the box by
            // design; nothing runs far off.
            assert!(l >= -400_000 && t >= -300_000 && r <= 800_000 && b <= 600_000, "{name}: {:?}", (l, t, r, b));
        }
    }

    #[test]
    fn rect_and_ellipse_are_what_they_say() {
        let p = preset("rect").unwrap().paths(1000, 500, &[]);
        assert_eq!(p[0].cmds, vec![PathCmd::Move(0, 0), PathCmd::Line(1000, 0), PathCmd::Line(1000, 500), PathCmd::Line(0, 500), PathCmd::Close]);
        let e = preset("ellipse").unwrap().paths(1000, 500, &[]);
        assert_eq!(bounds(&e), (0, 0, 1000, 500));
        // An adjust value moves a guide: a round rectangle's corner.
        let r0 = preset("roundRect").unwrap().paths(1000, 1000, &[]);
        let r1 = preset("roundRect").unwrap().paths(1000, 1000, &[("adj".into(), 50000)]);
        assert_ne!(r0, r1);
        assert_eq!(r1[0].cmds[0], PathCmd::Move(0, 500));
    }

    #[test]
    fn arcs_sweep_the_right_way() {
        // A quarter arc clockwise from the top of a circle ends at its right.
        let mut b = Builder::new();
        b.path(0.0, 0.0, true, true);
        let (x, y) = (b.arg("500"), b.arg("0"));
        b.cmd(Cmd::Move(x, y));
        let (r, s, w) = (b.arg("500"), b.arg("16200000"), b.arg("5400000"));
        b.cmd(Cmd::Arc(r, r, s, w));
        let p = b.finish().paths(1000, 1000, &[]);
        assert_eq!(p[0].cmds.last(), Some(&PathCmd::Cubic(776, 0, 1000, 224, 1000, 500)));
    }
}
