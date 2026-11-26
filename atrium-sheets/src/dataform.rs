//! The Data Form and Quick Analysis totals (docs/spec/officina.md §16.147).
//!
//! The Data Form shows one record of the list around the cursor at a time
//! (the region's first row names the fields): Previous and Next step
//! through them, and its bar sets fields (`Qty = 5; Region = East`), adds a
//! record (`new Region = West; Qty = 2`) or finds the next one matching
//! (`find Region = East, Qty > 10`). Delete removes the record, the cells
//! below moving up within the list. Each change is one undo step.
//!
//! Quick totals put a row of totals under the selection or a column of them
//! to its right: Sum, Average, Count, % Total or Running Total, as formulas.

use officina_grid::addr::{a1, CellAddr, Rect};
use officina_grid::book::Input;
use officina_grid::ops::GridOp;
use officina_grid::Value;
use pergola::widgets::CellRange;

use crate::doc::Doc;

/// The list a Data Form works on: its area, the first row its fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Form {
    pub area: Rect,
    /// The record shown, from 0.
    pub record: u32,
}

/// The most fields a form shows.
pub const MAX_FIELDS: u32 = 32;

impl Form {
    /// How many records the list holds.
    pub fn count(&self) -> u32 {
        self.area.r1 - self.area.r0
    }

    fn row(&self) -> u32 {
        self.area.r0 + 1 + self.record
    }
}

/// The form for the list around the cursor, on the cursor's record (or
/// the first).
pub fn open(d: &Doc, cursor: (u32, u32)) -> Result<Form, String> {
    let area = crate::data::current_region(&d.wb, d.sheet, cursor.0, cursor.1);
    if matches!(d.wb.get(CellAddr::new(d.sheet, cursor.0, cursor.1)), Value::Empty) && area.r0 == area.r1 && area.c0 == area.c1 {
        return Err("Put the cursor in a list with its field names in the first row.".into());
    }
    if area.c1 - area.c0 + 1 > MAX_FIELDS {
        return Err(format!("The Data Form shows at most {MAX_FIELDS} fields."));
    }
    let record = cursor.0.saturating_sub(area.r0 + 1).min((area.r1 - area.r0).saturating_sub(1));
    Ok(Form { area, record })
}

/// The fields of the shown record: (field name, value as shown, formula).
pub fn fields(d: &Doc, f: &Form) -> Vec<(String, String, bool)> {
    let s = f.area.sheet;
    (f.area.c0..=f.area.c1).map(|c| {
        let name = d.wb.formatted(CellAddr::new(s, f.area.r0, c)).text;
        let at = CellAddr::new(s, f.row(), c);
        let shown = if f.count() == 0 { String::new() } else { d.wb.formatted(at).text };
        (if name.is_empty() { format!("Column {}", officina_grid::addr::col_name(c)) } else { name }, shown, d.wb.formula_text(at).is_some())
    }).collect()
}

/// The column of field `name` (without case).
fn field(d: &Doc, f: &Form, name: &str) -> Result<u32, String> {
    let n = name.trim();
    (f.area.c0..=f.area.c1).find(|&c| d.wb.formatted(CellAddr::new(f.area.sheet, f.area.r0, c)).text.trim().eq_ignore_ascii_case(n)).ok_or_else(|| format!("No field is called {n:?}."))
}

/// `Field = value; …` as (column, input). Formulas can't be typed in the
/// form, as in Excel.
fn assignments(d: &Doc, f: &Form, typed: &str) -> Result<Vec<(u32, Input)>, String> {
    let mut out = Vec::new();
    for part in typed.split(';').map(str::trim).filter(|p| !p.is_empty()) {
        let (name, value) = part.split_once('=').ok_or_else(|| format!("{part:?}: type a field, =, and its value."))?;
        let c = field(d, f, name)?;
        let v = value.trim();
        let input = if v.is_empty() {
            Input::Empty
        } else if let Ok(x) = v.parse::<f64>() {
            Input::Value(Value::Num(x))
        } else {
            Input::Value(Value::str(v))
        };
        out.push((c, input));
    }
    if out.is_empty() {
        return Err("Type a field and its value, such as Qty = 5.".into());
    }
    Ok(out)
}

