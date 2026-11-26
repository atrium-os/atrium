//! More of Excel's Formulas tab (docs/spec/officina.md §16.145): Calculation
//! Options (automatic or manual), the Watch Window, Create Names from
//! Selection, Apply Names, Auto Outline and Workbook Statistics. Each change
//! is one undo step.

use std::sync::Arc;

use officina_grid::addr::{a1, col_name, CellAddr, Rect, MAX_COLS, MAX_ROWS};
use officina_grid::book::Input;
use officina_grid::formula::ast::{CellRef, Coord, Expr, RangeRef};
use officina_grid::ops::GridOp;
use officina_grid::outline::{Group, Outline};
use officina_grid::shift::Axis;
use officina_grid::watch::MAX_WATCHES;
use officina_grid::Workbook;
use pergola::widgets::CellRange;

use crate::doc::Doc;

/// The most formula cells Apply Names and Auto Outline look at.
pub const MAX_CELLS: u64 = 100_000;

pub(crate) fn sheet_ref(wb: &Workbook, s: u16) -> String {
    let name = wb.sheet_name(s);
    if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') && !name.starts_with(|c: char| c.is_ascii_digit()) {
        name
    } else {
        format!("'{}'", name.replace('\'', "''"))
    }
}

fn refused(d: &Doc) -> Result<(), String> {
    if crate::sheet_protection::refused(&d.status) { Err(d.status.clone()) } else { Ok(()) }
}

// ---------- Calculation Options ----------

/// Switches between automatic and manual calculation. Back to automatic,
/// everything waiting is worked out at once.
pub fn toggle_manual(d: &mut Doc) -> Result<String, String> {
    let manual = !d.wb.manual_calc();
    d.status.clear();
    d.run("Calculation Options", vec![GridOp::Calculation(manual)]);
    refused(d)?;
    Ok(if manual {
        "Calculation is manual: edits wait for Calculate Now (F9).".into()
    } else {
        "Calculation is automatic.".into()
    })
}

// ---------- Watch Window ----------

/// A row of the Watch Window.
#[derive(Debug, Clone, PartialEq)]
pub struct WatchRow {
    pub sheet: u16,
    pub row: u32,
    pub col: u32,
    /// `Sheet!A1`.
    pub place: String,
    /// The value as the cell shows it.
    pub value: String,
    /// The formula, if any.
    pub formula: Option<String>,
}

/// Every watched cell, sheet by sheet, each in the order it was added.
pub fn watch_rows(wb: &Workbook) -> Vec<WatchRow> {
    let mut out = Vec::new();
    for s in 0..wb.sheet_count() as u16 {
        for &(row, col) in wb.sheet(s).watches() {
            let at = CellAddr::new(s, row, col);
            out.push(WatchRow { sheet: s, row, col, place: format!("{}!{}", wb.sheet_name(s), a1(row, col)), value: wb.formatted(at).text, formula: wb.formula_text(at) });
        }
    }
    out
}

/// Add Watch: the selection's cells (those with something in them when it
/// is more than one cell, as Excel does) join the Watch Window.
pub fn add_watches(d: &mut Doc, sel: CellRange) -> Result<String, String> {
    let s = d.sheet;
    let mut list = d.wb.sheet(s).watches().to_vec();
    let one = sel.r0 == sel.r1 && sel.c0 == sel.c1;
    let cells = u64::from(sel.r1 - sel.r0 + 1) * u64::from(sel.c1 - sel.c0 + 1);
    if cells > 100_000 {
        return Err("Select fewer than 100,000 cells to watch.".into());
    }
    let mut added = 0;
    for r in sel.r0..=sel.r1 {
        for c in sel.c0..=sel.c1 {
            let at = CellAddr::new(s, r, c);
            if list.contains(&(r, c)) || (!one && d.wb.formula_text(at).is_none() && matches!(d.wb.get(at), officina_grid::Value::Empty)) {
                continue;
            }
            if list.len() >= MAX_WATCHES {
                return Err(format!("A sheet watches at most {MAX_WATCHES} cells."));
            }
            list.push((r, c));
            added += 1;
        }
    }
    if added == 0 {
        return Err("Those cells are watched already, or empty.".into());
    }
    d.status.clear();
    d.run("Add Watch", vec![GridOp::Watches { sheet: s, list }]);
    refused(d)?;
    Ok(format!("Watching {added} more {}.", if added == 1 { "cell" } else { "cells" }))
}

