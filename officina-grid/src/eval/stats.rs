//! Statistics over lists and pairs of lists (docs/spec/officina.md §16.135):
//! deviations and means, correlation and the least-squares line, covariance,
//! MODE.MULT, and MAXA, MINA and AVERAGEA, which count text and logicals.

use super::funcs::each_num;
use super::{Array, Ctx, Ev};
use crate::formula::Expr;
use crate::value::{ErrorKind, Value};
use std::collections::HashMap;
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

/// Every number SUM would read in `a`, at most the array limit.
fn all_numbers(c: &Ctx, a: &[Expr]) -> Result<Vec<f64>, ErrorKind> {
    let mut v = Vec::new();
    let mut over = false;
    each_num(c, a, &mut |x| {
        if v.len() < super::MAX_ARRAY as usize { v.push(x) } else { over = true }
    })?;
    if over { Err(ErrorKind::Limit) } else { Ok(v) }
}

fn mean(xs: &[f64]) -> f64 {
    xs.iter().sum::<f64>() / xs.len() as f64
}

/// AVEDEV: the mean absolute deviation from the mean.
pub(super) fn avedev(c: &Ctx, a: &[Expr]) -> Ev {
    let xs = tri!(all_numbers(c, a));
    if xs.is_empty() {
        return Ev::err(ErrorKind::Num);
    }
    let m = mean(&xs);
    n(xs.iter().map(|x| (x - m).abs()).sum::<f64>() / xs.len() as f64)
}

/// DEVSQ: the sum of squared deviations from the mean.
pub(super) fn devsq(c: &Ctx, a: &[Expr]) -> Ev {
    let xs = tri!(all_numbers(c, a));
    if xs.is_empty() {
        return Ev::err(ErrorKind::Num);
    }
    let m = mean(&xs);
    n(xs.iter().map(|x| (x - m) * (x - m)).sum())
}

/// GEOMEAN and HARMEAN: positive numbers only.
fn positive(c: &Ctx, a: &[Expr]) -> Result<Vec<f64>, ErrorKind> {
    let xs = all_numbers(c, a)?;
    if xs.is_empty() || xs.iter().any(|&x| x <= 0.0) {
        return Err(ErrorKind::Num);
    }
    Ok(xs)
}
pub(super) fn geomean(c: &Ctx, a: &[Expr]) -> Ev {
    let xs = tri!(positive(c, a));
    n((xs.iter().map(|x| x.ln()).sum::<f64>() / xs.len() as f64).exp())
}
pub(super) fn harmean(c: &Ctx, a: &[Expr]) -> Ev {
    let xs = tri!(positive(c, a));
    n(xs.len() as f64 / xs.iter().map(|x| 1.0 / x).sum::<f64>())
}

/// STANDARDIZE(x, mean, standard_dev).
pub(super) fn standardize(c: &Ctx, a: &[Expr]) -> Ev {
    let num = |i: usize| c.number(&c.scalar(&a[i]));
    let (x, m, sd) = (tri!(num(0)), tri!(num(1)), tri!(num(2)));
    if sd <= 0.0 {
        return Ev::err(ErrorKind::Num);
    }
    n((x - m) / sd)
}

/// Two lists of equal length, keeping the places where both hold numbers
/// (text, logicals and blanks in either drop the pair). An error in either
/// is the result; lists of different lengths are #N/A.
struct Pairs {
    n: f64,
    mx: f64,
    my: f64,
    /// Sums of products of deviations: xx, yy, xy.
    sxx: f64,
    syy: f64,
    sxy: f64,
}

fn pairs(c: &Ctx, ys: &Expr, xs: &Expr) -> Result<Pairs, ErrorKind> {
    let ys = c.to_array(c.eval(ys))?;
    let xs = c.to_array(c.eval(xs))?;
    if ys.vals.len() != xs.vals.len() {
        return Err(ErrorKind::NA);
    }
    let mut both = Vec::new();
    for (y, x) in ys.vals.iter().zip(&xs.vals) {
        match (y, x) {
            (Value::Err(k), _) | (_, Value::Err(k)) => return Err(*k),
            (Value::Num(y), Value::Num(x)) => both.push((*x, *y)),
            _ => {}
        }
    }
    let count = both.len() as f64;
    if both.is_empty() {
        return Ok(Pairs { n: 0.0, mx: 0.0, my: 0.0, sxx: 0.0, syy: 0.0, sxy: 0.0 });
    }
    let mx = both.iter().map(|p| p.0).sum::<f64>() / count;
    let my = both.iter().map(|p| p.1).sum::<f64>() / count;
    let (mut sxx, mut syy, mut sxy) = (0.0, 0.0, 0.0);
    for (x, y) in &both {
        sxx += (x - mx) * (x - mx);
        syy += (y - my) * (y - my);
        sxy += (x - mx) * (y - my);
    }
    Ok(Pairs { n: count, mx, my, sxx, syy, sxy })
}

fn div(x: f64, y: f64) -> Ev {
    if y == 0.0 { Ev::err(ErrorKind::Div0) } else { n(x / y) }
}

