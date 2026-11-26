//! Review-tab tools (docs/spec/officina.md §16.146): Allow Edit Ranges,
//! Ignore Error and Reset Ignored Errors for Error Checking, Convert to
//! Number, and the Document Inspector. Each change is one undo step, except
//! removing macros, which (as Remove Macros) is not undone.

use officina_grid::addr::{a1, CellAddr, Rect};
use officina_grid::book::Input;
use officina_grid::ops::GridOp;
use officina_grid::sheetareas::{EditRange, Ignored, MAX_AREAS, MAX_EDIT_RANGES, MAX_IGNORED};
use officina_grid::shift::{shift_op, Axis};
use officina_grid::{Value, Workbook};
use pergola::widgets::CellRange;

use crate::doc::Doc;

fn refused(d: &Doc) -> Result<(), String> {
    if crate::sheet_protection::refused(&d.status) { Err(d.status.clone()) } else { Ok(()) }
}

fn area_text(&(r0, c0, r1, c1): &(u32, u32, u32, u32)) -> String {
    if (r0, c0) == (r1, c1) { a1(r0, c0) } else { format!("{}:{}", a1(r0, c0), a1(r1, c1)) }
}

// ---------- Allow Edit Ranges ----------

/// The bar's starting text: a new range over the selection, or the
/// existing ones to read.
pub fn ranges_start(d: &Doc, sel: CellRange) -> String {
    let list = d.wb.sheet(d.sheet).edit_ranges();
    let n = (1..).find(|i| !list.iter().any(|e| e.title.eq_ignore_ascii_case(&format!("Range{i}")))).unwrap_or(1);
    format!("add Range{n}: {}", area_text(&(sel.r0, sel.c0, sel.r1, sel.c1)))
}

/// What the sheet's edit ranges are, in a line.
pub fn ranges_summary(wb: &Workbook, s: u16) -> String {
    let list = wb.sheet(s).edit_ranges();
    if list.is_empty() {
        return "No edit ranges on this sheet.".into();
    }
    let each: Vec<String> = list.iter().map(|e| {
        let areas: Vec<String> = e.areas.iter().map(area_text).collect();
        format!("{} ({}{})", e.title, areas.join(", "), if e.password.is_some() { ", password" } else { "" })
    }).collect();
    format!("Edit ranges: {}.", each.join("; "))
}

/// Allow Edit Ranges (`add Inputs: B2:B9, D1`, `delete Inputs`, `list`):
/// cells that can be changed while the sheet is protected. Changing them
/// is refused while it is.
pub fn edit_ranges(d: &mut Doc, typed: &str) -> Result<String, String> {
    let s = d.sheet;
    let t = typed.trim();
    let (verb, rest) = t.split_once(char::is_whitespace).unwrap_or((t, ""));
    let mut list = d.wb.sheet(s).edit_ranges().to_vec();
    let msg = match verb.to_lowercase().as_str() {
        "list" | "" => return Err(ranges_summary(&d.wb, s)),
        "add" => {
            let (title, cells) = rest.split_once(':').ok_or("Type add, a title, a colon and the cells, such as: add Inputs: B2:B9, D1")?;
            let title = title.trim();
            if title.is_empty() || title.len() > officina_grid::sheetareas::MAX_TITLE {
                return Err("Give the range a title of up to 255 characters.".into());
            }
            let mut areas = Vec::new();
            for part in cells.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                let r = crate::whatif::parse_area(&d.wb, s, part)?;
                if r.sheet != s {
                    return Err(format!("{part} is on another sheet: an edit range is on its own sheet."));
                }
                areas.push((r.r0, r.c0, r.r1, r.c1));
            }
            if areas.is_empty() {
                return Err("Type the cells after the colon, such as B2:B9, D1.".into());
            }
            if areas.len() > MAX_AREAS {
                return Err(format!("An edit range covers at most {MAX_AREAS} areas."));
            }
            let was = list.iter().position(|e| e.title.eq_ignore_ascii_case(title));
            let er = EditRange { title: title.to_string(), areas, password: None };
            let shown = format!("{title} ({})", er.areas.iter().map(area_text).collect::<Vec<_>>().join(", "));
            match was {
                Some(i) => list[i] = er,
                None if list.len() >= MAX_EDIT_RANGES => return Err(format!("A sheet keeps at most {MAX_EDIT_RANGES} edit ranges.")),
                None => list.push(er),
            }
            if was.is_some() { format!("{shown} changed.") } else { format!("{shown} can be edited while the sheet is protected.") }
        }
        "delete" | "remove" => {
            let title = rest.trim();
            let i = list.iter().position(|e| e.title.eq_ignore_ascii_case(title)).ok_or_else(|| format!("No edit range is called {title:?}."))?;
            let gone = list.remove(i);
            format!("{} deleted.", gone.title)
        }
        _ => return Err("Type add TITLE: cells, delete TITLE, or list.".into()),
    };
    d.status.clear();
    d.run("Allow Edit Ranges", vec![GridOp::EditRanges { sheet: s, list }]);
    refused(d)?;
    Ok(msg)
}

