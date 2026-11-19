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
use officina_grid::style::{HAlign, VAlign};

/// The vertical alignment of the selection's first cell.
fn v_at(wb: &Workbook, s: u16, sel: CellRange) -> VAlign {
    wb.style(CellAddr { sheet: s, row: sel.r0, col: sel.c0 }).v_align
}
use officina_grid::tables::Table;
use officina_grid::{Value, Workbook};
use pergola::widgets::{CellRange, MenuItem};

use crate::doc::Doc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataCmd {
    /// The selection's rows hidden, or the hidden ones in it shown.
    HideRows,
    UnhideRows,
    /// The selection's columns hidden (no width), or the hidden ones in it
    /// shown at the default width.
    HideCols,
    UnhideCols,
    /// Paste Special: the cells last copied here, only their values (formula
    /// results), only formulas and values, or only formats.
    PasteValues,
    PasteFormulas,
    PasteFormats,
    /// The cells last copied here with rows turned into columns.
    PasteTransposed,
    /// The copied numbers added to, taken from, multiplied with or
    /// dividing the numbers where they land.
    PasteAdd,
    PasteSubtract,
    PasteMultiply,
    PasteDivide,
    /// The copied cells, blank ones leaving what is under them.
    PasteSkipBlanks,
    /// The bar for an exact height of the selection's rows, or width of
    /// its columns.
    RowHeight,
    ColWidth,
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
    /// Open the value list of the cursor's filter column ([`column_values`]); the
    /// window runs it, as it opens a panel.
    ChooseValues,
    /// Drop every condition: all rows show.
    ShowAll,
    FormatAsTable,
    /// The table at the cursor becomes plain cells.
    RemoveTable,
    Chart(ChartKind),
    /// The chart over the cursor (or using its cell).
    DeleteChart,
    /// A pivot table of the data: the cursor's column down the rows, its number
    /// columns summed.
    InsertPivot,
    /// The pivot table at the cursor, recomputed from its source.
    RefreshPivot,
    RefreshAllPivots,
    DeletePivot,
    /// The cursor's column of the pivot's source as (or no longer as) a column
    /// field of the pivot beside it.
    PivotColumns,
    /// Open the conditional-format panel on the selection ([`crate::condfmt`]); the
    /// window runs it, as it opens a panel.
    CondFormats,
    /// Open the pivot field list of the pivot at the cursor (or whose source holds
    /// it) ([`crate::pivotpane`]); the window runs it.
    PivotFields,
    /// Rows above the selection, as many as it spans.
    InsertRows,
    /// Columns left of the selection, as many as it spans.
    InsertCols,
    /// The selection's rows.
    DeleteRows,
    /// The selection's columns.
    DeleteCols,
    /// The selection as one merged cell, its top-left's value centred across it.
    Merge,
    /// The selection's text centred across its columns without merging
    /// them (again: back to General).
    CenterAcross,
    /// Text in the selection broken onto lines to its column's width, or
    /// not.
    WrapText,
    /// The selection's text at the top, middle or bottom of its cells.
    /// The fill just made, done again: copying, as a series, only its
    /// formats, or without them.
    FillCopy,
    FillSeries,
    FillFormats,
    FillNoFormats,
    AlignTop,
    AlignMiddle,
    AlignBottom,
    /// The merged areas the selection meets, back to separate cells.
    Unmerge,
    /// Write or change the cursor's cell comment; the window runs it (a bar
    /// under the grid).
    Comment,
    /// Take the comments off the selection's cells.
    DeleteComments,
    /// Follow the cursor's link: a place in the workbook, or an address
    /// handed to the link opener; the window runs it.
    OpenLink,
    /// Write or change the cursor's cell link; the window runs it (a bar
    /// under the grid).
    Link,
    /// Take the links off the selection's cells.
    RemoveLinks,
    /// Open the data validation panel on the cursor's rule or the selection
    /// ([`crate::validation`]); the window runs it.
    Validation,
    /// Take validation rules off the selection's cells.
    ClearValidation,
    /// Ring the cells their rule doesn't allow, or stop; the window runs it.
    CircleInvalid,
    /// Pick the cursor's cell value from its validation list; the window
    /// runs it, as it opens a panel (§16.83).
    ChooseFromList,
    /// Keep the rows above the cursor and the columns left of it in view.
    FreezePanes,
    /// Keep the first row in view.
    FreezeTopRow,
    /// Let every row and column scroll again.
    Unfreeze,
    /// Compare this workbook with another copy ([`crate::review`]); the window
    /// asks for the copy.
    CompareWith,
    /// A new sheet at the end; the window runs it, as it shows the sheet.
    AddSheet,
    /// Rename the sheet on screen; the window runs it (a bar under the grid).
    RenameSheet,
    /// Delete the sheet on screen, or move it a place; the window runs them.
    DeleteSheet,
    MoveSheetLeft,
    MoveSheetRight,
    /// The tab colour bar for the sheet on screen; the window runs it.
    TabColor,
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

/// The cells of `sel` with comments.
fn comments_in(wb: &Workbook, s: u16, sel: CellRange) -> impl Iterator<Item = (u32, u32)> + '_ {
    wb.comments(s).range((sel.r0, 0)..=(sel.r1, u32::MAX)).map(|(&k, _)| k).filter(move |&(_, c)| (sel.c0..=sel.c1).contains(&c))
}

fn links_in(wb: &Workbook, s: u16, sel: CellRange) -> impl Iterator<Item = (u32, u32)> + '_ {
    wb.links(s).range((sel.r0, 0)..=(sel.r1, u32::MAX)).map(|(&k, _)| k).filter(move |&(_, c)| (sel.c0..=sel.c1).contains(&c))
}

/// The most cells one merge may span (a whole column of a million rows is
/// fine; a whole sheet is not).
const MAX_MERGE_CELLS: u64 = 1 << 24;

/// The most columns one Hide Columns hides (Excel's sheet width).
const MAX_HIDE_COLS: u32 = 16_384;

