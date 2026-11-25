//! Text and information functions and number bases (docs/spec/officina.md
//! §16.135): REPLACE, DOLLAR, NUMBERVALUE, ROMAN and ARABIC, BASE and
//! DECIMAL, the twelve BIN/OCT/DEC/HEX conversions, TYPE, ERROR.TYPE, SHEET,
//! SHEETS, ISFORMULA and ISNONTEXT.

use super::funcs::rect_arg;
use super::more::fixed_text;
use super::{to_text, Ctx, Ev};
use crate::addr::CellAddr;
use crate::formula::Expr;
use crate::strings::MAX_TEXT;
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

fn missing(a: &Expr) -> bool {
    matches!(a, Expr::Name(n) if n.is_empty())
}
fn given(a: &[Expr], i: usize) -> Option<&Expr> {
    a.get(i).filter(|x| !missing(x))
}
fn num(c: &Ctx, a: &Expr) -> Result<f64, ErrorKind> {
    c.number(&c.scalar(a))
}
fn text(c: &Ctx, a: &Expr) -> Result<Arc<str>, ErrorKind> {
    to_text(&c.scalar(a))
}
fn s(x: &str) -> Ev {
    if x.len() > MAX_TEXT { Ev::err(ErrorKind::Value) } else { Ev::V(Value::str(x)) }
}
fn n(x: f64) -> Ev {
    if x.is_finite() { Ev::V(Value::num(x)) } else { Ev::err(ErrorKind::Num) }
}

/// REPLACE(old_text, start_num, num_chars, new_text), counting characters.
pub(super) fn replace(c: &Ctx, a: &[Expr]) -> Ev {
    let old = tri!(text(c, &a[0]));
    let start = tri!(num(c, &a[1])).trunc();
    let count = tri!(num(c, &a[2])).trunc();
    let new = tri!(text(c, &a[3]));
    if start < 1.0 || count < 0.0 {
        return Ev::err(ErrorKind::Value);
    }
    let byte_at = |k: f64| old.char_indices().nth((k as usize).min(old.len())).map_or(old.len(), |(i, _)| i);
    let from = byte_at(start - 1.0);
    let to = byte_at(start - 1.0 + count);
    s(&format!("{}{}{}", &old[..from], new, &old[to..]))
}

/// DOLLAR(number, [decimals]): FIXED with a dollar sign, and negative
/// amounts in parentheses.
pub(super) fn dollar(c: &Ctx, a: &[Expr]) -> Ev {
    let x = tri!(num(c, &a[0]));
    let d = match given(a, 1) {
        Some(arg) => tri!(num(c, arg)).trunc(),
        None => 2.0,
    };
    if d > 127.0 {
        return Ev::err(ErrorKind::Value);
    }
    let t = fixed_text(x, d.max(-128.0) as i32, true);
    match t.strip_prefix('-') {
        Some(abs) => s(&format!("(${abs})")),
        None => s(&format!("${t}")),
    }
}

/// NUMBERVALUE(text, [decimal_separator], [group_separator]): a number read
/// from text in any locale's separators; spaces are ignored and each
/// trailing % divides by 100.
pub(super) fn numbervalue(c: &Ctx, a: &[Expr]) -> Ev {
    let t = tri!(text(c, &a[0]));
    let sep = |i: usize, default: char| -> Result<char, ErrorKind> {
        match given(a, i) {
            Some(arg) => text(c, arg)?.chars().next().ok_or(ErrorKind::Value),
            None => Ok(default),
        }
    };
    let (dec, group) = (tri!(sep(1, '.')), tri!(sep(2, ',')));
    if dec == group {
        return Ev::err(ErrorKind::Value);
    }
    let mut body: String = t.chars().filter(|ch| !ch.is_whitespace()).collect();
    let mut scale = 1.0;
    while let Some(rest) = body.strip_suffix('%') {
        body = rest.to_string();
        scale /= 100.0;
    }
    if body.is_empty() {
        return n(0.0);
    }
    let (whole, frac) = match body.split_once(dec) {
        Some((w, f)) => (w.to_string(), Some(f.to_string())),
        None => (body, None),
    };
    if frac.as_deref().is_some_and(|f| f.contains(dec) || f.contains(group)) {
        return Ev::err(ErrorKind::Value);
    }
    let whole: String = whole.chars().filter(|&ch| ch != group).collect();
    let plain = match frac {
        Some(f) => format!("{whole}.{f}"),
        None => whole,
    };
    let ok = !plain.is_empty() && plain.chars().all(|ch| ch.is_ascii_digit() || matches!(ch, '.' | '-' | '+'));
    match plain.parse::<f64>() {
        Ok(x) if ok => n(x * scale),
        _ => Ev::err(ErrorKind::Value),
    }
}

