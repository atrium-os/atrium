//! The Data menu: sort, filter, tables and charts on the data around the selection
//! (docs/spec/officina.md §8.4). Each command is one undoable step in the journal.
//!
//! "The data around the selection" is the selection when it covers more than one
//! cell, else the table or filter the cursor is in, else the current region: the
//! block of non-empty cells around the cursor bounded by empty rows and columns, as
//! Excel finds it.

use std::sync::Arc;

use officina_grid::addr::{CellAddr, Rect};
use officina_grid::charts::{Anchor, Chart, ChartKind, ChartText, Grouping, LegendPos, Series};
use officina_grid::filter::{AutoFilter, Cond};
use officina_grid::ops::GridOp;
use officina_grid::sort::{sort_op, SortKey};
use officina_grid::tables::Table;
use officina_grid::{Value, Workbook};
use pergola::widgets::{CellRange, MenuItem};

use crate::doc::Doc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataCmd {
    SortAsc,
    SortDesc,
    /// Turn the auto-filter on (on the data) or off.
    Filter,
    /// Show only rows with the cursor's value in its column.
    KeepValue,
    /// Hide rows with the cursor's value in its column.
    HideValue,
    /// Drop the cursor column's condition.
    ClearColumn,
    /// Drop every condition: all rows show.
    ShowAll,
    FormatAsTable,
    /// The table at the cursor becomes plain cells.
    RemoveTable,
    Chart(ChartKind),
    /// The chart over the cursor (or using its cell).
    DeleteChart,
}

/// The block of non-empty cells around (`row`, `col`), bounded by empty rows and
/// columns. Cost: the block's perimeter times the passes (a few), never its area.
pub fn current_region(wb: &Workbook, sheet: u16, row: u32, col: u32) -> Rect {
    let (rows, cols) = wb.extent(sheet);
    let full = |r: u32, c: u32| r < rows && c < cols && !matches!(wb.get(CellAddr::new(sheet, r, c)), Value::Empty);
    let (mut r0, mut c0, mut r1, mut c1) = (row, col, row, col);
    let row_has = |r: u32, c0: u32, c1: u32| (c0.saturating_sub(1)..=(c1 + 1).min(cols)).any(|c| full(r, c));
    let col_has = |c: u32, r0: u32, r1: u32| (r0.saturating_sub(1)..=(r1 + 1).min(rows)).any(|r| full(r, c));
    loop {
        let before = (r0, c0, r1, c1);
        while r0 > 0 && row_has(r0 - 1, c0, c1) {
            r0 -= 1;
        }
        while r1 + 1 < rows && row_has(r1 + 1, c0, c1) {
            r1 += 1;
        }
        while c0 > 0 && col_has(c0 - 1, r0, r1) {
            c0 -= 1;
        }
        while c1 + 1 < cols && col_has(c1 + 1, r0, r1) {
            c1 += 1;
        }
        if (r0, c0, r1, c1) == before {
            break;
        }
    }
    Rect { sheet, r0, c0, r1, c1 }
}

/// Whether the first row of `r` is a header: every cell text, and some column has a
/// non-text value under it.
pub fn has_header(wb: &Workbook, r: &Rect) -> bool {
    if r.r1 == r.r0 {
        return false;
    }
    let at = |row: u32, c: u32| wb.get(CellAddr::new(r.sheet, row, c));
    (r.c0..=r.c1).all(|c| matches!(at(r.r0, c), Value::Str(_))) && (r.c0..=r.c1).any(|c| !matches!(at(r.r0 + 1, c), Value::Str(_) | Value::Empty))
}

/// What the commands work on.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    /// The data, its header row included when it has one.
    pub rect: Rect,
    pub header: bool,
    /// The cursor.
    pub row: u32,
    pub col: u32,
}

