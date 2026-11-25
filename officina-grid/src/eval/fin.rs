//! Excel's financial functions for loans and cash flows (docs/spec/officina.md
//! §16.133): PMT, IPMT, PPMT, FV, PV, NPER, RATE, NPV and IRR, on Excel's
//! sign convention (money paid out is negative).

use super::funcs::each_num;
use super::{Ctx, Ev};
use crate::formula::Expr;
use crate::value::{ErrorKind, Value};

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
fn missing(a: &Expr) -> bool {
    matches!(a, Expr::Name(n) if n.is_empty())
}
fn arg(c: &Ctx, a: &[Expr], i: usize, default: f64) -> Result<f64, ErrorKind> {
    match a.get(i) {
        Some(x) if !missing(x) => c.number(&c.scalar(x)),
        _ => Ok(default),
    }
}
/// Payment timing: 0 at the end of each period, 1 at the start.
fn timing(c: &Ctx, a: &[Expr], i: usize) -> Result<f64, ErrorKind> {
    Ok(if arg(c, a, i, 0.0)? != 0.0 { 1.0 } else { 0.0 })
}

fn pmt_of(r: f64, nper: f64, pv: f64, fv: f64, t: f64) -> f64 {
    if r == 0.0 {
        return -(pv + fv) / nper;
    }
    let g = (1.0 + r).powf(nper);
    -(r * (fv + pv * g)) / ((1.0 + r * t) * (g - 1.0))
}
fn fv_of(r: f64, nper: f64, pmt: f64, pv: f64, t: f64) -> f64 {
    if r == 0.0 {
        return -(pv + pmt * nper);
    }
    let g = (1.0 + r).powf(nper);
    -(pv * g + pmt * (1.0 + r * t) * (g - 1.0) / r)
}

pub(super) fn pmt(c: &Ctx, a: &[Expr]) -> Ev {
    let (r, nper, pv) = (tri!(arg(c, a, 0, 0.0)), tri!(arg(c, a, 1, 0.0)), tri!(arg(c, a, 2, 0.0)));
    let (fv, t) = (tri!(arg(c, a, 3, 0.0)), tri!(timing(c, a, 4)));
    if nper == 0.0 {
        return Ev::err(ErrorKind::Num);
    }
    n(pmt_of(r, nper, pv, fv, t))
}

pub(super) fn fv(c: &Ctx, a: &[Expr]) -> Ev {
    let (r, nper, pmt) = (tri!(arg(c, a, 0, 0.0)), tri!(arg(c, a, 1, 0.0)), tri!(arg(c, a, 2, 0.0)));
    let (pv, t) = (tri!(arg(c, a, 3, 0.0)), tri!(timing(c, a, 4)));
    n(fv_of(r, nper, pmt, pv, t))
}

pub(super) fn pv(c: &Ctx, a: &[Expr]) -> Ev {
    let (r, nper, pmt) = (tri!(arg(c, a, 0, 0.0)), tri!(arg(c, a, 1, 0.0)), tri!(arg(c, a, 2, 0.0)));
    let (fv, t) = (tri!(arg(c, a, 3, 0.0)), tri!(timing(c, a, 4)));
    if r == 0.0 {
        return n(-(fv + pmt * nper));
    }
    let g = (1.0 + r).powf(nper);
    n(-(fv + pmt * (1.0 + r * t) * (g - 1.0) / r) / g)
}

pub(super) fn nper(c: &Ctx, a: &[Expr]) -> Ev {
    let (r, pmt, pv) = (tri!(arg(c, a, 0, 0.0)), tri!(arg(c, a, 1, 0.0)), tri!(arg(c, a, 2, 0.0)));
    let (fv, t) = (tri!(arg(c, a, 3, 0.0)), tri!(timing(c, a, 4)));
    if r == 0.0 {
        return if pmt == 0.0 { Ev::err(ErrorKind::Num) } else { n(-(pv + fv) / pmt) };
    }
    let num = pmt * (1.0 + r * t) - fv * r;
    let den = pv * r + pmt * (1.0 + r * t);
    if den == 0.0 || num / den <= 0.0 || r <= -1.0 {
        return Ev::err(ErrorKind::Num);
    }
    n((num / den).ln() / (1.0 + r).ln())
}