// ---------- Ignore Error ----------

/// Ignore Error: Error Checking leaves the cursor's cell alone for the
/// rule it breaks.
pub fn ignore_error(d: &mut Doc, cursor: (u32, u32)) -> Result<String, String> {
    let s = d.sheet;
    let at = CellAddr::new(s, cursor.0, cursor.1);
    let (rule, _) = crate::audit::problem_at(&d.wb, at).ok_or_else(|| format!("Error Checking finds nothing at {}.", a1(cursor.0, cursor.1)))?;
    let mut list = d.wb.sheet(s).ignored_errors().to_vec();
    if list.len() >= MAX_IGNORED {
        return Err(format!("A sheet keeps at most {MAX_IGNORED} ignored errors: Reset Ignored Errors first."));
    }
    list.push(Ignored { r0: cursor.0, c0: cursor.1, r1: cursor.0, c1: cursor.1, rules: rule });
    d.status.clear();
    d.run("Ignore Error", vec![GridOp::IgnoredErrors { sheet: s, list }]);
    refused(d)?;
    Ok(format!("Error Checking will leave {} alone.", a1(cursor.0, cursor.1)))
}

/// Reset Ignored Errors: every sheet's ignored errors forgotten.
pub fn reset_ignored(d: &mut Doc) -> Result<String, String> {
    let ops: Vec<GridOp> = (0..d.wb.sheet_count() as u16)
        .filter(|&s| !d.wb.sheet(s).ignored_errors().is_empty())
        .map(|s| GridOp::IgnoredErrors { sheet: s, list: Vec::new() })
        .collect();
    if ops.is_empty() {
        return Err("No errors are being ignored.".into());
    }
    d.status.clear();
    d.run("Reset Ignored Errors", ops);
    refused(d)?;
    Ok("Error Checking shows every error again.".into())
}

/// Convert to Number: the selection's numbers stored as text become numbers.
pub fn convert_to_number(d: &mut Doc, sel: CellRange) -> Result<String, String> {
    let s = d.sheet;
    let rect = Rect { sheet: s, r0: sel.r0, c0: sel.c0, r1: sel.r1, c1: sel.c1 };
    if rect.cells() > 1_000_000 {
        return Err("Select fewer than 1,000,000 cells.".into());
    }
    let mut sets = Vec::new();
    let mut texts = Vec::new();
    d.wb.each_cell(&rect, &mut |r, c, cell| {
        if matches!(cell, officina_grid::column::Cell::Str(_)) {
            texts.push((r, c));
        }
    });
    for (r, c) in texts {
        let at = CellAddr::new(s, r, c);
        if d.wb.formula_text(at).is_some() {
            continue;
        }
        if let Value::Str(t) = d.wb.get(at) {
            if let Ok(v) = t.trim().parse::<f64>() {
                if v.is_finite() && t.chars().any(|ch| ch.is_ascii_digit()) {
                    sets.push(GridOp::set(at, Input::Value(Value::Num(v))));
                }
            }
        }
    }
    if sets.is_empty() {
        return Err("No numbers stored as text in the selection.".into());
    }
    let n = sets.len();
    d.status.clear();
    if !d.run_validated("Convert to Number", sets) {
        return Err(if d.status.is_empty() { "Convert to Number was refused.".into() } else { d.status.clone() });
    }
    refused(d)?;
    Ok(format!("{n} {} converted to numbers.", if n == 1 { "cell" } else { "cells" }))
}

