//! Literal delimiter matching for TEXTAFTER and TEXTBEFORE. A delimiter array is a set of
//! alternatives, while the other arguments can lift to an output array.
//! TEXTSPLIT splits on the same kind of delimiter set.

use super::{funcs::text_key, to_bool, to_text, Array, Ctx, Ev, MAX_ARRAY};
use crate::formula::Expr;
use crate::strings::MAX_TEXT;
use crate::value::{ErrorKind, Value};
use std::sync::Arc;

const MAX_DELIMITERS: u64 = 256;
const MAX_SEARCH_BYTES: usize = 8 * MAX_TEXT;

pub(super) fn delimiters(ctx: &Ctx, value: Ev) -> Result<Vec<Arc<str>>, ErrorKind> {
    match value {
        Ev::V(value) => Ok(vec![to_text(&value)?]),
        Ev::R(rect) if rect.cells() > MAX_DELIMITERS => Err(ErrorKind::Limit),
        other => {
            let array = ctx.to_array(other)?;
            if array.vals.len() as u64 > MAX_DELIMITERS {
                return Err(ErrorKind::Limit);
            }
            array.vals.iter().map(to_text).collect()
        }
    }
}

pub(super) fn after<'a>(
    text: &'a str,
    delimiters: &[Arc<str>],
    instance: f64,
    mode: f64,
    match_end: f64,
    explicit_instance: bool,
) -> Result<Option<&'a str>, ErrorKind> {
    around(text, delimiters, instance, mode, match_end, explicit_instance, false)
}

/// TEXTBEFORE: the text before the chosen delimiter, found as TEXTAFTER
/// finds it.
pub(super) fn before<'a>(
    text: &'a str,
    delimiters: &[Arc<str>],
    instance: f64,
    mode: f64,
    match_end: f64,
    explicit_instance: bool,
) -> Result<Option<&'a str>, ErrorKind> {
    around(text, delimiters, instance, mode, match_end, explicit_instance, true)
}

fn around<'a>(
    text: &'a str,
    delimiters: &[Arc<str>],
    instance: f64,
    mode: f64,
    match_end: f64,
    explicit_instance: bool,
    before: bool,
) -> Result<Option<&'a str>, ErrorKind> {
    if instance == 0.0 || !matches!(mode, 0.0 | 1.0) || !matches!(match_end, 0.0 | 1.0) {
        return Err(ErrorKind::Value);
    }
    let reverse = instance < 0.0;
    if delimiters.iter().any(|delimiter| delimiter.is_empty()) {
        // An empty delimiter matches at the start (or, from the end, the end).
        return Ok(Some(match (reverse, before) {
            (false, false) | (true, true) => text,
            _ => "",
        }));
    }
    let text_len = text.chars().count();
    let count = instance.abs() as usize;
    if explicit_instance
        && (count > text_len
            || delimiters
                .iter()
                .all(|delimiter| delimiter.chars().count() > text_len))
    {
        return Err(ErrorKind::Value);
    }

    let mut budget = MAX_SEARCH_BYTES;
    let index = if mode == 0.0 {
        find_after(
            text,
            delimiters.iter().map(AsRef::as_ref),
            count,
            reverse,
            match_end == 1.0,
            &mut budget,
        )?
    } else {
        let key = text_key(text);
        let keys: Vec<_> = delimiters
            .iter()
            .map(|delimiter| text_key(delimiter))
            .collect();
        let index = find_after(
            &key,
            keys.iter().map(String::as_str),
            count,
            reverse,
            match_end == 1.0,
            &mut budget,
        )?;
        // Lowercasing can change the number of UTF-8 bytes. Return a slice
        // of the original string, with the user's spelling and case intact.
        index.map(|(start, end)| (original_end(text, start), original_end(text, end)))
    };
    Ok(index.map(|(start, end)| if before { &text[..start] } else { &text[end..] }))
}

fn original_end(text: &str, end: usize) -> usize {
    let mut folded_bytes = 0;
    for (offset, ch) in text.char_indices() {
        if folded_bytes >= end {
            return offset;
        }
        folded_bytes += ch.to_lowercase().map(char::len_utf8).sum::<usize>();
    }
    text.len()
}

fn find_after<'a>(
    mut text: &str,
    delimiters: impl Iterator<Item = &'a str> + Clone,
    count: usize,
    reverse: bool,
    match_end: bool,
    budget: &mut usize,
) -> Result<Option<(usize, usize)>, ErrorKind> {
    // The chosen delimiter's start and end, in `text` as given.
    let mut offset = 0;
    let mut start_at = 0;
    for hit_num in 1..=count {
        let mut best = None;
        for delimiter in delimiters.clone() {
            let hit = if reverse {
                text.rfind(delimiter)
            } else {
                text.find(delimiter)
            };
            let scanned = hit.map_or(text.len(), |pos| {
                if reverse {
                    text.len() - pos
                } else {
                    pos + delimiter.len()
                }
            });
            *budget = budget.checked_sub(scanned.max(1)).ok_or(ErrorKind::Limit)?;
            if let Some(start) = hit {
                let end = start + delimiter.len();
                if best.is_none_or(|(a, b)| if reverse { end > b } else { start < a }) {
                    best = Some((start, end));
                }
            }
        }
        let Some((start, end)) = best else {
            // Matching the end: an empty delimiter past the text's end (or before its start).
            return Ok((match_end && hit_num == count).then(|| {
                if reverse {
                    (0, 0)
                } else {
                    (offset + text.len(), offset + text.len())
                }
            }));
        };
        if reverse {
            text = &text[..start];
            start_at = start;
            offset = end;
        } else {
            start_at = offset + start;
            text = &text[end..];
            offset += end;
        }
    }
    Ok(Some((start_at, offset)))
}

