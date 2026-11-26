//! Formula auditing (docs/spec/officina.md §16.142): Error Checking steps
//! to the next cell holding an error, a number stored as text or a formula
//! Excel's rules flag (§16.146: inconsistent with its neighbours, leaving
//! out a number beside its range, unlocked) and says why; Trace Error follows an error back through the cells it came from
//! to its source; Evaluate Formula's steps come from
//! [`officina_grid::eval::steps`].

use officina_grid::addr::{a1, CellAddr, Rect};
use officina_grid::sheetareas::{EVAL_ERROR, INCONSISTENT, NUMBER_AS_TEXT, OMITS_ADJACENT, UNLOCKED_FORMULA};
use officina_grid::column::Cell;
use officina_grid::value::ErrorKind;
use officina_grid::{Value, Workbook};

/// How far Trace Error follows an error back.
pub const MAX_TRACE: usize = 64;
/// The most cells of one precedent area Trace Error looks through.
pub const MAX_TRACE_CELLS: u64 = 100_000;

/// What an error value means, in a sentence.
pub fn reason(k: ErrorKind) -> &'static str {
    match k {
        ErrorKind::Div0 => "divides by zero or by an empty cell",
        ErrorKind::NA => "a value isn't available: a lookup found no match",
        ErrorKind::Name => "uses a name or function that doesn't exist",
        ErrorKind::Ref => "refers to cells that were deleted or are off the sheet",
        ErrorKind::Value => "has an argument of the wrong kind, such as text where a number goes",
        ErrorKind::Num => "has a number that is out of range or doesn't converge",
        ErrorKind::Null => "intersects two ranges that don't meet",
        ErrorKind::Circ => "is part of a circular reference",
        ErrorKind::Spill => "can't spill: cells it would fill aren't empty",
        ErrorKind::Calc => "is a calculation Excel refuses, such as an empty result",
        ErrorKind::Limit => "is over Officina's limits",
    }
}

fn number_text(s: &str) -> bool {
    let t = s.trim();
    !t.is_empty() && t.parse::<f64>().is_ok_and(f64::is_finite) && t.chars().any(|c| c.is_ascii_digit())
}

/// The formula's expression at a cell, if it holds one.
fn expr_at(wb: &Workbook, sheet: u16, row: u32, col: u32) -> Option<std::sync::Arc<officina_grid::formula::ast::Expr>> {
    wb.group_at(sheet, row, col).map(|g| wb.group(g).expr.clone())
}

/// What Error Checking finds at `a`, if anything: the rule
/// ([`officina_grid::sheetareas`]'s bits) and what to say. Rules the cell
/// is told to ignore are passed over.
pub fn problem_at(wb: &Workbook, a: CellAddr) -> Option<(u16, String)> {
    let ignored = officina_grid::sheetareas::ignored_at(wb.sheet(a.sheet).ignored_errors(), a.row, a.col);
    let here = a1(a.row, a.col);
    let live = |rule: u16| ignored & rule == 0;
    match wb.get(a) {
        Value::Err(k) if live(EVAL_ERROR) => return Some((EVAL_ERROR, format!("{here} {}: it {}.", k.text(), reason(k)))),
        Value::Str(s) if live(NUMBER_AS_TEXT) && wb.formula_text(a).is_none() && number_text(&s) => {
            return Some((NUMBER_AS_TEXT, format!("{here} is a number stored as text: formulas won't count it. Convert to Number fixes it.")));
        }
        _ => {}
    }
    let e = expr_at(wb, a.sheet, a.row, a.col)?;
    if live(INCONSISTENT) {
        let at = |r: Option<u32>, c: Option<u32>| r.zip(c).and_then(|(r, c)| expr_at(wb, a.sheet, r, c));
        let pairs = [
            (at(a.row.checked_sub(1), Some(a.col)), at(a.row.checked_add(1), Some(a.col)), "above and below"),
            (at(Some(a.row), a.col.checked_sub(1)), at(Some(a.row), a.col.checked_add(1)), "left and right"),
        ];
        for (x, y, side) in pairs {
            if let (Some(x), Some(y)) = (x, y) {
                if x == y && *x != *e {
                    return Some((INCONSISTENT, format!("{here}'s formula differs from the ones {side} of it, which agree.")));
                }
            }
        }
    }
    if live(OMITS_ADJACENT) {
        let number = |r: u32, c: u32| (r, c) != (a.row, a.col) && matches!(wb.get(CellAddr::new(a.sheet, r, c)), Value::Num(_));
        for p in crate::data::precedents(wb, a.sheet, a.row, a.col) {
            if p.sheet != a.sheet || p.cells() < 2 {
                continue;
            }
            let near = if p.c0 == p.c1 {
                [p.r0.checked_sub(1).map(|r| (r, p.c0)), p.r1.checked_add(1).filter(|&r| r < officina_grid::addr::MAX_ROWS).map(|r| (r, p.c0))]
            } else if p.r0 == p.r1 {
                [p.c0.checked_sub(1).map(|c| (p.r0, c)), p.c1.checked_add(1).filter(|&c| c < officina_grid::addr::MAX_COLS).map(|c| (p.r0, c))]
            } else {
                continue;
            };
            if let Some((r, c)) = near.into_iter().flatten().find(|&(r, c)| number(r, c)) {
                return Some((OMITS_ADJACENT, format!("{here}'s formula leaves out {}, a number next to the cells it reads.", a1(r, c))));
            }
        }
    }
    if live(UNLOCKED_FORMULA) && wb.style(a).unlocked {
        return Some((UNLOCKED_FORMULA, format!("{here} holds a formula but is unlocked: protecting the sheet won't keep it from being changed.")));
    }
    None
}

