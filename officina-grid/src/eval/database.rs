//! The database functions (docs/spec/officina.md §16.139): DSUM, DAVERAGE,
//! DCOUNT, DCOUNTA, DGET, DMAX, DMIN, DPRODUCT, DSTDEV, DSTDEVP, DVAR and
//! DVARP. A database is a range whose first row names its fields; the
//! criteria range's first row names fields too, the conditions in one row
//! must all hold and any one row may hold.

use super::criteria::Criterion;
use super::{Ctx, Ev};
use crate::formula::Expr;
use crate::value::{ErrorKind, Value};

fn missing(a: &Expr) -> bool {
    matches!(a, Expr::Name(n) if n.is_empty())
}

fn same_label(a: &Value, b: &str) -> bool {
    match a {
        Value::Str(s) => s.trim().to_lowercase() == b.trim().to_lowercase(),
        Value::Num(x) => super::to_text(&Value::Num(*x)).is_ok_and(|t| &*t == b.trim()),
        _ => false,
    }
}

fn label(v: &Value) -> Option<String> {
    match v {
        Value::Str(s) if !s.trim().is_empty() => Some(s.to_string()),
        Value::Num(_) | Value::Bool(_) => super::to_text(v).ok().map(|t| t.to_string()),
        _ => None,
    }
}

/// A criterion as the database functions read it: plain text (no
/// comparison sign, not a number or logical) matches text that begins
/// with it, as Excel's database criteria and Advanced Filter do.
fn criterion(v: &Value) -> Option<Criterion> {
    match v {
        Value::Empty => None,
        Value::Str(s) if s.is_empty() => None,
        Value::Str(s) => {
            let plain = !s.starts_with(['=', '<', '>'])
                && super::parse_number(s).is_none()
                && !s.eq_ignore_ascii_case("TRUE")
                && !s.eq_ignore_ascii_case("FALSE");
            if plain && !s.ends_with('*') {
                Some(Criterion::new(&Value::str(&format!("{s}*"))))
            } else {
                Some(Criterion::new(v))
            }
        }
        _ => Some(Criterion::new(v)),
    }
}

/// The field's value in each record the criteria pick, or `None` for each
/// when `field` is omitted (DCOUNT and DCOUNTA then count records).
fn select(c: &Ctx, a: &[Expr]) -> Result<Vec<Option<Value>>, ErrorKind> {
    let db = c.to_array(c.eval(&a[0]))?;
    if db.rows < 1 {
        return Err(ErrorKind::Value);
    }
    let field = match a.get(1).filter(|x| !missing(x)) {
        None => None,
        Some(x) => Some(match c.scalar(x) {
            Value::Err(k) => return Err(k),
            Value::Num(k) => {
                let k = k.trunc();
                if k < 1.0 || k > db.cols as f64 {
                    return Err(ErrorKind::Value);
                }
                k as usize - 1
            }
            Value::Str(name) => (0..db.cols).find(|&j| same_label(&db.vals[j], &name)).ok_or(ErrorKind::Value)?,
            _ => return Err(ErrorKind::Value),
        }),
    };
    let crit = c.to_array(c.eval(&a[2]))?;
    let picked = picks(&db.vals, db.cols, &crit.vals, crit.cols).ok_or(ErrorKind::Value)?;
    let mut out = Vec::new();
    for (r, _) in picked.iter().enumerate().filter(|p| *p.1) {
        let rec = &db.vals[(r + 1) * db.cols..(r + 2) * db.cols];
        out.push(field.map(|f| rec[f].clone()));
    }
    Ok(out)
}

/// Which records (the rows after the first) of the database `db`, `cols`
/// wide with its field names first, the criteria range `crit` (`ccols`
/// wide, labels first) picks: every condition of one criteria row holds,
/// for any row. `None` when the criteria are malformed: fewer than two
/// rows, or a condition under a label that names no field (Excel's
/// computed criteria aren't supported). Advanced Filter shares it.
pub fn picks(db: &[Value], cols: usize, crit: &[Value], ccols: usize) -> Option<Vec<bool>> {
    if cols == 0 || ccols == 0 || db.len() < cols || crit.len() < 2 * ccols {
        return None;
    }
    let (rows, crows) = (db.len() / cols, crit.len() / ccols);
    let mut conds_by_row: Vec<Vec<(usize, Criterion)>> = Vec::new();
    for r in 1..crows {
        let mut conds = Vec::new();
        for j in 0..ccols {
            let Some(cr) = criterion(&crit[r * ccols + j]) else { continue };
            let name = label(&crit[j])?;
            let col = (0..cols).find(|&k| same_label(&db[k], &name))?;
            conds.push((col, cr));
        }
        conds_by_row.push(conds);
    }
    Some((1..rows).map(|r| {
        let rec = &db[r * cols..(r + 1) * cols];
        conds_by_row.iter().any(|conds| conds.iter().all(|(col, cr)| cr.test(&rec[*col])))
    }).collect())
}