/// The Data menu as it reads now, with the command of each item.
/// The selection's top-left cell is centred across: the command then takes
/// it off.
fn across_at(wb: &Workbook, s: u16, sel: CellRange) -> bool {
    wb.style(CellAddr { sheet: s, row: sel.r0, col: sel.c0 }).h_align == HAlign::CenterAcross
}

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
    let (nr, nc) = (sel.r1 - sel.r0 + 1, sel.c1 - sel.c0 + 1);
    let plural = |n: u32, one: &str, many: &str| if n == 1 { one.to_string() } else { format!("{n} {many}") };
    let circling = d.circling;
    let special = d.clip.as_ref().is_some_and(|(c, _)| !c.cut);
    let mut v = vec![
        (MenuItem::new("Fill: Copy Cells").enabled(d.can_refill()), Some(DataCmd::FillCopy)),
        (MenuItem::new("Fill: Series").enabled(d.can_refill()), Some(DataCmd::FillSeries)),
        (MenuItem::new("Fill: Formats Only").enabled(d.can_refill()), Some(DataCmd::FillFormats)),
        (MenuItem::new("Fill: Without Formatting").enabled(d.can_refill()), Some(DataCmd::FillNoFormats)),
        (MenuItem::new("Paste Values").enabled(special).separated(), Some(DataCmd::PasteValues)),
        (MenuItem::new("Paste Formulas").enabled(special), Some(DataCmd::PasteFormulas)),
        (MenuItem::new("Paste Formats").enabled(special), Some(DataCmd::PasteFormats)),
        (MenuItem::new("Paste Transposed").enabled(special), Some(DataCmd::PasteTransposed)),
        (MenuItem::new("Paste Skipping Blanks").enabled(special), Some(DataCmd::PasteSkipBlanks)),
        (MenuItem::new("Paste and Add").enabled(special), Some(DataCmd::PasteAdd)),
        (MenuItem::new("Paste and Subtract").enabled(special), Some(DataCmd::PasteSubtract)),
        (MenuItem::new("Paste and Multiply").enabled(special), Some(DataCmd::PasteMultiply)),
        (MenuItem::new("Paste and Divide").enabled(special), Some(DataCmd::PasteDivide)),
        (MenuItem::new(format!("Insert {} Above", plural(nr, "Row", "Rows"))).separated(), Some(DataCmd::InsertRows)),
        (MenuItem::new(format!("Insert {} Left", plural(nc, "Column", "Columns"))), Some(DataCmd::InsertCols)),
        (MenuItem::new(format!("Delete {}", plural(nr, "Row", "Rows"))), Some(DataCmd::DeleteRows)),
        (MenuItem::new(format!("Delete {}", plural(nc, "Column", "Columns"))), Some(DataCmd::DeleteCols)),
        (MenuItem::new(format!("Hide {}", plural(nr, "Row", "Rows"))).separated(), Some(DataCmd::HideRows)),
        (MenuItem::new("Unhide Rows").enabled(wb.sheet(s).hidden.iter().any(|&(a, b)| a <= sel.r1 && b >= sel.r0)), Some(DataCmd::UnhideRows)),
        (MenuItem::new(format!("Hide {}", plural(nc, "Column", "Columns"))).enabled(nc <= MAX_HIDE_COLS), Some(DataCmd::HideCols)),
        (MenuItem::new("Row Height…"), Some(DataCmd::RowHeight)),
        (MenuItem::new("Column Width…"), Some(DataCmd::ColWidth)),
        (MenuItem::new("Unhide Columns").enabled(wb.sheet(s).col_sizes.overrides().iter().any(|&(c, w)| w <= 0.0 && (sel.c0..=sel.c1).contains(&c))), Some(DataCmd::UnhideCols)),
        (MenuItem::new(if wb.comment(s, row, col).is_some() { "Edit Comment…" } else { "New Comment…" }).separated(), Some(DataCmd::Comment)),
        (MenuItem::new(if nr > 1 || nc > 1 { "Delete Comments" } else { "Delete Comment" }).enabled(comments_in(wb, s, sel).next().is_some()), Some(DataCmd::DeleteComments)),
        (MenuItem::new("Open Link").enabled(wb.link_at(CellAddr::new(s, row, col)).is_some()).separated(), Some(DataCmd::OpenLink)),
        (MenuItem::new(if wb.link(s, row, col).is_some() { "Edit Link…" } else { "Insert Link…" }), Some(DataCmd::Link)),
        (MenuItem::new(if nr > 1 || nc > 1 { "Remove Links" } else { "Remove Link" }).enabled(links_in(wb, s, sel).next().is_some()), Some(DataCmd::RemoveLinks)),
        (MenuItem::new("Choose from List…").enabled(wb.validation_at(CellAddr::new(s, row, col)).is_some_and(|v| v.allow == officina_grid::validation::Allow::List)).separated(), Some(DataCmd::ChooseFromList)),
        if wb.sheet(s).freeze != (0, 0) {
            (MenuItem::new("Unfreeze Panes").separated(), Some(DataCmd::Unfreeze))
        } else {
            (MenuItem::new("Freeze Panes").enabled(cursor != (0, 0)).separated(), Some(DataCmd::FreezePanes))
        },
        if wb.merges_in(&Rect { sheet: s, r0: sel.r0, c0: sel.c0, r1: sel.r1, c1: sel.c1 }).next().is_some() {
            (MenuItem::new("Unmerge Cells").separated(), Some(DataCmd::Unmerge))
        } else {
            (MenuItem::new("Merge Cells").enabled(nr > 1 || nc > 1).separated(), Some(DataCmd::Merge))
        },
        (MenuItem::new("Center Across Selection").checked(across_at(wb, s, sel)).enabled(nc > 1 || across_at(wb, s, sel)), Some(DataCmd::CenterAcross)),
        (MenuItem::new("Wrap Text").checked(wb.style(CellAddr { sheet: s, row: sel.r0, col: sel.c0 }).wrap), Some(DataCmd::WrapText)),
        (MenuItem::new("Align Top").checked(v_at(wb, s, sel) == VAlign::Top), Some(DataCmd::AlignTop)),
        (MenuItem::new("Align Middle").checked(v_at(wb, s, sel) == VAlign::Center), Some(DataCmd::AlignMiddle)),
        (MenuItem::new("Align Bottom").checked(v_at(wb, s, sel) == VAlign::Bottom), Some(DataCmd::AlignBottom)),
        (MenuItem::new("Sort A to Z").enabled(some).separated(), Some(DataCmd::SortAsc)),
        (MenuItem::new("Sort Z to A").enabled(some), Some(DataCmd::SortDesc)),
        (MenuItem::new("Filter").checked(filter.is_some()).enabled(some || filter.is_some()).separated(), Some(DataCmd::Filter)),
        (MenuItem::new(format!("Show Only {shown}")).enabled(in_filter.is_some()), Some(DataCmd::KeepValue)),
        (MenuItem::new(format!("Hide {shown}")).enabled(in_filter.is_some()), Some(DataCmd::HideValue)),
        (MenuItem::new(format!("Choose Values in Column {}…", officina_grid::addr::col_name(col))).enabled(filter.is_some_and(|f| (f.rect.r0..=f.rect.r1).contains(&row) && (f.rect.c0..=f.rect.c1).contains(&col))), Some(DataCmd::ChooseValues)),
        (MenuItem::new(format!("Clear Filter in Column {}", officina_grid::addr::col_name(col))).enabled(col_filtered), Some(DataCmd::ClearColumn)),
        (MenuItem::new("Show All Rows").enabled(any_cond), Some(DataCmd::ShowAll)),
    ];
    if let Some(i) = v.iter().position(|x| x.1 == Some(DataCmd::FreezePanes)) {
        v.insert(i + 1, (MenuItem::new("Freeze Top Row"), Some(DataCmd::FreezeTopRow)));
    }
    match table {
        Some(t) => v.push((MenuItem::new(format!("Convert “{}” to Range", short(&t.name))).separated(), Some(DataCmd::RemoveTable))),
        None => v.push((MenuItem::new("Format as Table").enabled(some).separated(), Some(DataCmd::FormatAsTable))),
    }
    v.push((MenuItem::new("Insert Column Chart").enabled(some).separated(), Some(DataCmd::Chart(ChartKind::Column))));
    v.push((MenuItem::new("Insert Line Chart").enabled(some), Some(DataCmd::Chart(ChartKind::Line))));
    v.push((MenuItem::new("Insert Pie Chart").enabled(some), Some(DataCmd::Chart(ChartKind::Pie))));
    v.push((MenuItem::new("Delete Chart").enabled(chart_at(wb, s, row, col).is_some()), Some(DataCmd::DeleteChart)));
    let here = wb.pivot_at(CellAddr::new(s, row, col));
    match here {
        Some((i, _)) => {
            let name = short(&wb.pivots()[i].name);
            v.push((MenuItem::new(format!("Refresh “{name}”")).separated(), Some(DataCmd::RefreshPivot)));
            v.push((MenuItem::new(format!("Delete “{name}”")), Some(DataCmd::DeletePivot)));
        }
        None => {
            v.push((MenuItem::new("Insert Pivot Table").enabled(some).separated(), Some(DataCmd::InsertPivot)));
            let src = pivot_of_source(wb, s, row, col);
            let label = match src {
                Some((i, f)) if wb.pivots()[i].cols.iter().any(|x| x.field == f) => format!("Take “{}” off the Pivot's Columns", short(&wb.pivot_field_name(&wb.pivots()[i], f))),
                Some((i, f)) => format!("Pivot “{}” Across the Columns", short(&wb.pivot_field_name(&wb.pivots()[i], f))),
                None => "Pivot This Column Across".to_string(),
            };
            v.push((MenuItem::new(label).enabled(src.is_some()), Some(DataCmd::PivotColumns)));
        }
    }
    v.push((MenuItem::new("Pivot Table Fields…").enabled(crate::pivotpane::pivot_here(wb, s, row, col).is_some()), Some(DataCmd::PivotFields)));
    v.push((MenuItem::new("Refresh All Pivot Tables").enabled(!wb.pivots().is_empty()), Some(DataCmd::RefreshAllPivots)));
    v.push((MenuItem::new("Conditional Formatting…").separated(), Some(DataCmd::CondFormats)));
    let sel_rect = Rect { sheet: s, r0: sel.r0, c0: sel.c0, r1: sel.r1, c1: sel.c1 };
    let validated = wb.validations().iter().any(|v| v.ranges.iter().any(|r| r.sheet == s && r.r0 <= sel_rect.r1 && sel_rect.r0 <= r.r1 && r.c0 <= sel_rect.c1 && sel_rect.c0 <= r.c1));
    v.push((MenuItem::new("Data Validation…"), Some(DataCmd::Validation)));
    v.push((MenuItem::new("Clear Validation").enabled(validated), Some(DataCmd::ClearValidation)));
    v.push((MenuItem::new("Circle Invalid Data").checked(circling).enabled(!wb.validations().is_empty() || circling), Some(DataCmd::CircleInvalid)));
    v.push((MenuItem::new("Compare with…").separated(), Some(DataCmd::CompareWith)));
    v.push((MenuItem::new("Add Sheet").separated(), Some(DataCmd::AddSheet)));
    v.push((MenuItem::new("Rename Sheet…"), Some(DataCmd::RenameSheet)));
    v.push((MenuItem::new("Tab Colour…"), Some(DataCmd::TabColor)));
    let n = wb.sheet_count() as u16;
    v.push((MenuItem::new("Move Sheet Left").enabled(s > 0), Some(DataCmd::MoveSheetLeft)));
    v.push((MenuItem::new("Move Sheet Right").enabled(s + 1 < n), Some(DataCmd::MoveSheetRight)));
    v.push((MenuItem::new("Delete Sheet").enabled(n > 1), Some(DataCmd::DeleteSheet)));
    v
}