/// The most formula cells Error Checking looks through for its formula rules.
pub const MAX_FORMULA_CELLS: u64 = 200_000;

/// Error Checking: the next cell after `from` on the sheet, by rows and
/// wrapping round, with a problem, and what it is.
pub fn next_error(wb: &Workbook, sheet: u16, from: (u32, u32)) -> Option<((u32, u32), String)> {
    let (rows, cols) = wb.extent(sheet);
    if rows == 0 || cols == 0 {
        return None;
    }
    let all = Rect { sheet, r0: 0, c0: 0, r1: rows - 1, c1: cols - 1 };
    // Cells that might have a problem: errors and text, and formula cells.
    let mut cands: Vec<(u32, u32)> = Vec::new();
    wb.each_cell(&all, &mut |r, c, cell| {
        if matches!(cell, Cell::Err(_) | Cell::Str(_)) {
            cands.push((r, c));
        }
    });
    let mut n = 0u64;
    for (_, g) in wb.groups() {
        if g.sheet != sheet {
            continue;
        }
        for r in g.r0..=g.r1 {
            n += 1;
            if n > MAX_FORMULA_CELLS {
                break;
            }
            cands.push((r, g.col));
        }
    }
    cands.sort_unstable();
    cands.dedup();
    let split = cands.partition_point(|&p| p <= from);
    let order = cands[split..].iter().chain(&cands[..split]);
    for &(r, c) in order {
        if let Some((_, why)) = problem_at(wb, CellAddr::new(sheet, r, c)) {
            return Some(((r, c), why));
        }
    }
    None
}

/// Trace Error: from the error at `a`, each cell it reads that holds an
/// error, back to the first one whose own inputs hold none (its source).
/// The path starts with `a`.
pub fn trace_error(wb: &Workbook, a: CellAddr) -> Result<Vec<CellAddr>, String> {
    let Value::Err(k) = wb.get(a) else {
        return Err(format!("{} holds no error to trace.", a1(a.row, a.col)));
    };
    let _ = k;
    let mut path = vec![a];
    let mut at = a;
    while path.len() < MAX_TRACE {
        let mut next = None;
        for area in crate::data::precedents(wb, at.sheet, at.row, at.col) {
            if area.cells() > MAX_TRACE_CELLS {
                continue;
            }
            let mut hit: Option<(u32, u32)> = None;
            wb.each_cell(&area, &mut |r, c, cell| {
                if matches!(cell, Cell::Err(_)) && hit.is_none_or(|h| (r, c) < h) {
                    hit = Some((r, c));
                }
            });
            if let Some((r, c)) = hit {
                next = Some(CellAddr::new(area.sheet, r, c));
                break;
            }
        }
        match next {
            Some(n) if !path.contains(&n) => {
                path.push(n);
                at = n;
            }
            _ => break,
        }
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::Doc;

    #[test]
    fn error_checking_steps_through_problems_and_wraps() {
        let mut d = Doc::from_csv("e.csv", b"1,0,x\n'42,ok,\n").unwrap().0;
        d.type_into(0, 2, "=A1/B1");
        d.type_into(1, 0, "42");
        d.type_into(2, 1, "=NOPE()");
        d.wb.recalc();
        // A2 typed as 42 is a number; make it text explicitly.
        d.wb.set_value(CellAddr::new(0, 1, 0), Value::str("42"));
        let (at, why) = next_error(&d.wb, 0, (0, 0)).unwrap();
        assert_eq!(at, (0, 2));
        assert!(why.contains("C1 #DIV/0!") && why.contains("divides by zero"), "{why}");
        let (at, why) = next_error(&d.wb, 0, at).unwrap();
        assert_eq!(at, (1, 0));
        assert!(why.contains("number stored as text"), "{why}");
        let (at, why) = next_error(&d.wb, 0, at).unwrap();
        assert_eq!(at, (2, 1));
        assert!(why.contains("#NAME?"), "{why}");
        assert_eq!(next_error(&d.wb, 0, at).unwrap().0, (0, 2), "wraps round");
        let clean = Doc::from_csv("c.csv", b"1,2\n").unwrap().0;
        assert!(next_error(&clean.wb, 0, (0, 0)).is_none());
    }

    #[test]
    fn trace_error_follows_the_error_to_its_source() {
        let mut d = Doc::from_csv("t.csv", b"5,0\n").unwrap().0;
        d.type_into(1, 0, "=A1/B1");
        d.type_into(2, 0, "=SUM(A1:A2)");
        d.type_into(3, 0, "=A3*2");
        d.wb.recalc();
        let path = trace_error(&d.wb, CellAddr::new(0, 3, 0)).unwrap();
        assert_eq!(path.iter().map(|a| a1(a.row, a.col)).collect::<Vec<_>>(), ["A4", "A3", "A2"]);
        assert!(trace_error(&d.wb, CellAddr::new(0, 0, 0)).unwrap_err().contains("no error"));
    }
}
