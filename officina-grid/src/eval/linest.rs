//! Least-squares regression (docs/spec/officina.md §16.136): LINEST and
//! LOGEST with their statistics, and TREND and GROWTH. One to 64 predictor
//! variables; with a constant the data are centred before the normal
//! equations are solved, which keeps digits for data such as years.

use super::{to_bool, Array, Ctx, Ev};
use crate::formula::Expr;
use crate::value::{ErrorKind, Value};
use std::sync::Arc;

/// Predictor variables LINEST takes (Excel's limit too).
const MAX_VARS: usize = 64;

fn missing(a: &Expr) -> bool {
    matches!(a, Expr::Name(n) if n.is_empty())
}
fn given(a: &[Expr], i: usize) -> Option<&Expr> {
    a.get(i).filter(|x| !missing(x))
}

/// Numbers only: any other value in regression data is #VALUE!.
fn numbers(arr: &Array) -> Result<Vec<f64>, ErrorKind> {
    arr.vals
        .iter()
        .map(|v| match v {
            Value::Num(x) => Ok(*x),
            Value::Err(k) => Err(*k),
            _ => Err(ErrorKind::Value),
        })
        .collect()
}

/// Observations: `y[i]` and the row `x[i]` of predictors.
struct Data {
    y: Vec<f64>,
    x: Vec<Vec<f64>>,
    k: usize,
}

/// The known ys and xs, with the xs' orientation: for a column of ys each x
/// column is a variable, for a row each x row is; a list of xs the same size
/// as the ys is one variable; no xs are 1, 2, 3, ….
fn data(c: &Ctx, ys: &Expr, xs: Option<&Expr>, log: bool) -> Result<(Data, bool), ErrorKind> {
    let ya = c.to_array(c.eval(ys))?;
    let mut y = numbers(&ya)?;
    if log {
        if y.iter().any(|&v| v <= 0.0) {
            return Err(ErrorKind::Num);
        }
        y.iter_mut().for_each(|v| *v = v.ln());
    }
    let n = y.len();
    let by_column = ya.cols == 1;
    let Some(xs) = xs else {
        let x = (1..=n).map(|i| vec![i as f64]).collect();
        return Ok((Data { y, x, k: 1 }, by_column));
    };
    let xa = c.to_array(c.eval(xs))?;
    let xv = numbers(&xa)?;
    let x: Vec<Vec<f64>> = if xv.len() == n {
        xv.iter().map(|&v| vec![v]).collect()
    } else if by_column && xa.rows == n && ya.cols == 1 {
        (0..n).map(|i| xv[i * xa.cols..(i + 1) * xa.cols].to_vec()).collect()
    } else if ya.rows == 1 && xa.cols == n {
        (0..n).map(|i| (0..xa.rows).map(|r| xv[r * xa.cols + i]).collect()).collect()
    } else {
        return Err(ErrorKind::Ref);
    };
    let k = x.first().map_or(0, Vec::len);
    if k == 0 || k > MAX_VARS {
        return Err(ErrorKind::Value);
    }
    Ok((Data { y, x, k }, by_column))
}

/// A least-squares fit: coefficients for each variable, the constant, and
/// what the statistics need.
struct Fit {
    m: Vec<f64>,
    b: f64,
    /// (X'X)⁻¹ over the (centred, with a constant) predictors.
    inv: Vec<Vec<f64>>,
    means: Vec<f64>,
    n: f64,
    ss_reg: f64,
    ss_resid: f64,
}