/// The pivot whose source holds the cursor, and the cursor's field of it.
fn pivot_of_source(wb: &Workbook, sheet: u16, row: u32, col: u32) -> Option<(usize, u32)> {
    wb.pivots().iter().position(|p| p.source.sheet == sheet && (p.source.r0..=p.source.r1).contains(&row) && (p.source.c0..=p.source.c1).contains(&col)).map(|i| (i, col - wb.pivots()[i].source.c0))
}

/// A first pivot of `t`: the cursor's column (or the first text column) down the
/// rows, the other number columns summed (at most three), or a count when there
/// are none.
pub fn pivot_for(wb: &Workbook, t: &Target, name: &str) -> Option<officina_grid::pivot::Pivot> {
    use officina_grid::pivot::{Agg, DataField, Pivot, PivotField};
    let r = t.rect;
    if !t.header || r.r1 <= r.r0 {
        return None;
    }
    let s = r.sheet;
    let numeric = |c: u32| {
        let n = (r.r0 + 1..=r.r1).filter(|&row| matches!(wb.get(CellAddr::new(s, row, c)), Value::Num(_))).count();
        n * 2 > (r.r1 - r.r0) as usize
    };
    let cursor = (r.c0..=r.c1).contains(&t.col).then_some(t.col).filter(|&c| !numeric(c));
    let rows = cursor.or_else(|| (r.c0..=r.c1).find(|&c| !numeric(c)))?;
    let data: Vec<DataField> = (r.c0..=r.c1).filter(|&c| c != rows && numeric(c)).take(3).map(|c| DataField { field: c - r.c0, agg: Agg::Sum, name: None }).collect();
    let data = if data.is_empty() { vec![DataField { field: rows - r.c0, agg: Agg::Count, name: None }] } else { data };
    let mut p = Pivot::new(name, r, CellAddr::new(s, r.r0, (r.c1 + 2).min(officina_grid::addr::MAX_COLS - 1)));
    p.rows = vec![PivotField::new(rows - r.c0)];
    p.data = data;
    Some(p)
}

/// Runs `cmd`; says what happened (or why nothing did) in the status line.
pub fn run(d: &mut Doc, sel: CellRange, cursor: (u32, u32), cmd: DataCmd) {
    // Sorting, filtering and pivots read values: finish recalculating first.
    if d.wb.recalc_pending() {
        d.wb.recalc();
    }
    let msg = match apply(d, sel, cursor, cmd) {
        Ok(m) | Err(m) => m,
    };
    d.status = msg;
}

/// The most values a column's value list offers.
pub const MAX_VALUES: usize = 10_000;

/// A filter column's values to choose from: the column (of the sheet), then each
/// distinct shown text with how many rows show it and whether the filter keeps it
/// now. `Err` says why there is no list (no filter there, too many values).
/// A double-click on the double line where `row` is hidden (by hand or
/// filtered): the hidden rows around it shown again, as Unhide Rows would.
/// False when `row` isn't hidden.
pub fn unhide_rows_at(d: &mut Doc, row: u32) -> bool {
    let sh = d.wb.sheet(d.sheet);
    if !sh.row_hidden(row) {
        return false;
    }
    let (mut lo, mut hi) = (row, row);
    while lo > 0 && sh.row_hidden(lo - 1) && row - lo < MAX_HIDE_ROWS_SCAN {
        lo -= 1;
    }
    while hi + 1 < officina_grid::addr::MAX_ROWS && sh.row_hidden(hi + 1) && hi - row < MAX_HIDE_ROWS_SCAN {
        hi += 1;
    }
    run(d, CellRange { r0: lo, c0: 0, r1: hi, c1: 0 }, (lo, 0), DataCmd::UnhideRows);
    true
}

