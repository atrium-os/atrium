//! More statistics (docs/spec/officina.md §16.138): KURT, SKEW and SKEW.P,
//! PERCENTRANK and its .INC and .EXC forms, TRIMMEAN, FREQUENCY, PROB, the
//! A forms of the variance and standard deviation, and AGGREGATE.

use super::funcs::each_num;
use super::more::percentile_vec;
use super::stats::a_values;
use super::{Array, Ctx, Ev, MAX_ARRAY};
use crate::formula::Expr;
use crate::value::{ErrorKind, Value};
use std::sync::Arc;

macro_rules! tri {
    ($x:expr) => {
        match $x {
            Ok(v) => v,
            Err(k) => return Ev::err(k),
        }
    };
}

fn n(x: f64) -> Ev {
    if x.is_finite() { Ev::V(Value::num(x)) } else { Ev::err(ErrorKind::Num) }
}
fn e(k: ErrorKind) -> Ev {
    Ev::err(k)
}
fn missing(a: &Expr) -> bool {
    matches!(a, Expr::Name(n) if n.is_empty())
}
fn num(c: &Ctx, a: &Expr) -> Result<f64, ErrorKind> {
    c.number(&c.scalar(a))
}
fn opt_num(c: &Ctx, args: &[Expr], i: usize, default: f64) -> Result<f64, ErrorKind> {
    match args.get(i) {
        Some(a) if !missing(a) => num(c, a),
        _ => Ok(default),
    }
}

/// The numbers of `a` as SUM reads them, at most [`MAX_ARRAY`].
fn numbers(c: &Ctx, a: &[Expr]) -> Result<Vec<f64>, ErrorKind> {
    let mut v = Vec::new();
    let mut over = false;
    each_num(c, a, &mut |x| {
        if v.len() < MAX_ARRAY as usize { v.push(x) } else { over = true }
    })?;
    if over { Err(ErrorKind::Limit) } else { Ok(v) }
}

fn mean(v: &[f64]) -> f64 {
    v.iter().sum::<f64>() / v.len() as f64
}
/// The sum of squared deviations from the mean (two passes).
fn devsq(v: &[f64]) -> f64 {
    let m = mean(v);
    v.iter().map(|x| (x - m) * (x - m)).sum()
}
fn variance(v: &[f64], sample: bool) -> Result<f64, ErrorKind> {
    let k = v.len();
    if k == 0 || (sample && k < 2) {
        return Err(ErrorKind::Div0);
    }
    Ok(devsq(v) / (k - usize::from(sample)) as f64)
}

// ---------- shape ----------

/// Σ((x − mean)/s)^p with s the sample (or population) standard deviation.
fn standard_moment(v: &[f64], p: i32, sample: bool) -> Result<f64, ErrorKind> {
    let s = variance(v, sample)?.sqrt();
    if s == 0.0 {
        return Err(ErrorKind::Div0);
    }
    let m = mean(v);
    Ok(v.iter().map(|x| ((x - m) / s).powi(p)).sum())
}

pub(super) fn kurt(c: &Ctx, a: &[Expr]) -> Ev {
    let v = tri!(numbers(c, a));
    if v.len() < 4 {
        return e(ErrorKind::Div0);
    }
    let k = v.len() as f64;
    let s4 = tri!(standard_moment(&v, 4, true));
    n(k * (k + 1.0) / ((k - 1.0) * (k - 2.0) * (k - 3.0)) * s4 - 3.0 * (k - 1.0) * (k - 1.0) / ((k - 2.0) * (k - 3.0)))
}
pub(super) fn skew(c: &Ctx, a: &[Expr]) -> Ev {
    let v = tri!(numbers(c, a));
    if v.len() < 3 {
        return e(ErrorKind::Div0);
    }
    let k = v.len() as f64;
    n(k / ((k - 1.0) * (k - 2.0)) * tri!(standard_moment(&v, 3, true)))
}
pub(super) fn skew_p(c: &Ctx, a: &[Expr]) -> Ev {
    let v = tri!(numbers(c, a));
    if v.is_empty() {
        return e(ErrorKind::Div0);
    }
    n(tri!(standard_moment(&v, 3, false)) / v.len() as f64)
}

// ---------- the A variances ----------

fn a_variance(c: &Ctx, a: &[Expr], sample: bool, root: bool) -> Ev {
    let v = tri!(a_values(c, a));
    let var = tri!(variance(&v, sample));
    n(if root { var.sqrt() } else { var })
}
pub(super) fn stdeva(c: &Ctx, a: &[Expr]) -> Ev {
    a_variance(c, a, true, true)
}
pub(super) fn stdevpa(c: &Ctx, a: &[Expr]) -> Ev {
    a_variance(c, a, false, true)
}
pub(super) fn vara(c: &Ctx, a: &[Expr]) -> Ev {
    a_variance(c, a, true, false)
}
pub(super) fn varpa(c: &Ctx, a: &[Expr]) -> Ev {
    a_variance(c, a, false, false)
}

// ---------- ranks ----------

