//! Engineering and maths functions (docs/spec/officina.md §16.138): the
//! bitwise functions, DELTA and GESTEP, permutations and combinations,
//! SERIESSUM, the paired sums of squares, the reciprocal trigonometric
//! functions and the .PRECISE roundings. Each follows Excel's documented
//! results and errors.

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

// ---------- bits ----------

/// Bitwise operands: whole numbers from 0 to 2^48 − 1.
const BIT_LIMIT: f64 = 281_474_976_710_656.0;

fn bits(c: &Ctx, a: &Expr) -> Result<u64, ErrorKind> {
    let x = num(c, a)?;
    if x.fract() != 0.0 || !(0.0..BIT_LIMIT).contains(&x) {
        return Err(ErrorKind::Num);
    }
    Ok(x as u64)
}
fn bitwise(c: &Ctx, a: &[Expr], f: fn(u64, u64) -> u64) -> Ev {
    let (x, y) = (tri!(bits(c, &a[0])), tri!(bits(c, &a[1])));
    n(f(x, y) as f64)
}
pub(super) fn bitand(c: &Ctx, a: &[Expr]) -> Ev {
    bitwise(c, a, |x, y| x & y)
}
pub(super) fn bitor(c: &Ctx, a: &[Expr]) -> Ev {
    bitwise(c, a, |x, y| x | y)
}
pub(super) fn bitxor(c: &Ctx, a: &[Expr]) -> Ev {
    bitwise(c, a, |x, y| x ^ y)
}
/// BITLSHIFT (`sign` 1) and BITRSHIFT (−1): shifts of at most 53 places,
/// and a result that stays under 2^48.
fn shift(c: &Ctx, a: &[Expr], sign: f64) -> Ev {
    let x = tri!(bits(c, &a[0]));
    let s = tri!(num(c, &a[1])).trunc() * sign;
    if s.abs() > 53.0 {
        return e(ErrorKind::Num);
    }
    let r = if s >= 0.0 { x as f64 * 2f64.powi(s as i32) } else { (x >> (-s) as u32) as f64 };
    if r >= BIT_LIMIT { e(ErrorKind::Num) } else { n(r) }
}
pub(super) fn bitlshift(c: &Ctx, a: &[Expr]) -> Ev {
    shift(c, a, 1.0)
}
pub(super) fn bitrshift(c: &Ctx, a: &[Expr]) -> Ev {
    shift(c, a, -1.0)
}

pub(super) fn delta(c: &Ctx, a: &[Expr]) -> Ev {
    let (x, y) = (tri!(num(c, &a[0])), tri!(opt_num(c, a, 1, 0.0)));
    n(f64::from(u8::from(x == y)))
}
pub(super) fn gestep(c: &Ctx, a: &[Expr]) -> Ev {
    let (x, step) = (tri!(num(c, &a[0])), tri!(opt_num(c, a, 1, 0.0)));
    n(f64::from(u8::from(x >= step)))
}

// ---------- counting ----------

/// C(nn, k) for whole nn ≥ k ≥ 0, None past f64's range.
fn choose(nn: f64, k: f64) -> Option<f64> {
    let k = k.min(nn - k);
    let mut r = 1.0;
    for i in 0..k as u64 {
        r = r * (nn - i as f64) / (i as f64 + 1.0);
        if !r.is_finite() {
            return None;
        }
    }
    Some(r.round())
}

pub(super) fn permut(c: &Ctx, a: &[Expr]) -> Ev {
    let (nn, k) = (tri!(num(c, &a[0])).trunc(), tri!(num(c, &a[1])).trunc());
    if nn <= 0.0 && k > 0.0 || nn < 0.0 || k < 0.0 || k > nn {
        return e(ErrorKind::Num);
    }
    let mut r = 1.0;
    for i in 0..k as u64 {
        r *= nn - i as f64;
        if !r.is_finite() {
            return e(ErrorKind::Num);
        }
    }
    n(r)
}
pub(super) fn permutationa(c: &Ctx, a: &[Expr]) -> Ev {
    let (nn, k) = (tri!(num(c, &a[0])).trunc(), tri!(num(c, &a[1])).trunc());
    if nn < 0.0 || k < 0.0 {
        return e(ErrorKind::Num);
    }
    n(nn.powf(k))
}
pub(super) fn combina(c: &Ctx, a: &[Expr]) -> Ev {
    let (nn, k) = (tri!(num(c, &a[0])).trunc(), tri!(num(c, &a[1])).trunc());
    if nn < 0.0 || k < 0.0 || nn < k || nn > 1e9 {
        return e(ErrorKind::Num);
    }
    if nn == 0.0 {
        return n(1.0);
    }
    choose(nn + k - 1.0, k).map_or(e(ErrorKind::Num), n)
}
pub(super) fn factdouble(c: &Ctx, a: &[Expr]) -> Ev {
    let x = tri!(num(c, &a[0])).trunc();
    if x < 0.0 {
        return e(ErrorKind::Num);
    }
    if x > 400.0 {
        return e(ErrorKind::Num);
    }
    let mut r = 1.0;
    let mut k = x;
    while k > 1.0 {
        r *= k;
        k -= 2.0;
    }
    n(r)
}
pub(super) fn multinomial(c: &Ctx, a: &[Expr]) -> Ev {
    let mut parts = Vec::new();
    tri!(each_num(c, a, &mut |x| parts.push(x.trunc())));
    if parts.iter().any(|&x| x < 0.0) {
        return e(ErrorKind::Num);
    }
    // ∏ C(running total, part): exact while the result fits.
    let (mut total, mut r) = (0.0, 1.0);
    for p in parts {
        total += p;
        if total > 1e9 {
            return e(ErrorKind::Num);
        }
        match choose(total, p) {
            Some(cnk) => r *= cnk,
            None => return e(ErrorKind::Num),
        }
        if !r.is_finite() {
            return e(ErrorKind::Num);
        }
    }
    n(r)
}