/// CORREL and PEARSON(array1, array2).
pub(super) fn correl(c: &Ctx, a: &[Expr]) -> Ev {
    let p = tri!(pairs(c, &a[0], &a[1]));
    div(p.sxy, (p.sxx * p.syy).sqrt())
}
/// RSQ(known_ys, known_xs): the square of PEARSON.
pub(super) fn rsq(c: &Ctx, a: &[Expr]) -> Ev {
    let p = tri!(pairs(c, &a[0], &a[1]));
    div(p.sxy * p.sxy, p.sxx * p.syy)
}
/// SLOPE(known_ys, known_xs) of the least-squares line.
pub(super) fn slope(c: &Ctx, a: &[Expr]) -> Ev {
    let p = tri!(pairs(c, &a[0], &a[1]));
    div(p.sxy, p.sxx)
}
/// INTERCEPT(known_ys, known_xs) of the least-squares line.
pub(super) fn intercept(c: &Ctx, a: &[Expr]) -> Ev {
    let p = tri!(pairs(c, &a[0], &a[1]));
    if p.sxx == 0.0 {
        return Ev::err(ErrorKind::Div0);
    }
    n(p.my - p.sxy / p.sxx * p.mx)
}
/// FORECAST and FORECAST.LINEAR(x, known_ys, known_xs): the line's value at x.
pub(super) fn forecast(c: &Ctx, a: &[Expr]) -> Ev {
    let x = tri!(c.number(&c.scalar(&a[0])));
    let p = tri!(pairs(c, &a[1], &a[2]));
    if p.sxx == 0.0 {
        return Ev::err(ErrorKind::Div0);
    }
    let b = p.sxy / p.sxx;
    n(p.my - b * p.mx + b * x)
}
/// STEYX(known_ys, known_xs): the standard error of the predicted y.
pub(super) fn steyx(c: &Ctx, a: &[Expr]) -> Ev {
    let p = tri!(pairs(c, &a[0], &a[1]));
    if p.n < 3.0 || p.sxx == 0.0 {
        return Ev::err(ErrorKind::Div0);
    }
    n(((p.syy - p.sxy * p.sxy / p.sxx).max(0.0) / (p.n - 2.0)).sqrt())
}
/// COVAR and COVARIANCE.P: the population covariance.
pub(super) fn covariance_p(c: &Ctx, a: &[Expr]) -> Ev {
    let p = tri!(pairs(c, &a[0], &a[1]));
    div(p.sxy, p.n)
}
/// COVARIANCE.S: the sample covariance.
pub(super) fn covariance_s(c: &Ctx, a: &[Expr]) -> Ev {
    let p = tri!(pairs(c, &a[0], &a[1]));
    if p.n < 2.0 {
        return Ev::err(ErrorKind::Div0);
    }
    n(p.sxy / (p.n - 1.0))
}

/// MODE.MULT: every most frequent number (seen more than once), in the order
/// each first appears, as a column.
pub(super) fn mode_mult(c: &Ctx, a: &[Expr]) -> Ev {
    let xs = tri!(all_numbers(c, a));
    let mut counts: HashMap<u64, (usize, usize)> = HashMap::new();
    for (i, x) in xs.iter().enumerate() {
        let key = if *x == 0.0 { 0 } else { x.to_bits() };
        counts.entry(key).or_insert((0, i)).0 += 1;
    }
    let most = counts.values().map(|v| v.0).max().unwrap_or(0);
    if most < 2 {
        return Ev::err(ErrorKind::NA);
    }
    let mut modes: Vec<(usize, f64)> = counts.values().filter(|v| v.0 == most).map(|v| (v.1, xs[v.1])).collect();
    modes.sort_unstable_by_key(|m| m.0);
    let vals: Vec<Value> = modes.into_iter().map(|m| Value::num(m.1)).collect();
    Ev::A(Arc::new(Array { rows: vals.len(), cols: 1, vals }))
}

/// The values MAXA, MINA and AVERAGEA count: in ranges and arrays text is 0
/// and logicals 0 or 1 (blanks are skipped); direct arguments are coerced.
fn a_values(c: &Ctx, a: &[Expr]) -> Result<Vec<f64>, ErrorKind> {
    let mut out = Vec::new();
    for arg in a {
        match c.eval(arg) {
            Ev::V(Value::Empty) => {}
            Ev::V(x) => out.push(c.number(&x)?),
            other => {
                let arr = c.to_array(other)?;
                for x in &arr.vals {
                    match x {
                        Value::Num(x) => out.push(*x),
                        Value::Bool(b) => out.push(f64::from(u8::from(*b))),
                        Value::Str(_) => out.push(0.0),
                        Value::Err(k) => return Err(*k),
                        Value::Empty => {}
                    }
                }
            }
        }
        if out.len() > super::MAX_ARRAY as usize {
            return Err(ErrorKind::Limit);
        }
    }
    Ok(out)
}
pub(super) fn maxa(c: &Ctx, a: &[Expr]) -> Ev {
    let xs = tri!(a_values(c, a));
    n(xs.into_iter().reduce(f64::max).unwrap_or(0.0))
}
pub(super) fn mina(c: &Ctx, a: &[Expr]) -> Ev {
    let xs = tri!(a_values(c, a));
    n(xs.into_iter().reduce(f64::min).unwrap_or(0.0))
}
pub(super) fn averagea(c: &Ctx, a: &[Expr]) -> Ev {
    let xs = tri!(a_values(c, a));
    div(xs.iter().sum(), xs.len() as f64)
}