// ---------- Document Inspector ----------

/// What the Document Inspector looks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finding {
    Comments,
    HiddenRowsCols,
    HiddenSheets,
    Scenarios,
    Filters,
    Macros,
    PivotTables,
    Watches,
    Properties,
}

/// One line of the inspector: what, how many, what it says, and whether
/// Remove All can take it away.
#[derive(Debug, Clone, PartialEq)]
pub struct Inspected {
    pub what: Finding,
    pub title: &'static str,
    pub count: usize,
    pub detail: String,
    pub removable: bool,
}

/// Hidden rows (and columns) of sheet `s` within what is used, as runs.
fn hidden_runs(wb: &Workbook, s: u16, axis: Axis) -> Vec<(u32, u32)> {
    let sh = wb.sheet(s);
    let (rows, cols) = wb.extent(s);
    let hidden = |i: u32| match axis {
        Axis::Rows => sh.row_hidden(i) || sh.row_sizes.size(i) == 0.0,
        Axis::Cols => sh.col_sizes.size(i) == 0.0,
    };
    let end = if axis == Axis::Rows { rows } else { cols };
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for i in 0..end.min(1_048_576) {
        if hidden(i) {
            match runs.last_mut() {
                Some(last) if last.1 + 1 == i => last.1 = i,
                _ => runs.push((i, i)),
            }
        }
    }
    runs
}

