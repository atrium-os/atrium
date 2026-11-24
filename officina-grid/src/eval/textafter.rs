//! Literal delimiter matching for TEXTAFTER. A delimiter array is a set of
//! alternatives, while the other arguments can lift to an output array.

use super::{funcs::text_key, to_text, Ctx, Ev};
use crate::strings::MAX_TEXT;
use crate::value::ErrorKind;
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
    if instance == 0.0 || !matches!(mode, 0.0 | 1.0) || !matches!(match_end, 0.0 | 1.0) {
        return Err(ErrorKind::Value);
    }
    let reverse = instance < 0.0;
    if delimiters.iter().any(|delimiter| delimiter.is_empty()) {
        return Ok(Some(if reverse { "" } else { text }));
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
        index.map(|end| original_end(text, end))
    };
    Ok(index.map(|end| &text[end..]))
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
) -> Result<Option<usize>, ErrorKind> {
    let mut offset = 0;
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
            return Ok((match_end && hit_num == count).then(|| {
                if reverse {
                    0
                } else {
                    offset + text.len()
                }
            }));
        };
        if reverse {
            text = &text[..start];
            offset = end;
        } else {
            text = &text[end..];
            offset += end;
        }
    }
    Ok(Some(offset))
}