/// SERIESSUM(x, n, m, coefficients): Σ aᵢ·x^(n + i·m).
pub(super) fn seriessum(c: &Ctx, a: &[Expr]) -> Ev {
    let (x, p, m) = (tri!(num(c, &a[0])), tri!(num(c, &a[1])), tri!(num(c, &a[2])));
    let arr = tri!(c.to_array(c.eval(&a[3])));
    let mut sum = 0.0;
    for (i, v) in arr.vals.iter().enumerate() {
        let k = match v {
            Value::Num(k) => *k,
            Value::Err(err) => return e(*err),
            _ => return e(ErrorKind::Value),
        };
        sum += k * x.powf(p + i as f64 * m);
    }
    n(sum)
}

pub(super) fn sqrtpi(c: &Ctx, a: &[Expr]) -> Ev {
    let x = tri!(num(c, &a[0]));
    if x < 0.0 { e(ErrorKind::Num) } else { n((x * std::f64::consts::PI).sqrt()) }
}

/// SUMX2MY2, SUMX2PY2 and SUMXMY2: the two arrays must be the same size;
/// pairs where either value isn't a number are skipped.
fn paired(c: &Ctx, a: &[Expr], f: fn(f64, f64) -> f64) -> Ev {
    let x = tri!(c.to_array(c.eval(&a[0])));
    let y = tri!(c.to_array(c.eval(&a[1])));
    if x.vals.len() != y.vals.len() {
        return e(ErrorKind::NA);
    }
    let mut sum = 0.0;
    for (u, v) in x.vals.iter().zip(&y.vals) {
        match (u, v) {
            (Value::Err(k), _) | (_, Value::Err(k)) => return e(*k),
            (Value::Num(u), Value::Num(v)) => sum += f(*u, *v),
            _ => {}
        }
    }
    n(sum)
}
pub(super) fn sumx2my2(c: &Ctx, a: &[Expr]) -> Ev {
    paired(c, a, |x, y| x * x - y * y)
}
pub(super) fn sumx2py2(c: &Ctx, a: &[Expr]) -> Ev {
    paired(c, a, |x, y| x * x + y * y)
}
pub(super) fn sumxmy2(c: &Ctx, a: &[Expr]) -> Ev {
    paired(c, a, |x, y| (x - y) * (x - y))
}

// ---------- reciprocal trigonometry ----------

/// Excel refuses angles of 2^27 or more.
const MAX_ANGLE: f64 = 134_217_728.0;

/// 1 / f(x): #NUM! for a huge angle, #DIV/0! where f is zero.
fn reciprocal(c: &Ctx, a: &[Expr], f: fn(f64) -> f64, bounded: bool) -> Ev {
    let x = tri!(num(c, &a[0]));
    if bounded && x.abs() >= MAX_ANGLE {
        return e(ErrorKind::Num);
    }
    let d = f(x);
    if d == 0.0 { e(ErrorKind::Div0) } else { n(1.0 / d) }
}
pub(super) fn cot(c: &Ctx, a: &[Expr]) -> Ev {
    reciprocal(c, a, f64::tan, true)
}
pub(super) fn coth(c: &Ctx, a: &[Expr]) -> Ev {
    reciprocal(c, a, f64::tanh, true)
}
pub(super) fn csc(c: &Ctx, a: &[Expr]) -> Ev {
    reciprocal(c, a, f64::sin, true)
}
pub(super) fn csch(c: &Ctx, a: &[Expr]) -> Ev {
    reciprocal(c, a, f64::sinh, true)
}
pub(super) fn sec(c: &Ctx, a: &[Expr]) -> Ev {
    reciprocal(c, a, f64::cos, true)
}
pub(super) fn sech(c: &Ctx, a: &[Expr]) -> Ev {
    let x = tri!(num(c, &a[0]));
    if x.abs() >= MAX_ANGLE { e(ErrorKind::Num) } else { n(1.0 / x.cosh()) }
}
pub(super) fn acot(c: &Ctx, a: &[Expr]) -> Ev {
    let x = tri!(num(c, &a[0]));
    n(std::f64::consts::FRAC_PI_2 - x.atan())
}
pub(super) fn acoth(c: &Ctx, a: &[Expr]) -> Ev {
    let x = tri!(num(c, &a[0]));
    if x.abs() <= 1.0 { e(ErrorKind::Num) } else { n(0.5 * ((x + 1.0) / (x - 1.0)).ln()) }
}

// ---------- rounding ----------

/// CEILING.PRECISE, ISO.CEILING (`up`) and FLOOR.PRECISE: toward +∞ or −∞
/// whatever the signs, to a multiple of the significance's size.
fn precise(c: &Ctx, a: &[Expr], up: bool) -> Ev {
    let x = tri!(num(c, &a[0]));
    let s = tri!(opt_num(c, a, 1, 1.0)).abs();
    if s == 0.0 {
        return n(0.0);
    }
    let q = super::more::snap(x / s);
    n(if up { q.ceil() } else { q.floor() } * s)
}
pub(super) fn ceiling_precise(c: &Ctx, a: &[Expr]) -> Ev {
    precise(c, a, true)
}
pub(super) fn floor_precise(c: &Ctx, a: &[Expr]) -> Ev {
    precise(c, a, false)
}