/// What the bar typed: set the shown record's fields, add a record
/// (`new …`), or find the next one matching (`find …`). The form after.
pub fn run(d: &mut Doc, f: Form, typed: &str) -> Result<(Form, String), String> {
    let t = typed.trim();
    let word = |w: &str| t.get(..w.len()).filter(|p| p.eq_ignore_ascii_case(w)).and_then(|_| t.get(w.len()..));
    let n = f.count();
    if t.eq_ignore_ascii_case("next") || t.eq_ignore_ascii_case("prev") || t.eq_ignore_ascii_case("previous") {
        if n == 0 {
            return Err("The list has no records yet: type new and the fields.".into());
        }
        let record = if t.eq_ignore_ascii_case("next") { (f.record + 1).min(n - 1) } else { f.record.saturating_sub(1) };
        return Ok((Form { record, ..f }, format!("Record {} of {n}.", record + 1)));
    }
    if t.eq_ignore_ascii_case("delete") {
        return delete(d, f);
    }
    if let Some(rest) = word("find") {
        let at = find(d, &f, rest)?;
        return Ok((Form { record: at, ..f }, format!("Record {} of {n}.", at + 1)));
    }
    if let Some(rest) = word("new") {
        return add(d, f, rest);
    }
    if f.count() == 0 {
        return Err("The list has no records yet: type new and the fields.".into());
    }
    let sets = assignments(d, &f, t)?;
    let row = f.row();
    let s = f.area.sheet;
    for (c, _) in &sets {
        if d.wb.formula_text(CellAddr::new(s, row, *c)).is_some() {
            return Err("That field is worked out by a formula: it can't be typed in the form.".into());
        }
    }
    let ops = sets.into_iter().map(|(c, i)| GridOp::set(CellAddr::new(s, row, c), i)).collect();
    d.status.clear();
    if !d.run_validated("Data Form", ops) {
        return Err(if d.status.is_empty() { "The change was refused.".into() } else { d.status.clone() });
    }
    Ok((f, format!("Record {} of {} changed.", f.record + 1, f.count())))
}

fn add(d: &mut Doc, f: Form, typed: &str) -> Result<(Form, String), String> {
    let sets = assignments(d, &f, typed)?;
    let s = f.area.sheet;
    let row = f.area.r1 + 1;
    if row >= officina_grid::addr::MAX_ROWS {
        return Err("The list reaches the last row.".into());
    }
    if (f.area.c0..=f.area.c1).any(|c| !matches!(d.wb.get(CellAddr::new(s, row, c)), Value::Empty)) {
        return Err(format!("Row {} under the list isn't empty: clear it to add a record.", row + 1));
    }
    let typed_cols: Vec<u32> = sets.iter().map(|(c, _)| *c).collect();
    let mut ops: Vec<GridOp> = sets.into_iter().map(|(c, i)| GridOp::set(CellAddr::new(s, row, c), i)).collect();
    // Formulas in the record above carry on into the new one.
    if f.count() > 0 {
        for c in (f.area.c0..=f.area.c1).filter(|c| !typed_cols.contains(c)) {
            if let Some(g) = d.wb.group_at(s, row - 1, c) {
                let text = officina_grid::formula::to_a1(&d.wb.group(g).expr, row, c, &|id| d.wb.sheet_name(id));
                ops.push(GridOp::set(CellAddr::new(s, row, c), Input::Formula(text)));
            }
        }
    }
    d.status.clear();
    if !d.run_validated("Data Form", ops) {
        return Err(if d.status.is_empty() { "The record was refused.".into() } else { d.status.clone() });
    }
    let area = Rect { r1: row, ..f.area };
    let n = area.r1 - area.r0;
    Ok((Form { area, record: n - 1 }, format!("Record {n} of {n} added.")))
}