/// How far a double-click on hidden rows' line looks for the run's ends.
const MAX_HIDE_ROWS_SCAN: u32 = 1 << 20;

/// The same for hidden columns around `col`.
pub fn unhide_cols_at(d: &mut Doc, col: u32) -> bool {
    let sizes = &d.wb.sheet(d.sheet).col_sizes;
    if sizes.size(col) > 0.0 {
        return false;
    }
    let (mut lo, mut hi) = (col, col);
    while lo > 0 && col - lo < MAX_HIDE_COLS && sizes.size(lo - 1) <= 0.0 {
        lo -= 1;
    }
    while hi < officina_grid::addr::MAX_COLS - 1 && hi - lo + 1 < MAX_HIDE_COLS && sizes.size(hi + 1) <= 0.0 {
        hi += 1;
    }
    run(d, CellRange { r0: 0, c0: lo, r1: 0, c1: hi }, (0, lo), DataCmd::UnhideCols);
    true
}

pub fn column_values(d: &Doc, col: u32) -> Result<Vec<(String, u32, bool)>, String> {
    let f = d.wb.sheet(d.sheet).filter.as_ref().ok_or("There is no filter on this sheet.")?;
    if !(f.rect.c0..=f.rect.c1).contains(&col) {
        return Err("Put the cursor in a column of the filter.".into());
    }
    let all = d.wb.filter_values(&f.rect, col, MAX_VALUES + 1);
    if all.len() > MAX_VALUES {
        return Err(format!("This column has more than {MAX_VALUES} different values; use Show Only or Hide."));
    }
    let now = f.conds.iter().find(|(c, _)| *c == col).map(|(_, c)| c.clone());
    let key = officina_grid::eval::funcs::text_key;
    Ok(all
        .into_iter()
        .map(|(t, n)| {
            let on = match &now {
                Some(Cond::Values(v)) => v.iter().any(|x| key(x) == key(&t)),
                // A number condition: what it keeps now, as rows show it.
                Some(Cond::Compare(..)) => (f.rect.r0 + 1..=f.rect.r1).any(|r| !d.wb.sheet(d.sheet).row_hidden(r) && key(&d.wb.formatted(CellAddr::new(d.sheet, r, col)).text) == key(&t)),
                None => true,
            };
            (t, n, on)
        })
        .collect())
}

/// Keeps the rows of column `col` showing one of `keep` (every row when `keep`
/// holds every value: the column's condition goes), one undo step.
pub fn keep_values(d: &mut Doc, col: u32, keep: Vec<String>, every: bool) {
    let msg = (|| {
        let s = d.sheet;
        let f = d.wb.sheet(s).filter.clone().ok_or("There is no filter on this sheet.")?;
        let mut conds = f.conds.clone();
        conds.retain(|(c, _)| *c != col);
        if !every {
            conds.push((col, Cond::Values(keep)));
            conds.sort_by_key(|(c, _)| *c);
        }
        if conds == f.conds {
            return Ok::<String, String>("The filter is as it was.".into());
        }
        let st = d.wb.filtered(s, Some(AutoFilter { rect: f.rect, conds }));
        let hidden = hidden_in(&st, &f.rect);
        d.run("Filter", vec![GridOp::Filter(st)]);
        Ok(if hidden == 0 { "Every row shows.".into() } else { format!("{hidden} rows hidden by the filter.") })
    })();
    d.status = match msg {
        Ok(m) | Err(m) => m,
    };
}

fn hidden_in(st: &officina_grid::filter::FilterState, r: &Rect) -> u32 {
    st.hidden.iter().filter(|(a, b)| *b > r.r0 && *a <= r.r1).map(|(a, b)| b.min(&r.r1) - a.max(&(r.r0 + 1)) + 1).sum()
}