/// Removes row `i` of the Watch Window.
pub fn delete_watch(d: &mut Doc, i: usize) -> Result<String, String> {
    let row = watch_rows(&d.wb).into_iter().nth(i).ok_or("That watch is gone.")?;
    let list: Vec<(u32, u32)> = d.wb.sheet(row.sheet).watches().iter().copied().filter(|&at| at != (row.row, row.col)).collect();
    d.status.clear();
    d.run("Delete Watch", vec![GridOp::Watches { sheet: row.sheet, list }]);
    refused(d)?;
    Ok(format!("{} is no longer watched.", row.place))
}

/// What the Watch Window's buttons ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchCmd {
    /// Add the selection's cells.
    Add,
    /// Go to row `.0`'s cell.
    Go(usize),
    Delete(usize),
    Close,
}

// ---------- Create Names from Selection ----------

/// Which edges of the selection hold the labels.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Edges {
    pub top: bool,
    pub left: bool,
    pub bottom: bool,
    pub right: bool,
}

/// The bar's starting text: Excel's guess, the top row and/or left column
/// when they hold text.
pub fn create_start(d: &Doc, sel: CellRange) -> String {
    let text = |r: u32, c: u32| matches!(d.wb.get(CellAddr::new(d.sheet, r, c)), officina_grid::Value::Str(_));
    let top = sel.r1 > sel.r0 && (sel.c0..=sel.c1).any(|c| text(sel.r0, c));
    let left = sel.c1 > sel.c0 && (sel.r0 + u32::from(top)..=sel.r1).all(|r| text(r, sel.c0));
    match (top, left) {
        (true, true) => "top, left".into(),
        (false, true) => "left".into(),
        _ => "top".into(),
    }
}

/// A label made into a name: spaces and other characters a name can't
/// hold become `_`; `_` goes first when it would start with a digit, and
/// last when it would spell a cell. None when nothing is left.
pub fn name_from_label(label: &str) -> Option<String> {
    let t = label.trim();
    if t.is_empty() {
        return None;
    }
    let mut n: String = t.chars().map(|c| if c.is_alphanumeric() || c == '_' || c == '.' { c } else { '_' }).collect();
    if !n.starts_with(|c: char| c.is_alphabetic() || c == '_') {
        n.insert(0, '_');
    } else if !officina_grid::names::valid_name(&n) {
        // Q1, R2C3: a cell's spelling, so `_` after, as Excel does.
        n.push('_');
    }
    n.truncate(officina_grid::names::MAX_NAME_LEN);
    officina_grid::names::valid_name(&n).then_some(n)
}