/// Inspect Document: what the workbook holds that a reader may not see.
pub fn inspect(d: &Doc) -> Vec<Inspected> {
    let wb = &d.wb;
    let sheets = 0..wb.sheet_count() as u16;
    let comments: usize = sheets.clone().map(|s| wb.comments(s).keys().count()).sum();
    let (mut rows, mut cols) = (0u64, 0u64);
    for s in sheets.clone() {
        rows += hidden_runs(wb, s, Axis::Rows).iter().map(|&(a, b)| u64::from(b - a + 1)).sum::<u64>();
        cols += hidden_runs(wb, s, Axis::Cols).iter().map(|&(a, b)| u64::from(b - a + 1)).sum::<u64>();
    }
    let hidden_sheets: Vec<String> = sheets.clone().filter(|&s| wb.sheet(s).sheet_hidden).map(|s| wb.sheet_name(s)).collect();
    let scenarios: usize = sheets.clone().map(|s| wb.sheet(s).scenarios().len()).sum();
    let filters = sheets.clone().filter(|&s| wb.filter_state(s).filter.is_some()).count();
    let watches: usize = sheets.clone().map(|s| wb.sheet(s).watches().len()).sum();
    let pivots = wb.pivots().len();
    let macros = wb.macros().is_some();
    let n = |k: u64, one: &str, many: &str| format!("{k} {}", if k == 1 { one } else { many });
    let props: Vec<&str> = officina_grid::docprops::FIELDS.iter().enumerate().filter(|(i, _)| !wb.properties().get(*i).is_empty()).map(|(_, f)| f.0).collect();
    vec![
        Inspected { what: Finding::Comments, title: "Comments", count: comments, detail: if comments > 0 { format!("{} found.", n(comments as u64, "comment", "comments")) } else { "None found.".into() }, removable: true },
        Inspected { what: Finding::Properties, title: "Document properties", count: props.len(), detail: if props.is_empty() { "None found.".into() } else { format!("The file names its {}.", props.join(", ")) }, removable: true },
        Inspected { what: Finding::HiddenRowsCols, title: "Hidden rows and columns", count: (rows + cols) as usize, detail: if rows + cols > 0 { format!("{} and {} with data around them. Remove All deletes them and what they hold.", n(rows, "hidden row", "hidden rows"), n(cols, "hidden column", "hidden columns")) } else { "None found.".into() }, removable: true },
        Inspected { what: Finding::HiddenSheets, title: "Hidden worksheets", count: hidden_sheets.len(), detail: if hidden_sheets.is_empty() { "None found.".into() } else { format!("{}. Remove All deletes them.", hidden_sheets.join(", ")) }, removable: true },
        Inspected { what: Finding::Scenarios, title: "Defined scenarios", count: scenarios, detail: if scenarios > 0 { format!("{} found.", n(scenarios as u64, "scenario", "scenarios")) } else { "None found.".into() }, removable: true },
        Inspected { what: Finding::Filters, title: "Active filters", count: filters, detail: if filters > 0 { format!("{} filtered. Remove All takes the filters off and shows their rows.", n(filters as u64, "sheet is", "sheets are")) } else { "None found.".into() }, removable: true },
        Inspected { what: Finding::Watches, title: "Watched cells", count: watches, detail: if watches > 0 { format!("{} in the Watch Window.", n(watches as u64, "cell", "cells")) } else { "None found.".into() }, removable: true },
        Inspected { what: Finding::Macros, title: "Macros", count: usize::from(macros), detail: if macros { "Macros found (kept, never run). Remove All takes them out; it can't be undone.".into() } else { "None found.".into() }, removable: true },
        Inspected { what: Finding::PivotTables, title: "PivotTables", count: pivots, detail: if pivots > 0 { format!("{} keep copies of their source data.", n(pivots as u64, "PivotTable", "PivotTables")) } else { "None found.".into() }, removable: false },
    ]
}

/// Remove All for one finding, as one step.
pub fn remove(d: &mut Doc, what: Finding) -> Result<String, String> {
    let count = d.wb.sheet_count() as u16;
    let mut ops = Vec::new();
    match what {
        Finding::Comments => {
            for s in 0..count {
                let set: Vec<_> = d.wb.comments(s).keys().map(|&(r, c)| (r, c, None)).collect();
                if !set.is_empty() {
                    ops.push(GridOp::Comments { sheet: s, set });
                }
            }
        }
        Finding::Scenarios => {
            for s in (0..count).filter(|&s| !d.wb.sheet(s).scenarios().is_empty()) {
                ops.push(GridOp::Scenarios { sheet: s, list: Vec::new() });
            }
        }
        Finding::Watches => {
            for s in (0..count).filter(|&s| !d.wb.sheet(s).watches().is_empty()) {
                ops.push(GridOp::Watches { sheet: s, list: Vec::new() });
            }
        }
        Finding::Filters => {
            for s in (0..count).filter(|&s| d.wb.filter_state(s).filter.is_some()) {
                ops.push(GridOp::Filter(officina_grid::filter::FilterState { sheet: s, filter: None, hidden: Vec::new() }));
            }
        }
        Finding::Macros => {
            if d.wb.macros().is_none() {
                return Err("No macros to remove.".into());
            }
            d.remove_macros();
            return Ok("Macros removed. The file keeps them until you save it.".into());
        }
        Finding::Properties => {
            if !d.wb.properties().is_empty() {
                ops.push(GridOp::Properties(Default::default()));
            }
        }
        Finding::PivotTables => return Err("PivotTables stay: delete one by deleting its cells.".into()),
        Finding::HiddenSheets => return remove_hidden_sheets(d),
        Finding::HiddenRowsCols => return remove_hidden_rows_cols(d),
    }
    if ops.is_empty() {
        return Err("Nothing of that kind to remove.".into());
    }
    d.status.clear();
    d.run("Inspect Document", ops);
    refused(d)?;
    Ok("Removed.".into())
}

