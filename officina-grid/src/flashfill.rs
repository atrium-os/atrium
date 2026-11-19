//! Flash Fill: a column filled by example. From the cells typed in a column
//! beside some data, learn how each was made from the cells beside it (words
//! of them, their first letters, whole cells, in another case, with fixed
//! text between) and fill the column's empty cells the same way.
//!
//! The learning is a search over short programs of pieces, checked against
//! every example as it grows, so an example that disagrees prunes it early.
//! What no program explains is refused rather than guessed.

use crate::addr::{CellAddr, MAX_COLS, MAX_ROWS};
use crate::book::Workbook;
use crate::ops::GridOp;
use crate::value::Value;

/// Columns read on each side of the one filled.
const MAX_SOURCES: u32 = 8;
/// Rows filled at most.
pub const MAX_FILL: u32 = 100_000;
/// Pieces other than fixed text a program may have.
const MAX_PIECES: usize = 12;
/// Steps of the search before it gives up.
const MAX_STEPS: usize = 200_000;
/// Examples it learns from at most (the first ones).
const MAX_EXAMPLES: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Case {
    Same,
    Upper,
    Lower,
    Proper,
}

/// One piece of an output.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    /// Fixed text.
    Text(String),
    /// Source cell `.0` whole.
    Whole(usize, Case),
    /// Word `.1` of source cell `.0` (from the end when negative: -1 last).
    Word(usize, i32, Case),
    /// The first letter of that word.
    Initial(usize, i32, Case),
}

