//! Trace Precedents, Trace Dependents and Remove Arrows (docs/spec/officina.md
//! §16.147): arrows drawn over the grid from the cells a formula reads to
//! the formula, a level more each time, as Excel draws them. They are a
//! view of the sheet, not part of it: not saved, not undone, and gone after
//! the next edit.

use officina_grid::addr::{CellAddr, Rect};
use officina_grid::Value;

use crate::doc::Doc;

/// The most arrows shown at once.
pub const MAX_ARROWS: usize = 500;

/// One arrow: from the cells read to the formula reading them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arrow {
    /// What is read (a box is drawn round more than one cell).
    pub from: Rect,
    /// The formula's cell.
    pub to: CellAddr,
    /// The cells read hold an error (Excel draws those red).
    pub error: bool,
}

fn has_error(d: &Doc, r: &Rect) -> bool {
    if r.cells() > 10_000 {
        return false;
    }
    let mut e = false;
    d.wb.each(r, &mut |_, _, v| {
        if matches!(v, Value::Err(_)) {
            e = true;
        }
    });
    e
}

fn push(d: &mut Doc, a: Arrow) -> bool {
    if d.arrows.contains(&a) {
        return false;
    }
    if d.arrows.len() >= MAX_ARROWS {
        return false;
    }
    d.arrows.push(a);
    true
}

/// Trace Precedents from the cursor: the first time, arrows from what its
/// formula reads; again, from what those cells' formulas read, a level
/// further. The message says what was added.
pub fn trace_precedents(d: &mut Doc, cursor: (u32, u32)) -> Result<String, String> {
    let at = CellAddr::new(d.sheet, cursor.0, cursor.1);
    // The formulas at the far end of what is drawn: the cursor, then the
    // formula cells inside every area already drawn back from it.
    let mut ends = vec![at];
    let mut seen = vec![at];
    let mut i = 0;
    while i < ends.len() && i < 4 * MAX_ARROWS {
        let to = ends[i];
        i += 1;
        for a in d.arrows.iter().filter(|a| a.to == to) {
            let r = a.from;
            if r.cells() > 10_000 {
                continue;
            }
            for (_, g) in d.wb.groups() {
                if g.sheet != r.sheet || !(r.c0..=r.c1).contains(&g.col) || g.r1 < r.r0 || g.r0 > r.r1 {
                    continue;
                }
                for row in g.r0.max(r.r0)..=g.r1.min(r.r1) {
                    let c = CellAddr::new(g.sheet, row, g.col);
                    if !seen.contains(&c) && seen.len() < 4 * MAX_ARROWS {
                        seen.push(c);
                        ends.push(c);
                    }
                }
            }
        }
    }
    let mut added = 0;
    for to in ends {
        if d.arrows.iter().any(|a| a.to == to) {
            continue;
        }
        for from in crate::data::precedents(&d.wb, to.sheet, to.row, to.col) {
            let error = has_error(d, &from);
            if push(d, Arrow { from, to, error }) {
                added += 1;
            }
        }
    }
    if added == 0 {
        return Err(if d.arrows.iter().any(|a| a.to == at) { "No more precedents to trace.".into() } else { "This cell has no formula reading other cells.".into() });
    }
    Ok(format!("{added} precedent {} drawn. Remove Arrows takes them away.", if added == 1 { "arrow" } else { "arrows" }))
}

/// Trace Dependents from the cursor: arrows to the formulas reading it,
/// then, again, to the formulas reading those.
pub fn trace_dependents(d: &mut Doc, cursor: (u32, u32)) -> Result<String, String> {
    let at = CellAddr::new(d.sheet, cursor.0, cursor.1);
    let cell = |a: CellAddr| Rect { sheet: a.sheet, r0: a.row, c0: a.col, r1: a.row, c1: a.col };
    // The cells at the far end: the cursor, then each formula drawn to.
    let mut ends = vec![at];
    let mut i = 0;
    while i < ends.len() && ends.len() < 4 * MAX_ARROWS {
        let from = ends[i];
        i += 1;
        let next: Vec<CellAddr> = d.arrows.iter().filter(|a| a.from == cell(from)).map(|a| a.to).collect();
        for n in next {
            if !ends.contains(&n) {
                ends.push(n);
            }
        }
    }
    let mut added = 0;
    for from in ends {
        if d.arrows.iter().any(|a| a.from == cell(from)) {
            continue;
        }
        let error = matches!(d.wb.get(from), Value::Err(_));
        for to in crate::data::dependents(&d.wb, from.sheet, from.row, from.col, crate::data::MAX_DEPENDENTS) {
            if push(d, Arrow { from: cell(from), to, error }) {
                added += 1;
            }
        }
    }
    if added == 0 {
        return Err(if d.arrows.iter().any(|a| a.from == cell(at)) { "No more dependents to trace.".into() } else { "No formula reads this cell.".into() });
    }
    Ok(format!("{added} dependent {} drawn. Remove Arrows takes them away.", if added == 1 { "arrow" } else { "arrows" }))
}

/// Remove Arrows.
pub fn remove_arrows(d: &mut Doc) -> Result<String, String> {
    if d.arrows.is_empty() {
        return Err("No arrows to remove.".into());
    }
    d.arrows.clear();
    Ok("Arrows removed.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrows_trace_a_level_at_a_time_and_go() {
        let mut d = Doc::from_csv("a.csv", b"1,2\n").unwrap().0;
        d.type_into(0, 2, "=A1+B1");
        d.type_into(0, 3, "=C1*2");
        d.type_into(1, 3, "=SUM(A1:B1)+1/0");
        d.wb.recalc();
        assert!(trace_precedents(&mut d, (0, 0)).is_err(), "A1 has no formula");
        assert_eq!(trace_precedents(&mut d, (0, 3)).unwrap(), "1 precedent arrow drawn. Remove Arrows takes them away.");
        assert_eq!(d.arrows[0].from, Rect { sheet: 0, r0: 0, c0: 2, r1: 0, c1: 2 });
        // Again: C1's own two precedents.
        assert_eq!(trace_precedents(&mut d, (0, 3)).unwrap(), "2 precedent arrows drawn. Remove Arrows takes them away.");
        assert_eq!(trace_precedents(&mut d, (0, 3)).unwrap_err(), "No more precedents to trace.");
        assert_eq!(remove_arrows(&mut d).unwrap(), "Arrows removed.");
        // Dependents of A1: C1 and D2; then D1 from C1.
        assert_eq!(trace_dependents(&mut d, (0, 0)).unwrap(), "2 dependent arrows drawn. Remove Arrows takes them away.");
        assert_eq!(trace_dependents(&mut d, (0, 0)).unwrap(), "1 dependent arrow drawn. Remove Arrows takes them away.");
        assert!(d.arrows.iter().any(|a| a.to == CellAddr::new(0, 0, 3)));
        // An edit takes them away.
        d.type_into(0, 0, "5");
        assert!(d.arrows.is_empty());
        // An error source is marked.
        trace_dependents(&mut d, (1, 3)).unwrap_err();
        d.type_into(2, 3, "=D2");
        d.wb.recalc();
        trace_dependents(&mut d, (1, 3)).unwrap();
        assert!(d.arrows[0].error);
    }
}