/// The next record after the shown one (wrapping) matching every
/// condition of `criteria` (`Region = East, Qty > 10`): numbers compare as
/// numbers, text without case, and `=` on text matches its beginning, as
/// Excel's form does.
pub fn find(d: &Doc, f: &Form, criteria: &str) -> Result<u32, String> {
    let mut conds = Vec::new();
    for part in criteria.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let ops = ["<>", ">=", "<=", "=", ">", "<"];
        let (i, op) = ops.iter().filter_map(|op| part.find(op).map(|i| (i, *op))).min_by_key(|&(i, op)| (i, std::cmp::Reverse(op.len()))).ok_or_else(|| format!("{part:?}: type a field, a comparison and a value, such as Qty > 10."))?;
        let c = field(d, f, &part[..i])?;
        conds.push((c, op, part[i + op.len()..].trim().to_string()));
    }
    if conds.is_empty() {
        return Err("Type what to find, such as find Region = East.".into());
    }
    let n = f.count();
    let s = f.area.sheet;
    let holds = |rec: u32| conds.iter().all(|(c, op, want)| {
        let v = d.wb.get(CellAddr::new(s, f.area.r0 + 1 + rec, *c));
        match (&v, want.parse::<f64>()) {
            (Value::Num(x), Ok(w)) => match *op {
                "=" => *x == w,
                "<>" => *x != w,
                ">" => *x > w,
                "<" => *x < w,
                ">=" => *x >= w,
                _ => *x <= w,
            },
            _ => {
                let have = d.wb.formatted(CellAddr::new(s, f.area.r0 + 1 + rec, *c)).text.to_lowercase();
                let w = want.to_lowercase();
                match *op {
                    "=" => have.starts_with(&w),
                    "<>" => !have.starts_with(&w),
                    ">" => have > w,
                    "<" => have < w,
                    ">=" => have >= w,
                    _ => have <= w,
                }
            }
        }
    });
    (1..=n).map(|k| (f.record + k) % n.max(1)).find(|&rec| holds(rec)).ok_or_else(|| "No record matches.".into())
}

/// Deletes the shown record: its cells in the list go and those below
/// move up. The form after.
pub fn delete(d: &mut Doc, f: Form) -> Result<(Form, String), String> {
    if f.count() == 0 {
        return Err("The list has no records.".into());
    }
    let row = f.row();
    let sel = CellRange { r0: row, c0: f.area.c0, r1: row, c1: f.area.c1 };
    crate::data::shift_cells(d, sel, crate::data::DataCmd::DeleteCellsUp)?;
    if crate::sheet_protection::refused(&d.status) {
        return Err(d.status.clone());
    }
    let area = Rect { r1: f.area.r1 - 1, ..f.area };
    let n = area.r1 - area.r0;
    let record = f.record.min(n.saturating_sub(1));
    Ok((Form { area, record }, format!("Record deleted; {n} left.")))
}

// ---------- Quick Analysis totals ----------