/// Create from Selection (`top, left`, `bottom`, `right`): a workbook name
/// for each row or column of the selection, from its label, referring to
/// the rest of it. A name already there gets the new place, as Excel's
/// "replace" does. One step.
pub fn create_names(d: &mut Doc, sel: CellRange, typed: &str) -> Result<String, String> {
    let mut e = Edges::default();
    for w in typed.split(|c: char| c == ',' || c.is_whitespace()).filter(|w| !w.is_empty()) {
        match w.to_lowercase().as_str() {
            "top" => e.top = true,
            "left" => e.left = true,
            "bottom" => e.bottom = true,
            "right" => e.right = true,
            _ => return Err(format!("{w:?}: type top, left, bottom or right.")),
        }
    }
    if e == Edges::default() {
        return Err("Type where the labels are: top, left, bottom or right.".into());
    }
    if (e.top && e.bottom && sel.r1 - sel.r0 < 2) || (e.left && e.right && sel.c1 - sel.c0 < 2) || ((e.top || e.bottom) && sel.r0 == sel.r1) || ((e.left || e.right) && sel.c0 == sel.c1) {
        return Err("Select the labels and the cells they name together.".into());
    }
    let s = d.sheet;
    // The cells the names cover: inside the label rows and columns.
    let (dr0, dr1) = (sel.r0 + u32::from(e.top), sel.r1 - u32::from(e.bottom));
    let (dc0, dc1) = (sel.c0 + u32::from(e.left), sel.c1 - u32::from(e.right));
    let label = |r: u32, c: u32| d.wb.formatted(CellAddr::new(s, r, c)).text;
    let place = |r0: u32, c0: u32, r1: u32, c1: u32| {
        let at = |r: u32, c: u32| format!("${}${}", col_name(c), r + 1);
        let cells = if (r0, c0) == (r1, c1) { at(r0, c0) } else { format!("{}:{}", at(r0, c0), at(r1, c1)) };
        format!("={}!{cells}", sheet_ref(&d.wb, s))
    };
    let mut want: Vec<(String, String)> = Vec::new();
    let mut skipped = 0;
    let mut add = |text: String, refers: String, want: &mut Vec<(String, String)>| match name_from_label(&text) {
        Some(n) if !want.iter().any(|(m, _)| m.eq_ignore_ascii_case(&n)) => want.push((n, refers)),
        _ => skipped += 1,
    };
    if e.top || e.bottom {
        let lr = if e.top { sel.r0 } else { sel.r1 };
        for c in dc0..=dc1 {
            add(label(lr, c), place(dr0, c, dr1, c), &mut want);
        }
    }
    if e.left || e.right {
        let lc = if e.left { sel.c0 } else { sel.c1 };
        for r in dr0..=dr1 {
            add(label(r, lc), place(r, dc0, r, dc1), &mut want);
        }
    }
    if want.len() > 4096 {
        return Err("That makes more than 4,096 names: select fewer labels.".into());
    }
    if want.is_empty() {
        return Err("No labels to make names from.".into());
    }
    let old: Vec<_> = d.wb.names().iter().cloned().collect();
    for (n, refers) in &want {
        if let Err(err) = d.wb.define_name(n, None, refers) {
            d.wb.replace_names(old);
            return Err(format!("{n} can't be a name: {}", err.msg));
        }
    }
    let new: Vec<_> = d.wb.names().iter().cloned().collect();
    d.wb.replace_names(old);
    d.status.clear();
    d.run("Create Names", vec![GridOp::names(&d.wb, &new)]);
    refused(d)?;
    let n = want.len();
    let mut m = format!("{n} {} made: {}.", if n == 1 { "name" } else { "names" }, want.iter().take(6).map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", "));
    if n > 6 {
        m.pop();
        m.push_str(", …");
    }
    if skipped > 0 {
        m.push_str(&format!(" {skipped} empty or repeated {} skipped.", if skipped == 1 { "label" } else { "labels" }));
    }
    Ok(m)
}

// ---------- Apply Names ----------

/// The places names refer to that a formula on sheet `s` can use: (sheet,
/// cells, name), this sheet's own names before the workbook's.
fn name_places(wb: &Workbook, s: u16) -> Vec<(Rect, Arc<str>)> {
    let mut out = Vec::new();
    for own in [true, false] {
        for n in wb.names().iter() {
            if (own && n.scope != Some(s)) || (!own && n.scope.is_some()) || n.name.starts_with("_xlnm.") {
                continue;
            }
            let abs = |c: Coord| match c { Coord::Abs(v) => Some(v), Coord::Rel(_) => None };
            let rect = match n.expr.as_ref() {
                Expr::Ref(CellRef { sheet: Some(sh), row, col }) => abs(*row).zip(abs(*col)).map(|(r, c)| Rect { sheet: *sh, r0: r, c0: c, r1: r, c1: c }),
                Expr::Range(RangeRef { sheet: Some(sh), r0, c0, r1, c1, .. }) => match (abs(*r0), abs(*c0), abs(*r1), abs(*c1)) {
                    (Some(r0), Some(c0), Some(r1), Some(c1)) => Some(Rect { sheet: *sh, r0, c0, r1, c1 }),
                    _ => None,
                },
                _ => None,
            };
            if let Some(r) = rect {
                if !out.iter().any(|(o, _): &(Rect, Arc<str>)| *o == r) {
                    out.push((r, n.name.clone()));
                }
            }
        }
    }
    out
}