pub fn target(d: &Doc, sel: CellRange, cursor: (u32, u32)) -> Option<Target> {
    let s = d.sheet;
    let wb = &d.wb;
    let (row, col) = cursor;
    if sel.r0 != sel.r1 || sel.c0 != sel.c1 {
        let rect = Rect { sheet: s, r0: sel.r0, c0: sel.c0, r1: sel.r1, c1: sel.c1 };
        return Some(Target { header: has_header(wb, &rect), rect, row, col });
    }
    if let Some(t) = wb.table_at(s, row, col) {
        let mut rect = t.rect;
        if t.totals {
            rect.r1 -= 1;
        }
        return Some(Target { rect, header: t.header, row, col });
    }
    if let Some(f) = &wb.sheet(s).filter {
        let r = f.rect;
        if (r.r0..=r.r1).contains(&row) && (r.c0..=r.c1).contains(&col) {
            return Some(Target { rect: r, header: true, row, col });
        }
    }
    let rect = current_region(wb, s, row, col);
    if rect.r0 == rect.r1 && rect.c0 == rect.c1 && matches!(wb.get(CellAddr::new(s, row, col)), Value::Empty) {
        return None;
    }
    Some(Target { header: has_header(wb, &rect), rect, row, col })
}

fn short(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() > 24 {
        format!("{}…", s.chars().take(23).collect::<String>())
    } else {
        s.to_string()
    }
}

/// The chart under the cursor's cell, else the last one using it.
fn chart_at(wb: &Workbook, sheet: u16, row: u32, col: u32) -> Option<usize> {
    let cs = wb.charts();
    let over = cs.iter().rposition(|c| c.anchor.sheet == sheet && (c.anchor.from.0..=c.anchor.to.0).contains(&row) && (c.anchor.from.1..=c.anchor.to.1).contains(&col));
    over.or_else(|| {
        let has = |r: &Rect| r.sheet == sheet && (r.r0..=r.r1).contains(&row) && (r.c0..=r.c1).contains(&col);
        cs.iter().rposition(|c| c.series.iter().any(|s| has(&s.vals) || s.cats.as_ref().is_some_and(has)))
    })
}

/// The Data menu as it reads now, with the command of each item.
pub fn menu(d: &Doc, sel: CellRange, cursor: (u32, u32)) -> Vec<(MenuItem, Option<DataCmd>)> {
    let wb = &d.wb;
    let s = d.sheet;
    let (row, col) = cursor;
    let t = target(d, sel, cursor);
    let some = t.is_some();
    let filter = wb.sheet(s).filter.as_ref();
    let in_filter = filter.filter(|f| (f.rect.r0 + 1..=f.rect.r1).contains(&row) && (f.rect.c0..=f.rect.c1).contains(&col));
    let value = short(&wb.formatted(CellAddr::new(s, row, col)).text);
    let shown = if value.is_empty() { "(empty)".to_string() } else { format!("“{value}”") };
    let col_filtered = filter.is_some_and(|f| f.conds.iter().any(|(c, _)| *c == col));
    let any_cond = filter.is_some_and(|f| !f.conds.is_empty());
    let table = wb.table_at(s, row, col);
    let mut v = vec![
        (MenuItem::new("Sort A to Z").enabled(some), Some(DataCmd::SortAsc)),
        (MenuItem::new("Sort Z to A").enabled(some), Some(DataCmd::SortDesc)),
        (MenuItem::new("Filter").checked(filter.is_some()).enabled(some || filter.is_some()).separated(), Some(DataCmd::Filter)),
        (MenuItem::new(format!("Show Only {shown}")).enabled(in_filter.is_some()), Some(DataCmd::KeepValue)),
        (MenuItem::new(format!("Hide {shown}")).enabled(in_filter.is_some()), Some(DataCmd::HideValue)),
        (MenuItem::new(format!("Clear Filter in Column {}", officina_grid::addr::col_name(col))).enabled(col_filtered), Some(DataCmd::ClearColumn)),
        (MenuItem::new("Show All Rows").enabled(any_cond), Some(DataCmd::ShowAll)),
    ];
    match table {
        Some(t) => v.push((MenuItem::new(format!("Convert “{}” to Range", short(&t.name))).separated(), Some(DataCmd::RemoveTable))),
        None => v.push((MenuItem::new("Format as Table").enabled(some).separated(), Some(DataCmd::FormatAsTable))),
    }
    v.push((MenuItem::new("Insert Column Chart").enabled(some).separated(), Some(DataCmd::Chart(ChartKind::Column))));
    v.push((MenuItem::new("Insert Line Chart").enabled(some), Some(DataCmd::Chart(ChartKind::Line))));
    v.push((MenuItem::new("Insert Pie Chart").enabled(some), Some(DataCmd::Chart(ChartKind::Pie))));
    v.push((MenuItem::new("Delete Chart").enabled(chart_at(wb, s, row, col).is_some()), Some(DataCmd::DeleteChart)));
    v
}

