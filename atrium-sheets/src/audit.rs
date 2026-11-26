//! Formula auditing (docs/spec/officina.md §16.142): Error Checking steps
//! to the next cell holding an error or a number stored as text and says
//! why; Trace Error follows an error back through the cells it came from
//! to its source; Evaluate Formula's steps come from
//! [`officina_grid::eval::steps`].

use officina_grid::addr::{a1, CellAddr, Rect};
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

/// What Error Checking says about the cell, if anything.
fn problem(wb: &Workbook, a: CellAddr, cell: Cell) -> Option<String> {
    match cell {
        Cell::Err(k) => Some(format!("{} {}: it {}.", a1(a.row, a.col), k.text(), reason(k))),
        Cell::Str(_) if wb.formula_text(a).is_none() => match wb.get(a) {
            Value::Str(s) if number_text(&s) => Some(format!("{} is a number stored as text: formulas won't count it.", a1(a.row, a.col))),
            _ => None,
        },
        _ => None,
    }
}

/// Error Checking: the next cell after `from` on the sheet, by rows and
/// wrapping round, with a problem, and what it is.
pub fn next_error(wb: &Workbook, sheet: u16, from: (u32, u32)) -> Option<((u32, u32), String)> {
    let (rows, cols) = wb.extent(sheet);
    if rows == 0 || cols == 0 {
        return None;
    }
    let all = Rect { sheet, r0: 0, c0: 0, r1: rows - 1, c1: cols - 1 };
    // The first problem after `from` and the first overall, by (row, col).
    let (mut after, mut first): (Option<(u32, u32)>, Option<(u32, u32)>) = (None, None);
    wb.each_cell(&all, &mut |r, c, cell| {
        if !matches!(cell, Cell::Err(_) | Cell::Str(_)) || problem(wb, CellAddr::new(sheet, r, c), cell).is_none() {
            return;
        }
        if first.is_none_or(|f| (r, c) < f) {
            first = Some((r, c));
        }
        if (r, c) > from && after.is_none_or(|f| (r, c) < f) {
            after = Some((r, c));
        }
    });
    let at = after.or(first)?;
    let mut cell = Cell::Empty;
    wb.each_cell(&Rect { sheet, r0: at.0, c0: at.1, r1: at.0, c1: at.1 }, &mut |_, _, c| cell = c);
    Some((at, problem(wb, CellAddr::new(sheet, at.0, at.1), cell)?))
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
