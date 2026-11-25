//! The second batch of Excel's everyday functions (docs/spec/officina.md
//! §16.132): rounding to a multiple, integer maths, trigonometry, times and
//! working days, character codes, SWITCH and a few information functions.
//! Each follows Excel's documented results, errors included.

use super::funcs::each_num;
use super::dates::DateSystem;
use super::{to_text, Ctx, Ev};
use crate::formula::{BinOp, Expr};
use crate::value::{ErrorKind, Value};
use std::collections::HashSet;

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
fn b(x: bool) -> Ev {
    Ev::V(Value::Bool(x))
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
fn text(c: &Ctx, a: &Expr) -> Result<std::sync::Arc<str>, ErrorKind> {
    to_text(&c.scalar(a))
}

/// `q` with the error of a division taken off: a quotient within 1e-12 of
/// a whole number is that number, so CEILING(0.3, 0.1) is 0.3, not 0.4.
fn snap(q: f64) -> f64 {
    let r = q.round();
    if (q - r).abs() <= 1e-12 * r.abs().max(1.0) { r } else { q }
}

// ---------- rounding to a multiple ----------

pub(super) fn ceiling(c: &Ctx, a: &[Expr]) -> Ev {
    let (x, s) = (tri!(num(c, &a[0])), tri!(num(c, &a[1])));
    if s == 0.0 {
        return n(0.0);
    }
    if x > 0.0 && s < 0.0 {
        return e(ErrorKind::Num);
    }
    n(snap(x / s).ceil() * s)
}

pub(super) fn floor(c: &Ctx, a: &[Expr]) -> Ev {
    let (x, s) = (tri!(num(c, &a[0])), tri!(num(c, &a[1])));
    if s == 0.0 {
        return if x == 0.0 { n(0.0) } else { e(ErrorKind::Div0) };
    }
    if x > 0.0 && s < 0.0 {
        return e(ErrorKind::Num);
    }
    n(snap(x / s).floor() * s)
}

/// CEILING.MATH (`up`) and FLOOR.MATH: the significance's sign is ignored;
/// a negative number goes toward zero, or away from it with a mode.
fn math_round(c: &Ctx, a: &[Expr], up: bool) -> Ev {
    let x = tri!(num(c, &a[0]));
    let s = tri!(opt_num(c, a, 1, 1.0)).abs();
    let mode = tri!(opt_num(c, a, 2, 0.0));
    if s == 0.0 {
        return n(0.0);
    }
    let q = snap(x / s);
    // Up for positives (CEILING) goes to +inf; for negatives a mode turns it away from zero.
    let ceil = if x >= 0.0 || mode == 0.0 { up } else { !up };
    n(if ceil { q.ceil() } else { q.floor() } * s)
}
pub(super) fn ceiling_math(c: &Ctx, a: &[Expr]) -> Ev {
    math_round(c, a, true)
}
pub(super) fn floor_math(c: &Ctx, a: &[Expr]) -> Ev {
    math_round(c, a, false)
}

pub(super) fn mround(c: &Ctx, a: &[Expr]) -> Ev {
    let (x, m) = (tri!(num(c, &a[0])), tri!(num(c, &a[1])));
    if m == 0.0 || x == 0.0 {
        return n(0.0);
    }
    if (x < 0.0) != (m < 0.0) {
        return e(ErrorKind::Num);
    }
    let q = snap(x / m);
    // Halves away from zero, as ROUND.
    n(super::funcs::round_dec(q, 0, 0) * m)
}

pub(super) fn quotient(c: &Ctx, a: &[Expr]) -> Ev {
    let (x, d) = (tri!(num(c, &a[0])), tri!(num(c, &a[1])));
    if d == 0.0 {
        return e(ErrorKind::Div0);
    }
    n(snap(x / d).trunc())
}

pub(super) fn even(c: &Ctx, a: &[Expr]) -> Ev {
    let x = tri!(num(c, &a[0]));
    n(x.signum() * (snap(x.abs() / 2.0).ceil() * 2.0))
}

pub(super) fn odd(c: &Ctx, a: &[Expr]) -> Ev {
    let x = tri!(num(c, &a[0]));
    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    n(sign * (snap((x.abs() + 1.0) / 2.0).ceil() * 2.0 - 1.0))
}

// ---------- integers ----------

pub(super) fn fact(c: &Ctx, a: &[Expr]) -> Ev {
    let x = tri!(num(c, &a[0])).trunc();
    if !(0.0..=170.0).contains(&x) {
        return e(ErrorKind::Num);
    }
    n((2..=x as u32).fold(1.0, |p, k| p * f64::from(k)))
}

pub(super) fn combin(c: &Ctx, a: &[Expr]) -> Ev {
    let (nn, k) = (tri!(num(c, &a[0])).trunc(), tri!(num(c, &a[1])).trunc());
    if nn < 0.0 || k < 0.0 || k > nn || nn > 1e9 {
        return e(ErrorKind::Num);
    }
    let k = k.min(nn - k);
    let mut r = 1.0;
    for i in 0..k as u64 {
        r = r * (nn - i as f64) / (i as f64 + 1.0);
        if !r.is_finite() {
            return e(ErrorKind::Num);
        }
    }
    n(r.round())
}

/// The arguments as whole numbers for GCD and LCM: each 0 to 2^53,
/// fractions dropped.
fn integers(c: &Ctx, a: &[Expr]) -> Result<Vec<u64>, ErrorKind> {
    let mut v = Vec::new();
    let mut bad = false;
    each_num(c, a, &mut |x| {
        if !(0.0..9.007_199_254_740_992e15).contains(&x) {
            bad = true;
        } else {
            v.push(x.trunc() as u64);
        }
    })?;
    if bad { Err(ErrorKind::Num) } else { Ok(v) }
}
fn gcd2(mut x: u64, mut y: u64) -> u64 {
    while y != 0 {
        (x, y) = (y, x % y);
    }
    x
}
pub(super) fn gcd(c: &Ctx, a: &[Expr]) -> Ev {
    let v = tri!(integers(c, a));
    n(v.into_iter().fold(0, gcd2) as f64)
}
pub(super) fn lcm(c: &Ctx, a: &[Expr]) -> Ev {
    let v = tri!(integers(c, a));
    if v.contains(&0) {
        return n(0.0);
    }
    let mut l: u64 = 1;
    for x in v {
        let Some(next) = (l / gcd2(l, x)).checked_mul(x) else { return e(ErrorKind::Num) };
        l = next;
    }
    n(l as f64)
}

pub(super) fn sumsq(c: &Ctx, a: &[Expr]) -> Ev {
    let mut s = 0.0;
    tri!(each_num(c, a, &mut |x| s += x * x));
    n(s)
}

// ---------- trigonometry ----------

fn unary(c: &Ctx, a: &[Expr], f: fn(f64) -> Option<f64>) -> Ev {
    match f(tri!(num(c, &a[0]))) {
        Some(y) => n(y),
        None => e(ErrorKind::Num),
    }
}
pub(super) fn radians(c: &Ctx, a: &[Expr]) -> Ev {
    unary(c, a, |x| Some(x.to_radians()))
}
pub(super) fn degrees(c: &Ctx, a: &[Expr]) -> Ev {
    unary(c, a, |x| Some(x.to_degrees()))
}
/// Excel refuses angles of 2^27 or more for SIN, COS and TAN.
const MAX_ANGLE: f64 = 134_217_728.0;
pub(super) fn sin(c: &Ctx, a: &[Expr]) -> Ev {
    unary(c, a, |x| (x.abs() < MAX_ANGLE).then(|| x.sin()))
}
pub(super) fn cos(c: &Ctx, a: &[Expr]) -> Ev {
    unary(c, a, |x| (x.abs() < MAX_ANGLE).then(|| x.cos()))
}
pub(super) fn tan(c: &Ctx, a: &[Expr]) -> Ev {
    unary(c, a, |x| (x.abs() < MAX_ANGLE).then(|| x.tan()))
}
pub(super) fn asin(c: &Ctx, a: &[Expr]) -> Ev {
    unary(c, a, |x| (-1.0..=1.0).contains(&x).then(|| x.asin()))
}
pub(super) fn acos(c: &Ctx, a: &[Expr]) -> Ev {
    unary(c, a, |x| (-1.0..=1.0).contains(&x).then(|| x.acos()))
}
pub(super) fn atan(c: &Ctx, a: &[Expr]) -> Ev {
    unary(c, a, |x| Some(x.atan()))
}
/// ATAN2(x_num, y_num): Excel puts x first.
pub(super) fn atan2(c: &Ctx, a: &[Expr]) -> Ev {
    let (x, y) = (tri!(num(c, &a[0])), tri!(num(c, &a[1])));
    if x == 0.0 && y == 0.0 {
        return e(ErrorKind::Div0);
    }
    n(y.atan2(x))
}

// ---------- times and dates ----------

pub(super) fn time(c: &Ctx, a: &[Expr]) -> Ev {
    let (h, m, s) = (tri!(num(c, &a[0])).trunc(), tri!(num(c, &a[1])).trunc(), tri!(num(c, &a[2])).trunc());
    if [h, m, s].iter().any(|x| x.abs() > 32767.0) {
        return e(ErrorKind::Num);
    }
    let total = h * 3600.0 + m * 60.0 + s;
    if total < 0.0 {
        return e(ErrorKind::Num);
    }
    n(total.rem_euclid(86400.0) / 86400.0)
}

/// The time of day in `x` as whole seconds (rounded to the nearest, as
/// Excel shows it), or #NUM! for a negative serial.
fn seconds_of_day(c: &Ctx, a: &[Expr]) -> Result<u32, ErrorKind> {
    let x = num(c, &a[0])?;
    if x < 0.0 || x >= 2958466.0 {
        return Err(ErrorKind::Num);
    }
    let s = ((x - x.floor()) * 86400.0).round() as u32;
    Ok(if s >= 86400 { 0 } else { s })
}
pub(super) fn hour(c: &Ctx, a: &[Expr]) -> Ev {
    n(f64::from(tri!(seconds_of_day(c, a)) / 3600))
}
pub(super) fn minute(c: &Ctx, a: &[Expr]) -> Ev {
    n(f64::from(tri!(seconds_of_day(c, a)) / 60 % 60))
}
pub(super) fn second(c: &Ctx, a: &[Expr]) -> Ev {
    n(f64::from(tri!(seconds_of_day(c, a)) % 60))
}

/// A serial's whole day and its year, month and day, or #NUM! outside the
/// calendar.
fn day_of(c: &Ctx, x: f64) -> Result<(f64, (i64, i64, i64)), ErrorKind> {
    let d = x.floor();
    Ok((d, c.wb.date_system().ymd(d).ok_or(ErrorKind::Num)?))
}

pub(super) fn edate(c: &Ctx, a: &[Expr]) -> Ev {
    let start = tri!(num(c, &a[0]));
    let months = tri!(num(c, &a[1])).trunc();
    let (_, (_, _, day)) = tri!(day_of(c, start));
    let Some(last) = c.wb.date_system().month_end(start.floor(), months) else { return e(ErrorKind::Num) };
    let (_, (_, _, last_day)) = tri!(day_of(c, last));
    n(last - (last_day - day.min(last_day)) as f64)
}

pub(super) fn days(c: &Ctx, a: &[Expr]) -> Ev {
    let (end, start) = (tri!(num(c, &a[0])), tri!(num(c, &a[1])));
    let (end, _) = tri!(day_of(c, end));
    let (start, _) = tri!(day_of(c, start));
    n(end - start)
}

pub(super) fn datedif(c: &Ctx, a: &[Expr]) -> Ev {
    let (s, (y1, m1, d1)) = tri!(day_of(c, tri!(num(c, &a[0]))));
    let (t, (y2, m2, d2)) = tri!(day_of(c, tri!(num(c, &a[1]))));
    if s > t {
        return e(ErrorKind::Num);
    }
    let months = (y2 - y1) * 12 + (m2 - m1) - i64::from(d2 < d1);
    let system = c.wb.date_system();
    let r = match tri!(text(c, &a[2])).to_ascii_uppercase().as_str() {
        "Y" => months / 12,
        "M" => months,
        "D" => (t - s) as i64,
        "YM" => months % 12,
        // Days past the last whole month (DATE rolls a day past a month's end over).
        "MD" => {
            let Some(from) = system.date(y2 as f64, (m2 - i64::from(d2 < d1)) as f64, d1 as f64) else { return e(ErrorKind::Num) };
            (t - from) as i64
        }
        // Days past the last anniversary.
        "YD" => {
            let mut from = system.date(y2 as f64, m1 as f64, d1 as f64);
            if from.is_some_and(|f| f > t) {
                from = system.date((y2 - 1) as f64, m1 as f64, d1 as f64);
            }
            let Some(from) = from else { return e(ErrorKind::Num) };
            (t - from) as i64
        }
        _ => return e(ErrorKind::Num),
    };
    n(r as f64)
}

/// The most holidays NETWORKDAYS and WORKDAY take.
const MAX_HOLIDAYS: usize = 100_000;

fn holidays(c: &Ctx, a: &[Expr]) -> Result<HashSet<i64>, ErrorKind> {
    let mut set = HashSet::new();
    let Some(arg) = a.get(2).filter(|x| !missing(x)) else { return Ok(set) };
    let mut over = false;
    each_num(c, std::slice::from_ref(arg), &mut |x| {
        if set.len() < MAX_HOLIDAYS {
            set.insert(x.floor() as i64);
        } else {
            over = true;
        }
    })?;
    if over { Err(ErrorKind::Num) } else { Ok(set) }
}

fn weekend(c: &Ctx, day: i64) -> bool {
    matches!(c.wb.date_system().weekday(day), 0 | 6)
}

/// Monday-to-Friday days from `s` to `t` inclusive (s <= t).
fn weekdays(c: &Ctx, s: i64, t: i64) -> i64 {
    let total = t - s + 1;
    let (weeks, rest) = (total / 7, total % 7);
    weeks * 5 + (0..rest).filter(|i| !weekend(c, s + weeks * 7 + i)).count() as i64
}

pub(super) fn networkdays(c: &Ctx, a: &[Expr]) -> Ev {
    let (s, _) = tri!(day_of(c, tri!(num(c, &a[0]))));
    let (t, _) = tri!(day_of(c, tri!(num(c, &a[1]))));
    let hol = tri!(holidays(c, a));
    let (lo, hi, sign) = if s <= t { (s as i64, t as i64, 1.0) } else { (t as i64, s as i64, -1.0) };
    let off = hol.iter().filter(|&&h| (lo..=hi).contains(&h) && !weekend(c, h)).count() as i64;
    n(sign * (weekdays(c, lo, hi) - off) as f64)
}

pub(super) fn workday(c: &Ctx, a: &[Expr]) -> Ev {
    let (s, _) = tri!(day_of(c, tri!(num(c, &a[0]))));
    let k = tri!(num(c, &a[1])).trunc();
    let hol = tri!(holidays(c, a));
    if k.abs() > 3_000_000.0 {
        return e(ErrorKind::Num);
    }
    let step = if k < 0.0 { -1 } else { 1 };
    let (mut day, mut left) = (s as i64, k.abs() as i64);
    // Whole weeks first when no holiday can fall in them.
    if hol.is_empty() && left > 10 {
        let weeks = left / 5 - 1;
        day += step * weeks * 7;
        left -= weeks * 5;
    }
    while left > 0 {
        day += step;
        if !weekend(c, day) && !hol.contains(&day) {
            left -= 1;
        }
    }
    let system = c.wb.date_system();
    if system.ymd(day as f64).is_none() {
        return e(ErrorKind::Num);
    }
    n(day as f64)
}

pub(super) fn weeknum(c: &Ctx, a: &[Expr]) -> Ev {
    let (d, (y, _, _)) = tri!(day_of(c, tri!(num(c, &a[0]))));
    let kind = tri!(opt_num(c, a, 1, 1.0));
    let start = match kind as i64 {
        _ if kind.fract() != 0.0 => return e(ErrorKind::Num),
        1 | 17 => 0,
        2 | 11 => 1,
        12 => 2,
        13 => 3,
        14 => 4,
        15 => 5,
        16 => 6,
        21 => return iso_week(c, d as i64),
        _ => return e(ErrorKind::Num),
    };
    let system = c.wb.date_system();
    let Some(jan1) = system.date(y as f64, 1.0, 1.0) else { return e(ErrorKind::Num) };
    let lead = (system.weekday(jan1 as i64) - start).rem_euclid(7);
    n(((d - jan1) as i64 + lead).div_euclid(7) as f64 + 1.0)
}

fn iso_week(c: &Ctx, d: i64) -> Ev {
    let system = c.wb.date_system();
    let monday0 = (system.weekday(d) + 6) % 7;
    let thursday = d - monday0 + 3;
    let Some((y, _, _)) = system.ymd(thursday as f64) else { return e(ErrorKind::Num) };
    let Some(jan1) = system.date(y as f64, 1.0, 1.0) else { return e(ErrorKind::Num) };
    n(((thursday - jan1 as i64) / 7 + 1) as f64)
}

pub(super) fn isoweeknum(c: &Ctx, a: &[Expr]) -> Ev {
    let (d, _) = tri!(day_of(c, tri!(num(c, &a[0]))));
    iso_week(c, d as i64)
}

// ---------- text ----------

pub(super) fn proper(c: &Ctx, a: &[Expr]) -> Ev {
    let t = tri!(text(c, &a[0]));
    let mut out = String::with_capacity(t.len());
    let mut after_letter = false;
    for ch in t.chars() {
        if ch.is_alphabetic() {
            if after_letter { out.extend(ch.to_lowercase()) } else { out.extend(ch.to_uppercase()) }
        } else {
            out.push(ch);
        }
        after_letter = ch.is_alphabetic();
    }
    Ev::V(Value::str(&out))
}

pub(super) fn char_fn(c: &Ctx, a: &[Expr]) -> Ev {
    let x = tri!(num(c, &a[0])).trunc();
    if !(1.0..=255.0).contains(&x) {
        return e(ErrorKind::Value);
    }
    Ev::V(Value::str(&crate::csv::cp1252(x as u8).to_string()))
}

pub(super) fn code(c: &Ctx, a: &[Expr]) -> Ev {
    let t = tri!(text(c, &a[0]));
    let Some(ch) = t.chars().next() else { return e(ErrorKind::Value) };
    // The Windows-1252 byte, or ? for a character it lacks.
    let byte = (1..=255u8).find(|&b| crate::csv::cp1252(b) == ch).unwrap_or(b'?');
    n(f64::from(byte))
}

pub(super) fn unichar(c: &Ctx, a: &[Expr]) -> Ev {
    let x = tri!(num(c, &a[0])).trunc();
    match (x >= 1.0 && x <= 1_114_111.0).then(|| char::from_u32(x as u32)).flatten() {
        Some(ch) => Ev::V(Value::str(&ch.to_string())),
        None => e(ErrorKind::Value),
    }
}

pub(super) fn unicode(c: &Ctx, a: &[Expr]) -> Ev {
    match tri!(text(c, &a[0])).chars().next() {
        Some(ch) => n(f64::from(u32::from(ch))),
        None => e(ErrorKind::Value),
    }
}

pub(super) fn clean(c: &Ctx, a: &[Expr]) -> Ev {
    let t = tri!(text(c, &a[0]));
    Ev::V(Value::str(&t.chars().filter(|&ch| u32::from(ch) >= 32).collect::<String>()))
}

// ---------- logic and information ----------

pub(super) fn switch(c: &Ctx, a: &[Expr]) -> Ev {
    let key = c.scalar(&a[0]);
    if let Value::Err(k) = key {
        return e(k);
    }
    let rest = &a[1..];
    for pair in rest.chunks(2) {
        if pair.len() == 2 {
            let candidate = c.scalar(&pair[0]);
            if let Value::Err(k) = candidate {
                return e(k);
            }
            if super::binop(BinOp::Eq, &key, &candidate) == Value::Bool(true) {
                return c.eval(&pair[1]);
            }
        } else {
            return c.eval(&pair[0]);
        }
    }
    e(ErrorKind::NA)
}

fn parity(c: &Ctx, a: &[Expr]) -> Result<bool, ErrorKind> {
    let v = c.scalar(&a[0]);
    if matches!(v, Value::Bool(_)) {
        return Err(ErrorKind::Value);
    }
    Ok(c.number(&v)?.trunc() % 2.0 == 0.0)
}
pub(super) fn iseven(c: &Ctx, a: &[Expr]) -> Ev {
    b(tri!(parity(c, a)))
}
pub(super) fn isodd(c: &Ctx, a: &[Expr]) -> Ev {
    b(!tri!(parity(c, a)))
}

pub(super) fn n_fn(c: &Ctx, a: &[Expr]) -> Ev {
    match c.scalar(&a[0]) {
        Value::Num(x) => n(x),
        Value::Bool(x) => n(f64::from(u8::from(x))),
        Value::Err(k) => e(k),
        _ => n(0.0),
    }
}

pub(super) fn t_fn(c: &Ctx, a: &[Expr]) -> Ev {
    match c.scalar(&a[0]) {
        Value::Str(s) => Ev::V(Value::Str(s)),
        Value::Err(k) => e(k),
        _ => Ev::V(Value::str("")),
    }
}

// ---------- conditional extremes and statistics ----------

fn extreme_if(c: &Ctx, a: &[Expr], max: bool) -> Ev {
    let range = tri!(super::funcs::rect_arg(c, &a[0]));
    let agg = tri!(super::funcs::matching_extremes(c, range, &a[1..]));
    if let Some(k) = agg.error {
        return e(k);
    }
    // No matching number gives 0, as Excel.
    n(if max { agg.max } else { agg.min }.unwrap_or(0.0))
}
pub(super) fn maxifs(c: &Ctx, a: &[Expr]) -> Ev {
    extreme_if(c, a, true)
}
pub(super) fn minifs(c: &Ctx, a: &[Expr]) -> Ev {
    extreme_if(c, a, false)
}

/// The numbers of one argument as SUM reads them, at most [`super::MAX_ARRAY`].
fn numbers(c: &Ctx, a: &Expr) -> Result<Vec<f64>, ErrorKind> {
    let mut v = Vec::new();
    let mut over = false;
    each_num(c, std::slice::from_ref(a), &mut |x| {
        if v.len() < super::MAX_ARRAY as usize { v.push(x) } else { over = true }
    })?;
    if over { Err(ErrorKind::Limit) } else { Ok(v) }
}

/// RANK, RANK.EQ (`avg` false) and RANK.AVG: the number's place in the
/// reference's numbers, largest first unless `order` is non-zero.
fn rank_of(c: &Ctx, a: &[Expr], avg: bool) -> Ev {
    let x = tri!(num(c, &a[0]));
    // Excel takes only a reference here.
    let Ev::R(_) = c.eval(&a[1]) else { return e(ErrorKind::Value) };
    let v = tri!(numbers(c, &a[1]));
    let ascending = tri!(opt_num(c, a, 2, 0.0)) != 0.0;
    let ties = v.iter().filter(|&&y| y == x).count();
    if ties == 0 {
        return e(ErrorKind::NA);
    }
    let before = v.iter().filter(|&&y| if ascending { y < x } else { y > x }).count();
    let first = before as f64 + 1.0;
    n(if avg { first + (ties as f64 - 1.0) / 2.0 } else { first })
}
pub(super) fn rank_eq(c: &Ctx, a: &[Expr]) -> Ev {
    rank_of(c, a, false)
}
pub(super) fn rank_avg(c: &Ctx, a: &[Expr]) -> Ev {
    rank_of(c, a, true)
}

/// The `k` percentile of the numbers in `a[0]`: inclusive (0 to 1), or
/// exclusive (strictly between, and only where the data reaches).
fn percentile_of(c: &Ctx, a: &Expr, k: f64, exclusive: bool) -> Ev {
    let mut v = tri!(numbers(c, a));
    if v.is_empty() {
        return e(ErrorKind::Num);
    }
    v.sort_by(f64::total_cmp);
    let len = v.len() as f64;
    let h = if exclusive {
        if k <= 0.0 || k >= 1.0 {
            return e(ErrorKind::Num);
        }
        let h = (len + 1.0) * k - 1.0;
        if h < 0.0 || h > len - 1.0 {
            return e(ErrorKind::Num);
        }
        h
    } else {
        if !(0.0..=1.0).contains(&k) {
            return e(ErrorKind::Num);
        }
        (len - 1.0) * k
    };
    let lo = h.floor() as usize;
    let hi = (lo + 1).min(v.len() - 1);
    n(v[lo] + (h - lo as f64) * (v[hi] - v[lo]))
}
pub(super) fn percentile_inc(c: &Ctx, a: &[Expr]) -> Ev {
    let k = tri!(num(c, &a[1]));
    percentile_of(c, &a[0], k, false)
}
pub(super) fn percentile_exc(c: &Ctx, a: &[Expr]) -> Ev {
    let k = tri!(num(c, &a[1]));
    percentile_of(c, &a[0], k, true)
}
pub(super) fn quartile_inc(c: &Ctx, a: &[Expr]) -> Ev {
    let q = tri!(num(c, &a[1])).trunc();
    if !(0.0..=4.0).contains(&q) {
        return e(ErrorKind::Num);
    }
    percentile_of(c, &a[0], q / 4.0, false)
}
pub(super) fn quartile_exc(c: &Ctx, a: &[Expr]) -> Ev {
    let q = tri!(num(c, &a[1])).trunc();
    if !(1.0..=3.0).contains(&q) {
        return e(ErrorKind::Num);
    }
    percentile_of(c, &a[0], q / 4.0, true)
}

/// MODE and MODE.SNGL: the number that occurs most, the first met among
/// equals; #N/A when none repeats.
pub(super) fn mode_sngl(c: &Ctx, a: &[Expr]) -> Ev {
    let mut v = Vec::new();
    tri!(each_num(c, a, &mut |x| v.push(x)));
    let mut counts: std::collections::HashMap<u64, (usize, usize)> = std::collections::HashMap::new();
    for (i, x) in v.iter().enumerate() {
        let key = if *x == 0.0 { 0u64 } else { x.to_bits() };
        counts.entry(key).or_insert((0, i)).0 += 1;
    }
    match counts.values().filter(|c| c.0 > 1).max_by(|p, q| p.0.cmp(&q.0).then(q.1.cmp(&p.1))) {
        Some(&(_, i)) => n(v[i]),
        None => e(ErrorKind::NA),
    }
}

// The fourth batch (§16.134).

/// FIXED(number, [decimals], [no_commas]): the number rounded to `decimals`
/// places (left of the point when negative) as text, with thousands commas
/// unless `no_commas`.
pub(super) fn fixed(c: &Ctx, a: &[Expr]) -> Ev {
    let x = tri!(num(c, &a[0]));
    let d = tri!(opt_num(c, a, 1, 2.0)).trunc();
    let no_commas = match a.get(2).filter(|x| !missing(x)) {
        Some(arg) => tri!(super::to_bool(&c.scalar(arg))),
        None => false,
    };
    if d > 127.0 {
        return e(ErrorKind::Value);
    }
    Ev::V(Value::str(&fixed_text(x, d.max(-128.0) as i32, !no_commas)))
}

/// `x` rounded to `d` decimals as text, with thousands commas when asked.
pub(super) fn fixed_text(x: f64, d: i32, commas: bool) -> String {
    let rounded = super::funcs::round_dec(x, d, 0);
    let digits = format!("{:.*}", d.max(0) as usize, rounded.abs());
    let (whole, frac) = digits.split_once('.').map_or((digits.as_str(), None), |(w, f)| (w, Some(f)));
    let mut out = String::new();
    if rounded < 0.0 {
        out.push('-');
    }
    for (i, ch) in whole.chars().enumerate() {
        if commas && i > 0 && (whole.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    if let Some(frac) = frac {
        out.push('.');
        out.push_str(frac);
    }
    out
}

/// YEARFRAC(start, end, [basis]): the fraction of a year between two dates
/// on a day-count basis: 0 US 30/360, 1 actual/actual, 2 actual/360,
/// 3 actual/365, 4 European 30/360.
pub(super) fn yearfrac(c: &Ctx, a: &[Expr]) -> Ev {
    let (s, t) = (tri!(num(c, &a[0])), tri!(num(c, &a[1])));
    let basis = tri!(opt_num(c, a, 2, 0.0)).trunc();
    let (s, _) = tri!(day_of(c, s));
    let (t, _) = tri!(day_of(c, t));
    if basis.fract() != 0.0 || !(0.0..=4.0).contains(&basis) {
        return e(ErrorKind::Num);
    }
    match year_frac(c.wb.date_system(), s.min(t), s.max(t), basis as i32) {
        Some(r) => n(r),
        None => e(ErrorKind::Num),
    }
}

/// Days from `s` to `t` (whole-day serials, s ≤ t) on a 30/360 count: the
/// US (NASD) rules with their February month-ends, or the European ones.
pub(super) fn days_360(system: DateSystem, s: f64, t: f64, european: bool) -> Option<f64> {
    let (y1, m1, mut d1) = system.ymd(s)?;
    let (y2, m2, mut d2) = system.ymd(t)?;
    if european {
        (d1, d2) = (d1.min(30), d2.min(30));
    } else {
        let month_last = |day: f64| system.month_end(day, 0.0).is_some_and(|last| last == day);
        if d1 == 31 && d2 == 31 {
            (d1, d2) = (30, 30);
        } else if d1 == 31 {
            d1 = 30;
        } else if d1 == 30 && d2 == 31 {
            d2 = 30;
        } else if m1 == 2 && m2 == 2 && month_last(s) && month_last(t) {
            (d1, d2) = (30, 30);
        } else if m1 == 2 && month_last(s) {
            d1 = 30;
        }
    }
    Some(((y2 - y1) * 360 + (m2 - m1) * 30 + (d2 - d1)) as f64)
}

/// YEARFRAC's fraction of a year from `s` to `t` (s ≤ t) on `basis` 0–4.
pub(super) fn year_frac(system: DateSystem, s: f64, t: f64, basis: i32) -> Option<f64> {
    Some(match basis {
        0 => days_360(system, s, t, false)? / 360.0,
        1 => {
            let (y1, m1, d1) = system.ymd(s)?;
            let (y2, m2, d2) = system.ymd(t)?;
            let leap = |y: i64| (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
            let within_year = y1 == y2 || (y1 + 1 == y2 && (m1, d1) >= (m2, d2));
            if within_year {
                let feb29 = (leap(y1) && m1 <= 2) || (leap(y2) && (m2 > 2 || (m2 == 2 && d2 == 29)));
                let year = if (y1 == y2 && leap(y1)) || feb29 { 366.0 } else { 365.0 };
                (t - s) / year
            } else {
                let first = system.date(y1 as f64, 1.0, 1.0)?;
                let after = system.date((y2 + 1) as f64, 1.0, 1.0)?;
                (t - s) / ((after - first) / (y2 - y1 + 1) as f64)
            }
        }
        2 => (t - s) / 360.0,
        3 => (t - s) / 365.0,
        4 => days_360(system, s, t, true)? / 360.0,
        _ => return None,
    })
}

pub(super) fn sinh(c: &Ctx, a: &[Expr]) -> Ev {
    unary(c, a, |x| Some(x.sinh()))
}
pub(super) fn cosh(c: &Ctx, a: &[Expr]) -> Ev {
    unary(c, a, |x| Some(x.cosh()))
}
pub(super) fn tanh(c: &Ctx, a: &[Expr]) -> Ev {
    unary(c, a, |x| Some(x.tanh()))
}
pub(super) fn asinh(c: &Ctx, a: &[Expr]) -> Ev {
    unary(c, a, |x| Some(x.asinh()))
}
pub(super) fn acosh(c: &Ctx, a: &[Expr]) -> Ev {
    unary(c, a, |x| (x >= 1.0).then(|| x.acosh()))
}
pub(super) fn atanh(c: &Ctx, a: &[Expr]) -> Ev {
    unary(c, a, |x| (x.abs() < 1.0).then(|| x.atanh()))
}