/// Runs `cmd`; says what happened (or why nothing did) in the status line.
pub fn run(d: &mut Doc, sel: CellRange, cursor: (u32, u32), cmd: DataCmd) {
    let msg = match apply(d, sel, cursor, cmd) {
        Ok(m) | Err(m) => m,
    };
    d.status = msg;
}

fn apply(d: &mut Doc, sel: CellRange, cursor: (u32, u32), cmd: DataCmd) -> Result<String, String> {
    let s = d.sheet;
    let (row, col) = cursor;
    let need = || "Put the cursor in your data (or select it) first.".to_string();
    match cmd {
        DataCmd::SortAsc | DataCmd::SortDesc => {
            let t = target(d, sel, cursor).ok_or_else(need)?;
            if d.wb.sheet(s).filter.as_ref().is_some_and(|f| !f.conds.is_empty() && f.rect.r0 <= t.rect.r1 && f.rect.r1 >= t.rect.r0) {
                return Err("Show all rows before sorting: the filter hides some of them.".into());
            }
            let mut rect = t.rect;
            if t.header {
                rect.r0 += 1;
            }
            if rect.r0 > rect.r1 {
                return Err("There is nothing under the header to sort.".into());
            }
            let key = col.clamp(rect.c0, rect.c1);
            let descending = cmd == DataCmd::SortDesc;
            match sort_op(&d.wb, rect, &[SortKey { col: key, descending }]) {
                Some(op) => {
                    d.run("Sort", vec![op]);
                    Ok(format!("Sorted {} rows by column {}{}.", rect.r1 - rect.r0 + 1, officina_grid::addr::col_name(key), if t.header { " (the first row is kept as the header)" } else { "" }))
                }
                None => Ok("Already in that order.".into()),
            }
        }
        DataCmd::Filter => {
            if d.wb.sheet(s).filter.is_some() {
                let st = d.wb.filtered(s, None);
                d.run("Remove Filter", vec![GridOp::Filter(st)]);
                return Ok("Filter off; every row shows.".into());
            }
            let t = target(d, sel, cursor).ok_or_else(need)?;
            if t.rect.r1 == t.rect.r0 {
                return Err("A filter needs a header row and rows under it.".into());
            }
            let st = d.wb.filtered(s, Some(AutoFilter { rect: t.rect, conds: Vec::new() }));
            d.run("Filter", vec![GridOp::Filter(st)]);
            Ok(format!("Filter on {}: pick a cell and use Show Only or Hide.", range_text(&t.rect)))
        }
        DataCmd::KeepValue | DataCmd::HideValue | DataCmd::ClearColumn | DataCmd::ShowAll => {
            let f = d.wb.sheet(s).filter.clone().ok_or("There is no filter on this sheet.")?;
            let mut conds = f.conds.clone();
            let text = d.wb.formatted(CellAddr::new(s, row, col)).text;
            match cmd {
                DataCmd::ShowAll => conds.clear(),
                DataCmd::ClearColumn => conds.retain(|(c, _)| *c != col),
                _ => {
                    if !(f.rect.r0 + 1..=f.rect.r1).contains(&row) || !(f.rect.c0..=f.rect.c1).contains(&col) {
                        return Err("Put the cursor on a value under the filter's header.".into());
                    }
                    let keep: Vec<String> = if cmd == DataCmd::KeepValue {
                        vec![text.clone()]
                    } else {
                        // Every value the column shows now, but this one.
                        const MAX: usize = 10_000;
                        let all = d.wb.filter_values(&f.rect, col, MAX + 1);
                        if all.len() > MAX {
                            return Err("This column has too many different values to hide one; use Show Only.".into());
                        }
                        let now: Option<Vec<String>> = conds.iter().find(|(c, _)| *c == col).and_then(|(_, c)| match c {
                            Cond::Values(v) => Some(v.clone()),
                            _ => None,
                        });
                        let key = officina_grid::eval::funcs::text_key(&text);
                        all.into_iter().map(|(t, _)| t).filter(|t| now.as_ref().is_none_or(|n| n.iter().any(|x| officina_grid::eval::funcs::text_key(x) == officina_grid::eval::funcs::text_key(t)))).filter(|t| officina_grid::eval::funcs::text_key(t) != key).collect()
                    };
                    conds.retain(|(c, _)| *c != col);
                    conds.push((col, Cond::Values(keep)));
                    conds.sort_by_key(|(c, _)| *c);
                }
            }
            let st = d.wb.filtered(s, Some(AutoFilter { rect: f.rect, conds }));
            let hidden: u32 = st.hidden.iter().filter(|(a, b)| *b > f.rect.r0 && *a <= f.rect.r1).map(|(a, b)| b.min(&f.rect.r1) - a.max(&(f.rect.r0 + 1)) + 1).sum();
            d.run("Filter", vec![GridOp::Filter(st)]);
            Ok(if hidden == 0 { "Every row shows.".into() } else { format!("{hidden} rows hidden by the filter.") })
        }
        DataCmd::FormatAsTable => {
            let t = target(d, sel, cursor).ok_or_else(need)?;
            if d.wb.tables().iter().any(|x| x.rect.sheet == s && x.rect.r0 <= t.rect.r1 && x.rect.r1 >= t.rect.r0 && x.rect.c0 <= t.rect.c1 && x.rect.c1 >= t.rect.c0) {
                return Err("That overlaps a table already.".into());
            }
            let mut columns: Vec<Arc<str>> = Vec::new();
            let mut seen = std::collections::HashSet::new();
            for c in t.rect.c0..=t.rect.c1 {
                let mut n = if t.header { d.wb.formatted(CellAddr::new(s, t.rect.r0, c)).text.trim().to_string() } else { String::new() };
                if n.is_empty() || n.len() > officina_grid::tables::MAX_COL_NAME || !seen.insert(n.to_lowercase()) {
                    n = format!("Column{}", c - t.rect.c0 + 1);
                    seen.insert(n.to_lowercase());
                }
                columns.push(n.as_str().into());
            }
            let name = (1..).map(|i| format!("Table{i}")).find(|n| d.wb.table(n).is_none() && d.wb.names().iter().all(|x| !x.name.eq_ignore_ascii_case(n))).unwrap();
            let table = Table { name: name.as_str().into(), rect: t.rect, header: t.header, totals: false, columns, style: Some("TableStyleMedium2".into()), stripes: true };
            let mut all = d.wb.tables().to_vec();
            all.push(table.clone());
            // Check it the way the engine will, so a refusal is said, not dropped.
            let mut probe = Workbook::new();
            for i in 0..d.wb.sheet_count() {
                let _ = probe.add_sheet(&d.wb.sheet_name(i as u16));
            }
            probe.add_table(table).map_err(|e| format!("Couldn't make a table: {e}."))?;
            d.run("Format as Table", vec![GridOp::Tables(all)]);
            Ok(format!("{name} made from {}{}; formulas can use {name}[Column].", range_text(&t.rect), if t.header { "" } else { " (no header row found: its columns are Column1, Column2, …)" }))
        }
        DataCmd::RemoveTable => {
            let name = d.wb.table_at(s, row, col).map(|t| t.name.clone()).ok_or("The cursor isn't in a table.")?;
            let all: Vec<Table> = d.wb.tables().iter().filter(|t| t.name != name).cloned().collect();
            d.run("Convert to Range", vec![GridOp::Tables(all)]);
            Ok(format!("{name} is plain cells now; formulas that used it show #REF!."))
        }
        DataCmd::Chart(kind) => {
            let t = target(d, sel, cursor).ok_or_else(need)?;
            let chart = chart_for(&d.wb, &t, kind).ok_or("There are no numbers there to chart.")?;
            let mut all = d.wb.charts().to_vec();
            all.push(chart);
            if all.len() > officina_grid::charts::MAX_CHARTS {
                return Err("This workbook has as many charts as it can hold.".into());
            }
            d.run("Insert Chart", vec![GridOp::Charts(all)]);
            Ok(format!("Chart of {} added to the right of the data.", range_text(&t.rect)))
        }
        DataCmd::DeleteChart => {
            let i = chart_at(&d.wb, s, row, col).ok_or("There is no chart here.")?;
            let mut all = d.wb.charts().to_vec();
            all.remove(i);
            d.run("Delete Chart", vec![GridOp::Charts(all)]);
            Ok("Chart deleted.".into())
        }
    }
}