/// Deletes the hidden sheets, last first, as one step.
fn remove_hidden_sheets(d: &mut Doc) -> Result<String, String> {
    let hidden: Vec<u16> = (0..d.wb.sheet_count() as u16).rev().filter(|&s| d.wb.sheet(s).sheet_hidden).collect();
    if hidden.is_empty() {
        return Err("No hidden worksheets.".into());
    }
    let mut first = true;
    for &s in &hidden {
        let op = officina_grid::sheetops::delete_sheet_op(&d.wb, s).ok_or("The only sheet can't be deleted.")?;
        d.status.clear();
        if first {
            if !d.run_action("Inspect Document", vec![op]) {
                return Err(d.status.clone());
            }
            first = false;
        } else {
            d.extend(vec![op]);
        }
    }
    Ok(format!("{} deleted.", if hidden.len() == 1 { "1 hidden worksheet".to_string() } else { format!("{} hidden worksheets", hidden.len()) }))
}

/// Deletes the hidden rows and columns within what each sheet uses, last
/// first so the places found stay right, as one step.
fn remove_hidden_rows_cols(d: &mut Doc) -> Result<String, String> {
    let mut plan: Vec<(u16, Axis, u32, u32)> = Vec::new();
    for s in 0..d.wb.sheet_count() as u16 {
        for axis in [Axis::Rows, Axis::Cols] {
            for (a, b) in hidden_runs(&d.wb, s, axis).into_iter().rev() {
                plan.push((s, axis, a, b));
            }
        }
    }
    if plan.is_empty() {
        return Err("No hidden rows or columns.".into());
    }
    if plan.len() > 10_000 {
        return Err("More than 10,000 runs of hidden rows and columns: delete them a sheet at a time.".into());
    }
    let mut first = true;
    let (mut rows, mut cols) = (0u64, 0u64);
    for (s, axis, a, b) in plan {
        let n = u64::from(b - a + 1);
        let op = shift_op(&d.wb, s, axis, a, -(n as i64)).ok_or("These rows or columns can't be deleted.")?;
        d.status.clear();
        if first {
            if !d.run_action("Inspect Document", vec![op]) {
                return Err(if d.status.is_empty() { "Refused.".into() } else { d.status.clone() });
            }
            first = false;
        } else {
            d.extend(vec![op]);
        }
        if axis == Axis::Rows { rows += n } else { cols += n }
    }
    Ok(format!("Deleted {rows} hidden rows and {cols} hidden columns."))
}

#[cfg(test)]
mod tests {
    #[test]
    fn properties_bar_sets_names_clears_and_inspector_removes() {
        use super::*;
        let mut d = crate::doc::Doc::from_csv("p.csv", b"a\n").unwrap().0;
        assert_eq!(properties_start(&d), "title: ");
        assert!(properties(&mut d, "colour: red").unwrap_err().contains("isn't a property"));
        assert!(properties(&mut d, "Budget").is_err());
        properties(&mut d, "title: Budget 2027; Author: Ana Rao; keywords: plan, money").unwrap();
        assert_eq!(properties_start(&d), "title: Budget 2027; author: Ana Rao; keywords: plan, money");
        properties(&mut d, "author:").unwrap();
        assert_eq!(d.wb.properties().get(2), "");
        let found = inspect(&d);
        let line = found.iter().find(|f| f.what == Finding::Properties).unwrap();
        assert_eq!((line.count, line.detail.as_str()), (2, "The file names its title, keywords."));
        remove(&mut d, Finding::Properties).unwrap();
        assert!(d.wb.properties().is_empty());
        assert!(d.undo());
        assert_eq!(d.wb.properties().get(0), "Budget 2027");
        properties(&mut d, "clear").unwrap();
        assert!(d.wb.properties().is_empty());
    }

