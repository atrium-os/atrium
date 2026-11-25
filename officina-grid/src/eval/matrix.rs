//! Matrix functions (docs/spec/officina.md §16.138): MMULT, MINVERSE,
//! MDETERM and MUNIT. Matrices hold numbers only; anything else is #VALUE!.

use super::{Array, Ctx, Ev, MAX_ARRAY};
use crate::formula::Expr;
use crate::value::{ErrorKind, Value};
use std::sync::Arc;

/// Multiply-adds one call may do (a 1000 × 1000 product or inverse).
const MAX_WORK: u64 = 1 << 30;

struct Matrix {
    rows: usize,
    cols: usize,
    vals: Vec<f64>,
}

fn matrix(c: &Ctx, a: &Expr) -> Result<Matrix, ErrorKind> {
    let arr = c.to_array(c.eval(a))?;
    let vals = arr
        .vals
        .iter()
        .map(|v| match v {
            Value::Num(x) => Ok(*x),
            Value::Err(k) => Err(*k),
            _ => Err(ErrorKind::Value),
        })
        .collect::<Result<Vec<f64>, ErrorKind>>()?;
    Ok(Matrix { rows: arr.rows, cols: arr.cols, vals })
}

fn result(rows: usize, cols: usize, vals: Vec<f64>) -> Ev {
    let vals = vals.into_iter().map(|x| if x.is_finite() { Value::num(x) } else { Value::Err(ErrorKind::Num) }).collect();
    Ev::A(Arc::new(Array { rows, cols, vals }))
}

fn run(f: impl FnOnce() -> Result<Ev, ErrorKind>) -> Ev {
    f().unwrap_or_else(Ev::err)
}

/// MMULT(array1, array2): array1's columns must match array2's rows.
pub(super) fn mmult(c: &Ctx, a: &[Expr]) -> Ev {
    run(|| {
        let (x, y) = (matrix(c, &a[0])?, matrix(c, &a[1])?);
        if x.cols != y.rows {
            return Err(ErrorKind::Value);
        }
        if (x.rows as u64) * (x.cols as u64) * (y.cols as u64) > MAX_WORK || (x.rows as u64) * (y.cols as u64) > MAX_ARRAY {
            return Err(ErrorKind::Limit);
        }
        let mut out = vec![0.0; x.rows * y.cols];
        for i in 0..x.rows {
            for k in 0..x.cols {
                let xv = x.vals[i * x.cols + k];
                for j in 0..y.cols {
                    out[i * y.cols + j] += xv * y.vals[k * y.cols + j];
                }
            }
        }
        Ok(result(x.rows, y.cols, out))
    })
}

/// A square matrix within the work limit.
fn square(c: &Ctx, a: &Expr) -> Result<Matrix, ErrorKind> {
    let m = matrix(c, a)?;
    if m.rows != m.cols {
        return Err(ErrorKind::Value);
    }
    if (m.rows as u64).pow(3) > MAX_WORK {
        return Err(ErrorKind::Limit);
    }
    Ok(m)
}

/// Gauss–Jordan elimination with partial pivoting on `m`, carrying `inv`
/// along when given; the determinant, or 0 when a pivot vanishes.
fn eliminate(m: &mut Matrix, mut inv: Option<&mut Vec<f64>>) -> f64 {
    let k = m.rows;
    let scale = m.vals.iter().fold(0.0f64, |s, v| s.max(v.abs()));
    let mut det = 1.0;
    for col in 0..k {
        let pivot = (col..k).max_by(|&i, &j| m.vals[i * k + col].abs().total_cmp(&m.vals[j * k + col].abs())).unwrap_or(col);
        let p = m.vals[pivot * k + col];
        if p.abs() <= scale * 1e-15 {
            return 0.0;
        }
        if pivot != col {
            for j in 0..k {
                m.vals.swap(col * k + j, pivot * k + j);
                if let Some(inv) = inv.as_deref_mut() {
                    inv.swap(col * k + j, pivot * k + j);
                }
            }
            det = -det;
        }
        det *= p;
        for j in 0..k {
            m.vals[col * k + j] /= p;
            if let Some(inv) = inv.as_deref_mut() {
                inv[col * k + j] /= p;
            }
        }
        for i in 0..k {
            if i == col {
                continue;
            }
            let f = m.vals[i * k + col];
            if f == 0.0 {
                continue;
            }
            for j in 0..k {
                m.vals[i * k + j] -= f * m.vals[col * k + j];
                if let Some(inv) = inv.as_deref_mut() {
                    inv[i * k + j] -= f * inv[col * k + j];
                }
            }
        }
    }
    det
}

/// MDETERM(array).
pub(super) fn mdeterm(c: &Ctx, a: &[Expr]) -> Ev {
    run(|| {
        let mut m = square(c, &a[0])?;
        let det = eliminate(&mut m, None);
        // Integer matrices have integer determinants: drop the rounding noise.
        let snapped = if det.abs() < 1e15 && (det - det.round()).abs() < 1e-9 * det.abs().max(1.0) { det.round() } else { det };
        Ok(Ev::V(Value::num(snapped)))
    })
}

/// MINVERSE(array): #NUM! for a singular matrix.
pub(super) fn minverse(c: &Ctx, a: &[Expr]) -> Ev {
    run(|| {
        let mut m = square(c, &a[0])?;
        let k = m.rows;
        let mut inv: Vec<f64> = (0..k * k).map(|i| if i / k == i % k { 1.0 } else { 0.0 }).collect();
        if eliminate(&mut m, Some(&mut inv)) == 0.0 {
            return Err(ErrorKind::Num);
        }
        Ok(result(k, k, inv))
    })
}

/// MUNIT(dimension): the identity matrix.
pub(super) fn munit(c: &Ctx, a: &[Expr]) -> Ev {
    run(|| {
        let k = c.number(&c.scalar(&a[0]))?.trunc();
        if k < 1.0 {
            return Err(ErrorKind::Value);
        }
        if k * k > MAX_ARRAY as f64 {
            return Err(ErrorKind::Limit);
        }
        let k = k as usize;
        Ok(result(k, k, (0..k * k).map(|i| if i / k == i % k { 1.0 } else { 0.0 }).collect()))
    })
}