fn range_text(r: &Rect) -> String {
    let one = |row: u32, col: u32| format!("{}{}", officina_grid::addr::col_name(col), row + 1);
    format!("{}:{}", one(r.r0, r.c0), one(r.r1, r.c1))
}

/// A chart of `t`: the first column as categories when it isn't numbers, every other
/// column with a number as a series named by its header.
pub fn chart_for(wb: &Workbook, t: &Target, kind: ChartKind) -> Option<Chart> {
    let r = t.rect;
    let s = r.sheet;
    let d0 = if t.header { r.r0 + 1 } else { r.r0 };
    if d0 > r.r1 {
        return None;
    }
    let numbers = |c: u32| (d0..=r.r1).filter(|&row| matches!(wb.get(CellAddr::new(s, row, c)), Value::Num(_))).count();
    let rows = (r.r1 - d0 + 1) as usize;
    let cats_col = (r.c1 > r.c0 && numbers(r.c0) * 2 < rows).then_some(r.c0);
    let first = cats_col.map_or(r.c0, |c| c + 1);
    let mut series: Vec<Series> = (first..=r.c1)
        .filter(|&c| numbers(c) > 0)
        .take(officina_grid::charts::MAX_SERIES)
        .map(|c| Series {
            name: t.header.then(|| ChartText::Cell(CellAddr::new(s, r.r0, c))),
            cats: cats_col.map(|cc| Rect { sheet: s, r0: d0, c0: cc, r1: r.r1, c1: cc }),
            vals: Rect { sheet: s, r0: d0, c0: c, r1: r.r1, c1: c },
            kind: None,
        })
        .collect();
    if series.is_empty() {
        return None;
    }
    if kind.round() {
        series.truncate(1);
    }
    let legend = (series.len() > 1 || kind.round()).then_some(LegendPos::Bottom);
    // Right of the data, past any cells it would cover (looking a little way).
    let r0 = r.r0.min(officina_grid::addr::MAX_ROWS - 17);
    let free = |c0: u32| (r0..=r0 + 16).all(|row| (c0..=c0 + 7).all(|c| matches!(wb.get(CellAddr::new(s, row, c)), Value::Empty)));
    let start = (r.c1 + 2).min(officina_grid::addr::MAX_COLS - 9);
    let c0 = (start..start.saturating_add(64).min(officina_grid::addr::MAX_COLS - 9)).find(|&c| free(c)).unwrap_or(start);
    Some(Chart { kind, grouping: Grouping::Standard, title: None, series, legend, x_title: None, y_title: None, anchor: Anchor { sheet: s, from: (r0, c0, 0.0, 0.0), to: (r0 + 16, c0 + 7, 0.0, 0.0) } })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> Doc {
        let mut d = Doc::from_csv("t.csv", b"Fruit,Qty,Price\npear,3,1.5\napple,10,0.5\nfig,7,2\napple,1,0.4\n\n,,,,lone\n").unwrap().0;
        d.wb.recalc();
        d
    }
    fn one(r: u32, c: u32) -> CellRange {
        CellRange { r0: r, c0: c, r1: r, c1: c }
    }
    fn cell(d: &Doc, r: u32, c: u32) -> String {
        d.wb.formatted(CellAddr::new(0, r, c)).text
    }

    #[test]
    fn region_and_header() {
        let d = doc();
        let t = target(&d, one(2, 1), (2, 1)).unwrap();
        assert_eq!(t.rect, Rect { sheet: 0, r0: 0, c0: 0, r1: 4, c1: 2 });
        assert!(t.header);
        // A lone cell is its own region (CSV drops the blank line: it is on row 6);
        // an empty cell is none.
        assert_eq!(cell(&d, 5, 4), "lone");
        assert_eq!(target(&d, one(5, 4), (5, 4)).unwrap().rect, Rect { sheet: 0, r0: 5, c0: 4, r1: 5, c1: 4 });
        assert!(target(&d, one(9, 9), (9, 9)).is_none());
    }

    #[test]
    fn sort_keeps_the_header_and_undoes() {
        let mut d = doc();
        run(&mut d, one(1, 0), (1, 0), DataCmd::SortAsc);
        assert_eq!((1..=4).map(|r| cell(&d, r, 0)).collect::<Vec<_>>(), ["apple", "apple", "fig", "pear"]);
        assert_eq!(cell(&d, 0, 0), "Fruit");
        run(&mut d, one(1, 1), (1, 1), DataCmd::SortDesc);
        assert_eq!((1..=4).map(|r| cell(&d, r, 1)).collect::<Vec<_>>(), ["10", "7", "3", "1"]);
        assert!(d.undo());
        assert!(d.undo());
        assert_eq!(cell(&d, 1, 0), "pear");
    }

    #[test]
    fn filter_show_only_hide_and_clear() {
        let mut d = doc();
        run(&mut d, one(1, 0), (1, 0), DataCmd::Filter);
        assert!(d.wb.sheet(0).filter.is_some());
        // Cursor on "apple" (row 2): show only apples.
        run(&mut d, one(2, 0), (2, 0), DataCmd::KeepValue);
        assert!(d.wb.sheet(0).row_hidden(1) && !d.wb.sheet(0).row_hidden(2) && d.wb.sheet(0).row_hidden(3) && !d.wb.sheet(0).row_hidden(4), "{}", d.status);
        // Sorting with rows hidden is refused.
        run(&mut d, one(2, 1), (2, 1), DataCmd::SortAsc);
        assert!(d.status.starts_with("Show all rows"));
        run(&mut d, one(2, 0), (2, 0), DataCmd::ShowAll);
        assert!(!(1..=4).any(|r| d.wb.sheet(0).row_hidden(r)));
        // Hide apples: the others stay.
        run(&mut d, one(2, 0), (2, 0), DataCmd::HideValue);
        assert!(!d.wb.sheet(0).row_hidden(1) && d.wb.sheet(0).row_hidden(2) && !d.wb.sheet(0).row_hidden(3) && d.wb.sheet(0).row_hidden(4));
        // Then hide figs too: the condition narrows.
        run(&mut d, one(3, 0), (3, 0), DataCmd::HideValue);
        assert!(!d.wb.sheet(0).row_hidden(1) && d.wb.sheet(0).row_hidden(3));
        run(&mut d, one(1, 0), (1, 0), DataCmd::ClearColumn);
        assert!(!(1..=4).any(|r| d.wb.sheet(0).row_hidden(r)));
        run(&mut d, one(1, 0), (1, 0), DataCmd::Filter);
        assert!(d.wb.sheet(0).filter.is_none());
        let labels: Vec<String> = menu(&d, one(1, 0), (1, 0)).into_iter().map(|(m, _)| m.label).collect();
        assert!(labels.contains(&"Show Only “pear”".to_string()), "{labels:?}");
    }

    #[test]
    fn table_and_chart_from_the_region() {
        let mut d = doc();
        run(&mut d, one(1, 1), (1, 1), DataCmd::FormatAsTable);
        let t = d.wb.table("Table1").expect(&d.status).clone();
        assert_eq!(t.columns.iter().map(|c| c.to_string()).collect::<Vec<_>>(), ["Fruit", "Qty", "Price"]);
        d.type_into(0, 5, "=SUM(Table1[Qty])");
        assert_eq!(cell(&d, 0, 5), "21");
        run(&mut d, one(1, 1), (1, 1), DataCmd::Chart(ChartKind::Column));
        let c = d.wb.charts()[0].clone();
        assert_eq!(c.series.len(), 2, "Qty and Price; Fruit is the categories");
        assert_eq!(c.series[0].cats, Some(Rect { sheet: 0, r0: 1, c0: 0, r1: 4, c1: 0 }));
        assert_eq!(c.series[0].name, Some(ChartText::Cell(CellAddr::new(0, 0, 1))));
        // Right of the data, past the formula in F1.
        assert_eq!(c.anchor.from, (0, 6, 0.0, 0.0));
        run(&mut d, one(1, 1), (1, 1), DataCmd::Chart(ChartKind::Pie));
        assert_eq!(d.wb.charts()[1].series.len(), 1);
        // Delete the chart under a cell of its anchor.
        run(&mut d, one(3, 7), (3, 7), DataCmd::DeleteChart);
        assert_eq!(d.wb.charts().len(), 1);
        run(&mut d, one(2, 2), (2, 2), DataCmd::RemoveTable);
        assert!(d.wb.tables().is_empty());
        assert_eq!(cell(&d, 0, 5), "#REF!");
        // Undo all the way: no table, no chart.
        while d.undo() {}
        assert!(d.wb.tables().is_empty() && d.wb.charts().is_empty());
    }
}
