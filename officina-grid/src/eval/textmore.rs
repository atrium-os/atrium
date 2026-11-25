//! Text and reference functions (docs/spec/officina.md §16.139): REGEXTEST,
//! REGEXEXTRACT and REGEXREPLACE, ARRAYTOTEXT and VALUETOTEXT, ENCODEURL,
//! ASC and DBCS (JIS), PERCENTOF, AREAS and TRIMRANGE.

use super::funcs::each_num;
use super::{to_text, Array, Ctx, Ev};
use crate::addr::Rect;
use crate::formula::Expr;
use crate::value::{ErrorKind, Value};
use regex::{Regex, RegexBuilder};
use std::cell::RefCell;
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

fn missing(a: &Expr) -> bool {
    matches!(a, Expr::Name(n) if n.is_empty())
}
fn text(c: &Ctx, a: &Expr) -> Result<Arc<str>, ErrorKind> {
    to_text(&c.scalar(a))
}
fn opt_num(c: &Ctx, args: &[Expr], i: usize, default: f64) -> Result<f64, ErrorKind> {
    match args.get(i) {
        Some(a) if !missing(a) => c.number(&c.scalar(a)),
        _ => Ok(default),
    }
}
fn s(t: String) -> Ev {
    Ev::V(Value::str(&t))
}

// ---------- regular expressions ----------

/// Compiled patterns kept per thread (a lifted REGEXTEST compiles once).
const CACHED_PATTERNS: usize = 64;
/// The most memory one compiled pattern may take.
const PATTERN_BYTES: usize = 1 << 21;
/// The longest text ARRAYTOTEXT builds (Excel's cell text limit).
const MAX_TEXT: usize = 32_767;

thread_local! {
    static PATTERNS: RefCell<HashMap<(String, bool), Regex>> = RefCell::new(HashMap::new());
}

/// The pattern compiled (case-insensitive when `fold`); a pattern the
/// engine can't take (look-around, back-references) or too large is
/// #VALUE!.
fn pattern(p: &str, fold: bool) -> Result<Regex, ErrorKind> {
    PATTERNS.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(r) = cache.get(&(p.to_string(), fold)) {
            return Ok(r.clone());
        }
        let r = RegexBuilder::new(p)
            .case_insensitive(fold)
            .size_limit(PATTERN_BYTES)
            .dfa_size_limit(PATTERN_BYTES)
            .build()
            .map_err(|_| ErrorKind::Value)?;
        if cache.len() >= CACHED_PATTERNS {
            cache.clear();
        }
        cache.insert((p.to_string(), fold), r.clone());
        Ok(r)
    })
}

/// case_sensitivity: 0 (the default) matches case, 1 ignores it.
fn fold(c: &Ctx, a: &[Expr], i: usize) -> Result<bool, ErrorKind> {
    match opt_num(c, a, i, 0.0)?.trunc() {
        0.0 => Ok(false),
        1.0 => Ok(true),
        _ => Err(ErrorKind::Value),
    }
}

pub(super) fn regextest(c: &Ctx, a: &[Expr]) -> Ev {
    let t = tri!(text(c, &a[0]));
    let r = tri!(pattern(&tri!(text(c, &a[1])), tri!(fold(c, a, 2))));
    Ev::V(Value::Bool(r.is_match(&t)))
}