const ROMAN: [(char, u32); 7] = [('M', 1000), ('D', 500), ('C', 100), ('L', 50), ('X', 10), ('V', 5), ('I', 1)];

/// ROMAN(number, [form]): 0 to 3999 as a Roman numeral. Form 0 is classic;
/// forms 1 to 4 allow ever longer subtractions (499 is CDXCIX, LDVLIV,
/// XDIX, VDIV and ID); TRUE is form 0 and FALSE form 4.
pub(super) fn roman(c: &Ctx, a: &[Expr]) -> Ev {
    let x = tri!(num(c, &a[0])).trunc();
    let form = match given(a, 1).map(|arg| c.scalar(arg)) {
        None => 0.0,
        Some(Value::Bool(b)) => if b { 0.0 } else { 4.0 },
        Some(v) => tri!(c.number(&v)).trunc(),
    };
    if !(0.0..=3999.0).contains(&x) || !(0.0..=4.0).contains(&form) {
        return Ev::err(ErrorKind::Value);
    }
    // Every single symbol and every subtraction this form allows, largest first.
    let mut steps: Vec<(u32, String)> = ROMAN.iter().map(|&(ch, v)| (v, ch.to_string())).collect();
    for (i, &(big, bv)) in ROMAN.iter().enumerate() {
        for (j, &(small, sv)) in ROMAN.iter().enumerate().skip(i + 1) {
            let gap = j - i;
            let allowed = if form == 0.0 { gap <= 2 && matches!(small, 'I' | 'X' | 'C') } else { gap <= 1 + form as usize };
            if allowed && bv - sv != sv {
                steps.push((bv - sv, format!("{small}{big}")));
            }
        }
    }
    // Largest value first; a single symbol before a pair of equal value.
    steps.sort_by(|p, q| q.0.cmp(&p.0).then(p.1.len().cmp(&q.1.len())));
    let mut rest = x as u32;
    let mut out = String::new();
    while rest > 0 {
        let Some((v, t)) = steps.iter().find(|st| st.0 <= rest) else { break };
        out.push_str(t);
        rest -= v;
    }
    s(&out)
}

/// ARABIC(text): the value of a Roman numeral, either case, with an optional
/// leading minus; the empty text is 0.
pub(super) fn arabic(c: &Ctx, a: &[Expr]) -> Ev {
    let t = tri!(text(c, &a[0]));
    let t = t.trim();
    if t.len() > 255 {
        return Ev::err(ErrorKind::Value);
    }
    let (sign, body) = match t.strip_prefix('-') {
        Some(rest) => (-1.0, rest),
        None => (1.0, t),
    };
    let mut values = Vec::new();
    for ch in body.chars() {
        let up = ch.to_ascii_uppercase();
        let Some(&(_, v)) = ROMAN.iter().find(|r| r.0 == up) else { return Ev::err(ErrorKind::Value) };
        values.push(f64::from(v));
    }
    let mut total = 0.0;
    for (i, v) in values.iter().enumerate() {
        if values.get(i + 1).is_some_and(|next| next > v) { total -= v } else { total += v }
    }
    n(sign * total)
}

const DIGITS: &[u8; 36] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
/// The largest integer BASE and DECIMAL take (2^53).
const MAX_EXACT: f64 = 9_007_199_254_740_992.0;

fn radix_arg(c: &Ctx, a: &Expr) -> Result<u32, ErrorKind> {
    let r = num(c, a)?.trunc();
    if (2.0..=36.0).contains(&r) { Ok(r as u32) } else { Err(ErrorKind::Num) }
}