    #[test]
    fn mark_as_final_refuses_edits_until_turned_off() {
        use super::*;
        let mut d = crate::doc::Doc::from_csv("f.csv", b"a\n").unwrap().0;
        assert!(toggle_final(&mut d).unwrap().starts_with("Marked as final"));
        assert!(!d.type_into(0, 0, "b"));
        assert_eq!(d.status, crate::sheet_protection::FINAL_REFUSED);
        assert_eq!(d.wb.get(officina_grid::CellAddr::new(0, 0, 0)), officina_grid::Value::str("a"));
        // The properties still change, and the mark survives the file.
        properties(&mut d, "title: Report").unwrap();
        let bytes = officina_grid::native::save_to_vec(&d.wb);
        let back = crate::files::load_with("f.osheet", &bytes, None).unwrap();
        assert!(back.wb.properties().marked_final);
        assert!(back.status.contains("Marked as final"), "{}", back.status);
        // Off again: edits go through; undo puts the mark back.
        assert!(toggle_final(&mut d).unwrap().starts_with("No longer"));
        assert!(d.type_into(0, 0, "b"));
        assert!(d.undo() && d.undo());
        assert!(d.wb.properties().marked_final);
    }

    use super::*;

    fn doc(csv: &[u8]) -> Doc {
        let mut d = Doc::from_csv("c.csv", csv).unwrap().0;
        d.wb.recalc();
        d
    }

    #[test]
    fn edit_ranges_open_cells_on_a_protected_sheet() {
        let mut d = doc(b"1,2\n3,4\n");
        let sel = CellRange { r0: 0, c0: 1, r1: 1, c1: 1 };
        assert_eq!(ranges_start(&d, sel), "add Range1: B1:B2");
        assert!(edit_ranges(&mut d, "add Inputs: B1:B2, A2").unwrap().contains("can be edited"));
        assert!(edit_ranges(&mut d, "list").unwrap_err().contains("Inputs (B1:B2, A2)"));
        d.wb.sheet_mut(0).protection = Some(officina_grid::protection::SheetProtection::new(officina_grid::protection::ProtectionFormat::Excel));
        let wb = &d.wb;
        assert!(wb.can_edit_cells(Rect { sheet: 0, r0: 0, c0: 1, r1: 1, c1: 1 }));
        assert!(wb.can_edit_cells(Rect { sheet: 0, r0: 1, c0: 0, r1: 1, c1: 1 }));
        assert!(!wb.can_edit_cells(Rect { sheet: 0, r0: 0, c0: 0, r1: 0, c1: 0 }), "A1 is locked");
        assert!(!wb.can_edit_cells(Rect { sheet: 0, r0: 0, c0: 0, r1: 1, c1: 1 }), "a selection reaching A1 is refused");
        // Changing the ranges is refused while the sheet is protected.
        assert!(edit_ranges(&mut d, "delete Inputs").is_err());
        d.wb.sheet_mut(0).protection = None;
        assert_eq!(edit_ranges(&mut d, "delete Inputs").unwrap(), "Inputs deleted.");
        assert!(d.undo());
        assert_eq!(d.wb.sheet(0).edit_ranges().len(), 1);
        // A row inserted above moves them.
        let op = shift_op(&d.wb, 0, Axis::Rows, 0, 1).unwrap();
        d.run("Insert Rows", vec![op]);
        assert_eq!(d.wb.sheet(0).edit_ranges()[0].areas, [(1, 1, 2, 1), (2, 0, 2, 0)]);
        let back = officina_grid::native::load(&officina_grid::native::save_to_vec(&d.wb)).unwrap();
        assert_eq!(back.sheet(0).edit_ranges(), d.wb.sheet(0).edit_ranges());
        assert!(edit_ranges(&mut d, "add : A1").is_err());
        assert!(edit_ranges(&mut d, "add X: Nope!A1").is_err());
    }

