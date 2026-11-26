//! Spelling (F7): the sheet's typed text checked word by word against the
//! built-in English (US) dictionary, as Excel's Spelling dialog does, one
//! word at a time in a bar.
//!
//! The check starts at the cursor, runs to the end of the sheet, wraps to
//! the top and stops where it began. Formulas, numbers and words with digits,
//! words in capitals, and web and mail addresses are passed over, as
//! Excel's defaults are. Ignore All lasts while Sheets runs.

use std::cell::RefCell;

use officina_grid::addr::{CellAddr, Rect, MAX_COLS, MAX_ROWS};
use officina_grid::book::Input;
use officina_grid::ops::GridOp;
use officina_grid::Value;
use officina_spell::{Dictionary, Speller};

use crate::doc::Doc;

/// The most text cells one check reads.
pub const MAX_CELLS: usize = 200_000;
/// The longest replacement typed.
const MAX_WORD: usize = 255;

thread_local! {
    static SPELLER: RefCell<Option<Speller>> = const { RefCell::new(None) };
}

fn with_speller<T>(f: impl FnOnce(&mut Speller) -> T) -> T {
    SPELLER.with(|s| f(s.borrow_mut().get_or_insert_with(|| Speller::new(Dictionary::en_us()))))
}

/// Where a check is: where it began, the word it stopped at, and whether
/// it has wrapped past the sheet's end.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    origin: (u32, u32),
    wrapped: bool,
    found: Found,
}

/// A misspelled word: its cell, its bytes there, and what to offer.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub at: (u32, u32),
    pub start: usize,
    pub end: usize,
    pub word: String,
    pub suggestions: Vec<String>,
}

/// What the bar shows next.
#[derive(Debug, Clone, PartialEq)]
pub enum Next {
    /// A word to look at: the cursor goes to its cell, the bar starts with
    /// the first suggestion, and the status says the word and the rest.
    Word { cursor: (u32, u32), text: String, say: String },
    /// The check is over.
    Done(String),
}

/// The sheet's typed text, row by row: (row, column, text). Formulas
/// and values that protection hides are left out.
fn texts(d: &Doc) -> Result<Vec<(u32, u32, String)>, String> {
    let s = d.sheet;
    let mut out = Vec::new();
    let mut over = false;
    d.wb.each(&Rect { sheet: s, r0: 0, c0: 0, r1: MAX_ROWS - 1, c1: MAX_COLS - 1 }, &mut |r, c, v| {
        if over {
            return;
        }
        if let Value::Str(t) = &v {
            let a = CellAddr::new(s, r, c);
            if matches!(d.wb.input(a), Input::Value(_)) && !d.wb.hides_value(a) {
                if out.len() == MAX_CELLS {
                    over = true;
                    return;
                }
                out.push((r, c, t.to_string()));
            }
        }
    });
    if over {
        return Err(format!("Spelling checks at most {MAX_CELLS} cells of text on a sheet."));
    }
    out.sort_by_key(|x| (x.0, x.1));
    Ok(out)
}

/// Whether the word at `s..e` of `text` is one a check passes over: digits,
/// all capitals, or part of a web or mail address.
fn passed_over(text: &str, s: usize, e: usize) -> bool {
    let w = &text[s..e];
    if w.chars().any(|c| c.is_ascii_digit()) || (w.chars().filter(|c| c.is_alphabetic()).count() > 1 && !w.chars().any(|c| c.is_lowercase())) {
        return true;
    }
    // The run of non-space characters the word is in.
    let a = text[..s].rfind(char::is_whitespace).map_or(0, |i| i + 1);
    let b = text[e..].find(char::is_whitespace).map_or(text.len(), |i| e + i);
    let chunk = &text[a..b];
    chunk.contains('@') || chunk.contains("://") || chunk.to_ascii_lowercase().starts_with("www.")
}