/// `e` at (`row`, `col`) on sheet `s` with each reference that is exactly
/// a name's place written as the name (relative or absolute alike, as
/// Excel's default).
fn named(e: &Expr, s: u16, row: u32, col: u32, places: &[(Rect, Arc<str>)], hits: &mut usize) -> Expr {
    let at = |rect: Rect| places.iter().find(|(r, _)| *r == rect).map(|(_, n)| n.clone());
    let b = |x: &Expr, hits: &mut usize| Box::new(named(x, s, row, col, places, hits));
    match e {
        Expr::Ref(c) => {
            let hit = c.row.resolve(row, MAX_ROWS).zip(c.col.resolve(col, MAX_COLS)).and_then(|(r, cc)| at(Rect { sheet: c.sheet.unwrap_or(s), r0: r, c0: cc, r1: r, c1: cc }));
            match hit {
                Some(n) => {
                    *hits += 1;
                    Expr::Name(n)
                }
                None => e.clone(),
            }
        }
        Expr::Range(g) => {
            let r = (g.r0.resolve(row, MAX_ROWS), g.c0.resolve(col, MAX_COLS), g.r1.resolve(row, MAX_ROWS), g.c1.resolve(col, MAX_COLS));
            let hit = match r {
                (Some(r0), Some(c0), Some(r1), Some(c1)) => at(Rect { sheet: g.sheet.unwrap_or(s), r0: r0.min(r1), c0: c0.min(c1), r1: r0.max(r1), c1: c0.max(c1) }),
                _ => None,
            };
            match hit {
                Some(n) => {
                    *hits += 1;
                    Expr::Name(n)
                }
                None => e.clone(),
            }
        }
        Expr::Neg(x) => Expr::Neg(b(x, hits)),
        Expr::Plus(x) => Expr::Plus(b(x, hits)),
        Expr::Pct(x) => Expr::Pct(b(x, hits)),
        Expr::Paren(x) => Expr::Paren(b(x, hits)),
        Expr::At(x) => Expr::At(b(x, hits)),
        Expr::Bin(op, x, y) => {
            let x = b(x, hits);
            Expr::Bin(*op, x, b(y, hits))
        }
        Expr::Call(f, args) => Expr::Call(f.clone(), args.iter().map(|a| named(a, s, row, col, places, hits)).collect()),
        Expr::Invoke(g, args) => {
            let g = b(g, hits);
            Expr::Invoke(g, args.iter().map(|a| named(a, s, row, col, places, hits)).collect())
        }
        other => other.clone(),
    }
}