fn invert(mut a: Vec<Vec<f64>>) -> Option<Vec<Vec<f64>>> {
    let k = a.len();
    let mut inv: Vec<Vec<f64>> = (0..k).map(|i| (0..k).map(|j| f64::from(u8::from(i == j))).collect()).collect();
    let scale = a.iter().flatten().fold(0.0f64, |m, v| m.max(v.abs()));
    for col in 0..k {
        let pivot = (col..k).max_by(|&i, &j| a[i][col].abs().total_cmp(&a[j][col].abs()))?;
        if a[pivot][col].abs() <= scale * 1e-13 {
            return None;
        }
        a.swap(col, pivot);
        inv.swap(col, pivot);
        let p = a[col][col];
        for j in 0..k {
            a[col][j] /= p;
            inv[col][j] /= p;
        }
        for i in 0..k {
            if i != col {
                let f = a[i][col];
                if f != 0.0 {
                    for j in 0..k {
                        a[i][j] -= f * a[col][j];
                        inv[i][j] -= f * inv[col][j];
                    }
                }
            }
        }
    }
    Some(inv)
}

fn fit(d: &Data, constant: bool) -> Result<Fit, ErrorKind> {
    let (nn, k) = (d.y.len(), d.k);
    if nn == 0 {
        return Err(ErrorKind::Value);
    }
    let n = nn as f64;
    let (means, ymean) = if constant {
        ((0..k).map(|j| d.x.iter().map(|r| r[j]).sum::<f64>() / n).collect::<Vec<_>>(), d.y.iter().sum::<f64>() / n)
    } else {
        (vec![0.0; k], 0.0)
    };
    let mut xtx = vec![vec![0.0; k]; k];
    let mut xty = vec![0.0; k];
    for (row, &y) in d.x.iter().zip(&d.y) {
        for i in 0..k {
            let xi = row[i] - means[i];
            xty[i] += xi * (y - ymean);
            for j in 0..k {
                xtx[i][j] += xi * (row[j] - means[j]);
            }
        }
    }
    let inv = invert(xtx).ok_or(ErrorKind::Num)?;
    let m: Vec<f64> = (0..k).map(|i| (0..k).map(|j| inv[i][j] * xty[j]).sum()).collect();
    let b = if constant { ymean - m.iter().zip(&means).map(|(m, x)| m * x).sum::<f64>() } else { 0.0 };
    let (mut ss_reg, mut ss_resid) = (0.0, 0.0);
    for (row, &y) in d.x.iter().zip(&d.y) {
        let fitted = b + m.iter().zip(row).map(|(m, x)| m * x).sum::<f64>();
        ss_resid += (y - fitted) * (y - fitted);
        ss_reg += (fitted - ymean) * (fitted - ymean);
    }
    Ok(Fit { m, b, inv, means, n, ss_reg, ss_resid })
}

fn flag(c: &Ctx, a: &[Expr], i: usize, default: bool) -> Result<bool, ErrorKind> {
    match given(a, i) {
        Some(x) => to_bool(&c.scalar(x)),
        None => Ok(default),
    }
}

fn array(rows: usize, cols: usize, vals: Vec<Value>) -> Ev {
    Ev::A(Arc::new(Array { rows, cols, vals }))
}

fn num(x: f64) -> Value {
    if x.is_finite() { Value::num(x) } else { Value::Err(ErrorKind::Num) }
}