    #[test]
    fn error_checking_rules_ignore_and_reset() {
        let mut d = doc(b"1,10\n2,20\n3,30\n4,40\n");
        // C1:C4 =A*B, but C3 adds instead: inconsistent.
        for r in 0..4u32 {
            let f = if r == 2 { format!("=A{0}+B{0}", r + 1) } else { format!("=A{0}*B{0}", r + 1) };
            d.type_into(r, 2, &f);
        }
        // D1 sums A1:A3 and leaves out A4's number.
        d.type_into(0, 3, "=SUM(A1:A3)");
        d.wb.recalc();
        let (at, why) = crate::audit::next_error(&d.wb, 0, (0, 0)).unwrap();
        assert_eq!(at, (0, 3));
        assert!(why.contains("leaves out A4"), "{why}");
        let (at, why) = crate::audit::next_error(&d.wb, 0, at).unwrap();
        assert_eq!(at, (2, 2));
        assert!(why.contains("differs from the ones above and below"), "{why}");
        assert_eq!(ignore_error(&mut d, (2, 2)).unwrap(), "Error Checking will leave C3 alone.");
        assert_eq!(crate::audit::next_error(&d.wb, 0, (2, 2)).unwrap().0, (0, 3), "C3 is passed over");
        assert!(ignore_error(&mut d, (2, 2)).is_err(), "nothing left to ignore there");
        let back = officina_grid::native::load(&officina_grid::native::save_to_vec(&d.wb)).unwrap();
        assert_eq!(back.sheet(0).ignored_errors(), d.wb.sheet(0).ignored_errors());
        assert_eq!(reset_ignored(&mut d).unwrap(), "Error Checking shows every error again.");
        assert_eq!(crate::audit::next_error(&d.wb, 0, (0, 3)).unwrap().0, (2, 2));
        // An unlocked formula.
        let mut u = doc(b"1\n");
        u.type_into(0, 1, "=A1");
        u.wb.recalc();
        assert!(crate::audit::next_error(&u.wb, 0, (0, 0)).is_none());
        let style = officina_grid::style::Style { unlocked: true, ..Default::default() };
        u.run("Format", vec![GridOp::style(Rect { sheet: 0, r0: 0, c0: 1, r1: 0, c1: 1 }, style)]);
        assert!(crate::audit::next_error(&u.wb, 0, (0, 0)).unwrap().1.contains("unlocked"));
    }

    #[test]
    fn convert_to_number_fixes_numbers_stored_as_text() {
        let mut d = doc(b"x\n");
        for (r, t) in [(0, "42"), (1, " 3.5"), (2, "word")] {
            d.wb.set_value(CellAddr::new(0, r, 0), Value::str(t));
        }
        let sel = CellRange { r0: 0, c0: 0, r1: 2, c1: 0 };
        assert_eq!(convert_to_number(&mut d, sel).unwrap(), "2 cells converted to numbers.");
        assert_eq!(d.wb.get(CellAddr::new(0, 0, 0)), Value::Num(42.0));
        assert_eq!(d.wb.get(CellAddr::new(0, 1, 0)), Value::Num(3.5));
        assert_eq!(d.wb.get(CellAddr::new(0, 2, 0)), Value::str("word"));
        assert!(convert_to_number(&mut d, sel).is_err());
    }