/// REGEXEXTRACT(text, pattern, [return_mode], [case_sensitivity]): 0 the
/// first match, 1 every match (a column), 2 the first match's capture
/// groups (a row). #N/A when nothing matches.
pub(super) fn regexextract(c: &Ctx, a: &[Expr]) -> Ev {
    let t = tri!(text(c, &a[0]));
    let mode = tri!(opt_num(c, a, 2, 0.0)).trunc();
    let r = tri!(pattern(&tri!(text(c, &a[1])), tri!(fold(c, a, 3))));
    let array = |rows: usize, cols: usize, vals: Vec<Value>| Ev::A(Arc::new(Array { rows, cols, vals }));
    match mode {
        0.0 => match r.find(&t) {
            Some(m) => s(m.as_str().to_string()),
            None => Ev::err(ErrorKind::NA),
        },
        1.0 => {
            let vals: Vec<Value> = r.find_iter(&t).take(super::MAX_ARRAY as usize).map(|m| Value::str(m.as_str())).collect();
            if vals.is_empty() { Ev::err(ErrorKind::NA) } else { array(vals.len(), 1, vals) }
        }
        2.0 => match r.captures(&t) {
            None => Ev::err(ErrorKind::NA),
            Some(caps) if caps.len() == 1 => s(caps[0].to_string()),
            Some(caps) => {
                let vals: Vec<Value> = caps.iter().skip(1).map(|g| Value::str(g.map_or("", |g| g.as_str()))).collect();
                array(1, vals.len(), vals)
            }
        },
        _ => Ev::err(ErrorKind::Value),
    }
}

/// REGEXREPLACE(text, pattern, replacement, [occurrence], [case_sensitivity]):
/// every match (occurrence 0), the nth, or the nth from the end (negative).
/// `$1` and `${name}` in the replacement are the groups.
pub(super) fn regexreplace(c: &Ctx, a: &[Expr]) -> Ev {
    let t = tri!(text(c, &a[0]));
    let rep = tri!(text(c, &a[2]));
    let which = tri!(opt_num(c, a, 3, 0.0)).trunc();
    let r = tri!(pattern(&tri!(text(c, &a[1])), tri!(fold(c, a, 4))));
    if which == 0.0 {
        return s(r.replace_all(&t, &*rep).into_owned());
    }
    let all: Vec<regex::Captures> = r.captures_iter(&t).collect();
    let i = if which > 0.0 { which as usize - 1 } else { all.len().wrapping_sub((-which) as usize) };
    let Some(caps) = all.get(i) else { return s(t.to_string()) };
    let whole = caps.get(0).map_or(0..0, |m| m.range());
    let mut out = String::with_capacity(t.len());
    out.push_str(&t[..whole.start]);
    caps.expand(&rep, &mut out);
    out.push_str(&t[whole.end..]);
    s(out)
}

// ---------- values as text ----------

/// One value as ARRAYTOTEXT and VALUETOTEXT write it; `strict` quotes text.
fn shown(v: &Value, strict: bool) -> String {
    match v {
        Value::Str(t) if strict => format!("\"{}\"", t.replace('"', "\"\"")),
        Value::Str(t) => t.to_string(),
        Value::Err(k) => k.to_string(),
        Value::Empty => String::new(),
        other => to_text(other).map(|t| t.to_string()).unwrap_or_default(),
    }
}

fn format_arg(c: &Ctx, a: &[Expr], i: usize) -> Result<bool, ErrorKind> {
    match opt_num(c, a, i, 0.0)?.trunc() {
        0.0 => Ok(false),
        1.0 => Ok(true),
        _ => Err(ErrorKind::Value),
    }
}

pub(super) fn valuetotext(c: &Ctx, a: &[Expr]) -> Ev {
    let strict = tri!(format_arg(c, a, 1));
    s(shown(&c.scalar(&a[0]), strict))
}

/// ARRAYTOTEXT(array, [format]): "1, 2, 3" (concise) or "{1,2;3,4}" (strict,
/// text quoted).
pub(super) fn arraytotext(c: &Ctx, a: &[Expr]) -> Ev {
    let strict = tri!(format_arg(c, a, 1));
    let arr = match c.eval(&a[0]) {
        Ev::V(v) => Arc::new(Array { rows: 1, cols: 1, vals: vec![v] }),
        other => tri!(c.to_array(other)),
    };
    let mut out = String::new();
    if strict {
        out.push('{');
    }
    for (i, v) in arr.vals.iter().enumerate() {
        if i > 0 {
            out.push_str(match (strict, i % arr.cols == 0) {
                (false, _) => ", ",
                (true, true) => ";",
                (true, false) => ",",
            });
        }
        out.push_str(&shown(v, strict));
        if out.len() > MAX_TEXT {
            return Ev::err(ErrorKind::Value);
        }
    }
    if strict {
        out.push('}');
    }
    s(out)
}