/// IPMT's interest in period `per`, as Excel works it out from the balance.
fn ipmt_of(r: f64, per: f64, nper: f64, pv: f64, fv: f64, t: f64) -> f64 {
    let pmt = pmt_of(r, nper, pv, fv, t);
    let interest = if per == 1.0 {
        if t > 0.0 { 0.0 } else { -pv }
    } else if t > 0.0 {
        fv_of(r, per - 2.0, pmt, pv, 1.0) - pmt
    } else {
        fv_of(r, per - 1.0, pmt, pv, 0.0)
    };
    interest * r
}

fn period_args(c: &Ctx, a: &[Expr]) -> Result<(f64, f64, f64, f64, f64, f64), ErrorKind> {
    let (r, per, nper, pv) = (arg(c, a, 0, 0.0)?, arg(c, a, 1, 0.0)?, arg(c, a, 2, 0.0)?, arg(c, a, 3, 0.0)?);
    let (fv, t) = (arg(c, a, 4, 0.0)?, timing(c, a, 5)?);
    if per < 1.0 || per > nper || nper == 0.0 {
        return Err(ErrorKind::Num);
    }
    Ok((r, per, nper, pv, fv, t))
}

pub(super) fn ipmt(c: &Ctx, a: &[Expr]) -> Ev {
    let (r, per, nper, pv, fv, t) = tri!(period_args(c, a));
    n(ipmt_of(r, per, nper, pv, fv, t))
}

pub(super) fn ppmt(c: &Ctx, a: &[Expr]) -> Ev {
    let (r, per, nper, pv, fv, t) = tri!(period_args(c, a));
    n(pmt_of(r, nper, pv, fv, t) - ipmt_of(r, per, nper, pv, fv, t))
}

/// Newton's method on `f` from `guess`: at most 100 steps, done when a
/// step is under 1e-10; None when it doesn't settle (#NUM!, as Excel).
fn solve(guess: f64, f: impl Fn(f64) -> f64) -> Option<f64> {
    let mut r = guess;
    for _ in 0..100 {
        let y = f(r);
        let h = 1e-6 * r.abs().max(1e-3);
        let d = (f(r + h) - f(r - h)) / (2.0 * h);
        if !y.is_finite() || !d.is_finite() || d == 0.0 {
            return None;
        }
        let next = r - y / d;
        if !next.is_finite() || next <= -1.0 {
            return None;
        }
        if (next - r).abs() < 1e-10 {
            return Some(next);
        }
        r = next;
    }
    None
}

pub(super) fn rate(c: &Ctx, a: &[Expr]) -> Ev {
    let (nper, pmt, pv) = (tri!(arg(c, a, 0, 0.0)), tri!(arg(c, a, 1, 0.0)), tri!(arg(c, a, 2, 0.0)));
    let (fv, t, guess) = (tri!(arg(c, a, 3, 0.0)), tri!(timing(c, a, 4)), tri!(arg(c, a, 5, 0.1)));
    if nper <= 0.0 {
        return Ev::err(ErrorKind::Num);
    }
    // The balance after nper periods; zero at the rate sought.
    let balance = |r: f64| {
        if r.abs() < 1e-12 {
            pv + pmt * nper + fv
        } else {
            let g = (1.0 + r).powf(nper);
            pv * g + pmt * (1.0 + r * t) * (g - 1.0) / r + fv
        }
    };
    match solve(guess, balance) {
        Some(r) => n(r),
        None => Ev::err(ErrorKind::Num),
    }
}

/// The present value of `values` at `r`, the first a period from now.
fn npv_of(r: f64, values: &[f64]) -> f64 {
    values.iter().enumerate().map(|(i, v)| v / (1.0 + r).powi(i as i32 + 1)).sum()
}

/// The most cash flows NPV and IRR take.
const MAX_FLOWS: usize = 1 << 20;

fn flows(c: &Ctx, a: &[Expr]) -> Result<Vec<f64>, ErrorKind> {
    let mut v = Vec::new();
    let mut over = false;
    each_num(c, a, &mut |x| if v.len() < MAX_FLOWS { v.push(x) } else { over = true })?;
    if over { Err(ErrorKind::Limit) } else { Ok(v) }
}

pub(super) fn npv(c: &Ctx, a: &[Expr]) -> Ev {
    let r = tri!(arg(c, a, 0, 0.0));
    if r == -1.0 {
        return Ev::err(ErrorKind::Div0);
    }
    let v = tri!(flows(c, &a[1..]));
    n(npv_of(r, &v))
}