fn numbers(v: &[Option<Value>]) -> Vec<f64> {
    v.iter().filter_map(|x| match x {
        Some(Value::Num(x)) => Some(*x),
        _ => None,
    }).collect()
}

fn run(c: &Ctx, a: &[Expr], f: impl FnOnce(Vec<Option<Value>>) -> Result<Value, ErrorKind>) -> Ev {
    match select(c, a).and_then(f) {
        Ok(Value::Num(x)) if !x.is_finite() => Ev::err(ErrorKind::Num),
        Ok(v) => Ev::V(v),
        Err(k) => Ev::err(k),
    }
}

fn variance(v: &[f64], sample: bool) -> Result<f64, ErrorKind> {
    let k = v.len();
    if k == 0 || (sample && k < 2) {
        return Err(ErrorKind::Div0);
    }
    let m = v.iter().sum::<f64>() / k as f64;
    Ok(v.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / (k - usize::from(sample)) as f64)
}

pub(super) fn dsum(c: &Ctx, a: &[Expr]) -> Ev {
    run(c, a, |v| Ok(Value::num(numbers(&v).iter().sum())))
}
pub(super) fn daverage(c: &Ctx, a: &[Expr]) -> Ev {
    run(c, a, |v| {
        let x = numbers(&v);
        if x.is_empty() { Err(ErrorKind::Div0) } else { Ok(Value::num(x.iter().sum::<f64>() / x.len() as f64)) }
    })
}
pub(super) fn dcount(c: &Ctx, a: &[Expr]) -> Ev {
    run(c, a, |v| Ok(Value::num(v.iter().filter(|x| matches!(x, None | Some(Value::Num(_)))).count() as f64)))
}
pub(super) fn dcounta(c: &Ctx, a: &[Expr]) -> Ev {
    run(c, a, |v| {
        Ok(Value::num(v.iter().filter(|x| !matches!(x, Some(Value::Empty)) && !matches!(x, Some(Value::Str(s)) if s.is_empty())).count() as f64))
    })
}
pub(super) fn dget(c: &Ctx, a: &[Expr]) -> Ev {
    run(c, a, |v| match v.as_slice() {
        [] => Err(ErrorKind::Value),
        [one] => Ok(one.clone().unwrap_or(Value::Empty)),
        _ => Err(ErrorKind::Num),
    })
}
pub(super) fn dmax(c: &Ctx, a: &[Expr]) -> Ev {
    run(c, a, |v| Ok(Value::num(numbers(&v).into_iter().reduce(f64::max).unwrap_or(0.0))))
}
pub(super) fn dmin(c: &Ctx, a: &[Expr]) -> Ev {
    run(c, a, |v| Ok(Value::num(numbers(&v).into_iter().reduce(f64::min).unwrap_or(0.0))))
}
pub(super) fn dproduct(c: &Ctx, a: &[Expr]) -> Ev {
    run(c, a, |v| {
        let x = numbers(&v);
        Ok(Value::num(if x.is_empty() { 0.0 } else { x.iter().product() }))
    })
}
pub(super) fn dstdev(c: &Ctx, a: &[Expr]) -> Ev {
    run(c, a, |v| variance(&numbers(&v), true).map(|x| Value::num(x.sqrt())))
}
pub(super) fn dstdevp(c: &Ctx, a: &[Expr]) -> Ev {
    run(c, a, |v| variance(&numbers(&v), false).map(|x| Value::num(x.sqrt())))
}
pub(super) fn dvar(c: &Ctx, a: &[Expr]) -> Ev {
    run(c, a, |v| variance(&numbers(&v), true).map(Value::num))
}
pub(super) fn dvarp(c: &Ctx, a: &[Expr]) -> Ev {
    run(c, a, |v| variance(&numbers(&v), false).map(Value::num))
}