fn apply(d: &mut Doc, sel: CellRange, cursor: (u32, u32), cmd: DataCmd) -> Result<String, String> {
    let s = d.sheet;
    let (row, col) = cursor;
    let need = || "Put the cursor in your data (or select it) first.".to_string();
    match cmd {
        DataCmd::HideRows | DataCmd::UnhideRows => {
            let mut st = d.wb.filter_state(s);
            let had: u64 = st.hidden.iter().map(|(a, b)| (b - a + 1) as u64).sum();
            if cmd == DataCmd::HideRows {
                st.hidden.push((sel.r0, sel.r1));
            } else {
                st.hidden = st.hidden.iter().flat_map(|&(a, b)| {
                    let mut keep = Vec::new();
                    if a < sel.r0 {
                        keep.push((a, b.min(sel.r0 - 1)));
                    }
                    if b > sel.r1 {
                        keep.push((a.max(sel.r1 + 1), b));
                    }
                    if b < sel.r0 || a > sel.r1 {
                        keep = vec![(a, b)];
                    }
                    keep
                }).collect();
            }
            st.hidden = officina_grid::filter::merge(st.hidden);
            let now: u64 = st.hidden.iter().map(|(a, b)| (b - a + 1) as u64).sum();
            if now == had {
                return Err(if cmd == DataCmd::HideRows { "Those rows are hidden already.".into() } else { "No hidden rows there. Select the rows around them.".into() });
            }
            d.run(if cmd == DataCmd::HideRows { "Hide Rows" } else { "Unhide Rows" }, vec![GridOp::Filter(st)]);
            let n = now.abs_diff(had);
            Ok(if cmd == DataCmd::HideRows { format!("{n} rows hidden.") } else { format!("{n} rows shown.") })
        }
        DataCmd::HideCols | DataCmd::UnhideCols => {
            if cmd == DataCmd::HideCols && sel.c1 - sel.c0 + 1 > MAX_HIDE_COLS {
                return Err(format!("Hide at most {MAX_HIDE_COLS} columns at once."));
            }
            let old = d.wb.sheet(s).col_sizes.overrides().to_vec();
            let inside = |c: u32| (sel.c0..=sel.c1).contains(&c);
            let mut over: Vec<(u32, f64)> = old.iter().copied().filter(|&(c, w)| !inside(c) || (cmd == DataCmd::UnhideCols && w > 0.0)).collect();
            if cmd == DataCmd::HideCols {
                // Remember the widths, to show them again as they were.
                for c in sel.c0..=sel.c1 {
                    let w = d.wb.sheet(s).col_sizes.size(c);
                    if w > 0.0 {
                        d.hidden_widths.insert((s, c), w);
                    }
                }
                over.extend((sel.c0..=sel.c1).map(|c| (c, 0.0)));
            } else {
                for &(c, w) in &old {
                    if inside(c) && w <= 0.0 {
                        if let Some(&was) = d.hidden_widths.get(&(s, c)) {
                            over.push((c, was));
                        }
                    }
                }
            }
            over.sort_by_key(|o| o.0);
            if over == old {
                return Err(if cmd == DataCmd::HideCols { "Those columns are hidden already.".into() } else { "No hidden columns there. Select the columns around them.".into() });
            }
            let hidden = |v: &[(u32, f64)]| v.iter().filter(|o| o.1 <= 0.0).count();
            let n = hidden(&over).abs_diff(hidden(&old));
            d.run(if cmd == DataCmd::HideCols { "Hide Columns" } else { "Unhide Columns" }, vec![GridOp::Sizes { sheet: s, rows: false, over }]);
            Ok(if cmd == DataCmd::HideCols { format!("{n} columns hidden.") } else { format!("{n} columns shown.") })
        }
        DataCmd::PasteValues
        | DataCmd::PasteFormulas
        | DataCmd::PasteFormats
        | DataCmd::PasteTransposed
        | DataCmd::PasteSkipBlanks
        | DataCmd::PasteAdd
        | DataCmd::PasteSubtract
        | DataCmd::PasteMultiply
        | DataCmd::PasteDivide => {
            use officina_grid::clip::{Arith, Special, What};
            let of = Special::of;
            let math = |a| Special { arith: Some(a), ..of(What::Values) };
            let (sp, label) = match cmd {
                DataCmd::PasteValues => (of(What::Values), "Paste Values"),
                DataCmd::PasteFormulas => (of(What::Formulas), "Paste Formulas"),
                DataCmd::PasteFormats => (of(What::Formats), "Paste Formats"),
                DataCmd::PasteSkipBlanks => (Special { skip_blanks: true, ..of(What::All) }, "Paste Skipping Blanks"),
                DataCmd::PasteAdd => (math(Arith::Add), "Paste and Add"),
                DataCmd::PasteSubtract => (math(Arith::Sub), "Paste and Subtract"),
                DataCmd::PasteMultiply => (math(Arith::Mul), "Paste and Multiply"),
                DataCmd::PasteDivide => (math(Arith::Div), "Paste and Divide"),
                _ => (Special { transpose: true, ..of(What::All) }, "Paste Transposed"),
            };
            let Some((clip, _)) = d.clip.as_ref() else {
                return Err("Copy some cells first.".into());
            };
            let (ops, _) = clip.paste_special(&d.wb, s, sel.r0, sel.c0, sp)?;
            d.run(label, ops);
            Ok(format!("{label}: done."))
        }
        DataCmd::CenterAcross => {
            let r = Rect { sheet: s, r0: sel.r0, c0: sel.c0, r1: sel.r1, c1: sel.c1 };
            let off = across_at(&d.wb, s, sel);
            if !off && r.c1 == r.c0 {
                return Err("Select the columns to centre across first.".into());
            }
            let to = if off { HAlign::General } else { HAlign::CenterAcross };
            d.run("Center Across Selection", vec![GridOp::restyle(&d.wb, r, |st| officina_grid::style::Style { h_align: to, ..st.clone() })]);
            Ok(if off { "Text no longer centred across the selection.".into() } else { "Text centred across the selection.".into() })
        }
        DataCmd::FillCopy | DataCmd::FillSeries | DataCmd::FillFormats | DataCmd::FillNoFormats => {
            use crate::doc::FillHow;
            let how = match cmd {
                DataCmd::FillCopy => FillHow::Copy,
                DataCmd::FillSeries => FillHow::Series,
                DataCmd::FillFormats => FillHow::FormatsOnly,
                _ => FillHow::NoFormats,
            };
            if d.refill(how) { Ok("Filled again.".into()) } else { Err(std::mem::take(&mut d.status)) }
        }
        DataCmd::AlignTop | DataCmd::AlignMiddle | DataCmd::AlignBottom => {
            let r = Rect { sheet: s, r0: sel.r0, c0: sel.c0, r1: sel.r1, c1: sel.c1 };
            let (to, word) = match cmd {
                DataCmd::AlignTop => (VAlign::Top, "top"),
                DataCmd::AlignMiddle => (VAlign::Center, "middle"),
                _ => (VAlign::Bottom, "bottom"),
            };
            d.run("Vertical Alignment", vec![GridOp::restyle(&d.wb, r, |st| officina_grid::style::Style { v_align: to, ..st.clone() })]);
            Ok(format!("Text at the {word} of its cells."))
        }
        DataCmd::WrapText => {
            let r = Rect { sheet: s, r0: sel.r0, c0: sel.c0, r1: sel.r1, c1: sel.c1 };
            let on = !d.wb.style(CellAddr { sheet: s, row: sel.r0, col: sel.c0 }).wrap;
            d.run("Wrap Text", vec![GridOp::restyle(&d.wb, r, |st| officina_grid::style::Style { wrap: on, ..st.clone() })]);
            Ok(if on { "Text wraps in its cells.".into() } else { "Text no longer wraps.".into() })
        }
        DataCmd::Merge => {
            let r = d.wb.grow_to_merges(Rect { sheet: s, r0: sel.r0, c0: sel.c0, r1: sel.r1, c1: sel.c1 });
            if r.cells() < 2 {
                return Err("Select two or more cells to merge.".into());
            }
            if r.cells() > MAX_MERGE_CELLS {
                return Err("That's too many cells to merge into one.".into());
            }
            // As Excel does: only the top-left value stays (the others are cleared).
            let mut others = 0u64;
            d.wb.each(&r, &mut |row, col, v| {
                if (row, col) != (r.r0, r.c0) && v != officina_grid::Value::Empty {
                    others += 1;
                }
            });
            let mut ops = Vec::new();
            if r.c1 > r.c0 {
                ops.push(GridOp::clear(Rect { r0: r.r0, r1: r.r0, c0: r.c0 + 1, ..r }));
            }
            if r.r1 > r.r0 {
                ops.push(GridOp::clear(Rect { r0: r.r0 + 1, ..r }));
            }
            ops.push(GridOp::restyle(&d.wb, Rect { r1: r.r0, c1: r.c0, ..r }, |st| officina_grid::style::Style { h_align: officina_grid::style::HAlign::Center, ..st.clone() }));
            let mut list: Vec<Rect> = d.wb.merges(s).iter().filter(|m| !m.intersects(&r)).copied().collect();
            list.push(r);
            ops.push(GridOp::Merges { sheet: s, list });
            d.run("Merge Cells", ops);
            Ok(match others {
                0 => "Cells merged.".into(),
                1 => "Cells merged; the one other value in them was cleared.".into(),
                n => format!("Cells merged; the {n} other values in them were cleared."),
            })
        }
        DataCmd::DeleteComments => {
            let set: Vec<_> = comments_in(&d.wb, s, sel).map(|(r, c)| (r, c, None)).collect();
            if set.is_empty() {
                return Ok("No comments there.".into());
            }
            let n = set.len();
            d.run(if n == 1 { "Delete Comment" } else { "Delete Comments" }, vec![GridOp::Comments { sheet: s, set }]);
            Ok(if n == 1 { "Comment deleted.".into() } else { format!("{n} comments deleted.") })
        }
        DataCmd::FreezePanes | DataCmd::FreezeTopRow | DataCmd::Unfreeze => {
            let (rows, cols) = match cmd {
                DataCmd::FreezePanes => cursor,
                DataCmd::FreezeTopRow => (1, 0),
                _ => (0, 0),
            };
            if d.wb.sheet(s).freeze == (rows, cols) {
                return Ok(String::new());
            }
            d.run(if (rows, cols) == (0, 0) { "Unfreeze Panes" } else { "Freeze Panes" }, vec![GridOp::Freeze { sheet: s, rows, cols }]);
            let rs = if rows == 1 { "top row".to_string() } else { format!("top {rows} rows") };
            let cs = if cols == 1 { "first column".to_string() } else { format!("first {cols} columns") };
            Ok(match (rows, cols) {
                (0, 0) => "Every row and column scrolls again.".into(),
                (1, 0) => "The top row stays in view.".into(),
                (_, 0) => format!("The {rs} stay in view."),
                (0, 1) => "The first column stays in view.".into(),
                (0, _) => format!("The {cs} stay in view."),
                _ => format!("The {rs} and {cs} stay in view."),
            })
        }
        DataCmd::ClearValidation => Ok(if crate::validation::clear(d, Rect { sheet: s, r0: sel.r0, c0: sel.c0, r1: sel.r1, c1: sel.c1 }) { "Validation cleared.".into() } else { "No validation there.".into() }),
        DataCmd::RemoveLinks => {
            let set: Vec<_> = links_in(&d.wb, s, sel).map(|(r, c)| (r, c, None)).collect();
            if set.is_empty() {
                return Ok("No links there.".into());
            }
            let n = set.len();
            d.run(if n == 1 { "Remove Link" } else { "Remove Links" }, vec![GridOp::Links { sheet: s, set }]);
            Ok(if n == 1 { "Link removed.".into() } else { format!("{n} links removed.") })
        }
        DataCmd::Unmerge => {
            let r = Rect { sheet: s, r0: sel.r0, c0: sel.c0, r1: sel.r1, c1: sel.c1 };
            let list: Vec<Rect> = d.wb.merges(s).iter().filter(|m| !m.intersects(&r)).copied().collect();
            if list.len() == d.wb.merges(s).len() {
                return Ok("Nothing merged there.".into());
            }
            d.run("Unmerge Cells", vec![GridOp::Merges { sheet: s, list }]);
            Ok("Cells unmerged.".into())
        }
        DataCmd::InsertRows | DataCmd::InsertCols | DataCmd::DeleteRows | DataCmd::DeleteCols => {
            use officina_grid::shift::{shift_op, Axis};
            let (axis, at, n, what) = match cmd {
                DataCmd::InsertRows => (Axis::Rows, sel.r0, (sel.r1 - sel.r0 + 1) as i64, "Insert Rows"),
                DataCmd::InsertCols => (Axis::Cols, sel.c0, (sel.c1 - sel.c0 + 1) as i64, "Insert Columns"),
                DataCmd::DeleteRows => (Axis::Rows, sel.r0, -((sel.r1 - sel.r0 + 1) as i64), "Delete Rows"),
                _ => (Axis::Cols, sel.c0, -((sel.c1 - sel.c0 + 1) as i64), "Delete Columns"),
            };
            let _ = (row, col);
            match shift_op(&d.wb, s, axis, at, n) {
                Some(op) => {
                    d.run(what, vec![op]);
                    let k = n.unsigned_abs();
                    let thing = match (axis, k) {
                        (Axis::Rows, 1) => "1 row".to_string(),
                        (Axis::Rows, k) => format!("{k} rows"),
                        (Axis::Cols, 1) => "1 column".to_string(),
                        (Axis::Cols, k) => format!("{k} columns"),
                    };
                    Ok(format!("{thing} {}.", if n > 0 { "inserted" } else { "deleted" }))
                }
                None if n > 0 => Err("There's no room: the sheet's last rows or columns are in use.".into()),
                None => Ok("Nothing there to delete.".into()),
            }
        }
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
            if !officina_grid::sort::merges_allow_sort(&d.wb, &rect) {
                return Err("To sort, every row needs the same merged cells; unmerge them first.".into());
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
            let hidden = hidden_in(&st, &f.rect);
            d.run("Filter", vec![GridOp::Filter(st)]);
            Ok(if hidden == 0 { "Every row shows.".into() } else { format!("{hidden} rows hidden by the filter.") })
        }
        // The window opens its value list for this one.
        DataCmd::CondFormats | DataCmd::PivotFields | DataCmd::CompareWith | DataCmd::AddSheet | DataCmd::RenameSheet | DataCmd::DeleteSheet | DataCmd::MoveSheetLeft | DataCmd::MoveSheetRight | DataCmd::TabColor | DataCmd::Comment | DataCmd::OpenLink | DataCmd::Link | DataCmd::ChooseFromList | DataCmd::Validation | DataCmd::CircleInvalid | DataCmd::RowHeight | DataCmd::ColWidth => Ok(String::new()),
        DataCmd::ChooseValues => column_values(d, col).map(|v| format!("{} values in column {}.", v.len(), officina_grid::addr::col_name(col))),
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
        DataCmd::InsertPivot => {
            let t = target(d, sel, cursor).ok_or_else(need)?;
            let name = d.wb.new_pivot_name();
            let p = pivot_for(&d.wb, &t, &name).ok_or("A pivot table needs a header row and a column of names or labels to group by.")?;
            let field = d.wb.pivot_field_name(&p, p.rows[0].field);
            let place = officina_grid::addr::a1(p.dest.row, p.dest.col);
            let op = d.wb.pivot_op(None, Some(p)).map_err(|e| format!("Couldn't place the pivot table at {place}: {e}."))?;
            d.run("Insert Pivot Table", vec![op]);
            Ok(format!("{name} at {place}: {} by {field}. Put the cursor in the data and use Pivot Across to add a column field.", range_text(&t.rect)))
        }
        DataCmd::PivotColumns => {
            let (i, f) = pivot_of_source(&d.wb, s, row, col).ok_or("Put the cursor in a pivot table's source data first.")?;
            let mut p = d.wb.pivots()[i].clone();
            let name = d.wb.pivot_field_name(&p, f);
            let on = p.cols.iter().any(|x| x.field == f);
            if on {
                p.cols.retain(|x| x.field != f);
            } else {
                p.rows.retain(|x| x.field != f);
                p.pages.retain(|x| x.field != f);
                if p.rows.is_empty() {
                    return Err(format!("{name} is the pivot's only row field; it stays down the rows."));
                }
                p.cols.push(officina_grid::pivot::PivotField::new(f));
            }
            let op = d.wb.pivot_op(Some(i), Some(p)).map_err(|e| format!("Couldn't change the pivot table: {e}."))?;
            d.run("Change Pivot Table", vec![op]);
            Ok(if on { format!("{name} taken off the columns.") } else { format!("{name} now runs across the columns.") })
        }
        DataCmd::RefreshPivot | DataCmd::DeletePivot => {
            let (i, _) = d.wb.pivot_at(CellAddr::new(s, row, col)).ok_or("The cursor isn't in a pivot table.")?;
            let p = d.wb.pivots()[i].clone();
            let name = p.name.clone();
            if cmd == DataCmd::DeletePivot {
                let op = d.wb.pivot_op(Some(i), None).map_err(|e| e.to_string())?;
                d.run("Delete Pivot Table", vec![op]);
                return Ok(format!("{name} deleted."));
            }
            let op = d.wb.pivot_op(Some(i), Some(p)).map_err(|e| format!("Couldn't refresh {name}: {e}."))?;
            d.run("Refresh Pivot Table", vec![op]);
            Ok(format!("{name} refreshed from its data."))
        }
        DataCmd::RefreshAllPivots => {
            // Each one after the last (one may grow into where another was).
            let mut bad = Vec::new();
            let mut n = 0;
            for i in 0..d.wb.pivots().len() {
                let p = d.wb.pivots()[i].clone();
                match d.wb.pivot_op(Some(i), Some(p.clone())) {
                    Ok(op) => {
                        d.run("Refresh Pivot Table", vec![op]);
                        n += 1;
                    }
                    Err(e) => bad.push(format!("{}: {e}", p.name)),
                }
            }
            if bad.is_empty() {
                Ok(format!("{n} pivot tables refreshed."))
            } else {
                Err(format!("{n} refreshed; couldn't refresh {}.", bad.join("; ")))
            }
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
    fn panes_freeze_at_the_cursor_and_unfreeze() {
        let mut d = doc();
        let titles = |d: &Doc| menu(d, one(2, 1), (2, 1)).into_iter().map(|(m, _)| m.label).collect::<Vec<_>>();
        assert!(titles(&d).iter().any(|t| t == "Freeze Top Row"));
        run(&mut d, one(2, 1), (2, 1), DataCmd::FreezePanes);
        assert_eq!(d.wb.sheet(0).freeze, (2, 1));
        assert_eq!(d.status, "The top 2 rows and first column stay in view.");
        assert!(titles(&d).iter().any(|t| t == "Unfreeze Panes"));
        run(&mut d, one(2, 1), (2, 1), DataCmd::Unfreeze);
        assert_eq!(d.wb.sheet(0).freeze, (0, 0));
        run(&mut d, one(2, 1), (2, 1), DataCmd::FreezeTopRow);
        assert_eq!(d.status, "The top row stays in view.");
        assert!(d.undo());
        assert_eq!(d.wb.sheet(0).freeze, (0, 0));
    }

    #[test]
    fn rows_merged_alike_sort() {
        let mut d = doc();
        // Qty:Price merged on every data row: the rows sort, the merges stay.
        let list: Vec<Rect> = (1..=4).map(|r| Rect { sheet: 0, r0: r, c0: 1, r1: r, c1: 2 }).collect();
        d.run("Merge Cells", vec![GridOp::Merges { sheet: 0, list: list.clone() }]);
        run(&mut d, one(2, 0), (2, 0), DataCmd::SortAsc);
        assert!(d.status.starts_with("Sorted 4 rows"), "{}", d.status);
        assert_eq!((1..=4).map(|r| cell(&d, r, 0)).collect::<Vec<_>>(), ["apple", "apple", "fig", "pear"]);
        assert_eq!(cell(&d, 4, 1), "3");
        assert_eq!(d.wb.merges(0), &list[..]);
    }

    #[test]
    fn paste_special_is_offered_after_a_copy_and_pastes_values() {
        let mut d = doc();
        let item = |d: &Doc| menu(d, one(0, 0), (0, 0)).into_iter().find(|(m, _)| m.label == "Paste Values").unwrap().0.enabled;
        assert!(!item(&d));
        d.type_into(0, 5, "=ROW()*3");
        let r = Rect { sheet: 0, r0: 0, c0: 5, r1: 0, c1: 5 };
        d.take_clip(r, false, String::new());
        assert!(item(&d));
        run(&mut d, one(9, 5), (9, 5), DataCmd::PasteValues);
        assert_eq!(d.status, "Paste Values: done.");
        assert_eq!(d.wb.formula_text(CellAddr { sheet: 0, row: 9, col: 5 }), None);
        assert_eq!(cell(&d, 9, 5), "3");
        run(&mut d, one(12, 0), (12, 0), DataCmd::PasteFormulas);
        assert_eq!(d.wb.formula_text(CellAddr { sheet: 0, row: 12, col: 0 }).as_deref(), Some("=ROW()*3"));
        assert_eq!(cell(&d, 12, 0), "39");
        // Paste and Add: 3 more on what is there.
        d.type_into(20, 5, "4");
        run(&mut d, one(20, 5), (20, 5), DataCmd::PasteAdd);
        assert_eq!((d.status.as_str(), cell(&d, 20, 5)), ("Paste and Add: done.", "7".to_string()));
        // After a cut: not offered.
        d.take_clip(r, true, String::new());
        assert!(!item(&d));
    }

    #[test]
    fn rows_and_columns_are_hidden_and_shown_again() {
        let mut d = doc();
        run(&mut d, CellRange { r0: 2, c0: 0, r1: 3, c1: 0 }, (2, 0), DataCmd::HideRows);
        assert_eq!(d.status, "2 rows hidden.");
        assert!(d.wb.sheet(0).row_hidden(2) && d.wb.sheet(0).row_hidden(3) && !d.wb.sheet(0).row_hidden(4));
        // Unhide over the rows around them.
        let around = CellRange { r0: 1, c0: 0, r1: 4, c1: 0 };
        assert!(menu(&d, around, (1, 0)).iter().any(|(m, _)| m.label == "Unhide Rows" && m.enabled));
        run(&mut d, around, (1, 0), DataCmd::UnhideRows);
        assert_eq!(d.status, "2 rows shown.");
        assert!(d.wb.sheet(0).hidden.is_empty());
        d.set_col_width(2, 120.0);
        run(&mut d, CellRange { r0: 0, c0: 1, r1: 0, c1: 2 }, (0, 1), DataCmd::HideCols);
        assert_eq!(d.status, "2 columns hidden.");
        assert_eq!(d.wb.sheet(0).col_sizes.size(1), 0.0);
        run(&mut d, CellRange { r0: 0, c0: 0, r1: 0, c1: 3 }, (0, 0), DataCmd::UnhideCols);
        assert_eq!(d.status, "2 columns shown.");
        // C comes back as wide as it was; B at the default.
        assert_eq!((d.wb.sheet(0).col_sizes.size(1), d.wb.sheet(0).col_sizes.size(2)), (64.0, 120.0));
        assert!(d.undo());
        assert_eq!(d.wb.sheet(0).col_sizes.size(2), 0.0);
        run(&mut d, one(0, 5), (0, 5), DataCmd::UnhideCols);
        assert!(d.status.starts_with("No hidden columns"), "{}", d.status);
        // A double-click on the double line shows the run there.
        run(&mut d, CellRange { r0: 0, c0: 1, r1: 0, c1: 2 }, (0, 1), DataCmd::HideCols);
        assert!(!unhide_cols_at(&mut d, 0));
        assert!(unhide_cols_at(&mut d, 2));
        assert_eq!((d.wb.sheet(0).col_sizes.size(1), d.wb.sheet(0).col_sizes.size(2)), (64.0, 120.0));
    }

    #[test]
    fn center_across_selection_turns_on_and_off_without_merging() {
        let mut d = doc();
        let sel = CellRange { r0: 0, c0: 0, r1: 0, c1: 2 };
        let item = |d: &Doc, sel| menu(d, sel, (0, 0)).into_iter().find(|(m, _)| m.label == "Center Across Selection").unwrap().0;
        assert!(item(&d, sel).enabled && item(&d, sel).mark == pergola::widgets::Mark::Check(false));
        assert!(!item(&d, one(0, 0)).enabled);
        let before = cell(&d, 0, 1);
        run(&mut d, sel, (0, 0), DataCmd::CenterAcross);
        assert_eq!(d.status, "Text centred across the selection.");
        let at = |d: &Doc, c| d.wb.style(CellAddr { sheet: 0, row: 0, col: c }).h_align;
        assert!((0..=2).all(|c| at(&d, c) == HAlign::CenterAcross));
        assert!(d.wb.merges(0).is_empty());
        assert_eq!(cell(&d, 0, 1), before);
        assert!(item(&d, sel).mark == pergola::widgets::Mark::Check(true));
        // Again: back to General; undo brings it back.
        run(&mut d, sel, (0, 0), DataCmd::CenterAcross);
        assert!((0..=2).all(|c| at(&d, c) == HAlign::General));
        assert!(d.undo());
        assert!((0..=2).all(|c| at(&d, c) == HAlign::CenterAcross));
    }

    #[test]
    fn wrap_text_toggles_and_a_hidden_row_s_line_shows_it() {
        let mut d = doc();
        let sel = CellRange { r0: 0, c0: 0, r1: 1, c1: 1 };
        run(&mut d, sel, (0, 0), DataCmd::WrapText);
        assert_eq!(d.status, "Text wraps in its cells.");
        assert!(d.wb.style(CellAddr { sheet: 0, row: 1, col: 1 }).wrap);
        assert!(menu(&d, sel, (0, 0)).iter().any(|(m, _)| m.label == "Wrap Text" && m.mark == pergola::widgets::Mark::Check(true)));
        run(&mut d, sel, (0, 0), DataCmd::WrapText);
        assert!(!d.wb.style(CellAddr { sheet: 0, row: 0, col: 0 }).wrap);
        // Vertical alignment: bottom by default, as Excel; top when asked.
        assert!(menu(&d, sel, (0, 0)).iter().any(|(m, _)| m.label == "Align Bottom" && m.mark == pergola::widgets::Mark::Check(true)));
        run(&mut d, sel, (0, 0), DataCmd::AlignTop);
        assert_eq!(d.wb.style(CellAddr { sheet: 0, row: 1, col: 1 }).v_align, VAlign::Top);
        // Hidden rows 3-4: a double-click on their line (row 3) shows both.
        run(&mut d, CellRange { r0: 2, c0: 0, r1: 3, c1: 0 }, (2, 0), DataCmd::HideRows);
        assert!(!unhide_rows_at(&mut d, 1));
        assert!(unhide_rows_at(&mut d, 2));
        assert!(!d.wb.sheet(0).row_hidden(2) && !d.wb.sheet(0).row_hidden(3));
    }

    #[test]
    fn merge_keeps_the_top_left_value_and_unmerge_and_undo_bring_cells_back() {
        let mut d = doc();
        let sel = CellRange { r0: 1, c0: 0, r1: 2, c1: 1 };
        let titles = menu(&d, sel, (1, 0)).into_iter().map(|(m, _)| m.label).collect::<Vec<_>>();
        assert!(titles.iter().any(|t| t == "Merge Cells"), "{titles:?}");
        let before = (cell(&d, 1, 0), cell(&d, 1, 1), cell(&d, 2, 0));
        run(&mut d, sel, (1, 0), DataCmd::Merge);
        assert!(d.status.starts_with("Cells merged; the 3 other values"), "{}", d.status);
        assert_eq!(d.wb.merges(0), &[Rect { sheet: 0, r0: 1, c0: 0, r1: 2, c1: 1 }]);
        assert_eq!(cell(&d, 1, 0), before.0);
        assert_eq!(cell(&d, 1, 1), "");
        // Sorting across it is refused.
        run(&mut d, one(3, 0), (3, 0), DataCmd::SortAsc);
        assert!(d.status.starts_with("To sort, every row needs the same merged cells"), "{}", d.status);
        // The cursor in it offers Unmerge.
        let titles = menu(&d, one(2, 1), (2, 1)).into_iter().map(|(m, _)| m.label).collect::<Vec<_>>();
        assert!(titles.iter().any(|t| t == "Unmerge Cells"), "{titles:?}");
        run(&mut d, one(2, 1), (2, 1), DataCmd::Unmerge);
        assert!(d.wb.merges(0).is_empty());
        assert_eq!(cell(&d, 1, 1), "");
        // Undo twice: merged again, then every value back.
        assert!(d.undo());
        assert_eq!(d.wb.merges(0).len(), 1);
        assert!(d.undo());
        assert!(d.wb.merges(0).is_empty());
        assert_eq!((cell(&d, 1, 0), cell(&d, 1, 1), cell(&d, 2, 0)), before);
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
    fn pivot_insert_refresh_and_undo() {
        let mut d = doc();
        run(&mut d, one(1, 0), (1, 0), DataCmd::InsertPivot);
        assert_eq!(d.status, "Couldn't place the pivot table at E1: there are cells in the way.");
        d.type_into(5, 4, "");
        run(&mut d, one(1, 0), (1, 0), DataCmd::InsertPivot);
        assert!(d.status.starts_with("PivotTable1 at E1: A1:C5 by Fruit."), "{}", d.status);
        let rows: Vec<String> = (0..6).map(|r| (4..7).map(|c| cell(&d, r, c)).collect::<Vec<_>>().join("|")).collect();
        assert_eq!(rows, ["|Values|", "Fruit|Sum of Qty|Sum of Price", "apple|11|0.9", "fig|7|2", "pear|3|1.5", "Grand Total|21|4.4"]);
        // The source changes; a refresh from inside the pivot follows it.
        d.type_into(1, 1, "30");
        assert_eq!(cell(&d, 4, 5), "3");
        let labels: Vec<String> = menu(&d, one(4, 5), (4, 5)).into_iter().map(|(m, _)| m.label).collect();
        assert!(labels.contains(&"Refresh “PivotTable1”".to_string()), "{labels:?}");
        run(&mut d, one(4, 5), (4, 5), DataCmd::RefreshPivot);
        assert_eq!(cell(&d, 4, 5), "30");
        assert_eq!(d.wb.pivot_at(CellAddr::new(0, 5, 4)).map(|x| x.1), Some(officina_grid::pivot::Role::Total));
        // The only row field can't go across.
        run(&mut d, one(2, 0), (2, 0), DataCmd::PivotColumns);
        assert!(d.status.contains("only row field"), "{}", d.status);
        assert!(d.undo());
        assert_eq!(cell(&d, 4, 5), "3");
        run(&mut d, one(4, 5), (4, 5), DataCmd::DeletePivot);
        assert!(d.wb.pivots().is_empty() && cell(&d, 1, 4).is_empty());
        assert!(d.undo());
        assert_eq!(cell(&d, 1, 4), "Fruit");
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
