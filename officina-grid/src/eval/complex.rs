//! Complex numbers as Excel writes them, text like `3+4i` or `-2.5j`
//! (docs/spec/officina.md §16.138): COMPLEX and the IM functions.

use super::{Ctx, Ev};
use crate::formula::Expr;
use crate::value::{ErrorKind, Value};

type R<T> = Result<T, ErrorKind>;

#[derive(Clone, Copy)]
struct Z {
    re: f64,
    im: f64,
}

impl Z {
    fn mul(self, o: Z) -> Z {
        Z { re: self.re * o.re - self.im * o.im, im: self.re * o.im + self.im * o.re }
    }
    fn div(self, o: Z) -> Option<Z> {
        let d = o.re * o.re + o.im * o.im;
        if d == 0.0 {
            return None;
        }
        Some(Z { re: (self.re * o.re + self.im * o.im) / d, im: (self.im * o.re - self.re * o.im) / d })
    }
    fn abs(self) -> f64 {
        self.re.hypot(self.im)
    }
    fn arg(self) -> f64 {
        self.im.atan2(self.re)
    }
    fn exp(self) -> Z {
        let m = self.re.exp();
        Z { re: m * self.im.cos(), im: m * self.im.sin() }
    }
    fn ln(self) -> Option<Z> {
        if self.re == 0.0 && self.im == 0.0 {
            return None;
        }
        Some(Z { re: self.abs().ln(), im: self.arg() })
    }
    fn sin(self) -> Z {
        Z { re: self.re.sin() * self.im.cosh(), im: self.re.cos() * self.im.sinh() }
    }
    fn cos(self) -> Z {
        Z { re: self.re.cos() * self.im.cosh(), im: -self.re.sin() * self.im.sinh() }
    }
    fn sinh(self) -> Z {
        Z { re: self.re.sinh() * self.im.cos(), im: self.re.cosh() * self.im.sin() }
    }
    fn cosh(self) -> Z {
        Z { re: self.re.cosh() * self.im.cos(), im: self.re.sinh() * self.im.sin() }
    }
}

const ONE: Z = Z { re: 1.0, im: 0.0 };

/// A number in text: digits, a point and an exponent, with a sign.
fn number(s: &str) -> R<f64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-')) {
        return Err(ErrorKind::Num);
    }
    s.parse().map_err(|_| ErrorKind::Num)
}

/// Text as a complex number and its suffix ('i', 'j', or none when real).
fn parse(text: &str) -> R<(Z, Option<char>)> {
    let t = text.trim();
    if t.is_empty() {
        return Ok((Z { re: 0.0, im: 0.0 }, None));
    }
    let Some(suffix) = t.chars().last().filter(|ch| matches!(ch, 'i' | 'j')) else {
        return Ok((Z { re: number(t)?, im: 0.0 }, None));
    };
    let body = &t[..t.len() - 1];
    // The imaginary part starts at the last sign that isn't an exponent's.
    let bytes = body.as_bytes();
    let split = (1..bytes.len()).rev().find(|&i| matches!(bytes[i], b'+' | b'-') && !matches!(bytes[i - 1], b'e' | b'E')).unwrap_or(0);
    let (re, im) = body.split_at(split);
    let im = match im {
        "" | "+" => 1.0,
        "-" => -1.0,
        x => number(x)?,
    };
    let re = if re.is_empty() { 0.0 } else { number(re)? };
    Ok((Z { re, im }, Some(suffix)))
}

/// An argument as a complex number: text, a number, or a blank (zero).
fn arg(c: &Ctx, a: &Expr) -> R<(Z, Option<char>)> {
    match c.scalar(a) {
        Value::Num(x) => Ok((Z { re: x, im: 0.0 }, None)),
        Value::Empty => Ok((Z { re: 0.0, im: 0.0 }, None)),
        Value::Str(s) => parse(&s),
        Value::Bool(_) => Err(ErrorKind::Value),
        Value::Err(k) => Err(k),
    }
}

/// The suffix of results from inputs with `a` and `b`: mixing i and j is
/// #VALUE!.
fn join(a: Option<char>, b: Option<char>) -> R<Option<char>> {
    match (a, b) {
        (Some(x), Some(y)) if x != y => Err(ErrorKind::Value),
        (Some(x), _) | (_, Some(x)) => Ok(Some(x)),
        _ => Ok(None),
    }
}