fn missing(a: &Expr) -> bool {
    matches!(a, Expr::Name(n) if n.is_empty())
}

/// TEXTSPLIT(text, col_delimiter, [row_delimiter], [ignore_empty],
/// [match_mode], [pad_with]): the text cut into rows, then each row into
/// columns, with short rows padded (by #N/A unless `pad_with` says).
pub(super) fn textsplit(c: &Ctx, a: &[Expr]) -> Ev {
    match split(c, a) {
        Ok(array) => Ev::A(Arc::new(array)),
        Err(kind) => Ev::err(kind),
    }
}

fn split(c: &Ctx, a: &[Expr]) -> Result<Array, ErrorKind> {
    let text = to_text(&c.scalar(&a[0]))?;
    let given = |i: usize| a.get(i).filter(|arg| !missing(arg));
    let col_delims = match given(1) {
        Some(arg) => delimiters(c, c.eval(arg))?,
        None => Vec::new(),
    };
    let row_delims = match given(2) {
        Some(arg) => delimiters(c, c.eval(arg))?,
        None => Vec::new(),
    };
    if col_delims.is_empty() && row_delims.is_empty() {
        return Err(ErrorKind::Value);
    }
    if col_delims.iter().chain(&row_delims).any(|d| d.is_empty()) {
        return Err(ErrorKind::Value);
    }
    let ignore_empty = match given(3) {
        Some(arg) => to_bool(&c.scalar(arg))?,
        None => false,
    };
    let fold = match given(4) {
        Some(arg) => match c.number(&c.scalar(arg))? {
            0.0 => false,
            1.0 => true,
            _ => return Err(ErrorKind::Value),
        },
        None => false,
    };
    let pad = match given(5) {
        Some(arg) => c.scalar(arg),
        None => Value::Err(ErrorKind::NA),
    };

    let key = |ch: char| if fold { ch.to_lowercase().next().unwrap_or(ch) } else { ch };
    let chars: Vec<char> = text.chars().collect();
    let keys: Vec<char> = chars.iter().map(|&ch| key(ch)).collect();
    let fold_all = |list: &[Arc<str>]| -> Vec<Vec<char>> { list.iter().map(|d| d.chars().map(key).collect()).collect() };
    let (col_keys, row_keys) = (fold_all(&col_delims), fold_all(&row_delims));
    let mut budget = MAX_SEARCH_BYTES;

    let mut rows: Vec<Vec<String>> = Vec::new();
    for (start, end) in cut(&keys, 0, keys.len(), &row_keys, &mut budget)? {
        if ignore_empty && start == end {
            continue;
        }
        let mut row = Vec::new();
        for (from, to) in cut(&keys, start, end, &col_keys, &mut budget)? {
            if ignore_empty && from == to {
                continue;
            }
            row.push(chars[from..to].iter().collect::<String>());
        }
        if ignore_empty && row.is_empty() {
            continue;
        }
        rows.push(row);
    }
    if rows.is_empty() {
        return Err(ErrorKind::Calc);
    }
    let width = rows.iter().map(Vec::len).max().unwrap_or(0).max(1);
    if (rows.len() as u64).saturating_mul(width as u64) > MAX_ARRAY {
        return Err(ErrorKind::Limit);
    }
    let mut vals = Vec::with_capacity(rows.len() * width);
    for row in &rows {
        let filled = row.len();
        vals.extend(row.iter().map(|piece| Value::str(piece)));
        vals.extend(std::iter::repeat_n(pad.clone(), width - filled));
    }
    Ok(Array { rows: rows.len(), cols: width, vals })
}

/// The pieces of `keys[start..end]` between delimiters, as index ranges. At
/// each place the longest delimiter that matches wins.
fn cut(keys: &[char], start: usize, end: usize, delims: &[Vec<char>], budget: &mut usize) -> Result<Vec<(usize, usize)>, ErrorKind> {
    let mut pieces = Vec::new();
    if delims.is_empty() {
        pieces.push((start, end));
        return Ok(pieces);
    }
    let (mut from, mut at) = (start, start);
    while at < end {
        let mut best = 0;
        for delim in delims {
            *budget = budget.checked_sub(delim.len().max(1)).ok_or(ErrorKind::Limit)?;
            if delim.len() > best && at + delim.len() <= end && keys[at..at + delim.len()] == delim[..] {
                best = delim.len();
            }
        }
        if best > 0 {
            pieces.push((from, at));
            at += best;
            from = at;
        } else {
            at += 1;
        }
    }
    pieces.push((from, end));
    Ok(pieces)
}