/// The words of `s`: byte spans of its runs of letters and digits.
fn words(s: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, ch) in s.char_indices() {
        match (ch.is_alphanumeric(), start) {
            (true, None) => start = Some(i),
            (false, Some(a)) => {
                out.push((a, i));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(a) = start {
        out.push((a, s.len()));
    }
    out
}

fn cased(s: &str, c: Case) -> String {
    match c {
        Case::Same => s.to_string(),
        Case::Upper => s.to_uppercase(),
        Case::Lower => s.to_lowercase(),
        Case::Proper => {
            let mut out = String::with_capacity(s.len());
            let mut first = true;
            for ch in s.chars() {
                if ch.is_alphanumeric() {
                    if first { out.extend(ch.to_uppercase()) } else { out.extend(ch.to_lowercase()) }
                    first = false;
                } else {
                    out.push(ch);
                    first = true;
                }
            }
            out
        }
    }
}

fn word(src: &str, i: i32) -> Option<&str> {
    let ws = words(src);
    let k = if i >= 0 { i as usize } else { ws.len().checked_sub(i.unsigned_abs() as usize)? };
    ws.get(k).map(|&(a, b)| &src[a..b])
}

fn eval(p: &Piece, srcs: &[String]) -> Option<String> {
    match p {
        Piece::Text(t) => Some(t.clone()),
        Piece::Whole(c, k) => srcs.get(*c).filter(|s| !s.is_empty()).map(|s| cased(s, *k)),
        Piece::Word(c, i, k) => word(srcs.get(*c)?, *i).map(|w| cased(w, *k)),
        Piece::Initial(c, i, k) => word(srcs.get(*c)?, *i).and_then(|w| w.chars().next()).map(|ch| cased(&ch.to_string(), *k)),
    }
}

fn run(prog: &[Piece], srcs: &[String]) -> Option<String> {
    let mut out = String::new();
    for p in prog {
        out.push_str(&eval(p, srcs)?);
    }
    Some(out)
}

/// The pieces that could come next at byte `at` of `out`, from `srcs`:
/// longest first, fixed text (one character) last.
fn candidates(srcs: &[String], out: &str, at: usize) -> Vec<Piece> {
    let rest = &out[at..];
    let mut found: Vec<(usize, u8, Piece)> = Vec::new();
    let cases = [Case::Same, Case::Proper, Case::Upper, Case::Lower];
    for (c, s) in srcs.iter().enumerate() {
        if s.is_empty() {
            continue;
        }
        let n = words(s).len() as i32;
        let mut pieces = vec![];
        for k in cases {
            pieces.push((0u8, Piece::Whole(c, k)));
            for i in 0..n {
                // Counted from the nearer end first: the last word of a
                // longer cell is still its last.
                let near_start = i < n - 1 - i;
                let (s, e) = if near_start { (1, 2) } else { (2, 1) };
                pieces.push((s, Piece::Word(c, i, k)));
                pieces.push((e, Piece::Word(c, i - n, k)));
                pieces.push((s + 2, Piece::Initial(c, i, k)));
                pieces.push((e + 2, Piece::Initial(c, i - n, k)));
            }
        }
        for (rank, p) in pieces {
            if let Some(v) = eval(&p, srcs) {
                if !v.is_empty() && rest.starts_with(&v) && !found.iter().any(|f| f.2 == p) {
                    found.push((v.len(), rank, p));
                }
            }
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut v: Vec<Piece> = found.into_iter().map(|f| f.2).collect();
    if let Some(ch) = rest.chars().next() {
        v.push(Piece::Text(ch.to_string()));
    }
    v
}

/// A program making each example's output from its sources.
fn learn(examples: &[(Vec<String>, String)]) -> Option<Vec<Piece>> {
    let mut prog = Vec::new();
    let mut at = vec![0usize; examples.len()];
    let mut steps = 0;
    fn go(ex: &[(Vec<String>, String)], prog: &mut Vec<Piece>, at: &mut Vec<usize>, steps: &mut usize) -> bool {
        *steps += 1;
        if *steps > MAX_STEPS {
            return false;
        }
        if at.iter().zip(ex).all(|(a, e)| *a == e.1.len()) {
            return true;
        }
        if at[0] == ex[0].1.len() || prog.iter().filter(|p| !matches!(p, Piece::Text(_))).count() >= MAX_PIECES {
            return false;
        }
        'next: for p in candidates(&ex[0].0, &ex[0].1, at[0]) {
            let mut moved = Vec::with_capacity(ex.len());
            for (k, e) in ex.iter().enumerate() {
                match eval(&p, &e.0) {
                    Some(v) if e.1[at[k]..].starts_with(&v) && (!v.is_empty() || k > 0) => moved.push(v.len()),
                    _ => continue 'next,
                }
            }
            for (a, m) in at.iter_mut().zip(&moved) {
                *a += m;
            }
            prog.push(p);
            if go(ex, prog, at, steps) {
                return true;
            }
            prog.pop();
            for (a, m) in at.iter_mut().zip(&moved) {
                *a -= m;
            }
        }
        false
    }
    go(examples, &mut prog, &mut at, &mut steps).then(|| merge(prog))
}

/// Fixed texts side by side as one.
fn merge(prog: Vec<Piece>) -> Vec<Piece> {
    let mut out: Vec<Piece> = Vec::new();
    for p in prog {
        match (out.last_mut(), p) {
            (Some(Piece::Text(a)), Piece::Text(b)) => a.push_str(&b),
            (_, p) => out.push(p),
        }
    }
    out
}

fn text(wb: &Workbook, sheet: u16, r: u32, c: u32) -> String {
    wb.formatted(CellAddr::new(sheet, r, c)).text
}

fn filled(wb: &Workbook, sheet: u16, r: u32, c: u32) -> bool {
    !matches!(wb.value(sheet, r, c), Value::Empty) || wb.formula_text(CellAddr::new(sheet, r, c)).is_some()
}

/// Flash Fill column `col` around row `row` of `sheet`: the rows beside data
/// in the columns next to it, those typed as examples, the empty ones
/// filled. The edit, and how many cells it fills.
pub fn flash_fill(wb: &Workbook, sheet: u16, row: u32, col: u32) -> Result<(Vec<GridOp>, u32), String> {
    if sheet as usize >= wb.sheet_count() || row >= MAX_ROWS || col >= MAX_COLS {
        return Err("Put the cursor in the column to fill.".into());
    }
    let side = |r: u32| (col > 0 && filled(wb, sheet, r, col - 1)) || (col + 1 < MAX_COLS && filled(wb, sheet, r, col + 1));
    // The rows: those around the cursor with data beside them.
    let (mut r0, mut r1) = (row, row);
    while r0 > 0 && side(r0 - 1) && row - r0 < MAX_FILL {
        r0 -= 1;
    }
    while r1 + 1 < MAX_ROWS && side(r1 + 1) && r1 - r0 < MAX_FILL {
        r1 += 1;
    }
    if !side(row) && r0 == r1 {
        return Err("Flash Fill fills a column beside data; type an example or two next to it first.".into());
    }
    // A header row (the first, when it is above every example) is left alone.
    // The columns read: next to it on each side, up to a column with nothing.
    let used = |c: u32| (r0..=r1).any(|r| filled(wb, sheet, r, c));
    let mut cols = Vec::new();
    let mut c = col;
    while c > 0 && col - c < MAX_SOURCES && used(c - 1) {
        c -= 1;
        cols.push(c);
    }
    let mut c = col;
    while c + 1 < MAX_COLS && c - col < MAX_SOURCES && used(c + 1) {
        c += 1;
        cols.push(c);
    }
    let srcs = |r: u32| cols.iter().map(|&c| text(wb, sheet, r, c)).collect::<Vec<_>>();
    let typed: Vec<u32> = (r0..=r1).filter(|&r| filled(wb, sheet, r, col)).collect();
    if typed.is_empty() {
        return Err("Type how the first cell should read, then Flash Fill the rest.".into());
    }
    // A first row typed above the examples that the others don't explain is
    // taken for a header: learnt without it (it is never filled, as it
    // holds something).
    let examples: Vec<(Vec<String>, String)> = typed.iter().take(MAX_EXAMPLES).map(|&r| (srcs(r), text(wb, sheet, r, col))).filter(|e| !e.1.is_empty()).collect();
    let prog = learn(&examples).or_else(|| (examples.len() > 1 && typed[0] == r0).then(|| learn(&examples[1..])).flatten());
    let Some(prog) = prog else {
        return Err("Flash Fill couldn't see how the examples were made; type another example.".into());
    };
    let mut cells = Vec::new();
    for r in r0..=r1 {
        if filled(wb, sheet, r, col) {
            continue;
        }
        if let Some(v) = run(&prog, &srcs(r)).filter(|v| !v.is_empty()) {
            cells.push((r, col, Some(Value::str(&v)), None));
        }
    }
    if cells.is_empty() {
        return Err("Nothing left to fill in that column.".into());
    }
    let n = cells.len() as u32;
    Ok((crate::clip::by_runs(sheet, cells), n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use officina_core::history::Op as _;

    fn ex(src: &[&str], out: &str) -> (Vec<String>, String) {
        (src.iter().map(|s| s.to_string()).collect(), out.to_string())
    }

    #[test]
    fn programs_are_learnt_from_examples() {
        // Last name, first initial.
        let p = learn(&[ex(&["Ada Lovelace"], "Lovelace, A."), ex(&["Alan Turing"], "Turing, A.")]).unwrap();
        assert_eq!(run(&p, &["Grace Brewster Hopper".into()]).as_deref(), Some("Hopper, G."));
        // From two columns, in another case.
        let p = learn(&[ex(&["ada", "lovelace"], "ADA LOVELACE")]).unwrap();
        assert_eq!(run(&p, &["alan".into(), "turing".into()]).as_deref(), Some("ALAN TURING"));
        // A part of an e-mail address.
        let p = learn(&[ex(&["ada@example.org"], "ada"), ex(&["alan.t@example.com"], "alan")]).unwrap();
        assert_eq!(run(&p, &["grace@navy.mil".into()]).as_deref(), Some("grace"));
        // Examples that disagree: nothing learnt.
        assert!(learn(&[ex(&["a b"], "a"), ex(&["c d"], "x")]).is_none());
    }

    #[test]
    fn a_column_is_filled_by_example() {
        let mut wb = Workbook::new();
        wb.add_sheet("S").unwrap();
        let names = ["Name", "Ada Lovelace", "Alan Turing", "Grace Hopper", "Edsger Dijkstra"];
        for (r, n) in names.iter().enumerate() {
            wb.set_value(CellAddr::new(0, r as u32, 0), Value::str(n));
        }
        wb.set_value(CellAddr::new(0, 0, 1), Value::str("Initials"));
        wb.set_value(CellAddr::new(0, 1, 1), Value::str("A.L."));
        let (ops, n) = flash_fill(&wb, 0, 2, 1).unwrap();
        assert_eq!(n, 3);
        for op in ops {
            op.apply(&mut wb);
        }
        let got: Vec<String> = (0..5).map(|r| text(&wb, 0, r, 1)).collect();
        assert_eq!(got, ["Initials", "A.L.", "A.T.", "G.H.", "E.D."]);
        // Nothing left: refused.
        assert!(flash_fill(&wb, 0, 2, 1).is_err());
        // No data beside: refused.
        assert!(flash_fill(&wb, 0, 20, 5).is_err());
    }
}