/// The first misspelled word at or after `from` (row, column, byte) in
/// sheet order, before `upto` when given.
fn first_after(cells: &[(u32, u32, String)], from: (u32, u32, usize), upto: Option<(u32, u32)>) -> Option<Found> {
    with_speller(|sp| {
        for (r, c, text) in cells.iter().filter(|x| (x.0, x.1) >= (from.0, from.1)) {
            if upto.is_some_and(|u| (*r, *c) >= u) {
                return None;
            }
            let skip = if (*r, *c) == (from.0, from.1) { from.2 } else { 0 };
            for (s, e) in officina_spell::words(text) {
                let (s, e) = (s as usize, e as usize);
                if s < skip || e > text.len() || !text.is_char_boundary(s) || !text.is_char_boundary(e) || passed_over(text, s, e) {
                    continue;
                }
                let word = &text[s..e];
                if !sp.check(word) {
                    return Some(Found { at: (*r, *c), start: s, end: e, word: word.to_string(), suggestions: sp.suggest(word).into_iter().take(8).collect() });
                }
            }
        }
        None
    })
}

/// The next word from `from`, wrapping once past the end to the run's
/// origin.
fn advance(d: &mut Doc, origin: (u32, u32), mut wrapped: bool, from: (u32, u32, usize)) -> Result<Next, String> {
    let cells = texts(d)?;
    let mut found = if wrapped { first_after(&cells, from, Some(origin)) } else { first_after(&cells, from, None) };
    if found.is_none() && !wrapped && origin != (0, 0) {
        wrapped = true;
        found = first_after(&cells, (0, 0, 0), Some(origin));
    }
    let Some(f) = found else {
        d.spelling = None;
        return Ok(Next::Done("The spelling check is complete.".into()));
    };
    let text = f.suggestions.first().cloned().unwrap_or_else(|| f.word.clone());
    let say = if f.suggestions.is_empty() {
        format!("Not in the dictionary: \"{}\" in {}. No suggestions.", f.word, officina_grid::addr::a1(f.at.0, f.at.1))
    } else {
        format!("Not in the dictionary: \"{}\" in {}. Suggestions: {}.", f.word, officina_grid::addr::a1(f.at.0, f.at.1), f.suggestions.join(", "))
    };
    let cursor = f.at;
    d.spelling = Some(Run { origin, wrapped, found: f });
    Ok(Next::Word { cursor, text, say })
}

/// Starts a check at the cursor.
pub fn begin(d: &mut Doc, cursor: (u32, u32)) -> Result<Next, String> {
    advance(d, cursor, false, (cursor.0, cursor.1, 0))
}