    #[test]
    fn the_inspector_finds_and_removes() {
        let mut d = doc(b"a,b,c\n1,2,3\n4,5,6\n7,8,9\n");
        d.wb.add_sheet("Secret").unwrap();
        d.run("Hide", vec![GridOp::HideSheet { sheet: 1, hidden: true }]);
        d.wb.sheet_mut(0).row_sizes.set(2, 0.0);
        d.wb.sheet_mut(0).col_sizes.set(1, 0.0);
        let found = inspect(&d);
        let get = |w: Finding| found.iter().find(|i| i.what == w).unwrap().clone();
        assert_eq!(get(Finding::HiddenSheets).count, 1);
        assert!(get(Finding::HiddenSheets).detail.starts_with("Secret."));
        assert_eq!(get(Finding::HiddenRowsCols).count, 2);
        assert_eq!(get(Finding::Comments).count, 0);
        assert!(remove(&mut d, Finding::Comments).is_err());
        assert_eq!(remove(&mut d, Finding::HiddenSheets).unwrap(), "1 hidden worksheet deleted.");
        assert_eq!(d.wb.sheet_count(), 1);
        assert_eq!(remove(&mut d, Finding::HiddenRowsCols).unwrap(), "Deleted 1 hidden rows and 1 hidden columns.");
        // Row 3 (4,5,6) and column B went; the rest closed up.
        assert_eq!(d.wb.get(CellAddr::new(0, 2, 0)), Value::Num(7.0));
        assert_eq!(d.wb.get(CellAddr::new(0, 0, 1)), Value::str("c"));
        assert!(d.undo());
        assert_eq!(d.wb.get(CellAddr::new(0, 2, 0)), Value::Num(4.0));
        assert_eq!(d.wb.get(CellAddr::new(0, 0, 1)), Value::str("b"));
        assert!(d.undo());
        assert_eq!(d.wb.sheet_count(), 2);
    }
}

/// Mark as Final: on, the workbook refuses edits (Excel also sets the
/// content status to Final); off, it takes them again. One step either way.
pub fn toggle_final(d: &mut Doc) -> Result<String, String> {
    let mut p = d.wb.properties().clone();
    p.marked_final = !p.marked_final;
    let on = p.marked_final;
    d.status.clear();
    d.run("Mark as Final", vec![GridOp::Properties(p)]);
    refused(d)?;
    Ok(if on {
        "Marked as final: the workbook is read only until Mark as Final is turned off.".into()
    } else {
        "No longer marked as final: the workbook can be edited.".into()
    })
}

/// The Properties bar's text: each property set, as `title: Budget; author: Ana`.
pub fn properties_start(d: &Doc) -> String {
    let p = d.wb.properties();
    let set: Vec<String> = officina_grid::docprops::FIELDS.iter().enumerate().filter(|(i, _)| !p.get(*i).is_empty()).map(|(i, f)| format!("{}: {}", f.0, p.get(i).replace('\n', " ").replace(';', ","))).collect();
    if set.is_empty() { "title: ".into() } else { set.join("; ") }
}

/// Properties…: `title: Budget; author: Ana Rao` sets the properties it
/// names (`subject:` alone clears one; `clear` clears them all). One step.
pub fn properties(d: &mut Doc, typed: &str) -> Result<String, String> {
    use officina_grid::docprops::{DocProps, FIELDS};
    let mut p = d.wb.properties().clone();
    let t = typed.trim();
    if t.eq_ignore_ascii_case("clear") || t.eq_ignore_ascii_case("none") {
        p = DocProps::default();
    } else {
        let mut named = 0;
        for part in t.split(';').map(str::trim).filter(|x| !x.is_empty()) {
            let (k, v) = part.split_once(':').ok_or_else(|| format!("{part:?}: type a property, a colon and its value, such as title: Budget"))?;
            let i = DocProps::index(k).ok_or_else(|| format!("{:?} isn't a property: use {}.", k.trim(), FIELDS.iter().map(|f| f.0).collect::<Vec<_>>().join(", ")))?;
            p.set(i, v);
            named += 1;
        }
        if named == 0 {
            return Err("Type a property, a colon and its value, such as title: Budget".into());
        }
    }
    if &p == d.wb.properties() {
        return Ok("The properties are already so.".into());
    }
    d.status.clear();
    d.run("Properties", vec![GridOp::Properties(p)]);
    refused(d)?;
    let set = FIELDS.iter().enumerate().filter(|(i, _)| !d.wb.properties().get(*i).is_empty()).count();
    Ok(if set == 0 { "The document has no properties now.".into() } else { format!("Properties set: {}.", properties_start(d)) })
}