/// A part as Excel writes it: 15 significant digits.
fn part(x: f64) -> String {
    let r: f64 = format!("{x:.14e}").parse().unwrap_or(x);
    let a = r.abs();
    if a != 0.0 && !(1e-9..1e15).contains(&a) {
        let s = format!("{r:E}");
        return match s.split_once('E') {
            Some((m, e)) if !e.starts_with('-') => format!("{m}E+{e}"),
            _ => s,
        };
    }
    format!("{r}")
}

fn format(z: Z, suffix: char) -> R<String> {
    if !z.re.is_finite() || !z.im.is_finite() {
        return Err(ErrorKind::Num);
    }
    let (re, im) = (if z.re == 0.0 { 0.0 } else { z.re }, if z.im == 0.0 { 0.0 } else { z.im });
    if im == 0.0 {
        return Ok(part(re));
    }
    let imag = match im {
        1.0 => suffix.to_string(),
        -1.0 => format!("-{suffix}"),
        _ => format!("{}{suffix}", part(im)),
    };
    if re == 0.0 {
        return Ok(imag);
    }
    let sign = if imag.starts_with('-') { "" } else { "+" };
    Ok(format!("{}{sign}{imag}", part(re)))
}

fn out(r: R<(Z, Option<char>)>) -> Ev {
    match r.and_then(|(z, s)| format(z, s.unwrap_or('i'))) {
        Ok(t) => Ev::V(Value::str(&t)),
        Err(k) => Ev::err(k),
    }
}
fn real(r: R<f64>) -> Ev {
    match r {
        Ok(x) if x.is_finite() => Ev::V(Value::num(x)),
        Ok(_) => Ev::err(ErrorKind::Num),
        Err(k) => Ev::err(k),
    }
}