/// Apply Names: in the selection's formulas (the sheet's when one cell is
/// selected), each reference that is exactly a name's place becomes the
/// name. One step.
pub fn apply_names(d: &mut Doc, sel: CellRange) -> Result<String, String> {
    let s = d.sheet;
    let places = name_places(&d.wb, s);
    if places.is_empty() {
        return Err("No names refer to cells: define some first (Define Name… or Create from Selection…).".into());
    }
    let whole = sel.r0 == sel.r1 && sel.c0 == sel.c1;
    let (r0, c0, r1, c1) = if whole { (0, 0, MAX_ROWS - 1, MAX_COLS - 1) } else { (sel.r0, sel.c0, sel.r1, sel.c1) };
    let mut sets = Vec::new();
    let (mut cells, mut hits) = (0u64, 0usize);
    let sheet_name = |id: u16| d.wb.sheet_name(id);
    for (_, g) in d.wb.groups() {
        if g.sheet != s || !(c0..=c1).contains(&g.col) || g.r1 < r0 || g.r0 > r1 {
            continue;
        }
        for row in g.r0.max(r0)..=g.r1.min(r1) {
            cells += 1;
            if cells > MAX_CELLS {
                return Err(format!("Select fewer than {MAX_CELLS} formula cells."));
            }
            let before = hits;
            let e = named(&g.expr, s, row, g.col, &places, &mut hits);
            if hits != before {
                let text = officina_grid::formula::to_a1(&e, row, g.col, &sheet_name);
                sets.push(GridOp::set(CellAddr::new(s, row, g.col), Input::Formula(text)));
            }
        }
    }
    if sets.is_empty() {
        return Err("No references in those formulas are exactly a name's cells.".into());
    }
    let n = sets.len();
    d.status.clear();
    if !d.run_validated("Apply Names", sets) {
        return Err(if d.status.is_empty() { "Apply Names was refused.".into() } else { d.status.clone() });
    }
    refused(d)?;
    Ok(format!("{hits} {} named in {n} {}.", if hits == 1 { "reference" } else { "references" }, if n == 1 { "formula" } else { "formulas" }))
}

// ---------- Auto Outline ----------

/// Auto Outline: rows (and columns) totalled by a formula just below or
/// above them (just right or left of them) are grouped, nesting where the
/// totals nest, over the selection or, for one cell, the sheet. The
/// sheet's outline on that axis is replaced. One step.
pub fn auto_outline(d: &mut Doc, sel: CellRange) -> Result<String, String> {
    let s = d.sheet;
    let whole = sel.r0 == sel.r1 && sel.c0 == sel.c1;
    let (r0, c0, r1, c1) = if whole { (0, 0, MAX_ROWS - 1, MAX_COLS - 1) } else { (sel.r0, sel.c0, sel.r1, sel.c1) };
    let (mut rows, mut cols): (Vec<(u32, u32)>, Vec<(u32, u32)>) = (Vec::new(), Vec::new());
    let mut cells = 0u64;
    for (_, g) in d.wb.groups() {
        if g.sheet != s || !(c0..=c1).contains(&g.col) || g.r1 < r0 || g.r0 > r1 {
            continue;
        }
        for row in g.r0.max(r0)..=g.r1.min(r1) {
            cells += 1;
            if cells > MAX_CELLS {
                return Err(format!("Select fewer than {MAX_CELLS} formula cells."));
            }
            for p in crate::data::precedents(&d.wb, s, row, g.col) {
                if p.sheet != s {
                    continue;
                }
                // A column of cells just above or below in the same column.
                if p.c0 == g.col && p.c1 == g.col && (p.r1 + 1 == row || p.r0 == row + 1) && !rows.contains(&(p.r0, p.r1)) {
                    rows.push((p.r0, p.r1));
                }
                // A row of cells just left or right in the same row.
                if p.r0 == row && p.r1 == row && (p.c1 + 1 == g.col || p.c0 == g.col + 1) && !cols.contains(&(p.c0, p.c1)) {
                    cols.push((p.c0, p.c1));
                }
            }
        }
    }
    if rows.is_empty() && cols.is_empty() {
        return Err("No totals found: Auto Outline groups the cells a formula such as =SUM(B2:B5) totals just beside it.".into());
    }
    let mut ops = Vec::new();
    let mut made = [0usize; 2];
    for (k, (axis, mut spans)) in [(Axis::Rows, rows), (Axis::Cols, cols)].into_iter().enumerate() {
        if spans.is_empty() {
            continue;
        }
        // Widest first, so totals of totals are the outer groups; one that
        // would cross another is left out.
        spans.sort_by_key(|&(a, b)| (std::cmp::Reverse(b - a), a));
        let mut groups: Vec<Group> = Vec::new();
        for (a, b) in spans {
            let mut next = groups.clone();
            next.push(Group { start: a, end: b, collapsed: false });
            if Outline::new(next.clone(), axis).is_some() {
                groups = next;
            }
        }
        made[k] = groups.len();
        let outline = Outline::new(groups, axis).ok_or("The outline is too deep or too large.")?;
        ops.push(GridOp::Outline { sheet: s, axis, outline });
    }
    d.status.clear();
    if !d.run_validated("Auto Outline", ops) {
        return Err(if d.status.is_empty() { "Auto Outline was refused.".into() } else { d.status.clone() });
    }
    refused(d)?;
    let part = |n: usize, what: &str| format!("{n} {what} {}", if n == 1 { "group" } else { "groups" });
    Ok(match made {
        [r, 0] => format!("{}.", part(r, "row")),
        [0, c] => format!("{}.", part(c, "column")),
        [r, c] => format!("{} and {}.", part(r, "row"), part(c, "column")),
    })
}