/// LINEST (`log` false) and LOGEST: a row of coefficients, last variable
/// first and the constant last, and with `stats` four more rows: standard
/// errors, r² and the standard error of y, F and the degrees of freedom,
/// and the regression and residual sums of squares. LOGEST fits ln y and
/// gives e to each coefficient.
fn estimate(c: &Ctx, a: &[Expr], log: bool) -> Ev {
    let run = || -> Result<Ev, ErrorKind> {
        let (d, _) = data(c, &a[0], given(a, 1), log)?;
        let constant = flag(c, a, 2, true)?;
        let stats = flag(c, a, 3, false)?;
        let f = fit(&d, constant)?;
        let k = d.k;
        let cols = k + 1;
        let shown = |x: f64| num(if log { x.exp() } else { x });
        let mut vals: Vec<Value> = f.m.iter().rev().map(|&m| shown(m)).collect();
        vals.push(if constant { shown(f.b) } else { num(if log { 1.0 } else { 0.0 }) });
        if !stats {
            return Ok(array(1, cols, vals));
        }
        let p = k as f64 + f64::from(u8::from(constant));
        let df = f.n - p;
        let na = Value::Err(ErrorKind::NA);
        // Without a constant this is the sum of the fitted values' squares.
        let ss_reg = f.ss_reg;
        let sey2 = if df > 0.0 { f.ss_resid / df } else { f64::NAN };
        let se: Vec<Value> = (0..k).rev().map(|i| num((sey2 * f.inv[i][i]).sqrt())).collect();
        vals.extend(se);
        vals.push(if constant {
            let q: f64 = (0..k).map(|i| (0..k).map(|j| f.means[i] * f.inv[i][j] * f.means[j]).sum::<f64>()).sum();
            num((sey2 * (1.0 / f.n + q)).sqrt())
        } else {
            na.clone()
        });
        let total = ss_reg + f.ss_resid;
        let r2 = if total == 0.0 { f64::NAN } else { ss_reg / total };
        vals.push(num(r2));
        vals.push(num(sey2.sqrt()));
        vals.extend(std::iter::repeat_n(na.clone(), cols - 2));
        let fstat = if df > 0.0 && f.ss_resid > 0.0 { (ss_reg / k as f64) / (f.ss_resid / df) } else { f64::NAN };
        vals.push(num(fstat));
        vals.push(num(df.max(0.0)));
        vals.extend(std::iter::repeat_n(na.clone(), cols - 2));
        vals.push(num(ss_reg));
        vals.push(num(f.ss_resid));
        vals.extend(std::iter::repeat_n(na, cols - 2));
        Ok(array(5, cols, vals))
    };
    match run() {
        Ok(v) => v,
        Err(k) => Ev::err(k),
    }
}

pub(super) fn linest(c: &Ctx, a: &[Expr]) -> Ev {
    estimate(c, a, false)
}
pub(super) fn logest(c: &Ctx, a: &[Expr]) -> Ev {
    estimate(c, a, true)
}

/// TREND (`log` false) and GROWTH: the fitted line (or exponential curve)
/// at new xs, in the new xs' shape (the known xs when none are given).
fn predict(c: &Ctx, a: &[Expr], log: bool) -> Ev {
    let run = || -> Result<Ev, ErrorKind> {
        let (d, by_column) = data(c, &a[0], given(a, 1), log)?;
        let constant = flag(c, a, 3, true)?;
        let f = fit(&d, constant)?;
        let k = d.k;
        let (rows, cols, points): (usize, usize, Vec<Vec<f64>>) = match given(a, 2) {
            None => {
                let n = d.x.len();
                let (r, cl) = if by_column { (n, 1) } else { (1, n) };
                (r, cl, d.x.clone())
            }
            Some(e) => {
                let arr = c.to_array(c.eval(e))?;
                let v = numbers(&arr)?;
                if k == 1 {
                    (arr.rows, arr.cols, v.iter().map(|&x| vec![x]).collect())
                } else if by_column && arr.cols == k {
                    (arr.rows, 1, (0..arr.rows).map(|i| v[i * k..(i + 1) * k].to_vec()).collect())
                } else if !by_column && arr.rows == k {
                    (1, arr.cols, (0..arr.cols).map(|i| (0..k).map(|r| v[r * arr.cols + i]).collect()).collect())
                } else {
                    return Err(ErrorKind::Ref);
                }
            }
        };
        let vals = points
            .iter()
            .map(|x| {
                let y = f.b + f.m.iter().zip(x).map(|(m, x)| m * x).sum::<f64>();
                num(if log { y.exp() } else { y })
            })
            .collect();
        Ok(array(rows, cols, vals))
    };
    match run() {
        Ok(v) => v,
        Err(k) => Ev::err(k),
    }
}

pub(super) fn trend(c: &Ctx, a: &[Expr]) -> Ev {
    predict(c, a, false)
}
pub(super) fn growth(c: &Ctx, a: &[Expr]) -> Ev {
    predict(c, a, true)
}