/// What was typed in the bar for the word the check stopped at: a word to
/// change it to, `ignore` (or nothing) to leave it, `ignore all` to leave
/// it everywhere while Sheets runs, or `change all WORD` to change it in
/// every cell of the sheet. Then on to the next word.
pub fn answer(d: &mut Doc, typed: &str) -> Result<Next, String> {
    let Some(run) = d.spelling.clone() else { return Err("Start the spelling check again.".into()) };
    let f = &run.found;
    let a = CellAddr::new(d.sheet, f.at.0, f.at.1);
    // The cell may have changed since the word was found.
    let still = matches!(d.wb.input(a), Input::Value(Value::Str(ref t)) if t.get(f.start..f.end) == Some(f.word.as_str()));
    if !still {
        return advance(d, run.origin, run.wrapped, (f.at.0, f.at.1, f.start));
    }
    let t = typed.trim();
    let lower = t.to_ascii_lowercase();
    let clean = |w: &str| -> Result<String, String> {
        if w.is_empty() || w.len() > MAX_WORD || w.chars().any(char::is_control) {
            return Err(format!("Type the word to change it to (at most {MAX_WORD} bytes), or ignore."));
        }
        Ok(w.to_string())
    };
    let next_from = if t.is_empty() || lower == "ignore" || lower == "ignore once" {
        (f.at.0, f.at.1, f.end)
    } else if lower == "ignore all" {
        with_speller(|sp| sp.ignored.insert(f.word.clone()));
        (f.at.0, f.at.1, f.end)
    } else if let Some(w) = lower.strip_prefix("change all ").map(|_| &t["change all ".len()..]) {
        let w = clean(w.trim())?;
        let cells = texts(d)?;
        let mut ops = Vec::new();
        for (r, c, text) in &cells {
            let mut out = String::with_capacity(text.len());
            let mut last = 0;
            for (s, e) in officina_spell::words(text) {
                let (s, e) = (s as usize, e as usize);
                if text.get(s..e) == Some(f.word.as_str()) {
                    out.push_str(&text[last..s]);
                    out.push_str(&w);
                    last = e;
                }
            }
            if last > 0 {
                out.push_str(&text[last..]);
                ops.push(GridOp::set(CellAddr::new(d.sheet, *r, *c), Input::Value(Value::str(&out))));
            }
        }
        if !d.run_action("Spelling", ops) {
            return Err(std::mem::take(&mut d.status));
        }
        (f.at.0, f.at.1, f.start + w.len())
    } else {
        let w = clean(t)?;
        let Input::Value(Value::Str(old)) = d.wb.input(a) else { return Err("That cell changed.".into()) };
        let new = format!("{}{}{}", &old[..f.start], w, &old[f.end..]);
        if !d.run_action("Spelling", vec![GridOp::set(a, Input::Value(Value::str(&new)))]) {
            return Err(std::mem::take(&mut d.status));
        }
        (f.at.0, f.at.1, f.start + w.len())
    };
    advance(d, run.origin, run.wrapped, next_from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(csv: &[u8]) -> Doc {
        Doc::from_csv("s.csv", csv).unwrap().0
    }

    fn word(n: &Next) -> (u32, u32) {
        match n {
            Next::Word { cursor, .. } => *cursor,
            Next::Done(m) => panic!("{m}"),
        }
    }

    #[test]
    fn spelling_walks_the_sheet_from_the_cursor_and_wraps() {
        let mut d = doc(b"teh cat,fine\nNASA 3rd wrold,x@exmple.com\nrecieve,=1+1\n");
        // From B2: C... then A3, wrapping to A1, stopping before B2 again.
        let n = begin(&mut d, (1, 1)).unwrap();
        assert_eq!(word(&n), (2, 0), "B2's address is passed over");
        if let Next::Word { text, say, .. } = &n {
            assert_eq!(text, "receive");
            assert!(say.contains("\"recieve\" in A3") && say.contains("receive"), "{say}");
        }
        let n = answer(&mut d, "receive").unwrap();
        assert_eq!(d.wb.value(0, 2, 0), Value::str("receive"));
        assert_eq!(word(&n), (0, 0), "wrapped to A1");
        let n = answer(&mut d, "ignore").unwrap();
        // NASA and 3rd are passed over; wrold is the next.
        assert_eq!(word(&n), (1, 0));
        assert_eq!(answer(&mut d, "world").unwrap(), Next::Done("The spelling check is complete.".into()));
        assert_eq!(d.wb.value(0, 1, 0), Value::str("NASA 3rd world"));
        assert_eq!(d.wb.value(0, 0, 0), Value::str("teh cat"), "ignored once");
        // One undo per change.
        d.undo();
        assert_eq!(d.wb.value(0, 1, 0), Value::str("NASA 3rd wrold"));
    }

    #[test]
    fn ignore_all_and_change_all() {
        let mut d = doc(b"colr one,colr two\nabcxyzq,colr\n");
        let n = begin(&mut d, (0, 0)).unwrap();
        assert_eq!(word(&n), (0, 0));
        let n = answer(&mut d, "change all color").unwrap();
        assert_eq!((d.wb.value(0, 0, 0), d.wb.value(0, 0, 1), d.wb.value(0, 1, 1)), (Value::str("color one"), Value::str("color two"), Value::str("color")));
        assert_eq!(word(&n), (1, 0));
        assert_eq!(answer(&mut d, "ignore all").unwrap(), Next::Done("The spelling check is complete.".into()));
        // Ignored for the rest of the session.
        assert_eq!(begin(&mut d, (0, 0)).unwrap(), Next::Done("The spelling check is complete.".into()));
        d.undo();
        assert_eq!(d.wb.value(0, 1, 1), Value::str("colr"), "change all is one step");
        // A replacement is checked.
        let n = begin(&mut d, (0, 0)).unwrap();
        assert_eq!(word(&n), (0, 0));
        assert!(answer(&mut d, "bad\u{7}").is_err());
        assert!(answer(&mut d, &"w".repeat(256)).is_err());
        // A cell changed under the check: the check looks again.
        d.type_into(0, 0, "fine words");
        assert_eq!(word(&answer(&mut d, "color").unwrap()), (0, 1));
        assert_eq!(d.wb.value(0, 0, 0), Value::str("fine words"));
    }
}