fn to_radix(mut x: u64, radix: u32) -> String {
    let mut out = Vec::new();
    loop {
        out.push(DIGITS[(x % u64::from(radix)) as usize]);
        x /= u64::from(radix);
        if x == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// BASE(number, radix, [min_length]).
pub(super) fn base(c: &Ctx, a: &[Expr]) -> Ev {
    let x = tri!(num(c, &a[0])).trunc();
    let radix = tri!(radix_arg(c, &a[1]));
    let min = match given(a, 2) {
        Some(arg) => tri!(num(c, arg)).trunc(),
        None => 0.0,
    };
    if !(0.0..MAX_EXACT).contains(&x) || !(0.0..=255.0).contains(&min) {
        return Ev::err(ErrorKind::Num);
    }
    let digits = to_radix(x as u64, radix);
    s(&format!("{}{digits}", "0".repeat((min as usize).saturating_sub(digits.len()))))
}

/// DECIMAL(text, radix).
pub(super) fn decimal(c: &Ctx, a: &[Expr]) -> Ev {
    let t = tri!(text(c, &a[0]));
    let radix = tri!(radix_arg(c, &a[1]));
    if t.len() > 255 {
        return Ev::err(ErrorKind::Value);
    }
    let mut total = 0.0;
    for ch in t.chars() {
        let Some(d) = ch.to_digit(36).filter(|&d| d < radix) else { return Ev::err(ErrorKind::Num) };
        total = total * f64::from(radix) + f64::from(d);
        if total >= MAX_EXACT {
            return Ev::err(ErrorKind::Num);
        }
    }
    n(total)
}

/// A BIN, OCT or HEX number as Excel writes it: at most ten digits, the ten-
/// digit ones from the top half of the range being negative (two's
/// complement).
fn read_radix(c: &Ctx, a: &Expr, radix: u32) -> Result<i64, ErrorKind> {
    let t = match c.scalar(a) {
        Value::Num(x) if x >= 0.0 && x.fract() == 0.0 => format!("{x}"),
        Value::Num(_) => return Err(ErrorKind::Num),
        Value::Bool(_) => return Err(ErrorKind::Value),
        Value::Err(k) => return Err(k),
        v => to_text(&v)?.trim().to_string(),
    };
    if t.len() > 10 {
        return Err(ErrorKind::Num);
    }
    let mut x: i64 = 0;
    for ch in t.chars() {
        let d = ch.to_digit(radix).ok_or(ErrorKind::Num)?;
        x = x * i64::from(radix) + i64::from(d);
    }
    let span = i64::from(radix).pow(10);
    Ok(if x >= span / 2 { x - span } else { x })
}

/// `x` in `radix` as Excel writes it: negative numbers as ten digits of
/// two's complement, positive ones padded to `places` when given.
fn write_radix(c: &Ctx, x: i64, radix: u32, places: Option<&Expr>) -> Ev {
    let span = i64::from(radix).pow(10);
    if x < -span / 2 || x >= span / 2 {
        return Ev::err(ErrorKind::Num);
    }
    if x < 0 {
        return s(&to_radix((x + span) as u64, radix));
    }
    let digits = to_radix(x as u64, radix);
    match places {
        None => s(&digits),
        Some(arg) => {
            let p = tri!(num(c, arg)).trunc();
            if !(0.0..=10.0).contains(&p) || (p as usize) < digits.len() {
                return Ev::err(ErrorKind::Num);
            }
            s(&format!("{}{digits}", "0".repeat(p as usize - digits.len())))
        }
    }
}

fn convert(c: &Ctx, a: &[Expr], from: u32, to: u32) -> Ev {
    let x = if from == 10 {
        let x = tri!(num(c, &a[0])).trunc();
        if x.abs() > 1e12 {
            return Ev::err(ErrorKind::Num);
        }
        x as i64
    } else {
        tri!(read_radix(c, &a[0], from))
    };
    if to == 10 {
        return n(x as f64);
    }
    write_radix(c, x, to, given(a, 1))
}

pub(super) fn bin2dec(c: &Ctx, a: &[Expr]) -> Ev { convert(c, a, 2, 10) }
pub(super) fn bin2hex(c: &Ctx, a: &[Expr]) -> Ev { convert(c, a, 2, 16) }
pub(super) fn bin2oct(c: &Ctx, a: &[Expr]) -> Ev { convert(c, a, 2, 8) }
pub(super) fn dec2bin(c: &Ctx, a: &[Expr]) -> Ev { convert(c, a, 10, 2) }
pub(super) fn dec2hex(c: &Ctx, a: &[Expr]) -> Ev { convert(c, a, 10, 16) }
pub(super) fn dec2oct(c: &Ctx, a: &[Expr]) -> Ev { convert(c, a, 10, 8) }
pub(super) fn hex2bin(c: &Ctx, a: &[Expr]) -> Ev { convert(c, a, 16, 2) }
pub(super) fn hex2dec(c: &Ctx, a: &[Expr]) -> Ev { convert(c, a, 16, 10) }
pub(super) fn hex2oct(c: &Ctx, a: &[Expr]) -> Ev { convert(c, a, 16, 8) }
pub(super) fn oct2bin(c: &Ctx, a: &[Expr]) -> Ev { convert(c, a, 8, 2) }
pub(super) fn oct2dec(c: &Ctx, a: &[Expr]) -> Ev { convert(c, a, 8, 10) }
pub(super) fn oct2hex(c: &Ctx, a: &[Expr]) -> Ev { convert(c, a, 8, 16) }

/// TYPE(value): 1 number (or blank), 2 text, 4 logical, 16 error, 64 array
/// or range of more than one cell, 128 a lambda.
pub(super) fn type_fn(c: &Ctx, a: &[Expr]) -> Ev {
    let value = match c.eval(&a[0]) {
        Ev::R(r) if r.cells() > 1 => return n(64.0),
        Ev::A(arr) if arr.vals.len() > 1 => return n(64.0),
        Ev::F(_) => return n(128.0),
        Ev::A(arr) => arr.vals.first().cloned().unwrap_or(Value::Empty),
        Ev::R(r) => c.wb.value(r.sheet, r.r0, r.c0),
        Ev::V(v) => v,
    };
    n(match value {
        Value::Empty | Value::Num(_) => 1.0,
        Value::Str(_) => 2.0,
        Value::Bool(_) => 4.0,
        Value::Err(_) => 16.0,
    })
}

/// ERROR.TYPE(error): Excel's number for the error; #N/A for a value that
/// isn't one, and for Officina's own #CIRC! and #LIMIT!, which Excel lacks.
pub(super) fn error_type(c: &Ctx, a: &[Expr]) -> Ev {
    let Value::Err(k) = c.scalar(&a[0]) else { return Ev::err(ErrorKind::NA) };
    match k {
        ErrorKind::Null => n(1.0),
        ErrorKind::Div0 => n(2.0),
        ErrorKind::Value => n(3.0),
        ErrorKind::Ref => n(4.0),
        ErrorKind::Name => n(5.0),
        ErrorKind::Num => n(6.0),
        ErrorKind::NA => n(7.0),
        ErrorKind::Spill => n(9.0),
        ErrorKind::Calc => n(14.0),
        ErrorKind::Circ | ErrorKind::Limit => Ev::err(ErrorKind::NA),
    }
}

/// SHEET([value]): the 1-based number of the formula's sheet, a reference's
/// sheet, or the sheet a text names.
pub(super) fn sheet(c: &Ctx, a: &[Expr]) -> Ev {
    let Some(arg) = given(a, 0) else { return n(f64::from(c.sheet) + 1.0) };
    match c.eval(arg) {
        Ev::R(r) => n(f64::from(r.sheet) + 1.0),
        Ev::V(Value::Err(k)) => Ev::err(k),
        Ev::V(Value::Str(name)) => match c.wb.sheet_id(&name) {
            Some(id) => n(f64::from(id) + 1.0),
            None => Ev::err(ErrorKind::NA),
        },
        _ => Ev::err(ErrorKind::NA),
    }
}

/// SHEETS([reference]): the workbook's sheet count, or 1 for a reference.
pub(super) fn sheets(c: &Ctx, a: &[Expr]) -> Ev {
    let Some(arg) = given(a, 0) else { return n(c.wb.sheet_count() as f64) };
    match c.eval(arg) {
        Ev::R(_) => n(1.0),
        Ev::V(Value::Err(k)) => Ev::err(k),
        _ => Ev::err(ErrorKind::Value),
    }
}

/// ISFORMULA(reference): whether the cell holds a formula.
pub(super) fn isformula(c: &Ctx, a: &[Expr]) -> Ev {
    let r = tri!(rect_arg(c, &a[0]));
    Ev::V(Value::Bool(c.wb.formula_text(CellAddr::new(r.sheet, r.r0, r.c0)).is_some()))
}

/// ISNONTEXT(value).
pub(super) fn isnontext(c: &Ctx, a: &[Expr]) -> Ev {
    Ev::V(Value::Bool(!matches!(c.scalar(&a[0]), Value::Str(_))))
}