// ---------- Workbook Statistics ----------

/// Workbook Statistics: what the sheet on screen and the workbook hold, as
/// (label, number) lines, the sheet's first.
pub fn statistics(d: &Doc) -> Vec<(String, String)> {
    let wb = &d.wb;
    let s = d.sheet;
    let count_cells = |sheet: u16| {
        let (rows, cols) = wb.extent(sheet);
        let mut n = 0u64;
        if rows > 0 && cols > 0 {
            wb.each_cell(&Rect { sheet, r0: 0, c0: 0, r1: rows - 1, c1: cols - 1 }, &mut |_, _, cell| {
                if !cell.is_empty() {
                    n += 1;
                }
            });
        }
        n
    };
    let formulas = |sheet: Option<u16>| wb.groups().filter(|(_, g)| sheet.is_none_or(|x| g.sheet == x)).map(|(_, g)| u64::from(g.r1 - g.r0 + 1)).sum::<u64>();
    let pivots = wb.pivot_state();
    let n = |v: u64| v.to_string();
    let mut out = Vec::new();
    let (rows, cols) = wb.extent(s);
    out.push((format!("{}: end of sheet", wb.sheet_name(s)), if rows == 0 || cols == 0 { "—".into() } else { a1(rows - 1, cols - 1) }));
    out.push(("Cells with data".into(), n(count_cells(s))));
    out.push(("Formulas".into(), n(formulas(Some(s)))));
    out.push(("Tables".into(), n(wb.tables().iter().filter(|t| t.rect.sheet == s).count() as u64)));
    out.push(("PivotTables".into(), n(pivots.iter().filter(|(p, _)| p.dest.sheet == s).count() as u64)));
    out.push(("Charts".into(), n(wb.charts().iter().filter(|c| c.anchor.sheet == s).count() as u64)));
    out.push(("Pictures".into(), n(wb.pictures().iter().filter(|p| p.anchor.sheet == s).count() as u64)));
    out.push(("Comments".into(), n(wb.comments(s).keys().count() as u64)));
    out.push(("Sparklines".into(), n(wb.sheet(s).sparklines().len() as u64)));
    out.push(("Workbook: sheets".into(), n(wb.sheet_count() as u64)));
    out.push(("Cells with data".into(), n((0..wb.sheet_count() as u16).map(count_cells).sum())));
    out.push(("Formulas".into(), n(formulas(None))));
    out.push(("Tables".into(), n(wb.tables().len() as u64)));
    out.push(("PivotTables".into(), n(pivots.len() as u64)));
    out.push(("Charts".into(), n(wb.charts().len() as u64)));
    out.push(("Names".into(), n(wb.names().iter().filter(|nm| !nm.name.starts_with("_xlnm.")).count() as u64)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(csv: &[u8]) -> Doc {
        let mut d = Doc::from_csv("f.csv", csv).unwrap().0;
        d.wb.recalc();
        d
    }

    fn put(d: &mut Doc, cell: &str, text: &str) {
        let (r, c) = officina_grid::addr::parse_a1(cell).unwrap();
        let input = if text.starts_with('=') { Input::Formula(text.into()) } else { Input::Value(officina_grid::Value::Num(text.parse().unwrap())) };
        d.run("Typing", vec![GridOp::set(CellAddr::new(d.sheet, r, c), input)]);
    }

    fn shown(d: &Doc, cell: &str) -> String {
        let (r, c) = officina_grid::addr::parse_a1(cell).unwrap();
        d.wb.formatted(CellAddr::new(d.sheet, r, c)).text
    }

    #[test]
    fn manual_calculation_waits_for_calculate_now() {
        let mut d = doc(b"1\n");
        put(&mut d, "B1", "=A1*2");
        assert_eq!(shown(&d, "B1"), "2");
        assert_eq!(toggle_manual(&mut d).unwrap(), "Calculation is manual: edits wait for Calculate Now (F9).");
        put(&mut d, "A1", "5");
        // The reader waits; a formula typed now works itself out.
        assert_eq!(shown(&d, "B1"), "2");
        assert!(d.needs_calculate());
        put(&mut d, "C1", "=A1+1");
        assert_eq!(shown(&d, "C1"), "6");
        assert_eq!(shown(&d, "B1"), "2");
        assert!(!d.settle(), "nothing runs in the background");
        d.calculate_now(None);
        assert_eq!(shown(&d, "B1"), "10");
        assert!(!d.needs_calculate());
        // Kept in the file; undo puts automatic back and catches up.
        let back = officina_grid::native::load(&officina_grid::native::save_to_vec(&d.wb)).unwrap();
        assert!(back.manual_calc());
        put(&mut d, "A1", "7");
        assert_eq!(shown(&d, "B1"), "10");
        assert_eq!(toggle_manual(&mut d).unwrap(), "Calculation is automatic.");
        assert_eq!(shown(&d, "B1"), "14");
        put(&mut d, "A1", "8");
        assert_eq!(shown(&d, "B1"), "16");
    }

    #[test]
    fn watches_are_added_follow_their_cells_and_go() {
        let mut d = doc(b"1,2\n3,4\n");
        put(&mut d, "C1", "=A1+B1");
        assert!(add_watches(&mut d, CellRange { r0: 0, c0: 2, r1: 0, c1: 2 }).unwrap().contains("1 more cell"));
        // A range adds its filled cells only; again adds nothing.
        assert!(add_watches(&mut d, CellRange { r0: 0, c0: 0, r1: 3, c1: 2 }).unwrap().contains("4 more cells"));
        assert!(add_watches(&mut d, CellRange { r0: 0, c0: 2, r1: 0, c1: 2 }).is_err());
        let rows = watch_rows(&d.wb);
        assert_eq!(rows.len(), 5);
        assert_eq!((rows[0].place.as_str(), rows[0].value.as_str(), rows[0].formula.as_deref()), ("Sheet1!C1", "3", Some("=A1+B1")));
        // A row inserted above moves the watches with their cells.
        let op = officina_grid::shift::shift_op(&d.wb, 0, Axis::Rows, 0, 1).unwrap();
        d.run("Insert Rows", vec![op]);
        assert_eq!(watch_rows(&d.wb)[0].place, "Sheet1!C2");
        assert_eq!(watch_rows(&d.wb)[0].value, "3");
        let back = officina_grid::native::load(&officina_grid::native::save_to_vec(&d.wb)).unwrap();
        assert_eq!(back.sheet(0).watches(), d.wb.sheet(0).watches());
        assert_eq!(delete_watch(&mut d, 0).unwrap(), "Sheet1!C2 is no longer watched.");
        assert_eq!(watch_rows(&d.wb).len(), 4);
        assert!(d.undo());
        assert_eq!(watch_rows(&d.wb).len(), 5);
    }

    #[test]
    fn names_are_made_from_labels_and_applied_to_formulas() {
        let mut d = Doc::from_csv("n.csv", b",Q1,Q2\nNorth,1,2\nSouth West,3,4\n").unwrap().0;
        d.wb.recalc();
        let sel = CellRange { r0: 0, c0: 0, r1: 2, c1: 2 };
        assert_eq!(create_start(&d, sel), "top, left");
        let m = create_names(&mut d, sel, "top, left").unwrap();
        assert!(m.starts_with("4 names made: Q1_, Q2_, North, South_West."), "{m}");
        let refers = |name: &str| crate::data::name_rows(&d.wb).into_iter().find(|r| r.name == name).unwrap().refers;
        assert_eq!(refers("Q1_"), "=Sheet1!$B$2:$B$3");
        assert_eq!(refers("South_West"), "=Sheet1!$B$3:$C$3");
        assert_eq!(name_from_label("2024 sales"), Some("_2024_sales".into()));
        assert_eq!(name_from_label("A1"), Some("A1_".into()));
        assert_eq!(name_from_label("  "), None);
        assert!(create_names(&mut d, sel, "middle").is_err());
        // Apply Names: the exact ranges become the names; others stay.
        put(&mut d, "E1", "=SUM(B2:B3)+SUM($B$3:$C$3)");
        put(&mut d, "E2", "=SUM(B2:C2)");
        let m = apply_names(&mut d, CellRange { r0: 0, c0: 0, r1: 0, c1: 0 }).unwrap();
        assert_eq!(m, "3 references named in 2 formulas.");
        let f = |d: &Doc, cell: &str| { let (r, c) = officina_grid::addr::parse_a1(cell).unwrap(); d.wb.formula_text(CellAddr::new(0, r, c)).unwrap() };
        assert_eq!(f(&d, "E1"), "=SUM(Q1_)+SUM(South_West)");
        assert_eq!(f(&d, "E2"), "=SUM(North)");
        assert_eq!(shown(&d, "E1"), "11");
        assert!(d.undo());
        assert_eq!(f(&d, "E1"), "=SUM(B2:B3)+SUM($B$3:$C$3)");
    }

    #[test]
    fn auto_outline_groups_what_the_totals_add() {
        let mut d = doc(b"1,2,3\n4,5,6\n");
        // Quarter totals under each block, a year total under both.
        put(&mut d, "A3", "=SUM(A1:A2)");
        put(&mut d, "A4", "7");
        put(&mut d, "A5", "8");
        put(&mut d, "A6", "=SUM(A4:A5)");
        put(&mut d, "A7", "=SUM(A1:A6)");
        // A row total to the right.
        put(&mut d, "D1", "=SUM(A1:C1)");
        let m = auto_outline(&mut d, CellRange { r0: 0, c0: 0, r1: 0, c1: 0 }).unwrap();
        assert_eq!(m, "3 row groups and 1 column group.");
        let rows: Vec<(u32, u32)> = d.wb.sheet(0).outline(Axis::Rows).groups().iter().map(|g| (g.start, g.end)).collect();
        assert!(rows.contains(&(0, 5)) && rows.contains(&(0, 1)) && rows.contains(&(3, 4)), "{rows:?}");
        let cols: Vec<(u32, u32)> = d.wb.sheet(0).outline(Axis::Cols).groups().iter().map(|g| (g.start, g.end)).collect();
        assert_eq!(cols, [(0, 2)]);
        assert!(d.undo());
        assert!(d.wb.sheet(0).outline(Axis::Rows).groups().is_empty());
        let mut plain = doc(b"1,2\n");
        assert!(auto_outline(&mut plain, CellRange { r0: 0, c0: 0, r1: 0, c1: 0 }).is_err());
    }

    #[test]
    fn statistics_count_the_sheet_and_the_workbook() {
        let mut d = doc(b"1,2\n3,\n");
        put(&mut d, "C1", "=A1+B1");
        let st = statistics(&d);
        let get = |i: usize| st[i].1.clone();
        assert_eq!(st[0], ("Sheet1: end of sheet".to_string(), "C2".to_string()));
        assert_eq!(get(1), "4");
        assert_eq!(get(2), "1");
        assert_eq!(st[9], ("Workbook: sheets".to_string(), "1".to_string()));
    }
}
