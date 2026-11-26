//! Solver… (docs/spec/officina.md §16.162), as Excel's Solver add-in: set
//! an objective cell to its largest, smallest or a given value by changing
//! variable cells, subject to constraints. A linear model is solved
//! exactly (two-phase simplex, Bland's rule), with branch and bound for
//! integer and binary cells; any other model by Nelder–Mead on a penalty
//! for broken constraints, which finds a local optimum. The answer goes in
//! as one undo step, and `report` adds an Answer Report sheet.

use std::time::{Duration, Instant};

use officina_grid::addr::{a1, CellAddr, Rect};
use officina_grid::book::Input;
use officina_grid::ops::GridOp;
use officina_grid::{Value, Workbook};

use crate::doc::Doc;
use crate::whatif::parse_area;

/// Excel's limits: 200 variable cells; constraints beyond bounds.
const MAX_VARS: usize = 200;
const MAX_CONSTRAINTS: usize = 1000;
/// How long Solver may work, and how many models it may evaluate.
const DEADLINE: Duration = Duration::from_secs(20);
const MAX_EVALS: usize = 200_000;
/// Branch-and-bound nodes, and simplex pivots per solve.
const MAX_NODES: usize = 5000;
const MAX_PIVOTS: usize = 50_000;
/// Excel's default constraint precision.
const PRECISION: f64 = 1e-6;
const EPS: f64 = 1e-9;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Goal {
    Max,
    Min,
    Value(f64),
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Op {
    Le,
    Ge,
    Eq,
}

impl Op {
    fn text(self) -> &'static str {
        match self {
            Op::Le => "<=",
            Op::Ge => ">=",
            Op::Eq => "=",
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Rhs {
    Num(f64),
    Cell(CellAddr),
}

#[derive(Clone, Copy, Debug)]
struct Con {
    lhs: CellAddr,
    op: Op,
    rhs: Rhs,
}

struct Model {
    obj: CellAddr,
    goal: Goal,
    vars: Vec<CellAddr>,
    ints: Vec<bool>,
    cons: Vec<Con>,
    nonneg: bool,
    method: Method,
    report: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Method {
    Auto,
    Simplex,
    Nonlinear,
}

/// The bar's text to start with: the cursor's formula as the objective.
pub fn start(d: &Doc, cursor: (u32, u32)) -> String {
    let at = CellAddr::new(d.sheet, cursor.0, cursor.1);
    if d.wb.formula_text(at).is_some() { format!("max {}, by ", a1(cursor.0, cursor.1)) } else { String::new() }
}

fn cells(r: &Rect) -> Vec<CellAddr> {
    (r.r0..=r.r1).flat_map(|row| (r.c0..=r.c1).map(move |c| CellAddr::new(r.sheet, row, c))).collect()
}

fn parse(d: &Doc, typed: &str) -> Result<Model, String> {
    let wb = &d.wb;
    let here = d.sheet;
    let mut parts = typed.split(',').map(str::trim).filter(|p| !p.is_empty());
    let first = parts.next().ok_or("Type the objective: max B10, min B10 or B10 = 50; then by and the cells to change.")?;
    let low = first.to_lowercase();
    let one_cell = |t: &str| -> Result<CellAddr, String> {
        let r = parse_area(wb, here, t)?;
        if r.r0 != r.r1 || r.c0 != r.c1 { return Err(format!("{t:?}: the objective is one cell.")); }
        Ok(CellAddr::new(r.sheet, r.r0, r.c0))
    };
    let (obj, goal) = if let Some(c) = low.strip_prefix("max ") {
        (one_cell(c)?, Goal::Max)
    } else if let Some(c) = low.strip_prefix("min ") {
        (one_cell(c)?, Goal::Min)
    } else if let Some((c, v)) = first.split_once('=') {
        let v: f64 = v.trim().parse().ok().filter(|v: &f64| v.is_finite()).ok_or_else(|| format!("{:?} isn't a number to reach.", v.trim()))?;
        (one_cell(c)?, Goal::Value(v))
    } else {
        return Err("Start with the objective: max B10, min B10 or B10 = 50.".into());
    };
    let mut m = Model { obj, goal, vars: Vec::new(), ints: Vec::new(), cons: Vec::new(), nonneg: true, method: Method::Auto, report: false };
    let mut int_cells = Vec::new();
    for p in parts {
        let low = p.to_lowercase();
        match low.as_str() {
            "nonlinear" | "grg" | "grg nonlinear" => { m.method = Method::Nonlinear; continue; }
            "simplex" | "simplex lp" | "linear" => { m.method = Method::Simplex; continue; }
            "report" | "answer report" => { m.report = true; continue; }
            "unconstrained" | "negative" => { m.nonneg = false; continue; }
            _ => {}
        }
        if let Some(r) = low.strip_prefix("by ") {
            for t in r.split_whitespace() {
                for at in cells(&parse_area(wb, here, t)?) {
                    if !m.vars.contains(&at) { m.vars.push(at); }
                    if m.vars.len() > MAX_VARS { return Err(format!("Solver changes at most {MAX_VARS} cells.")); }
                }
            }
            continue;
        }
        if let Some(t) = low.strip_suffix(" int").or_else(|| low.strip_suffix(" integer")) {
            int_cells.extend(cells(&parse_area(wb, here, t)?).into_iter().map(|c| (c, false)));
            continue;
        }
        if let Some(t) = low.strip_suffix(" bin").or_else(|| low.strip_suffix(" binary")) {
            int_cells.extend(cells(&parse_area(wb, here, t)?).into_iter().map(|c| (c, true)));
            continue;
        }
        let (i, op, len) = ["<=", ">=", "=<", "=>", "="].iter().find_map(|o| p.find(o).map(|i| (i, *o, o.len()))).ok_or_else(|| format!("{p:?}: a constraint is CELLS <= value, >=, =, or CELLS int / bin."))?;
        let op = match op { "<=" | "=<" => Op::Le, ">=" | "=>" => Op::Ge, _ => Op::Eq };
        let lhs = cells(&parse_area(wb, here, &p[..i])?);
        let right = p[i + len..].trim();
        let rhs: Vec<Rhs> = match right.parse::<f64>() {
            Ok(v) if v.is_finite() => vec![Rhs::Num(v)],
            _ => cells(&parse_area(wb, here, right)?).into_iter().map(Rhs::Cell).collect(),
        };
        if rhs.len() != 1 && rhs.len() != lhs.len() {
            return Err(format!("{p:?}: the right side is one value or as many cells as the left."));
        }
        for (k, &l) in lhs.iter().enumerate() {
            m.cons.push(Con { lhs: l, op, rhs: rhs[if rhs.len() == 1 { 0 } else { k }] });
        }
        if m.cons.len() > MAX_CONSTRAINTS { return Err(format!("Use at most {MAX_CONSTRAINTS} constraints.")); }
    }
    if m.vars.is_empty() {
        return Err("Say which cells to change: by B1:B3.".into());
    }
    m.ints = vec![false; m.vars.len()];
    for (c, bin) in int_cells {
        let i = m.vars.iter().position(|v| *v == c).ok_or_else(|| format!("{} is marked int or bin but isn't a cell Solver changes.", a1(c.row, c.col)))?;
        m.ints[i] = true;
        if bin {
            m.cons.push(Con { lhs: c, op: Op::Ge, rhs: Rhs::Num(0.0) });
            m.cons.push(Con { lhs: c, op: Op::Le, rhs: Rhs::Num(1.0) });
        }
    }
    for &v in &m.vars {
        if v == m.obj { return Err("The objective can't be one of the cells Solver changes.".into()); }
        if wb.formula_text(v).is_some() { return Err(format!("{} holds a formula; Solver changes cells that hold numbers.", a1(v.row, v.col))); }
        if !matches!(wb.get(v), Value::Num(_) | Value::Empty) { return Err(format!("{} holds text; Solver changes cells that hold numbers.", a1(v.row, v.col))); }
    }
    if wb.formula_text(m.obj).is_none() {
        return Err(format!("{} has no formula, so changing other cells can't move it.", a1(m.obj.row, m.obj.col)));
    }
    Ok(m)
}

/// Sets the variables to `x` and reads the objective and each constraint
/// as lhs − rhs. `None` when any isn't a number.
struct Evaluator<'a> {
    wb: &'a mut Workbook,
    m: &'a Model,
    evals: usize,
    until: Instant,
}

impl Evaluator<'_> {
    fn out_of_time(&self) -> bool {
        self.evals >= MAX_EVALS || Instant::now() > self.until
    }
    fn eval(&mut self, x: &[f64]) -> Option<(f64, Vec<f64>)> {
        self.evals += 1;
        for (&v, &xi) in self.m.vars.iter().zip(x) {
            self.wb.set_value(v, Value::Num(xi));
        }
        self.wb.recalc();
        let n = |wb: &Workbook, a: CellAddr| match wb.get(a) { Value::Num(x) if x.is_finite() => Some(x), Value::Empty => Some(0.0), _ => None };
        let obj = n(self.wb, self.m.obj)?;
        let mut g = Vec::with_capacity(self.m.cons.len());
        for c in &self.m.cons {
            let r = match c.rhs { Rhs::Num(v) => v, Rhs::Cell(a) => n(self.wb, a)? };
            g.push(n(self.wb, c.lhs)? - r);
        }
        Some((obj, g))
    }
}

/// The model as linear functions, `value(x) = c0 + a·x`, if it is one.
struct Linear {
    obj: (f64, Vec<f64>),
    cons: Vec<(f64, Vec<f64>, Op)>,
}

fn linearise(ev: &mut Evaluator, n: usize) -> Option<Linear> {
    let (o0, g0) = ev.eval(&vec![0.0; n])?;
    let mut obj = vec![0.0; n];
    let mut cons = vec![vec![0.0; n]; g0.len()];
    for i in 0..n {
        let mut e = vec![0.0; n];
        e[i] = 1.0;
        let (o, g) = ev.eval(&e)?;
        obj[i] = o - o0;
        for (k, gk) in g.iter().enumerate() {
            cons[k][i] = gk - g0[k];
        }
    }
    // Two other points must agree, or it isn't linear.
    for scale in [1.0, -3.5] {
        let x: Vec<f64> = (0..n).map(|i| scale * (0.75 + 0.37 * (i % 7) as f64)).collect();
        let (o, g) = ev.eval(&x)?;
        let ok = |c0: f64, a: &[f64], got: f64| {
            let terms: Vec<f64> = a.iter().zip(&x).map(|(a, x)| a * x).collect();
            let want = c0 + terms.iter().sum::<f64>();
            (want - got).abs() <= 1e-7 * (1.0 + got.abs() + c0.abs() + terms.iter().map(|t| t.abs()).sum::<f64>())
        };
        if !ok(o0, &obj, o) {
            return None;
        }
        for k in 0..g.len() {
            if !ok(g0[k], &cons[k], g[k]) {
                return None;
            }
        }
    }
    let ops = ev.m.cons.iter().map(|c| c.op);
    Some(Linear { obj: (o0, obj), cons: g0.into_iter().zip(cons).zip(ops).map(|((c0, a), op)| (c0, a, op)).collect() })
}

#[derive(Debug, PartialEq)]
enum LpError {
    Infeasible,
    Unbounded,
    TooLong,
}

fn pivot(t: &mut [Vec<f64>], r: usize, j: usize) {
    let p = t[r][j];
    t[r].iter_mut().for_each(|x| *x /= p);
    let pr = t[r].clone();
    for (i, row) in t.iter_mut().enumerate() {
        if i != r && row[j] != 0.0 {
            let f = row[j];
            row.iter_mut().zip(&pr).for_each(|(x, p)| *x -= f * p);
        }
    }
}

/// Maximises `c·x` over x ≥ 0 subject to rows `a·x op b`: two phases,
/// Bland's rule so it can't cycle.
fn simplex(c: &[f64], rows: &[(Vec<f64>, Op, f64)]) -> Result<Vec<f64>, LpError> {
    let n = c.len();
    let m = rows.len();
    // Each row with b ≥ 0; columns: structural, slack or surplus, artificial.
    let norm: Vec<(Vec<f64>, Op, f64)> = rows
        .iter()
        .map(|(a, op, b)| if *b < 0.0 { (a.iter().map(|x| -x).collect(), match op { Op::Le => Op::Ge, Op::Ge => Op::Le, Op::Eq => Op::Eq }, -b) } else { (a.clone(), *op, *b) })
        .collect();
    let n_slack = norm.iter().filter(|r| r.1 != Op::Eq).count();
    let n_art = norm.iter().filter(|r| r.1 != Op::Le).count();
    let art0 = n + n_slack;
    let total = art0 + n_art;
    let mut t = vec![vec![0.0; total + 1]; m];
    let mut basis = vec![0usize; m];
    let (mut s, mut a) = (n, art0);
    for (i, (coef, op, b)) in norm.iter().enumerate() {
        t[i][..n].copy_from_slice(coef);
        t[i][total] = *b;
        match op {
            Op::Le => {
                t[i][s] = 1.0;
                basis[i] = s;
                s += 1;
            }
            Op::Ge => {
                t[i][s] = -1.0;
                s += 1;
                t[i][a] = 1.0;
                basis[i] = a;
                a += 1;
            }
            Op::Eq => {
                t[i][a] = 1.0;
                basis[i] = a;
                a += 1;
            }
        }
    }
    let mut pivots = 0;
    let mut run = |t: &mut Vec<Vec<f64>>, basis: &mut Vec<usize>, cost: &[f64], allowed: usize| -> Result<(), LpError> {
        loop {
            pivots += 1;
            if pivots > MAX_PIVOTS {
                return Err(LpError::TooLong);
            }
            let enter = (0..allowed).find(|&j| !basis.contains(&j) && cost[j] - (0..t.len()).map(|i| cost[basis[i]] * t[i][j]).sum::<f64>() > EPS);
            let Some(j) = enter else { return Ok(()) };
            let mut leave: Option<(usize, f64)> = None;
            for i in 0..t.len() {
                if t[i][j] > EPS {
                    let ratio = t[i][total] / t[i][j];
                    if leave.is_none_or(|(l, best)| ratio < best - EPS || (ratio <= best + EPS && basis[i] < basis[l])) {
                        leave = Some((i, ratio));
                    }
                }
            }
            let Some((r, _)) = leave else { return Err(LpError::Unbounded) };
            pivot(t, r, j);
            basis[r] = j;
        }
    };
    if n_art > 0 {
        let mut cost = vec![0.0; total];
        cost[art0..].iter_mut().for_each(|x| *x = -1.0);
        run(&mut t, &mut basis, &cost, total)?;
        let left: f64 = (0..m).filter(|&i| basis[i] >= art0).map(|i| t[i][total]).sum();
        if left > 1e-7 {
            return Err(LpError::Infeasible);
        }
        // Artificials still in the basis at zero: swap in another column.
        for i in 0..m {
            if basis[i] >= art0 {
                if let Some(j) = (0..art0).find(|&j| t[i][j].abs() > EPS && !basis.contains(&j)) {
                    pivot(&mut t, i, j);
                    basis[i] = j;
                }
            }
        }
    }
    let mut cost = vec![0.0; total];
    cost[..n].copy_from_slice(c);
    run(&mut t, &mut basis, &cost, art0)?;
    let mut x = vec![0.0; n];
    for i in 0..m {
        if basis[i] < n {
            x[basis[i]] = t[i][total];
        }
    }
    Ok(x)
}

/// The linear model as an LP, with x = p − q when the variables may go
/// negative, and `bounds` added by branch and bound.
fn solve_lp(lin: &Linear, goal: Goal, nonneg: bool, bounds: &[(usize, Op, f64)]) -> Result<Vec<f64>, LpError> {
    let n = lin.obj.1.len();
    let widen = |a: &[f64]| -> Vec<f64> { if nonneg { a.to_vec() } else { a.iter().copied().chain(a.iter().map(|x| -x)).collect() } };
    // c0 + a·x op 0, so a·x op −c0.
    let mut rows: Vec<(Vec<f64>, Op, f64)> = lin.cons.iter().map(|(c0, a, op)| (widen(a), *op, -c0)).collect();
    for &(i, op, v) in bounds {
        let mut a = vec![0.0; n];
        a[i] = 1.0;
        rows.push((widen(&a), op, v));
    }
    let c = match goal {
        Goal::Max => widen(&lin.obj.1),
        Goal::Min => widen(&lin.obj.1.iter().map(|x| -x).collect::<Vec<_>>()),
        Goal::Value(v) => {
            rows.push((widen(&lin.obj.1), Op::Eq, v - lin.obj.0));
            vec![0.0; if nonneg { n } else { 2 * n }]
        }
    };
    let y = simplex(&c, &rows)?;
    Ok(if nonneg { y } else { (0..n).map(|i| y[i] - y[n + i]).collect() })
}

fn value_of(lin: &Linear, x: &[f64]) -> f64 {
    lin.obj.0 + lin.obj.1.iter().zip(x).map(|(a, x)| a * x).sum::<f64>()
}

/// Branch and bound on the integer cells, depth first, the most fractional
/// cell first. `Err` when no node is feasible.
fn branch(lin: &Linear, m: &Model, until: Instant) -> Result<(Vec<f64>, bool), LpError> {
    let better = |a: f64, b: f64| match m.goal { Goal::Min => a < b - 1e-9, _ => a > b + 1e-9 };
    let mut best: Option<(f64, Vec<f64>)> = None;
    let mut stack: Vec<Vec<(usize, Op, f64)>> = vec![Vec::new()];
    let mut nodes = 0;
    let mut unbounded = false;
    let mut complete = true;
    while let Some(bounds) = stack.pop() {
        nodes += 1;
        if nodes > MAX_NODES || Instant::now() > until {
            complete = false;
            break;
        }
        let x = match solve_lp(lin, m.goal, m.nonneg, &bounds) {
            Ok(x) => x,
            Err(LpError::Unbounded) => { unbounded = true; continue; }
            Err(LpError::TooLong) => { complete = false; continue; }
            Err(LpError::Infeasible) => continue,
        };
        let v = value_of(lin, &x);
        if let (Some((b, _)), false) = (&best, matches!(m.goal, Goal::Value(_))) {
            if !better(v, *b) {
                continue;
            }
        }
        let frac = (0..x.len()).filter(|&i| m.ints[i]).map(|i| (i, (x[i] - x[i].round()).abs())).filter(|&(_, f)| f > 1e-6).max_by(|a, b| a.1.total_cmp(&b.1));
        match frac {
            None => {
                let x: Vec<f64> = x.iter().zip(&m.ints).map(|(&v, &int)| if int { v.round() } else { v }).collect();
                best = Some((v, x));
                if matches!(m.goal, Goal::Value(_)) {
                    break;
                }
            }
            Some((i, _)) => {
                let mut up = bounds.clone();
                up.push((i, Op::Ge, x[i].ceil()));
                let mut down = bounds;
                down.push((i, Op::Le, x[i].floor()));
                stack.push(up);
                stack.push(down);
            }
        }
    }
    match best {
        Some((_, x)) => Ok((x, complete)),
        None if unbounded => Err(LpError::Unbounded),
        None if !complete => Err(LpError::TooLong),
        None => Err(LpError::Infeasible),
    }
}

/// How far `g` (lhs − rhs) breaks its constraint, 0 when it holds.
fn breach(g: f64, op: Op) -> f64 {
    match op {
        Op::Le => g.max(0.0),
        Op::Ge => (-g).max(0.0),
        Op::Eq => g.abs(),
    }
}

/// Nelder–Mead on the objective plus a growing penalty for broken
/// constraints (and negative cells unless `unconstrained`).
fn nonlinear(ev: &mut Evaluator, x0: Vec<f64>) -> Option<Vec<f64>> {
    let m = ev.m;
    let n = x0.len();
    let sign = match m.goal { Goal::Max => -1.0, _ => 1.0 };
    let mut best = x0;
    for mu in [1e2, 1e4, 1e6, 1e8] {
        let f = |ev: &mut Evaluator, x: &[f64]| -> f64 {
            let Some((o, g)) = ev.eval(x) else { return f64::INFINITY };
            let obj = match m.goal { Goal::Value(v) => (o - v).powi(2), _ => sign * o };
            let mut pen: f64 = g.iter().zip(&m.cons).map(|(&g, c)| breach(g, c.op).powi(2)).sum();
            if m.nonneg {
                pen += x.iter().map(|&v| v.min(0.0).powi(2)).sum::<f64>();
            }
            obj + mu * pen
        };
        let mut pts: Vec<Vec<f64>> = vec![best.clone()];
        for i in 0..n {
            let mut p = best.clone();
            p[i] += (best[i].abs() * 0.1).max(1.0);
            pts.push(p);
        }
        let mut vals: Vec<f64> = pts.iter().map(|p| f(ev, p)).collect();
        for _ in 0..(400 * (n + 1)).min(20_000) {
            if ev.out_of_time() {
                break;
            }
            let mut idx: Vec<usize> = (0..=n).collect();
            idx.sort_by(|&a, &b| vals[a].total_cmp(&vals[b]));
            let (lo, hi, second) = (idx[0], idx[n], idx[n.saturating_sub(1)]);
            if (vals[hi] - vals[lo]).abs() <= 1e-12 * (1.0 + vals[lo].abs()) && pts.iter().all(|p| p.iter().zip(&pts[lo]).all(|(a, b)| (a - b).abs() <= 1e-10 * (1.0 + b.abs()))) {
                break;
            }
            let centroid: Vec<f64> = (0..n).map(|j| idx[..n].iter().map(|&i| pts[i][j]).sum::<f64>() / n as f64).collect();
            let along = |t: f64| -> Vec<f64> { (0..n).map(|j| centroid[j] + t * (pts[hi][j] - centroid[j])).collect() };
            let r = along(-1.0);
            let fr = f(ev, &r);
            if fr < vals[lo] {
                let e = along(-2.0);
                let fe = f(ev, &e);
                if fe < fr { pts[hi] = e; vals[hi] = fe; } else { pts[hi] = r; vals[hi] = fr; }
            } else if fr < vals[second] {
                pts[hi] = r;
                vals[hi] = fr;
            } else {
                let c = along(if fr < vals[hi] { -0.5 } else { 0.5 });
                let fc = f(ev, &c);
                if fc < vals[hi].min(fr) {
                    pts[hi] = c;
                    vals[hi] = fc;
                } else {
                    for i in 0..=n {
                        if i != lo {
                            pts[i] = (0..n).map(|j| pts[lo][j] + 0.5 * (pts[i][j] - pts[lo][j])).collect();
                            vals[i] = f(ev, &pts[i]);
                        }
                    }
                }
            }
        }
        let lo = (0..=n).min_by(|&a, &b| vals[a].total_cmp(&vals[b]))?;
        if !vals[lo].is_finite() {
            return None;
        }
        best = pts[lo].clone();
    }
    Some(best)
}

/// Whether `x` keeps every constraint to Excel's precision.
fn feasible(m: &Model, x: &[f64], g: &[f64], wb: &Workbook) -> bool {
    let rhs = |c: &Con| match c.rhs { Rhs::Num(v) => v, Rhs::Cell(a) => match wb.get(a) { Value::Num(v) => v, _ => 0.0 } };
    g.iter().zip(&m.cons).all(|(&g, c)| breach(g, c.op) <= PRECISION * (1.0 + rhs(c).abs()))
        && (!m.nonneg || x.iter().all(|&v| v >= -PRECISION))
}

/// Solver: `max B10, by B1:B3, B1:B3 >= 0, B1 int, report`.
pub fn run(d: &mut Doc, typed: &str) -> Result<String, String> {
    let m = parse(d, typed)?;
    if d.wb.recalc_pending() {
        d.wb.recalc();
    }
    let n = m.vars.len();
    let original: Vec<Value> = m.vars.iter().map(|&v| d.wb.get(v)).collect();
    let x0: Vec<f64> = original.iter().map(|v| if let Value::Num(x) = v { *x } else { 0.0 }).collect();
    let obj0 = d.wb.get(m.obj);
    let any_int = m.ints.iter().any(|&i| i);
    let until = Instant::now() + DEADLINE;
    let mut ev = Evaluator { wb: &mut d.wb, m: &m, evals: 0, until };
    let lin = if m.method == Method::Nonlinear { None } else { linearise(&mut ev, n) };
    let outcome: Result<(Vec<f64>, &str), String> = match (&lin, m.method) {
        (None, Method::Simplex) => Err("The model isn't linear, so Simplex can't solve it: leave out simplex, or say nonlinear.".into()),
        (None, _) if any_int => Err("Integer and binary cells need a linear model here.".into()),
        (Some(lin), _) => match branch(lin, &m, until) {
            Ok((x, true)) => Ok((x, if any_int { "Solver found an integer solution. All constraints are satisfied." } else { "Solver found a solution. All constraints and optimality conditions are satisfied." })),
            Ok((x, false)) => Ok((x, "Solver stopped at its limit; this is the best integer solution it found.")),
            Err(LpError::Infeasible) => Err("Solver could not find a feasible solution.".into()),
            Err(LpError::Unbounded) => Err("The objective cell's values do not converge: the model is unbounded. Add a constraint.".into()),
            Err(LpError::TooLong) => Err("Solver stopped at its limit without a solution.".into()),
        },
        (None, _) => match nonlinear(&mut ev, x0.clone()) {
            Some(x) => Ok((x, "Solver found a solution (nonlinear: a local optimum). All constraints are satisfied.")),
            None => Err("Solver could not find a point where the model gives numbers.".into()),
        },
    };
    // Check the answer on the sheet itself, then put the cells back.
    let checked = match outcome {
        Ok((x, say)) => match ev.eval(&x) {
            Some((o, g)) if feasible(&m, &x, &g, ev.wb) => {
                let hit = match m.goal { Goal::Value(v) => (o - v).abs() <= PRECISION * (1.0 + v.abs()), _ => true };
                if hit { Ok((x, o, g, say)) } else { Err("Solver could not reach the value set for the objective.".into()) }
            }
            Some(_) => Err("Solver could not find a feasible solution.".into()),
            None => Err("The model gives no number at Solver's answer.".into()),
        },
        Err(e) => Err(e),
    };
    for (&v, old) in m.vars.iter().zip(&original) {
        d.wb.set_value(v, old.clone());
    }
    d.wb.recalc();
    let (x, obj, g, say) = checked?;
    // 15 significant digits, as a typed number keeps.
    let x: Vec<f64> = x.iter().map(|&v| { let y: f64 = format!("{v:.14e}").parse().unwrap_or(v); if y == 0.0 { 0.0 } else { y } }).collect();
    let ops = m.vars.iter().zip(&x).map(|(&v, &val)| GridOp::set(v, Input::Value(Value::Num(val)))).collect();
    d.status.clear();
    if !d.run_action("Solver", ops) {
        return Err(std::mem::take(&mut d.status));
    }
    d.wb.recalc();
    if m.report {
        report(d, &m, &x0, &x, obj0, obj, &g, say);
        return Ok(format!("{say} The Answer Report is on its own sheet."));
    }
    Ok(say.to_string())
}

fn dollar(a: CellAddr) -> String {
    format!("${}${}", officina_grid::addr::col_name(a.col), a.row as u64 + 1)
}

#[allow(clippy::too_many_arguments)]
fn report(d: &mut Doc, m: &Model, x0: &[f64], x: &[f64], obj0: Value, obj: f64, g: &[f64], say: &str) {
    use crate::analysis::{num, text};
    let mut rows = vec![
        vec![text("Solver Answer Report")],
        vec![text(&format!("Result: {say}"))],
        Vec::new(),
        vec![text(match m.goal { Goal::Max => "Objective Cell (Max)", Goal::Min => "Objective Cell (Min)", Goal::Value(_) => "Objective Cell (Value Of)" })],
        vec![text("Cell"), text("Original Value"), text("Final Value")],
        vec![text(&dollar(m.obj)), Input::Value(obj0), num(obj)],
        Vec::new(),
        vec![text("Variable Cells")],
        vec![text("Cell"), text("Original Value"), text("Final Value"), text("Integer")],
    ];
    for i in 0..m.vars.len() {
        rows.push(vec![text(&dollar(m.vars[i])), num(x0[i]), num(x[i]), text(if m.ints[i] { "Integer" } else { "Contin" })]);
    }
    rows.push(Vec::new());
    rows.push(vec![text("Constraints")]);
    rows.push(vec![text("Cell"), text("Cell Value"), text("Formula"), text("Status"), text("Slack")]);
    for (c, &gv) in m.cons.iter().zip(g) {
        let rhs = match c.rhs { Rhs::Num(v) => crate::data::show_num(v), Rhs::Cell(a) => dollar(a) };
        let value = d.wb.get(c.lhs);
        let size = match c.rhs { Rhs::Num(v) => v.abs(), Rhs::Cell(a) => match d.wb.get(a) { Value::Num(v) => v.abs(), _ => 0.0 } };
        let binding = gv.abs() <= PRECISION * (1.0 + size);
        rows.push(vec![text(&dollar(c.lhs)), Input::Value(value), text(&format!("{}{}{rhs}", dollar(c.lhs), c.op.text())), text(if binding { "Binding" } else { "Not Binding" }), num(if binding { 0.0 } else { gv.abs() })]);
    }
    for (i, &v) in m.vars.iter().enumerate() {
        if m.ints[i] {
            rows.push(vec![text(&dollar(v)), num(x[i]), text(&format!("{}=Integer", dollar(v))), text("Binding"), num(0.0)]);
        }
    }
    let base = "Answer Report";
    let name = if d.wb.sheet_id(base).is_none() { base.to_string() } else { crate::data::copy_name(&d.wb, base) };
    let count = d.wb.sheet_count() as u16;
    d.extend(vec![GridOp::add_sheet(name)]);
    if d.wb.sheet_count() as u16 > count {
        d.extend(vec![GridOp::block(count, 0, 0, rows)]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn val(d: &Doc, r: u32, c: u32) -> f64 {
        match d.wb.get(CellAddr::new(0, r, c)) {
            Value::Num(x) => x,
            Value::Empty => 0.0,
            o => panic!("{o:?}"),
        }
    }

    /// Two products: profit 3x + 5y, machine hours x ≤ 4, 2y ≤ 12,
    /// 3x + 2y ≤ 18. The textbook answer is x = 2, y = 6, profit 36.
    fn model() -> Doc {
        let mut d = Doc::from_csv("s.csv", b"x,y\n0,0\n").unwrap().0;
        d.type_into(2, 0, "=3*A2+5*B2");
        d.type_into(3, 0, "=A2");
        d.type_into(3, 1, "=2*B2");
        d.type_into(3, 2, "=3*A2+2*B2");
        d.wb.recalc();
        d
    }

    #[test]
    fn simplex_finds_the_textbook_optimum_in_one_step() {
        let mut d = model();
        assert_eq!(start(&d, (2, 0)), "max A3, by ");
        let say = run(&mut d, "max A3, by A2:B2, A4 <= 4, B4 <= 12, C4 <= 18, report").unwrap();
        assert!(say.starts_with("Solver found a solution."), "{say}");
        assert_eq!((val(&d, 1, 0), val(&d, 1, 1), val(&d, 2, 0)), (2.0, 6.0, 36.0));
        // The report: binding B4 and C4, A4 with slack 2.
        let rep = d.wb.sheet_id("Answer Report").unwrap();
        let cell = |r, c| d.wb.get(CellAddr::new(rep, r, c));
        assert_eq!(cell(5, 2), Value::Num(36.0));
        assert_eq!(cell(14, 2), Value::str("$A$4<=4"));
        assert_eq!((cell(14, 3), cell(14, 4)), (Value::str("Not Binding"), Value::Num(2.0)));
        assert_eq!(cell(15, 3), Value::str("Binding"));
        // One step undoes the answer and its report.
        assert!(d.undo());
        assert_eq!((val(&d, 1, 0), val(&d, 1, 1)), (0.0, 0.0));
        assert!(d.wb.sheet_id("Answer Report").is_none());
    }

    #[test]
    fn integers_minimum_value_and_failures() {
        let mut d = model();
        // Maximise with C4 ≤ 17 and integer cells: x = 1, y = 6 or x = 2, y = 5 (33 or 31) → 33.
        run(&mut d, "max A3, by A2:B2, A4 <= 4, B4 <= 12, C4 <= 17, A2:B2 int").unwrap();
        assert_eq!((val(&d, 1, 0), val(&d, 1, 1)), (1.0, 6.0));
        // Value of: profit 15 using only y.
        run(&mut d, "A3 = 15, by A2:B2, A2 = 0").unwrap();
        assert_eq!((val(&d, 1, 0), val(&d, 1, 1)), (0.0, 3.0));
        // Min with 3x + 2y ≥ 9: all x costs 9, all y 22.5.
        run(&mut d, "min A3, by A2:B2, C4 >= 9").unwrap();
        assert_eq!((val(&d, 1, 0), val(&d, 1, 1)), (3.0, 0.0));
        assert!(run(&mut d, "max A3, by A2:B2").unwrap_err().contains("unbounded"));
        assert!(run(&mut d, "max A3, by A2:B2, A4 <= 4, A4 >= 5").unwrap_err().contains("feasible"));
        assert!(run(&mut d, "max A3, by A3").unwrap_err().contains("objective"));
        assert!(run(&mut d, "max A3").unwrap_err().contains("by"));
    }

    #[test]
    fn a_nonlinear_model_reaches_a_local_optimum() {
        // Minimise (x − 3)² + (y + 1)² + 2 with y unconstrained: 2 at (3, −1).
        let mut d = Doc::from_csv("n.csv", b"1,1\n").unwrap().0;
        d.type_into(1, 0, "=(A1-3)^2+(B1+1)^2+2");
        d.wb.recalc();
        let say = run(&mut d, "min A2, by A1:B1, unconstrained").unwrap();
        assert!(say.contains("local optimum"), "{say}");
        assert!((val(&d, 0, 0) - 3.0).abs() < 1e-4 && (val(&d, 0, 1) + 1.0).abs() < 1e-4, "{} {}", val(&d, 0, 0), val(&d, 0, 1));
        // With the cells kept non-negative, y stops at 0.
        run(&mut d, "min A2, by A1:B1").unwrap();
        assert!(val(&d, 0, 1).abs() < 1e-3, "{}", val(&d, 0, 1));
        assert!(run(&mut d, "min A2, by A1:B1, simplex").unwrap_err().contains("isn't linear"));
    }

    #[test]
    fn simplex_alone() {
        // Max x + y, x + 2y ≤ 4, 3x + y ≤ 6, x − y = 0 → x = y = 4/3.
        let x = simplex(&[1.0, 1.0], &[(vec![1.0, 2.0], Op::Le, 4.0), (vec![3.0, 1.0], Op::Le, 6.0), (vec![1.0, -1.0], Op::Eq, 0.0)]).unwrap();
        assert!((x[0] - 4.0 / 3.0).abs() < 1e-9 && (x[1] - 4.0 / 3.0).abs() < 1e-9);
        assert_eq!(simplex(&[1.0], &[(vec![1.0], Op::Ge, 2.0), (vec![1.0], Op::Le, 1.0)]), Err(LpError::Infeasible));
        assert_eq!(simplex(&[1.0], &[(vec![1.0], Op::Ge, 2.0)]), Err(LpError::Unbounded));
    }
}