/// COMPLEX(real_num, i_num, [suffix]).
pub(super) fn complex(c: &Ctx, a: &[Expr]) -> Ev {
    let run = || -> R<(Z, Option<char>)> {
        let re = c.number(&c.scalar(&a[0]))?;
        let im = c.number(&c.scalar(&a[1]))?;
        let suffix = match a.get(2).map(|x| c.scalar(x)) {
            None | Some(Value::Empty) => 'i',
            Some(Value::Str(s)) if &*s == "i" || s.is_empty() => 'i',
            Some(Value::Str(s)) if &*s == "j" => 'j',
            _ => return Err(ErrorKind::Value),
        };
        Ok((Z { re, im }, Some(suffix)))
    };
    out(run())
}
pub(super) fn imreal(c: &Ctx, a: &[Expr]) -> Ev {
    real(arg(c, &a[0]).map(|(z, _)| z.re))
}
pub(super) fn imaginary(c: &Ctx, a: &[Expr]) -> Ev {
    real(arg(c, &a[0]).map(|(z, _)| z.im))
}
pub(super) fn imabs(c: &Ctx, a: &[Expr]) -> Ev {
    real(arg(c, &a[0]).map(|(z, _)| z.abs()))
}
pub(super) fn imargument(c: &Ctx, a: &[Expr]) -> Ev {
    real(arg(c, &a[0]).and_then(|(z, _)| if z.re == 0.0 && z.im == 0.0 { Err(ErrorKind::Div0) } else { Ok(z.arg()) }))
}
pub(super) fn imconjugate(c: &Ctx, a: &[Expr]) -> Ev {
    out(arg(c, &a[0]).map(|(z, s)| (Z { re: z.re, im: -z.im }, s)))
}
/// IMSUM and IMPRODUCT over any number of arguments.
fn fold(c: &Ctx, a: &[Expr], start: Z, f: fn(Z, Z) -> Z) -> Ev {
    let run = || -> R<(Z, Option<char>)> {
        let (mut acc, mut suffix) = (start, None);
        for x in a {
            let (z, s) = arg(c, x)?;
            suffix = join(suffix, s)?;
            acc = f(acc, z);
        }
        Ok((acc, suffix))
    };
    out(run())
}
pub(super) fn imsum(c: &Ctx, a: &[Expr]) -> Ev {
    fold(c, a, Z { re: 0.0, im: 0.0 }, |x, y| Z { re: x.re + y.re, im: x.im + y.im })
}
pub(super) fn improduct(c: &Ctx, a: &[Expr]) -> Ev {
    fold(c, a, ONE, Z::mul)
}
fn binary(c: &Ctx, a: &[Expr], f: fn(Z, Z) -> Option<Z>) -> Ev {
    let run = || -> R<(Z, Option<char>)> {
        let ((x, s), (y, t)) = (arg(c, &a[0])?, arg(c, &a[1])?);
        Ok((f(x, y).ok_or(ErrorKind::Num)?, join(s, t)?))
    };
    out(run())
}
pub(super) fn imsub(c: &Ctx, a: &[Expr]) -> Ev {
    binary(c, a, |x, y| Some(Z { re: x.re - y.re, im: x.im - y.im }))
}
pub(super) fn imdiv(c: &Ctx, a: &[Expr]) -> Ev {
    binary(c, a, Z::div)
}
/// IMPOWER(inumber, number): by polar form.
pub(super) fn impower(c: &Ctx, a: &[Expr]) -> Ev {
    let run = || -> R<(Z, Option<char>)> {
        let (z, s) = arg(c, &a[0])?;
        let p = c.number(&c.scalar(&a[1]))?;
        if z.re == 0.0 && z.im == 0.0 {
            return if p > 0.0 { Ok((z, s)) } else { Err(ErrorKind::Num) };
        }
        let (r, t) = (z.abs().powf(p), z.arg() * p);
        Ok((Z { re: r * t.cos(), im: r * t.sin() }, s))
    };
    out(run())
}
fn unary(c: &Ctx, a: &[Expr], f: fn(Z) -> Option<Z>) -> Ev {
    out(arg(c, &a[0]).and_then(|(z, s)| Ok((f(z).ok_or(ErrorKind::Num)?, s))))
}
pub(super) fn imsqrt(c: &Ctx, a: &[Expr]) -> Ev {
    unary(c, a, |z| {
        let (r, t) = (z.abs().sqrt(), z.arg() / 2.0);
        Some(Z { re: r * t.cos(), im: r * t.sin() })
    })
}
pub(super) fn imexp(c: &Ctx, a: &[Expr]) -> Ev {
    unary(c, a, |z| Some(z.exp()))
}
pub(super) fn imln(c: &Ctx, a: &[Expr]) -> Ev {
    unary(c, a, Z::ln)
}
pub(super) fn imlog10(c: &Ctx, a: &[Expr]) -> Ev {
    unary(c, a, |z| z.ln().map(|l| Z { re: l.re / std::f64::consts::LN_10, im: l.im / std::f64::consts::LN_10 }))
}
pub(super) fn imlog2(c: &Ctx, a: &[Expr]) -> Ev {
    unary(c, a, |z| z.ln().map(|l| Z { re: l.re / std::f64::consts::LN_2, im: l.im / std::f64::consts::LN_2 }))
}
pub(super) fn imsin(c: &Ctx, a: &[Expr]) -> Ev {
    unary(c, a, |z| Some(z.sin()))
}
pub(super) fn imcos(c: &Ctx, a: &[Expr]) -> Ev {
    unary(c, a, |z| Some(z.cos()))
}
pub(super) fn imtan(c: &Ctx, a: &[Expr]) -> Ev {
    unary(c, a, |z| z.sin().div(z.cos()))
}
pub(super) fn imcot(c: &Ctx, a: &[Expr]) -> Ev {
    unary(c, a, |z| z.cos().div(z.sin()))
}
pub(super) fn imsec(c: &Ctx, a: &[Expr]) -> Ev {
    unary(c, a, |z| ONE.div(z.cos()))
}
pub(super) fn imcsc(c: &Ctx, a: &[Expr]) -> Ev {
    unary(c, a, |z| ONE.div(z.sin()))
}
pub(super) fn imsinh(c: &Ctx, a: &[Expr]) -> Ev {
    unary(c, a, |z| Some(z.sinh()))
}
pub(super) fn imcosh(c: &Ctx, a: &[Expr]) -> Ev {
    unary(c, a, |z| Some(z.cosh()))
}
pub(super) fn imsech(c: &Ctx, a: &[Expr]) -> Ev {
    unary(c, a, |z| ONE.div(z.cosh()))
}
pub(super) fn imcsch(c: &Ctx, a: &[Expr]) -> Ev {
    unary(c, a, |z| ONE.div(z.sinh()))
}