/// Quick totals (`sum below`, `average right`, `count`, `percent`,
/// `running`): a row of formulas under the selection (or a column to its
/// right) totalling each column (or row). One step.
pub fn quick_totals(d: &mut Doc, sel: CellRange, typed: &str) -> Result<String, String> {
    let words: Vec<String> = typed.split(|c: char| c == ',' || c.is_whitespace()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect();
    let mut kind = None;
    let mut right = false;
    for w in &words {
        match w.as_str() {
            "sum" => kind = Some("SUM"),
            "average" | "avg" => kind = Some("AVERAGE"),
            "count" => kind = Some("COUNT"),
            "percent" | "%" | "pct" => kind = Some("%"),
            "running" => kind = Some("RUN"),
            "below" | "rows" => right = false,
            "right" | "columns" => right = true,
            _ => return Err(format!("{w:?}: type sum, average, count, percent or running, and below or right.")),
        }
    }
    let kind = kind.ok_or("Type sum, average, count, percent or running, and below or right.")?;
    let s = d.sheet;
    let (lines, len) = if right { (sel.r1 - sel.r0 + 1, sel.c1 - sel.c0 + 1) } else { (sel.c1 - sel.c0 + 1, sel.r1 - sel.r0 + 1) };
    if len < 1 || lines > 16_384 {
        return Err("Select the numbers to total, up to 16,384 rows or columns.".into());
    }
    let out = if right { sel.c1 + 1 } else { sel.r1 + 1 };
    if out >= if right { officina_grid::addr::MAX_COLS } else { officina_grid::addr::MAX_ROWS } {
        return Err("There is no room past the selection.".into());
    }
    let abs = |r: u32, c: u32| format!("${}${}", officina_grid::addr::col_name(c), r + 1);
    let all = format!("{}:{}", abs(sel.r0, sel.c0), abs(sel.r1, sel.c1));
    let mut ops = Vec::new();
    let mut percent = Vec::new();
    for k in 0..lines {
        let (cell, line) = if right {
            let r = sel.r0 + k;
            ((r, out), format!("{}:{}", a1(r, sel.c0), a1(r, sel.c1)))
        } else {
            let c = sel.c0 + k;
            ((out, c), format!("{}:{}", a1(sel.r0, c), a1(sel.r1, c)))
        };
        let at = CellAddr::new(s, cell.0, cell.1);
        if !matches!(d.wb.get(at), Value::Empty) || d.wb.formula_text(at).is_some() {
            return Err(format!("{} isn't empty: the totals go {} the selection.", a1(cell.0, cell.1), if right { "right of" } else { "under" }));
        }
        let f = match kind {
            "%" => {
                percent.push(at);
                format!("=SUM({line})/SUM({all})")
            }
            // Each line's total so far: down a column for totals at the right.
            "RUN" => {
                let last = if right { a1(sel.r0 + k, sel.c1) } else { a1(sel.r1, sel.c0 + k) };
                let first = format!("{}:{last}", abs(sel.r0, sel.c0));
                format!("=SUM({first})")
            }
            k => format!("={k}({line})"),
        };
        ops.push(GridOp::set(at, Input::Formula(f)));
    }
    for at in percent {
        let mut st = d.wb.style(at).clone();
        st.num_fmt = "0.0%".into();
        ops.push(GridOp::style(Rect { sheet: s, r0: at.row, c0: at.col, r1: at.row, c1: at.col }, st));
    }
    let n = ops.len().min(lines as usize);
    d.status.clear();
    if !d.run_validated("Quick Totals", ops) {
        return Err(if d.status.is_empty() { "The totals were refused.".into() } else { d.status.clone() });
    }
    let what = match kind {
        "SUM" => "sums",
        "AVERAGE" => "averages",
        "COUNT" => "counts",
        "%" => "percentages of the total",
        _ => "running totals",
    };
    Ok(format!("{n} {what} {}.", if right { "to the right" } else { "below" }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> Doc {
        let mut d = Doc::from_csv("f.csv", b"Region,Qty,Price\nEast,5,2\nWest,12,3\nEastern,20,1\n").unwrap().0;
        d.wb.recalc();
        d
    }

    fn text(d: &Doc, r: u32, c: u32) -> String {
        d.wb.formatted(CellAddr::new(0, r, c)).text
    }

    #[test]
    fn the_form_steps_edits_finds_adds_and_deletes() {
        let mut d = doc();
        let f = open(&d, (2, 1)).unwrap();
        assert_eq!((f.record, f.count()), (1, 3));
        assert_eq!(fields(&d, &f)[0], ("Region".to_string(), "West".to_string(), false));
        let (f, m) = run(&mut d, f, "Qty = 13; price = 4").unwrap();
        assert_eq!(m, "Record 2 of 3 changed.");
        assert_eq!((text(&d, 2, 1), text(&d, 2, 2)), ("13".into(), "4".into()));
        // Find: text from its start, numbers by value, wrapping.
        let (f, m) = run(&mut d, f, "find Region = east, Qty > 10").unwrap();
        assert_eq!((f.record, m.as_str()), (2, "Record 3 of 3."));
        let (f, _) = run(&mut d, f, "find Region = East").unwrap();
        assert_eq!(f.record, 0);
        assert!(run(&mut d, f, "find Region = North").is_err());
        assert!(run(&mut d, f, "Colour = red").unwrap_err().contains("No field"));
        // New goes under the list.
        let (f, m) = run(&mut d, f, "new Region = South; Qty = 1").unwrap();
        assert_eq!((m.as_str(), f.count(), f.record), ("Record 4 of 4 added.", 4, 3));
        assert_eq!(text(&d, 4, 0), "South");
        // Delete moves the rest up.
        let f = Form { record: 0, ..f };
        let (f, m) = delete(&mut d, f).unwrap();
        assert_eq!((m.as_str(), f.count()), ("Record deleted; 3 left.", 3));
        assert_eq!(text(&d, 1, 0), "West");
        assert!(d.undo());
        assert_eq!(text(&d, 1, 0), "East");
        assert!(open(&Doc::from_csv("e.csv", b"").unwrap().0, (5, 5)).is_err());
    }

    #[test]
    fn the_bar_steps_records_and_follows_the_cursor() {
        let mut d = doc();
        assert_eq!(crate::whatif::form_start(&d, (1, 0)), "Region = East; Qty = 5; Price = 2");
        let (at, text, say) = crate::whatif::form_next(&mut d, (1, 2), "next").unwrap();
        assert_eq!((at, text.as_str(), say.as_str()), ((2, 2), "Region = West; Qty = 12; Price = 3", "Record 2 of 3."));
        let (at, _, _) = crate::whatif::form_next(&mut d, (3, 0), "next").unwrap();
        assert_eq!(at, (3, 0), "Next stops at the last record");
        let (at, _, _) = crate::whatif::form_next(&mut d, (1, 0), "PREV").unwrap();
        assert_eq!(at, (1, 0));
        let (at, text, _) = crate::whatif::form_next(&mut d, (1, 0), "delete").unwrap();
        assert_eq!((at, text.as_str()), ((1, 0), "Region = West; Qty = 12; Price = 3"));
        assert!(crate::whatif::form_next(&mut d, (1, 0), "fİnd Region = W").is_err());
    }

    #[test]
    fn quick_totals_go_below_or_right() {
        let mut d = doc();
        let sel = CellRange { r0: 1, c0: 1, r1: 3, c1: 2 };
        assert_eq!(quick_totals(&mut d, sel, "sum below").unwrap(), "2 sums below.");
        assert_eq!((text(&d, 4, 1), text(&d, 4, 2)), ("37".into(), "6".into()));
        assert_eq!(d.wb.formula_text(CellAddr::new(0, 4, 1)).unwrap(), "=SUM(B2:B4)");
        assert!(quick_totals(&mut d, sel, "average below").unwrap_err().contains("isn't empty"));
        assert!(d.undo());
        assert_eq!(quick_totals(&mut d, sel, "percent right").unwrap(), "3 percentages of the total to the right.");
        assert_eq!(text(&d, 1, 3), "16.3%");
        assert!(d.undo());
        assert_eq!(quick_totals(&mut d, sel, "running below").unwrap(), "2 running totals below.");
        assert_eq!((text(&d, 4, 1), text(&d, 4, 2)), ("37".into(), "43".into()));
        assert!(d.undo());
        assert_eq!(quick_totals(&mut d, sel, "running right").unwrap(), "3 running totals to the right.");
        assert_eq!((text(&d, 1, 3), text(&d, 2, 3), text(&d, 3, 3)), ("7".into(), "22".into(), "43".into()));
        assert!(quick_totals(&mut d, sel, "median").is_err());
    }
}
