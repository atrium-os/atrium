//! References made at recalculation time (docs/spec/officina.md §16.135):
//! INDIRECT reads a reference from text and OFFSET moves and resizes one.
//! Both are volatile, and recalculation orders their groups after the dirty
//! groups they could read (`Workbook::order_dynamic`).

use super::{to_bool, to_text, Ctx, Ev};
use crate::addr::{Rect, MAX_COLS, MAX_ROWS};
use crate::formula::{parse, Expr};
use crate::value::{ErrorKind, Value};

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

/// INDIRECT(ref_text, [a1]): the reference the text names, in A1 style or,
/// when `a1` is FALSE, R1C1 style relative to the formula's cell. Text that
/// isn't a reference is #REF!.
pub(super) fn indirect(c: &Ctx, a: &[Expr]) -> Ev {
    let text = tri!(to_text(&c.scalar(&a[0])));
    let a1 = match a.get(1).filter(|x| !missing(x)) {
        Some(x) => tri!(to_bool(&c.scalar(x))),
        None => true,
    };
    let text = if a1 { text.trim().to_string() } else { tri!(r1c1_to_a1(text.trim(), c.row, c.col).ok_or(ErrorKind::Ref)) };
    let Ok(expr) = parse(&text, c.row, c.col, &|name: &str| c.wb.sheet_id(name)) else { return Ev::err(ErrorKind::Ref) };
    if !matches!(expr, Expr::Ref(_) | Expr::Range(_) | Expr::Name(_) | Expr::Table(_)) {
        return Ev::err(ErrorKind::Ref);
    }
    match c.eval(&expr) {
        Ev::R(r) => Ev::R(r),
        Ev::V(Value::Err(k)) if k != ErrorKind::Name => Ev::err(k),
        _ => Ev::err(ErrorKind::Ref),
    }
}

/// An R1C1 reference or range (`R2C3`, `R[-1]C`, `Sheet!R1C1:R2C2`) as A1
/// text, or None when it isn't one or leaves the sheet.
fn r1c1_to_a1(text: &str, row: u32, col: u32) -> Option<String> {
    let (sheet, body) = match text.rfind('!') {
        Some(i) => (&text[..=i], &text[i + 1..]),
        None => ("", text),
    };
    let mut out = String::from(sheet);
    for (i, part) in body.split(':').enumerate() {
        if i > 1 {
            return None;
        }
        if i == 1 {
            out.push(':');
        }
        let upper = part.to_ascii_uppercase();
        let rest = upper.strip_prefix('R')?;
        let (r, rest) = coordinate(rest, row, MAX_ROWS)?;
        let rest = rest.strip_prefix('C')?;
        let (k, rest) = coordinate(rest, col, MAX_COLS)?;
        if !rest.is_empty() {
            return None;
        }
        out.push('$');
        out.push_str(&crate::addr::col_name(k));
        out.push('$');
        out.push_str(&(r + 1).to_string());
    }
    Some(out)
}

/// One R1C1 coordinate after its letter: `n` (1-based), `[n]` (relative to
/// `at`) or nothing (`at` itself), as a 0-based index below `limit`.
fn coordinate(s: &str, at: u32, limit: u32) -> Option<(u32, &str)> {
    let digits = |s: &str| s.find(|ch: char| !ch.is_ascii_digit() && ch != '-').unwrap_or(s.len());
    let (value, rest) = if let Some(inner) = s.strip_prefix('[') {
        let end = inner.find(']')?;
        let offset: i64 = inner[..end].parse().ok()?;
        (i64::from(at) + offset, &inner[end + 1..])
    } else {
        let n = digits(s);
        if n == 0 {
            (i64::from(at), s)
        } else {
            let one_based: i64 = s[..n].parse().ok()?;
            (one_based - 1, &s[n..])
        }
    };
    (0..i64::from(limit)).contains(&value).then(|| (value as u32, rest))
}

/// OFFSET(reference, rows, cols, [height], [width]): the reference moved by
/// `rows` and `cols` and given `height` × `width` cells (the reference's own
/// size by default). Leaving the sheet, or a size under one cell, is #REF!.
pub(super) fn offset(c: &Ctx, a: &[Expr]) -> Ev {
    let base = match c.eval(&a[0]) {
        Ev::R(r) => r,
        Ev::V(Value::Err(k)) => return Ev::err(k),
        _ => return Ev::err(ErrorKind::Value),
    };
    let arg = |i: usize, default: f64| -> Result<f64, ErrorKind> {
        match a.get(i) {
            Some(x) if !missing(x) => Ok(c.number(&c.scalar(x))?.trunc()),
            _ => Ok(default),
        }
    };
    let rows = tri!(arg(1, 0.0));
    let cols = tri!(arg(2, 0.0));
    let height = tri!(arg(3, base.rows() as f64));
    let width = tri!(arg(4, base.cols() as f64));
    if height < 1.0 || width < 1.0 {
        return Ev::err(ErrorKind::Ref);
    }
    let r0 = f64::from(base.r0) + rows;
    let c0 = f64::from(base.c0) + cols;
    let (r1, c1) = (r0 + height - 1.0, c0 + width - 1.0);
    if r0 < 0.0 || c0 < 0.0 || r1 >= f64::from(MAX_ROWS) || c1 >= f64::from(MAX_COLS) {
        return Ev::err(ErrorKind::Ref);
    }
    Ev::R(Rect { sheet: base.sheet, r0: r0 as u32, c0: c0 as u32, r1: r1 as u32, c1: c1 as u32 })
}
