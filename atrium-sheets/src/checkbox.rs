//! Checkboxes in cells (docs/spec/officina.md §16.159), as Excel's
//! Insert ▸ Checkbox: a cell format that shows TRUE as a ticked box and
//! FALSE as a clear one. A click on the box, or Space, flips it.

use officina_grid::book::Input;
use officina_grid::ops::GridOp;
use officina_grid::{CellAddr, Rect, Value};

use crate::doc::Doc;
use pergola::widgets::CellRange;

/// Cells looked at when filling or flipping, at most.
const MAX_CELLS: u64 = 100_000;

fn rect(d: &Doc, sel: CellRange) -> Rect {
    Rect { sheet: d.sheet, r0: sel.r0, c0: sel.c0, r1: sel.r1, c1: sel.c1 }
}

fn cells(sel: CellRange) -> u64 {
    u64::from(sel.r1 - sel.r0 + 1) * u64::from(sel.c1 - sel.c0 + 1)
}

/// Whether the cell at `cursor` has the checkbox format.
pub fn has(d: &Doc, cursor: (u32, u32)) -> bool {
    d.wb.style(CellAddr::new(d.sheet, cursor.0, cursor.1)).checkbox
}

/// Insert Checkbox: the selection takes the checkbox format and its empty
/// cells become FALSE. With the cursor on a checkbox, takes the format off
/// the selection instead (the values stay). One step.
pub fn insert(d: &mut Doc, sel: CellRange, cursor: (u32, u32)) -> Result<String, String> {
    let area = rect(d, sel);
    if has(d, cursor) {
        let op = GridOp::restyle(&d.wb, area, |st| officina_grid::style::Style { checkbox: false, ..st.clone() });
        d.status.clear();
        if !d.run_action("Remove Checkbox", vec![op]) {
            return Err(std::mem::take(&mut d.status));
        }
        return Ok("Checkboxes removed; the cells keep TRUE and FALSE.".into());
    }
    if cells(sel) > MAX_CELLS {
        return Err(format!("Insert checkboxes into at most {MAX_CELLS} cells at once."));
    }
    let mut ops = vec![GridOp::restyle(&d.wb, area, |st| officina_grid::style::Style { checkbox: true, ..st.clone() })];
    for r in sel.r0..=sel.r1 {
        for c in sel.c0..=sel.c1 {
            let at = CellAddr::new(d.sheet, r, c);
            if d.wb.input(at) == Input::Empty {
                ops.push(GridOp::set(at, Input::Value(Value::Bool(false))));
            }
        }
    }
    d.status.clear();
    if !d.run_action("Insert Checkbox", ops) {
        return Err(std::mem::take(&mut d.status));
    }
    Ok("Checkboxes inserted: click one, or press Space, to tick it.".into())
}

/// Ticks every checkbox in `sel` when any is clear, else clears them all.
/// Cells holding formulas or values other than TRUE and FALSE are left as
/// they are. One step.
pub fn toggle(d: &mut Doc, sel: CellRange) -> Result<String, String> {
    if cells(sel) > MAX_CELLS {
        return Err(format!("Flip at most {MAX_CELLS} checkboxes at once."));
    }
    let mut boxes = Vec::new();
    for r in sel.r0..=sel.r1 {
        for c in sel.c0..=sel.c1 {
            let at = CellAddr::new(d.sheet, r, c);
            if !d.wb.style(at).checkbox || d.wb.formula_text(at).is_some() {
                continue;
            }
            if let Value::Bool(on) = d.wb.get(at) {
                boxes.push((at, on));
            }
        }
    }
    if boxes.is_empty() {
        return Err("No checkbox here can be ticked (a formula decides it, or it holds another value).".into());
    }
    let on = boxes.iter().any(|&(_, b)| !b);
    let ops = boxes.iter().map(|&(at, _)| GridOp::set(at, Input::Value(Value::Bool(on)))).collect();
    d.status.clear();
    if !d.run_action("Checkbox", ops) {
        return Err(std::mem::take(&mut d.status));
    }
    let n = boxes.len();
    Ok(format!("{} {n} {}.", if on { "Ticked" } else { "Cleared" }, if n == 1 { "checkbox" } else { "checkboxes" }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_toggle_and_remove_checkboxes() {
        let mut d = Doc::from_csv("c.csv", b"Done,Task\n,Pens\nTRUE,Ink\nx,Paper\n").unwrap().0;
        d.wb.set_input(CellAddr::new(0, 4, 0), "=1>0").unwrap();
        let col = CellRange { r0: 1, c0: 0, r1: 4, c1: 0 };
        insert(&mut d, col, (1, 0)).unwrap();
        assert!(has(&d, (3, 0)));
        // The empty cell became FALSE; the others keep their values.
        assert_eq!(d.wb.get(CellAddr::new(0, 1, 0)), Value::Bool(false));
        assert_eq!(d.wb.get(CellAddr::new(0, 3, 0)), Value::str("x"));
        // Any clear one: all tick (the text and the formula are left alone).
        assert_eq!(toggle(&mut d, col).unwrap(), "Ticked 2 checkboxes.");
        assert_eq!(d.wb.get(CellAddr::new(0, 1, 0)), Value::Bool(true));
        assert_eq!(toggle(&mut d, CellRange { r0: 2, c0: 0, r1: 2, c1: 0 }).unwrap(), "Cleared 1 checkbox.");
        assert!(toggle(&mut d, CellRange { r0: 3, c0: 0, r1: 4, c1: 0 }).is_err());
        assert!(d.undo());
        assert_eq!(d.wb.get(CellAddr::new(0, 2, 0)), Value::Bool(true));
        // Again from a checkbox: the format comes off, the values stay.
        insert(&mut d, col, (1, 0)).unwrap();
        assert!(!has(&d, (1, 0)));
        assert_eq!(d.wb.get(CellAddr::new(0, 1, 0)), Value::Bool(true));
        // The format survives the native file.
        insert(&mut d, col, (1, 0)).unwrap();
        let back = officina_grid::native::load(&officina_grid::native::save_to_vec(&d.wb)).unwrap();
        assert!(back.style(CellAddr::new(0, 2, 0)).checkbox);
        assert!(insert(&mut d, CellRange { r0: 0, c0: 0, r1: 1000, c1: 200 }, (0, 1)).is_err());
    }
}