/// ENCODEURL(text): percent-encoding of every byte of the UTF-8 text but
/// letters, digits and `-_.~`.
pub(super) fn encodeurl(c: &Ctx, a: &[Expr]) -> Ev {
    let t = tri!(text(c, &a[0]));
    let mut out = String::with_capacity(t.len());
    for b in t.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    s(out)
}

/// ASC: full-width ASCII letters, digits, signs and the ideographic space
/// to their half-width forms.
pub(super) fn asc(c: &Ctx, a: &[Expr]) -> Ev {
    let t = tri!(text(c, &a[0]));
    s(t.chars()
        .map(|ch| match ch {
            '\u{3000}' => ' ',
            '\u{FF01}'..='\u{FF5E}' => char::from_u32(ch as u32 - 0xFEE0).unwrap_or(ch),
            _ => ch,
        })
        .collect())
}

/// DBCS (and JIS): the reverse of ASC.
pub(super) fn dbcs(c: &Ctx, a: &[Expr]) -> Ev {
    let t = tri!(text(c, &a[0]));
    s(t.chars()
        .map(|ch| match ch {
            ' ' => '\u{3000}',
            '!'..='~' => char::from_u32(ch as u32 + 0xFEE0).unwrap_or(ch),
            _ => ch,
        })
        .collect())
}

// ---------- sums and references ----------

/// PERCENTOF(data_subset, data_all): the subset's sum over the whole's.
pub(super) fn percentof(c: &Ctx, a: &[Expr]) -> Ev {
    let (mut part, mut whole) = (0.0, 0.0);
    tri!(each_num(c, std::slice::from_ref(&a[0]), &mut |x| part += x));
    tri!(each_num(c, std::slice::from_ref(&a[1]), &mut |x| whole += x));
    if whole == 0.0 {
        return Ev::err(ErrorKind::Div0);
    }
    let r = part / whole;
    if r.is_finite() { Ev::V(Value::num(r)) } else { Ev::err(ErrorKind::Num) }
}

/// AREAS(reference): a reference here is always one area.
pub(super) fn areas(c: &Ctx, a: &[Expr]) -> Ev {
    match c.eval(&a[0]) {
        Ev::R(_) => Ev::V(Value::num(1.0)),
        Ev::V(Value::Err(k)) => Ev::err(k),
        _ => Ev::err(ErrorKind::Value),
    }
}

/// TRIMRANGE(range, [trim_rows], [trim_cols]): the range without its blank
/// outer rows and columns (0 none, 1 leading, 2 trailing, 3 both, the
/// default); #REF! when every cell is blank.
pub(super) fn trimrange(c: &Ctx, a: &[Expr]) -> Ev {
    let r = match c.eval(&a[0]) {
        Ev::R(r) => r,
        Ev::V(Value::Err(k)) => return Ev::err(k),
        _ => return Ev::err(ErrorKind::Value),
    };
    let mode = |i: usize| -> Result<u8, ErrorKind> {
        let m = opt_num(c, a, i, 3.0)?.trunc();
        if (0.0..=3.0).contains(&m) { Ok(m as u8) } else { Err(ErrorKind::Value) }
    };
    let (rows, cols) = (tri!(mode(1)), tri!(mode(2)));
    let mut seen: Option<(u32, u32, u32, u32)> = None;
    c.wb.each_cell(&r, &mut |row, col, cell| {
        if cell.is_empty() {
            return;
        }
        seen = Some(match seen {
            None => (row, row, col, col),
            Some((r0, r1, c0, c1)) => (r0.min(row), r1.max(row), c0.min(col), c1.max(col)),
        });
    });
    let Some((r0, r1, c0, c1)) = seen else { return Ev::err(ErrorKind::Ref) };
    let mut out: Rect = r;
    if rows & 1 != 0 {
        out.r0 = r0;
    }
    if rows & 2 != 0 {
        out.r1 = r1;
    }
    if cols & 1 != 0 {
        out.c0 = c0;
    }
    if cols & 2 != 0 {
        out.c1 = c1;
    }
    Ev::R(out)
}