/// PERCENTRANK, PERCENTRANK.INC and (`exclusive`) .EXC: x's place in the
/// data, interpolated between neighbours and cut (not rounded) to
/// `significance` digits.
fn percentrank(c: &Ctx, a: &[Expr], exclusive: bool) -> Ev {
    let mut v = tri!(numbers(c, std::slice::from_ref(&a[0])));
    let x = tri!(num(c, &a[1]));
    let digits = tri!(opt_num(c, a, 2, 3.0)).trunc();
    if !(1.0..=15.0).contains(&digits) {
        return e(ErrorKind::Num);
    }
    if v.is_empty() {
        return e(ErrorKind::Num);
    }
    v.sort_by(f64::total_cmp);
    let (lo, hi) = (v[0], v[v.len() - 1]);
    if x < lo || x > hi {
        return e(ErrorKind::NA);
    }
    let k = v.len() as f64;
    let below = v.partition_point(|&y| y < x);
    let at = if v[below] == x {
        below as f64
    } else {
        // Strictly between v[below − 1] and v[below].
        let (l, h) = (v[below - 1], v[below]);
        below as f64 - 1.0 + (x - l) / (h - l)
    };
    let r = if exclusive {
        (at + 1.0) / (k + 1.0)
    } else if v.len() == 1 {
        1.0
    } else {
        at / (k - 1.0)
    };
    let scale = 10f64.powi(digits as i32);
    n(super::more::snap(r * scale).floor() / scale)
}
pub(super) fn percentrank_inc(c: &Ctx, a: &[Expr]) -> Ev {
    percentrank(c, a, false)
}
pub(super) fn percentrank_exc(c: &Ctx, a: &[Expr]) -> Ev {
    percentrank(c, a, true)
}

/// TRIMMEAN(array, percent): the mean without the top and bottom
/// percent/2 of the points, the count trimmed rounded down to even.
pub(super) fn trimmean(c: &Ctx, a: &[Expr]) -> Ev {
    let mut v = tri!(numbers(c, std::slice::from_ref(&a[0])));
    let p = tri!(num(c, &a[1]));
    if !(0.0..1.0).contains(&p) || v.is_empty() {
        return e(ErrorKind::Num);
    }
    let cut = ((v.len() as f64 * p).floor() as usize / 2).min((v.len() - 1) / 2);
    v.sort_by(f64::total_cmp);
    n(mean(&v[cut..v.len() - cut]))
}

/// FREQUENCY(data, bins): a column counting the data in each bin (values
/// up to the bin and above the one before), with one more row above the
/// last.
pub(super) fn frequency(c: &Ctx, a: &[Expr]) -> Ev {
    let data = tri!(numbers(c, std::slice::from_ref(&a[0])));
    let bins = tri!(numbers(c, std::slice::from_ref(&a[1])));
    let mut order: Vec<usize> = (0..bins.len()).collect();
    order.sort_by(|&i, &j| bins[i].total_cmp(&bins[j]));
    let sorted: Vec<f64> = order.iter().map(|&i| bins[i]).collect();
    let mut counts = vec![0.0; bins.len() + 1];
    for x in data {
        let at = sorted.partition_point(|&b| b < x);
        counts[order.get(at).copied().unwrap_or(bins.len())] += 1.0;
    }
    let vals = counts.into_iter().map(Value::num).collect::<Vec<_>>();
    Ev::A(Arc::new(Array { rows: vals.len(), cols: 1, vals }))
}

/// PROB(x_range, prob_range, lower, [upper]): the chance of x in
/// [lower, upper], or of exactly lower; the probabilities must sum to 1.
pub(super) fn prob(c: &Ctx, a: &[Expr]) -> Ev {
    let xs = tri!(c.to_array(c.eval(&a[0])));
    let ps = tri!(c.to_array(c.eval(&a[1])));
    if xs.vals.len() != ps.vals.len() {
        return e(ErrorKind::NA);
    }
    let lo = tri!(num(c, &a[2]));
    let hi = tri!(opt_num(c, a, 3, lo));
    let (mut total, mut within) = (0.0, 0.0);
    for (x, p) in xs.vals.iter().zip(&ps.vals) {
        let (x, p) = match (x, p) {
            (Value::Err(k), _) | (_, Value::Err(k)) => return e(*k),
            (Value::Num(x), Value::Num(p)) => (*x, *p),
            _ => return e(ErrorKind::Value),
        };
        if p <= 0.0 || p > 1.0 {
            return e(ErrorKind::Num);
        }
        total += p;
        if lo <= x && x <= hi {
            within += p;
        }
    }
    if (total - 1.0).abs() > 1e-9 {
        return e(ErrorKind::Num);
    }
    n(within)
}

// ---------- AGGREGATE ----------

/// What AGGREGATE gathers: the numbers and the count of non-blank values.
struct Gathered {
    nums: Vec<f64>,
    filled: u64,
}