pub(super) fn irr(c: &Ctx, a: &[Expr]) -> Ev {
    let v = tri!(flows(c, &a[..1]));
    let guess = tri!(arg(c, a, 1, 0.1));
    if !v.iter().any(|&x| x > 0.0) || !v.iter().any(|&x| x < 0.0) {
        return Ev::err(ErrorKind::Num);
    }
    // The flows' value now, the first one counted undiscounted.
    let value = |r: f64| v.iter().enumerate().map(|(i, x)| x / (1.0 + r).powi(i as i32)).sum::<f64>();
    match solve(guess, value) {
        Some(r) => n(r),
        None => Ev::err(ErrorKind::Num),
    }
}

// Depreciation (§16.134).

/// SLN(cost, salvage, life): straight-line depreciation for one period.
pub(super) fn sln(c: &Ctx, a: &[Expr]) -> Ev {
    let (cost, salvage, life) = (tri!(arg(c, a, 0, 0.0)), tri!(arg(c, a, 1, 0.0)), tri!(arg(c, a, 2, 0.0)));
    if life == 0.0 {
        return Ev::err(ErrorKind::Div0);
    }
    n((cost - salvage) / life)
}

/// SYD(cost, salvage, life, per): sum-of-years'-digits depreciation.
pub(super) fn syd(c: &Ctx, a: &[Expr]) -> Ev {
    let (cost, salvage, life, per) = (tri!(arg(c, a, 0, 0.0)), tri!(arg(c, a, 1, 0.0)), tri!(arg(c, a, 2, 0.0)), tri!(arg(c, a, 3, 0.0)));
    if life <= 0.0 || per <= 0.0 || per > life {
        return Ev::err(ErrorKind::Num);
    }
    n((cost - salvage) * (life - per + 1.0) * 2.0 / (life * (life + 1.0)))
}

/// DDB(cost, salvage, life, period, [factor]): declining-balance
/// depreciation at `factor` / `life` a period (double by default), never
/// taking the book value below `salvage`.
pub(super) fn ddb(c: &Ctx, a: &[Expr]) -> Ev {
    let (cost, salvage, life, period) = (tri!(arg(c, a, 0, 0.0)), tri!(arg(c, a, 1, 0.0)), tri!(arg(c, a, 2, 0.0)), tri!(arg(c, a, 3, 0.0)));
    let factor = tri!(arg(c, a, 4, 2.0));
    if cost < 0.0 || salvage < 0.0 || life <= 0.0 || period <= 0.0 || factor <= 0.0 || period > life {
        return Ev::err(ErrorKind::Num);
    }
    let rate = (factor / life).min(1.0);
    // The book value at the start of the period.
    let book = if rate == 1.0 {
        if period <= 1.0 { cost } else { 0.0 }
    } else {
        cost * (1.0 - rate).powf(period - 1.0)
    };
    n((book * rate).min(book - salvage).max(0.0))
}

/// DB(cost, salvage, life, period, [month]): fixed-declining-balance
/// depreciation at a rate rounded to three places, with `month` months in
/// the first year and the rest in period life + 1.
pub(super) fn db(c: &Ctx, a: &[Expr]) -> Ev {
    let (cost, salvage, life, period) = (tri!(arg(c, a, 0, 0.0)), tri!(arg(c, a, 1, 0.0)), tri!(arg(c, a, 2, 0.0)), tri!(arg(c, a, 3, 0.0)));
    let month = tri!(arg(c, a, 4, 12.0)).trunc();
    let period = period.trunc();
    if cost < 0.0 || salvage < 0.0 || life <= 0.0 || period < 1.0 || !(1.0..=12.0).contains(&month) || period > life + 1.0 {
        return Ev::err(ErrorKind::Num);
    }
    if cost == 0.0 {
        return n(0.0);
    }
    let rate = super::funcs::round_dec(1.0 - (salvage / cost).powf(1.0 / life), 3, 0);
    let first = cost * rate * month / 12.0;
    if period == 1.0 {
        return n(first);
    }
    // The book value at the start of the period.
    let book = (cost - first) * (1.0 - rate).powf(period - 2.0);
    if period > life {
        return n(book * rate * (12.0 - month) / 12.0);
    }
    n(book * rate)
}