fn gather(c: &Ctx, args: &[Expr], skip_hidden: bool, skip_errors: bool, skip_nested: bool) -> Result<Gathered, ErrorKind> {
    let mut g = Gathered { nums: Vec::new(), filled: 0 };
    let mut error = None;
    for arg in args {
        match c.eval(arg) {
            Ev::R(range) => {
                let sheet = c.wb.sheet(range.sheet);
                c.wb.each_cell(&range, &mut |row, col, cell| {
                    use crate::column::Cell;
                    if error.is_some()
                        || (skip_hidden && (sheet.row_filtered(row) || sheet.row_hidden(row) || sheet.row_sizes.size(row) == 0.0))
                        || (skip_nested && c.wb.is_subtotal(range.sheet, row, col))
                    {
                        return;
                    }
                    if !cell.is_empty() {
                        g.filled += 1;
                    }
                    match cell {
                        Cell::Num(x) if g.nums.len() < MAX_ARRAY as usize => g.nums.push(x),
                        Cell::Num(_) => error = Some(ErrorKind::Limit),
                        Cell::Err(k) if !skip_errors => error = Some(k),
                        _ => {}
                    }
                });
            }
            other => {
                let arr = c.to_array(other)?;
                for v in arr.vals.iter() {
                    if !matches!(v, Value::Empty) {
                        g.filled += 1;
                    }
                    match v {
                        Value::Num(x) => g.nums.push(*x),
                        Value::Err(k) if !skip_errors => return Err(*k),
                        _ => {}
                    }
                }
                if g.nums.len() > MAX_ARRAY as usize {
                    return Err(ErrorKind::Limit);
                }
            }
        }
        if let Some(k) = error {
            return Err(k);
        }
    }
    Ok(g)
}

/// The most frequent number, the first seen among ties; #N/A when none
/// repeats.
fn mode(v: &[f64]) -> Result<f64, ErrorKind> {
    let mut seen: std::collections::HashMap<u64, (usize, usize)> = std::collections::HashMap::new();
    for (i, x) in v.iter().enumerate() {
        seen.entry((x + 0.0).to_bits()).or_insert((0, i)).0 += 1;
    }
    let best = seen.values().filter(|s| s.0 > 1).max_by(|p, q| p.0.cmp(&q.0).then(q.1.cmp(&p.1)));
    best.map(|s| v[s.1]).ok_or(ErrorKind::NA)
}

/// AGGREGATE(function, options, ref1, …) for functions 1–13 and
/// AGGREGATE(function, options, array, k) for 14–19. Options 0–3 skip
/// nested SUBTOTAL and AGGREGATE cells; 1, 3, 5 and 7 skip hidden rows;
/// 2, 3, 6 and 7 skip errors.
pub(super) fn aggregate(c: &Ctx, a: &[Expr]) -> Ev {
    let f = tri!(num(c, &a[0])).trunc();
    let opt = tri!(opt_num(c, a, 1, 0.0)).trunc();
    if !(1.0..=19.0).contains(&f) || !(0.0..=7.0).contains(&opt) || a.len() < 3 {
        return e(ErrorKind::Value);
    }
    let (f, opt) = (f as u8, opt as u8);
    let with_k = f >= 14;
    if with_k && a.len() != 4 {
        return e(ErrorKind::Value);
    }
    let data = if with_k { &a[2..3] } else { &a[2..] };
    let g = tri!(gather(c, data, matches!(opt, 1 | 3 | 5 | 7), matches!(opt, 2 | 3 | 6 | 7), opt <= 3));
    let mut v = g.nums;
    let count = v.len();
    let r = match f {
        1 if count == 0 => Err(ErrorKind::Div0),
        1 => Ok(mean(&v)),
        2 => Ok(count as f64),
        3 => Ok(g.filled as f64),
        4 => Ok(v.iter().copied().reduce(f64::max).unwrap_or(0.0)),
        5 => Ok(v.iter().copied().reduce(f64::min).unwrap_or(0.0)),
        6 if count == 0 => Ok(0.0),
        6 => Ok(v.iter().product()),
        7 => variance(&v, true).map(f64::sqrt),
        8 => variance(&v, false).map(f64::sqrt),
        9 => Ok(v.iter().sum()),
        10 => variance(&v, true),
        11 => variance(&v, false),
        12 => percentile_vec(&mut v, 0.5, false),
        13 => mode(&v),
        _ => {
            let k = tri!(num(c, &a[3]));
            match f {
                14 | 15 => {
                    let k = k.ceil();
                    if count == 0 || k < 1.0 || k > count as f64 {
                        Err(ErrorKind::Num)
                    } else {
                        v.sort_by(f64::total_cmp);
                        let i = k as usize - 1;
                        Ok(if f == 14 { v[count - 1 - i] } else { v[i] })
                    }
                }
                16 => percentile_vec(&mut v, k, false),
                18 => percentile_vec(&mut v, k, true),
                _ => {
                    let q = k.trunc();
                    let ok = if f == 17 { (0.0..=4.0).contains(&q) } else { (1.0..=3.0).contains(&q) };
                    if ok { percentile_vec(&mut v, q / 4.0, f == 19) } else { Err(ErrorKind::Num) }
                }
            }
        }
    };
    match r {
        Ok(x) => n(x),
        Err(k) => e(k),
    }
}
