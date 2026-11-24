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
    Author(crate::app::AuthorCmd),
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
    /// The copied cells' comments only.
    PasteComments,
    /// The copied numbers added to, taken from, multiplied with or
    /// dividing the numbers where they land.
    PasteAdd,
    PasteSubtract,
    PasteMultiply,
    PasteDivide,
    /// The copied cells, blank ones leaving what is under them.
    PasteSkipBlanks,
    /// Fill the cursor's column by the examples typed in it.
    FlashFill,
    /// Keep the selected cells' entries as a custom list fills follow.
    AddCustomList,
    /// Forget the custom lists.
    ClearCustomLists,
    /// The panel listing the custom lists, each with a way to forget it.
    CustomLists,
    /// The Paste Special bar: what to paste, an operation, skipping blanks
    /// and turning, together.
    PasteSpecial,
    /// The bar for an exact height of the selection's rows, or width of
    /// its columns.
    RowHeight,
    ColWidth,
    SortAsc,
    SortDesc,
    /// Turn the auto-filter on (on the data) or off.
    Filter,
    /// Recalculate row visibility from the current filter conditions.
    ReapplyFilter,
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
    /// Rows of the data the same in every column as one above, taken out
    /// (the rows below move up), as Excel's Remove Duplicates.
    RemoveDuplicates,
    /// The selection's column split at the delimiter its cells use, into the
    /// empty columns to its right.
    TextToColumns,
    /// The table's total row, on or off.
    TotalRow,
    /// A SUM of the numbers next to the selection (Alt+=).
    AutoSum,
    /// The selection back to the default format; with its contents, comments
    /// and links too.
    ClearFormats,
    ClearAll,
    /// The selection's comments taken off.
    ClearComments,
    /// Ask for a cell, range, sheet or name to go to; the window runs it (a bar
    /// under the grid).
    GoTo,
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
    /// Empty cells where the selection is, what was there and below it in
    /// its columns moving down (or, right: in its rows moving right).
    InsertCellsDown,
    InsertCellsRight,
    /// The selection's cells taken out, what is below them in their
    /// columns moving up (or, left: right of them in their rows moving left).
    DeleteCellsUp,
    DeleteCellsLeft,
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
    NewThread,
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
    BrowseVersions,
    PrintComments,
    TrackChanges,
    ReviewTracked,
    MergeCopies,
    /// A new sheet at the end; the window runs it, as it shows the sheet.
    AddSheet,
    /// A copy of the shown sheet right after it; the window runs it.
    DuplicateSheet,
    /// The bar naming the selection; the window runs it.
    DefineName,
    /// The bar sorting by several columns; the window runs it.
    CustomSort,
    /// The shown sheet hidden (not the last that shows); the window runs it.
    HideSheet,
    /// Hidden sheet `.0` shown again and opened; the window runs it.
    UnhideSheet(u16),
    /// The Name Manager panel: every defined name; the window runs it.
    NameManager,
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
    let filter = filter_scope(d, row, col);
    let in_filter = filter.filter(|f| (f.first()..=f.rect().r1).contains(&row) && (f.rect().c0..=f.rect().c1).contains(&col));
    let value = short(&wb.formatted(CellAddr::new(s, row, col)).text);
    let shown = if value.is_empty() { "(empty)".to_string() } else { format!("“{value}”") };
    let col_filtered = filter.is_some_and(|f| f.conds().iter().any(|(c, cond)| *c + f.offset() == col && !cond.is_expression()));
    let any_cond = filter.is_some_and(|f| !f.conds().is_empty());
    let table = wb.table_at(s, row, col);
    let (nr, nc) = (sel.r1 - sel.r0 + 1, sel.c1 - sel.c0 + 1);
    let plural = |n: u32, one: &str, many: &str| if n == 1 { one.to_string() } else { format!("{n} {many}") };
    let circling = d.circling;
    let special = d.clip.as_ref().is_some_and(|c| !c.clip.cut);
    let mut v = vec![
        (MenuItem::new("Compare with…"), Some(DataCmd::CompareWith)),
        (MenuItem::new("Start Tracking Cell Changes"), Some(DataCmd::TrackChanges)),
        (MenuItem::new("Review Tracked Changes"), Some(DataCmd::ReviewTracked)),
        (MenuItem::new("Merge Copies…"), Some(DataCmd::MergeCopies)),
        (MenuItem::new("Browse Versions…"), Some(DataCmd::BrowseVersions)),
        (MenuItem::new("Fill: Copy Cells").enabled(d.can_refill()).separated(), Some(DataCmd::FillCopy)),
        (MenuItem::new("Fill: Series").enabled(d.can_refill()), Some(DataCmd::FillSeries)),
        (MenuItem::new("Fill: Formats Only").enabled(d.can_refill()), Some(DataCmd::FillFormats)),
        (MenuItem::new("Fill: Without Formatting").enabled(d.can_refill()), Some(DataCmd::FillNoFormats)),
        (MenuItem::new("Flash Fill").shortcut("Ctrl+E"), Some(DataCmd::FlashFill)),
        (MenuItem::new("Fill: Keep Selection as a List").enabled(sel.r0 == sel.r1 || sel.c0 == sel.c1), Some(DataCmd::AddCustomList)),
        (MenuItem::new("Fill: Custom Lists…"), Some(DataCmd::CustomLists)),
        (MenuItem::new("Paste Values").enabled(special).separated(), Some(DataCmd::PasteValues)),
        (MenuItem::new("Paste Formulas").enabled(special), Some(DataCmd::PasteFormulas)),
        (MenuItem::new("Paste Formats").enabled(special), Some(DataCmd::PasteFormats)),
        (MenuItem::new("Paste Transposed").enabled(special), Some(DataCmd::PasteTransposed)),
        (MenuItem::new("Paste Comments").enabled(special), Some(DataCmd::PasteComments)),
        (MenuItem::new("Paste Skipping Blanks").enabled(special), Some(DataCmd::PasteSkipBlanks)),
        (MenuItem::new("Paste and Add").enabled(special), Some(DataCmd::PasteAdd)),
        (MenuItem::new("Paste and Subtract").enabled(special), Some(DataCmd::PasteSubtract)),
        (MenuItem::new("Paste and Multiply").enabled(special), Some(DataCmd::PasteMultiply)),
        (MenuItem::new("Paste and Divide").enabled(special), Some(DataCmd::PasteDivide)),
        (MenuItem::new("Paste Special…").enabled(special), Some(DataCmd::PasteSpecial)),
        (MenuItem::new(format!("Insert {} Above", plural(nr, "Row", "Rows"))).separated(), Some(DataCmd::InsertRows)),
        (MenuItem::new(format!("Insert {} Left", plural(nc, "Column", "Columns"))), Some(DataCmd::InsertCols)),
        (MenuItem::new(format!("Delete {}", plural(nr, "Row", "Rows"))), Some(DataCmd::DeleteRows)),
        (MenuItem::new(format!("Delete {}", plural(nc, "Column", "Columns"))), Some(DataCmd::DeleteCols)),
        (MenuItem::new("Insert Cells, Shift Down"), Some(DataCmd::InsertCellsDown)),
        (MenuItem::new("Insert Cells, Shift Right"), Some(DataCmd::InsertCellsRight)),
        (MenuItem::new("Delete Cells, Shift Up"), Some(DataCmd::DeleteCellsUp)),
        (MenuItem::new("Delete Cells, Shift Left"), Some(DataCmd::DeleteCellsLeft)),
        (MenuItem::new(format!("Hide {}", plural(nr, "Row", "Rows"))).separated(), Some(DataCmd::HideRows)),
        (MenuItem::new("Unhide Rows").enabled(wb.sheet(s).hidden.iter().any(|&(a, b)| a <= sel.r1 && b >= sel.r0)), Some(DataCmd::UnhideRows)),
        (MenuItem::new(format!("Hide {}", plural(nc, "Column", "Columns"))).enabled(nc <= MAX_HIDE_COLS), Some(DataCmd::HideCols)),
        (MenuItem::new("Row Height…"), Some(DataCmd::RowHeight)),
        (MenuItem::new("Column Width…"), Some(DataCmd::ColWidth)),
        (MenuItem::new("Unhide Columns").enabled(wb.sheet(s).col_sizes.overrides().iter().any(|&(c, w)| w <= 0.0 && (sel.c0..=sel.c1).contains(&c))), Some(DataCmd::UnhideCols)),
        (MenuItem::new(match wb.comment(s, row, col) {
            Some(comment) if !comment.thread().is_empty() => "Open Comment Thread…",
            Some(_) => "Edit Comment…",
            None => "New Comment…",
        }).separated(), Some(DataCmd::Comment)),
        (MenuItem::new("New Comment Thread…").enabled(wb.comment(s, row, col).is_none()), Some(DataCmd::NewThread)),
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
        (MenuItem::new("Reapply Filter").enabled(any_cond).separated(), Some(DataCmd::ReapplyFilter)),
        (MenuItem::new("Sort A to Z").enabled(some).separated(), Some(DataCmd::SortAsc)),
        (MenuItem::new("Custom Sort…").enabled(some), Some(DataCmd::CustomSort)),
        (MenuItem::new("Sort Z to A").enabled(some), Some(DataCmd::SortDesc)),
        (MenuItem::new("Filter").checked(filter.is_some()).enabled(some || filter.is_some()).separated(), Some(DataCmd::Filter)),
        (MenuItem::new(format!("Show Only {shown}")).enabled(in_filter.is_some()), Some(DataCmd::KeepValue)),
        (MenuItem::new(format!("Hide {shown}")).enabled(in_filter.is_some()), Some(DataCmd::HideValue)),
        (MenuItem::new(format!("Choose Values in Column {}…", officina_grid::addr::col_name(col))).enabled(filter.is_some_and(|f| f.rect().contains(CellAddr::new(s, row, col)))), Some(DataCmd::ChooseValues)),
        (MenuItem::new(format!("Clear Filter in Column {}", officina_grid::addr::col_name(col))).enabled(col_filtered), Some(DataCmd::ClearColumn)),
        (MenuItem::new("Show All Rows").enabled(any_cond), Some(DataCmd::ShowAll)),
        (MenuItem::new("Remove Duplicates").enabled(some).separated(), Some(DataCmd::RemoveDuplicates)),
        (MenuItem::new("Text to Columns").enabled(sel.c0 == sel.c1), Some(DataCmd::TextToColumns)),
        (MenuItem::new("AutoSum").shortcut("Alt+="), Some(DataCmd::AutoSum)),
        (MenuItem::new("Clear Formats").separated(), Some(DataCmd::ClearFormats)),
        (MenuItem::new("Clear All"), Some(DataCmd::ClearAll)),
        (MenuItem::new("Clear Comments").enabled(comments_in(wb, s, sel).next().is_some()), Some(DataCmd::ClearComments)),
        (MenuItem::new("Go To…").shortcut("Ctrl+G"), Some(DataCmd::GoTo)),
    ];
    if let Some(i) = v.iter().position(|x| x.1 == Some(DataCmd::FreezePanes)) {
        v.insert(i + 1, (MenuItem::new("Freeze Top Row"), Some(DataCmd::FreezeTopRow)));
    }
    match table {
        Some(t) => {
            v.push((MenuItem::new(format!("Convert “{}” to Range", short(&t.name))).separated(), Some(DataCmd::RemoveTable)));
            v.push((MenuItem::new("Total Row").checked(t.totals), Some(DataCmd::TotalRow)));
        }
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
    v.push((MenuItem::new("Define Name…").separated(), Some(DataCmd::DefineName)));
    v.push((MenuItem::new("Name Manager…"), Some(DataCmd::NameManager)));
    v.push((MenuItem::new("Data Validation…"), Some(DataCmd::Validation)));
    v.push((MenuItem::new("Clear Validation").enabled(validated), Some(DataCmd::ClearValidation)));
    v.push((MenuItem::new("Circle Invalid Data").checked(circling).enabled(!wb.validations().is_empty() || circling), Some(DataCmd::CircleInvalid)));
    v.push((MenuItem::new("Add Sheet").separated(), Some(DataCmd::AddSheet)));
    v.push((MenuItem::new("Duplicate Sheet").enabled(wb.sheet_count() < officina_grid::book::MAX_SHEETS), Some(DataCmd::DuplicateSheet)));
    v.push((MenuItem::new("Hide Sheet").enabled(wb.shown_sheets() > 1), Some(DataCmd::HideSheet)));
    let hidden: Vec<u16> = (0..wb.sheet_count() as u16).filter(|&h| wb.sheet(h).sheet_hidden).collect();
    for &h in hidden.iter().take(MAX_UNHIDE_ITEMS) {
        v.push((MenuItem::new(format!("Unhide {}", wb.sheet_name(h))), Some(DataCmd::UnhideSheet(h))));
    }
    if hidden.len() > MAX_UNHIDE_ITEMS {
        v.push((MenuItem::new(format!("{} more hidden sheets not listed", hidden.len() - MAX_UNHIDE_ITEMS)).enabled(false), None));
    }
    v.push((MenuItem::new("Rename Sheet…"), Some(DataCmd::RenameSheet)));
    v.push((MenuItem::new("Tab Colour…"), Some(DataCmd::TabColor)));
    let n = wb.sheet_count() as u16;
    v.push((MenuItem::new("Move Sheet Left").enabled(s > 0), Some(DataCmd::MoveSheetLeft)));
    v.push((MenuItem::new("Move Sheet Right").enabled(s + 1 < n), Some(DataCmd::MoveSheetRight)));
    v.push((MenuItem::new("Delete Sheet").enabled(n > 1), Some(DataCmd::DeleteSheet)));
    use crate::app::{AuthorCmd, OutlineCmd, Split};
    use officina_grid::shift::Axis;
    let mut authoring = Vec::new();
    authoring.push((MenuItem::new("Find and Replace…").separated(), Some(DataCmd::Author(AuthorCmd::FindOpen))));
    for (label, mode) in [("Split Horizontally", Split::Rows), ("Split Vertically", Split::Columns),
        ("Split Both Ways", Split::Both), ("Remove Split", Split::None)] {
        authoring.push((MenuItem::new(label), Some(DataCmd::Author(AuthorCmd::Split(mode)))));
    }
    for (label, axis, op) in [
        ("Group Rows", Axis::Rows, OutlineCmd::Group), ("Group Columns", Axis::Cols, OutlineCmd::Group),
        ("Ungroup Rows", Axis::Rows, OutlineCmd::Ungroup), ("Ungroup Columns", Axis::Cols, OutlineCmd::Ungroup),
        ("Collapse or Expand Rows", Axis::Rows, OutlineCmd::ToggleSelection),
        ("Collapse or Expand Columns", Axis::Cols, OutlineCmd::ToggleSelection),
    ] {
        authoring.push((MenuItem::new(label), Some(DataCmd::Author(AuthorCmd::Outline(axis, op)))));
    }
    authoring.push((MenuItem::new("Edit Selected Chart…").enabled(d.wb.charts().iter().any(|c| c.anchor.sheet == s)),
        Some(DataCmd::Author(AuthorCmd::ChartOpen))));
    v.splice(5..5, authoring);
    v.push((MenuItem::new("Print with Comments…").separated().enabled(!wb.comments(s).is_empty()), Some(DataCmd::PrintComments)));
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
    let data: Vec<DataField> = (r.c0..=r.c1).filter(|&c| c != rows && numeric(c)).take(3).map(|c| DataField { field: c - r.c0, agg: Agg::Sum, name: None, show_as: Default::default() }).collect();
    let data = if data.is_empty() { vec![DataField { field: rows - r.c0, agg: Agg::Count, name: None, show_as: Default::default() }] } else { data };
    let mut p = Pivot::new(name, r, CellAddr::new(s, r.r0, (r.c1 + 2).min(officina_grid::addr::MAX_COLS - 1)));
    p.rows = vec![PivotField::new(rows - r.c0)];
    p.data = data;
    Some(p)
}

/// Runs `cmd`; says what happened (or why nothing did) in the status line.
/// Paste the cells last copied here at the selection's top left, as `sp`
/// says, as one undo step named `label`.
pub fn paste_with(d: &mut Doc, sel: CellRange, sp: officina_grid::clip::Special, label: &str) -> Result<String, String> {
    let Some(copy) = d.clip.as_ref() else {
        return Err("Copy some cells first.".into());
    };
    let (ops, _) = copy.clip.paste_special(&d.wb, d.sheet, sel.r0, sel.c0, sp)?;
    if !d.run_validated(label, ops) {
        return Err(d.status.clone());
    }
    Ok(if d.status.is_empty() { format!("{label}: done.") } else { d.status.clone() })
}

pub fn run(d: &mut Doc, sel: CellRange, cursor: (u32, u32), cmd: DataCmd) {
    // Sorting, filtering and pivots read values: finish recalculating first.
    if d.wb.recalc_pending() {
        d.wb.recalc();
    }
    d.status.clear();
    let msg = match apply(d, sel, cursor, cmd) {
        Ok(m) | Err(m) => m,
    };
    if d.status != crate::sheet_protection::REFUSED {
        d.status = msg;
    }
}

/// The most rows Remove Duplicates looks through.
/// Hidden sheets the Data menu offers to unhide by name.
pub const MAX_UNHIDE_ITEMS: usize = 32;

/// The rows a sort reorders (the header row left out) and the target they
/// came from; refused, with why, when a filter hides some or merges differ.
fn sort_rect(d: &Doc, sel: CellRange, cursor: (u32, u32)) -> Result<(Rect, Target), String> {
    let s = d.sheet;
    let t = target(d, sel, cursor).ok_or("Select the data to sort, or put the cursor in it.")?;
    if d.wb.sheet(s).filter.as_ref().is_some_and(|f| !f.conds.is_empty() && f.rect.r0 <= t.rect.r1 && f.rect.r1 >= t.rect.r0)
        || d.wb.sheet(s).table_hidden().iter().any(|&(a, b)| a <= t.rect.r1 && b >= t.rect.r0) {
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
    Ok((rect, t))
}

/// The most columns a Custom Sort sorts by (Excel allows 64 levels).
pub const MAX_SORT_KEYS: usize = 64;

/// The Custom Sort bar's text to start from: the cursor's column, by its
/// header when the data has one.
pub fn custom_sort_start(d: &Doc, sel: CellRange, cursor: (u32, u32)) -> String {
    match target(d, sel, cursor) {
        Some(t) if t.header => {
            let h = d.wb.formatted(officina_grid::CellAddr::new(d.sheet, t.rect.r0, cursor.1.clamp(t.rect.c0, t.rect.c1))).text;
            if h.is_empty() || h.contains(',') { officina_grid::addr::col_name(cursor.1) } else { h.to_string() }
        }
        _ => officina_grid::addr::col_name(cursor.1),
    }
}

/// Custom Sort: the data at the selection or cursor sorted by the columns
/// `spec` lists, first to last, as one step. Each is a column letter or a
/// header, optionally followed by `desc` (Z to A) or `asc`, separated by
/// commas or `then`: `Region, Sales desc`.
pub fn custom_sort(d: &mut Doc, sel: CellRange, cursor: (u32, u32), spec: &str) -> Result<String, String> {
    let (rect, t) = sort_rect(d, sel, cursor)?;
    let s = d.sheet;
    let header = |c: u32| t.header.then(|| d.wb.formatted(officina_grid::CellAddr::new(s, t.rect.r0, c)).text.trim().to_lowercase());
    let mut keys: Vec<SortKey> = Vec::new();
    let mut names = Vec::new();
    let spec = spec.replace(" then ", ",").replace(" THEN ", ",").replace(" Then ", ",");
    for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        if keys.len() == MAX_SORT_KEYS {
            return Err(format!("Custom Sort sorts by at most {MAX_SORT_KEYS} columns."));
        }
        let lower = part.to_lowercase();
        let (what, descending) = if let Some(w) = ["descending", "desc", "z-a", "z to a"].iter().find_map(|x| lower.strip_suffix(x)) {
            (w.trim(), true)
        } else if let Some(w) = ["ascending", "asc", "a-z", "a to z"].iter().find_map(|x| lower.strip_suffix(x)) {
            (w.trim(), false)
        } else {
            (lower.as_str(), false)
        };
        let by_header = (rect.c0..=rect.c1).find(|&c| header(c).is_some_and(|h| !h.is_empty() && h == what));
        let by_letter = officina_grid::addr::parse_col(&what.to_uppercase()).filter(|c| (rect.c0..=rect.c1).contains(c));
        let Some(col) = by_header.or(by_letter) else {
            return Err(format!("{part:?} isn't a column of the data: use a letter from {} to {} or a header.", officina_grid::addr::col_name(rect.c0), officina_grid::addr::col_name(rect.c1)));
        };
        if keys.iter().any(|k| k.col == col) {
            return Err(format!("Column {} is listed twice.", officina_grid::addr::col_name(col)));
        }
        keys.push(SortKey { col, descending });
        names.push(format!("{}{}", officina_grid::addr::col_name(col), if descending { " (Z to A)" } else { "" }));
    }
    if keys.is_empty() {
        return Err("Type the columns to sort by, such as: Region, Sales desc".into());
    }
    match sort_op(&d.wb, rect, &keys) {
        Some(op) => {
            d.run("Custom Sort", vec![op]);
            Ok(format!("Sorted {} rows by {}{}.", rect.r1 - rect.r0 + 1, names.join(", then "), if t.header { " (the first row is kept as the header)" } else { "" }))
        }
        None => Ok("Already in that order.".into()),
    }
}

/// The most used cells Duplicate Sheet copies.
pub const MAX_DUPLICATE_CELLS: u64 = 20_000_000;

/// The name a copy of sheet `name` gets: `name (2)`, or the next free number.
pub fn copy_name(wb: &Workbook, name: &str) -> String {
    let base = match name.rsplit_once(" (") {
        Some((b, n)) if n.strip_suffix(')').is_some_and(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit())) => b,
        _ => name,
    };
    (2..).map(|k| format!("{base} ({k})")).find(|n| wb.sheet_id(n).is_none()).unwrap_or_default()
}

/// Duplicate Sheet: a copy of the shown sheet placed right after it, as one
/// step: its cells with their formulas, formats, comments and links, merged
/// cells, row heights and column widths, frozen panes and tab colour.
/// Formulas in the copy that read its own sheet read the copy, as Excel's
/// copy does. Returns the copy's place.
pub fn duplicate_sheet(d: &mut Doc) -> Result<u16, String> {
    let s = d.sheet;
    let n = d.wb.sheet_count();
    if n >= officina_grid::book::MAX_SHEETS {
        return Err(format!("A workbook holds at most {} sheets.", officina_grid::book::MAX_SHEETS));
    }
    let (er, ec) = d.wb.extent(s);
    if er as u64 * ec as u64 > MAX_DUPLICATE_CELLS {
        return Err(format!("Duplicate Sheet copies at most {MAX_DUPLICATE_CELLS} cells."));
    }
    let name = copy_name(&d.wb, &d.wb.sheet_name(s));
    let clip = (er > 0 && ec > 0).then(|| officina_grid::clip::Clip::take(&d.wb, Rect { sheet: s, r0: 0, c0: 0, r1: er - 1, c1: ec - 1 }, false));
    d.run("Duplicate Sheet", vec![GridOp::add_sheet(name.clone())]);
    if d.wb.sheet_count() <= n {
        return Err(format!("Couldn't add a sheet called {name}."));
    }
    let t = n as u16;
    let sh = d.wb.sheet(s);
    let mut ops = vec![
        GridOp::LayoutSizes { sheet: t, rows: true, default: sh.row_sizes.default, over: sh.row_sizes.overrides().to_vec() },
        GridOp::LayoutSizes { sheet: t, rows: false, default: sh.col_sizes.default, over: sh.col_sizes.overrides().to_vec() },
        GridOp::TabColor { sheet: t, color: sh.tab_color },
        GridOp::Freeze { sheet: t, rows: sh.freeze.0, cols: sh.freeze.1, from: sh.freeze_from },
    ];
    if let Some(clip) = clip {
        let (paste, _) = clip.paste(&d.wb, t, 0, 0)?;
        ops.extend(paste);
    }
    let merges: Vec<Rect> = d.wb.merges(s).iter().map(|m| Rect { sheet: t, ..*m }).collect();
    if !merges.is_empty() {
        ops.push(GridOp::Merges { sheet: t, list: merges });
    }
    // The sheet's conditional formats, validations and own names, on the copy.
    let mut cfs = d.wb.cond_formats().to_vec();
    let more: Vec<_> = cfs.iter().filter(|f| f.sheet == s).map(|f| officina_grid::condfmt::CondFormat { sheet: t, ranges: f.ranges.iter().map(|r| Rect { sheet: t, ..*r }).collect(), ..f.clone() }).collect();
    if !more.is_empty() {
        cfs.extend(more);
        ops.push(GridOp::cond_formats(&d.wb, &cfs));
    }
    let mut vs = d.wb.validations().to_vec();
    let more: Vec<_> = vs.iter().filter(|v| v.sheet == s).map(|v| officina_grid::validation::Validation { sheet: t, ranges: v.ranges.iter().map(|r| Rect { sheet: t, ..*r }).collect(), ..v.clone() }).collect();
    if !more.is_empty() {
        vs.extend(more);
        ops.push(GridOp::validations(&d.wb, &vs));
    }
    let mut names: Vec<_> = d.wb.names().iter().cloned().collect();
    let more: Vec<_> = names.iter().filter(|n| n.scope == Some(s)).map(|n| officina_grid::names::NameDef { scope: Some(t), ..n.clone() }).collect();
    if !more.is_empty() {
        names.extend(more);
        ops.push(GridOp::names(&d.wb, &names));
    }
    // Charts and pictures: those on the sheet, and the cells they read
    // there, onto the copy.
    let on = |r: Rect| if r.sheet == s { Rect { sheet: t, ..r } } else { r };
    let mut charts = d.wb.charts().to_vec();
    let more: Vec<_> = charts.iter().filter(|c| c.anchor.sheet == s).map(|c| {
        let mut c = c.clone();
        c.anchor.sheet = t;
        for se in &mut c.series {
            se.vals = on(se.vals);
            se.cats = se.cats.map(on);
            if let Some(officina_grid::charts::ChartText::Cell(a)) = &mut se.name {
                if a.sheet == s {
                    a.sheet = t;
                }
            }
        }
        if let Some(officina_grid::charts::ChartText::Cell(a)) = &mut c.title {
            if a.sheet == s {
                a.sheet = t;
            }
        }
        c
    }).collect();
    if !more.is_empty() {
        charts.extend(more);
        ops.push(GridOp::Charts(charts));
    }
    let mut pictures = d.wb.pictures().to_vec();
    let more: Vec<_> = pictures.iter().filter(|p| p.anchor.sheet == s).map(|p| { let mut p = p.clone(); p.anchor.sheet = t; p }).collect();
    if !more.is_empty() {
        pictures.extend(more);
        ops.push(GridOp::Pictures(pictures));
    }
    // Tables, each under a fresh name (table names are workbook-wide).
    let mut tables = d.wb.tables().to_vec();
    let mut more = Vec::new();
    for tb in tables.iter().filter(|tb| tb.rect.sheet == s) {
        let taken = |n: &str| tables.iter().chain(&more).any(|x: &officina_grid::tables::Table| x.name.eq_ignore_ascii_case(n)) || d.wb.names().iter().any(|x| x.name.eq_ignore_ascii_case(n));
        let Some(name) = (2..).map(|k| format!("{}_{k}", tb.name)).take(10_000).find(|n| !taken(n)) else { continue };
        more.push(officina_grid::tables::Table { name: name.into(), rect: Rect { sheet: t, ..tb.rect }, ..tb.clone() });
    }
    if !more.is_empty() {
        tables.extend(more);
        ops.push(GridOp::Tables(tables));
    }
    // Moved last: everything above names the copy by its place at the end.
    if s + 1 < t {
        ops.push(GridOp::MoveSheet { from: t, to: s + 1 });
    }
    d.extend(ops);
    Ok(s + 1)
}

/// A Name Manager row: the name, its sheet when it belongs to one, and
/// what it refers to, written out.
#[derive(Debug, Clone, PartialEq)]
pub struct NameRow {
    pub name: String,
    pub scope: Option<u16>,
    pub refers: String,
}

/// The Name Manager's rows: every defined name, workbook ones first, then
/// by sheet, each by name. Excel's own names show without `_xlnm.`.
pub fn name_rows(wb: &Workbook) -> Vec<NameRow> {
    let mut v: Vec<NameRow> = wb.names().iter().map(|n| NameRow {
        name: n.name.to_string(),
        scope: n.scope,
        refers: officina_grid::formula::to_a1(&n.expr, 0, 0, &|id| wb.sheet_name(id)),
    }).collect();
    v.sort_by(|a, b| a.scope.cmp(&b.scope).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    v
}

/// How a name shows in the Name Manager.
pub fn shown_name(name: &str) -> &str {
    name.strip_prefix("_xlnm.").unwrap_or(name)
}

/// The place row `i` of the Name Manager refers to, if it is a place.
pub fn name_place(wb: &Workbook, own: u16, i: usize) -> Result<(u16, Rect), String> {
    let row = name_rows(wb).into_iter().nth(i).ok_or("That name is gone.")?;
    go_to_place(wb, row.scope.unwrap_or(own), &row.refers).map_err(|_| format!("{} is {}, not a place to go to.", shown_name(&row.name), row.refers))
}

/// Deletes row `i` of the Name Manager, as one step. Formulas using it
/// then show `#NAME?`, as in Excel.
pub fn delete_name(d: &mut Doc, i: usize) -> Result<String, String> {
    let row = name_rows(&d.wb).into_iter().nth(i).ok_or("That name is gone.")?;
    let keep: Vec<_> = d.wb.names().iter().filter(|n| !(n.scope == row.scope && *n.name == *row.name)).cloned().collect();
    let op = GridOp::names(&d.wb, &keep);
    d.run("Delete Name", vec![op]);
    Ok(format!("{} deleted.", shown_name(&row.name)))
}

/// Renames row `i` of the Name Manager to `new`, as one step: formulas and
/// other names using it follow ([`officina_grid::names::rename_ops`]).
pub fn rename_name(d: &mut Doc, i: usize, new: &str) -> Result<String, String> {
    let row = name_rows(&d.wb).into_iter().nth(i).ok_or("That name is gone.")?;
    let (ops, kept) = officina_grid::names::rename_ops(&d.wb, row.scope, &row.name, new)?;
    d.status.clear();
    d.run("Rename Name", ops);
    if d.status == crate::sheet_protection::REFUSED {
        return Err(d.status.clone());
    }
    let new = new.trim();
    Ok(match kept {
        0 => format!("{} is now {new}.", shown_name(&row.name)),
        1 => format!("{} is now {new}; 1 formula with its own {} (LET or LAMBDA) is unchanged.", shown_name(&row.name), row.name),
        k => format!("{} is now {new}; {k} formulas with their own {} (LET or LAMBDA) are unchanged.", shown_name(&row.name), row.name),
    })
}

/// The Name Manager panel's commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamesCmd {
    /// Go to what row `.0` refers to.
    Go(usize),
    /// Open the bar that renames row `.0`.
    Rename(usize),
    Delete(usize),
    Close,
}

/// Define Name: `name` given to the selection, for the whole workbook, as
/// one step. A name already used for the workbook now means the selection.
pub fn define_name(d: &mut Doc, sel: CellRange, name: &str) -> Result<String, String> {
    let name = name.trim();
    if !officina_grid::names::valid_name(name) {
        return Err(format!("{name:?} can't be a name: start with a letter or _, use letters, digits, _ and ., and don't spell a cell like A1."));
    }
    let s = d.sheet;
    let place = {
        let a = |r: u32, c: u32| format!("${}${}", officina_grid::addr::col_name(c), r + 1);
        let sheet = d.wb.sheet_name(s);
        let sheet = if sheet.chars().all(|c| c.is_alphanumeric() || c == '_') && !sheet.chars().next().is_some_and(|c| c.is_ascii_digit()) { sheet } else { format!("'{}'", sheet.replace('\'', "''")) };
        if sel.r0 == sel.r1 && sel.c0 == sel.c1 { format!("={sheet}!{}", a(sel.r0, sel.c0)) } else { format!("={sheet}!{}:{}", a(sel.r0, sel.c0), a(sel.r1, sel.c1)) }
    };
    // Worked out on the workbook, then put back and made again as one
    // undoable step.
    let old: Vec<_> = d.wb.names().iter().cloned().collect();
    let existed = d.wb.names().iter().any(|n| n.scope.is_none() && n.name.eq_ignore_ascii_case(name));
    d.wb.define_name(name, None, &place).map_err(|e| format!("Couldn't name it: {e:?}"))?;
    let new: Vec<_> = d.wb.names().iter().cloned().collect();
    d.wb.replace_names(old);
    let op = GridOp::names(&d.wb, &new);
    d.run("Define Name", vec![op]);
    let shown = place.trim_start_matches('=');
    Ok(if existed { format!("{name} now means {shown}.") } else { format!("Named {shown} {name}.") })
}

/// The most cells Insert or Delete Cells moves.
pub const MAX_SHIFT_CELLS: u64 = 2_000_000;

/// Insert or Delete Cells (Excel's shift down, right, up or left): the cells
/// from the selection (inserting) or past it (deleting) to the end of what
/// is used, in the selection's columns (down, up) or rows (right, left),
/// cut and pasted along by the selection's height or width, as one step.
/// Formulas reading the moved cells follow them, as a cut does.
fn shift_cells(d: &mut Doc, sel: CellRange, cmd: DataCmd) -> Result<String, String> {
    let s = d.sheet;
    let (er, ec) = d.wb.extent(s);
    let down = matches!(cmd, DataCmd::InsertCellsDown | DataCmd::DeleteCellsUp);
    let insert = matches!(cmd, DataCmd::InsertCellsDown | DataCmd::InsertCellsRight);
    let sel_rect = Rect { sheet: s, r0: sel.r0, c0: sel.c0, r1: sel.r1, c1: sel.c1 };
    // The block that moves: from the selection (inserting) or just past it
    // (deleting) to the end of what is used, across the selection.
    let (start, end) = match (down, insert) {
        (true, true) => (sel.r0, er),
        (true, false) => (sel.r1 + 1, er),
        (false, true) => (sel.c0, ec),
        (false, false) => (sel.c1 + 1, ec),
    };
    let block = (start < end).then(|| if down {
        Rect { sheet: s, r0: start, c0: sel.c0, r1: end - 1, c1: sel.c1 }
    } else {
        Rect { sheet: s, r0: sel.r0, c0: start, r1: sel.r1, c1: end - 1 }
    });
    // Merged cells crossing the edge of what moves can't move with it.
    let strip = if down {
        Rect { sheet: s, r0: sel.r0, c0: sel.c0, r1: officina_grid::addr::MAX_ROWS - 1, c1: sel.c1 }
    } else {
        Rect { sheet: s, r0: sel.r0, c0: sel.c0, r1: sel.r1, c1: officina_grid::addr::MAX_COLS - 1 }
    };
    // A move doesn't carry merged cells along, so none may be in the way.
    if d.wb.merges(s).iter().any(|m| m.intersects(&strip)) {
        return Err("Merged cells are among the cells that would move; unmerge them first.".into());
    }
    if block.is_some_and(|b| b.cells() > MAX_SHIFT_CELLS) {
        return Err(format!("That would move more than {MAX_SHIFT_CELLS} cells; insert or delete whole rows or columns instead."));
    }
    let (n_r, n_c) = (sel.r1 - sel.r0 + 1, sel.c1 - sel.c0 + 1);
    let label = if insert { "Insert Cells" } else { "Delete Cells" };
    let mut ops = Vec::new();
    let mut started = false;
    if !insert {
        // References to the deleted cells become #REF!; they go, and then
        // what is past them moves in, read from the workbook as it is now.
        let mut first = officina_grid::clip::kill_refs(&d.wb, sel_rect);
        first.push(GridOp::clear(sel_rect));
        first.push(GridOp::Restyle(officina_grid::ops::StyleRegion { rect: sel_rect, spans: Vec::new() }));
        d.run(label, first);
        if d.status == crate::sheet_protection::REFUSED {
            return Err(d.status.clone());
        }
        started = true;
    }
    if let Some(b) = block {
        let clip = officina_grid::clip::Clip::take(&d.wb, b, true);
        let (row, col) = match (down, insert) {
            (true, true) => (b.r0 + n_r, b.c0),
            (true, false) => (sel.r0, b.c0),
            (false, true) => (b.r0, b.c0 + n_c),
            (false, false) => (b.r0, sel.c0),
        };
        let (moved, _) = match clip.paste(&d.wb, s, row, col) {
            Ok(p) => p,
            Err(e) => {
                // Take back the half already done.
                if started {
                    d.undo();
                }
                return Err(if e.contains("fit") { "There is no room to move those cells along.".to_string() } else { e });
            }
        };
        ops.extend(moved);
    }
    if ops.is_empty() && !started {
        return Ok("Nothing to move there.".into());
    }
    if started {
        d.extend(ops);
    } else {
        d.run(label, ops);
    }
    let way = match cmd {
        DataCmd::InsertCellsDown => "inserted; the cells below moved down",
        DataCmd::InsertCellsRight => "inserted; the cells to the right moved right",
        DataCmd::DeleteCellsUp => "deleted; the cells below moved up",
        _ => "deleted; the cells to the right moved left",
    };
    let k = sel_rect.cells();
    Ok(format!("{k} {} {way}.", if k == 1 { "cell" } else { "cells" }))
}

pub const MAX_DEDUP_ROWS: u32 = 1_000_000;
/// The most rows Text to Columns and AutoSum read.
pub const MAX_SPLIT_ROWS: u32 = 100_000;
/// The most columns Text to Columns makes.
pub const MAX_SPLIT_COLS: usize = 256;

/// The rows of `rect` in order, split into the first of each kind and the
/// ones the same in every column as a row above (text matched without regard
/// to case, as Excel does).
fn duplicate_rows(wb: &Workbook, rect: &Rect) -> (Vec<u32>, Vec<u32>) {
    let key = |v: Value| match v {
        Value::Str(t) => format!("s{}", t.to_lowercase()),
        Value::Num(x) => format!("n{}", if x == 0.0 { 0.0 } else { x }),
        other => format!("{other:?}"),
    };
    let mut seen = std::collections::HashSet::new();
    let (mut kept, mut dups) = (Vec::new(), Vec::new());
    for r in rect.r0..=rect.r1 {
        let row: Vec<String> = (rect.c0..=rect.c1).map(|c| key(wb.get(CellAddr::new(rect.sheet, r, c)))).collect();
        if seen.insert(row) { kept.push(r) } else { dups.push(r) }
    }
    (kept, dups)
}

/// Splits the selected column (or the cursor's column of the data around it)
/// at the delimiter most of its cells use: tab, semicolon, comma, bar, then
/// space. The pieces go into that column and the ones to its right, which
/// must be empty; each is typed as it would be (numbers become numbers).
fn text_to_columns(d: &mut Doc, sel: CellRange, cursor: (u32, u32)) -> Result<String, String> {
    use officina_grid::Input;
    let s = d.sheet;
    if sel.c0 != sel.c1 {
        return Err("Select cells in one column to split.".into());
    }
    let (r0, r1, col) = if sel.r0 != sel.r1 {
        (sel.r0, sel.r1, sel.c0)
    } else {
        let t = target(d, sel, cursor).ok_or("Put the cursor in the column to split.")?;
        (t.rect.r0, t.rect.r1, cursor.1)
    };
    if r1 - r0 >= MAX_SPLIT_ROWS {
        return Err(format!("Text to Columns splits at most {MAX_SPLIT_ROWS} rows."));
    }
    let texts: Vec<Option<String>> = (r0..=r1).map(|r| match d.wb.input(CellAddr::new(s, r, col)) {
        Input::Value(Value::Str(t)) => Some(t.to_string()),
        _ => None,
    }).collect();
    const DELIMS: [(char, &str); 5] = [('\t', "tabs"), (';', "semicolons"), (',', "commas"), ('|', "bars"), (' ', "spaces")];
    let (delim, name) = DELIMS.iter()
        .map(|&(c, n)| (texts.iter().flatten().filter(|t| t.trim().contains(c)).count(), c, n))
        .filter(|&(k, ..)| k > 0)
        .fold(None, |best: Option<(usize, char, &str)>, x| match best { Some(b) if b.0 >= x.0 => Some(b), _ => Some(x) })
        .map(|(_, c, n)| (c, n))
        .ok_or("These cells have no tabs, semicolons, commas, bars or spaces to split at.")?;
    let pieces: Vec<Option<Vec<String>>> = texts.iter().map(|t| t.as_ref().filter(|t| t.trim().contains(delim)).map(|t| {
        if delim == ' ' { t.split_whitespace().map(str::to_string).collect() } else { t.split(delim).map(|p| p.trim().to_string()).collect() }
    })).collect();
    let wide = pieces.iter().flatten().map(Vec::len).max().unwrap_or(1);
    if wide > MAX_SPLIT_COLS || col as u64 + wide as u64 > officina_grid::addr::MAX_COLS as u64 {
        return Err(format!("That would make {wide} columns; Text to Columns makes at most {MAX_SPLIT_COLS}."));
    }
    let right = Rect { sheet: s, r0, c0: col + 1, r1, c1: col + wide as u32 - 1 };
    let mut busy = false;
    d.wb.each(&right, &mut |_, _, v| busy |= v != Value::Empty);
    if busy {
        return Err(format!("Columns {}–{} have data where the pieces would go; insert columns first.", officina_grid::addr::col_name(right.c0), officina_grid::addr::col_name(right.c1)));
    }
    let rows: Vec<Vec<Input>> = pieces.into_iter().zip(r0..=r1).map(|(p, r)| match p {
        Some(p) => p.into_iter().map(|t| if t.starts_with('=') { Input::Value(Value::str(&t)) } else { Input::typed(&t) }).collect(),
        None => vec![d.wb.input(CellAddr::new(s, r, col))],
    }).collect();
    let n = rows.iter().filter(|r| r.len() > 1).count();
    // Wide enough for every piece even when the last row isn't split.
    let mut rows = rows;
    if let Some(first) = rows.first_mut() {
        first.resize(wide.max(first.len()), Input::Empty);
    }
    d.run("Text to Columns", vec![GridOp::block(s, r0, col, rows)]);
    Ok(format!("Split {n} {} at {name} into {wide} columns.", if n == 1 { "cell" } else { "cells" }))
}

/// Alt+=: on one cell, a SUM of the run of numbers just above it (else just
/// left of it). On a range, a SUM of each of its columns in the row under it.
fn auto_sum(d: &mut Doc, sel: CellRange) -> Result<String, String> {
    use officina_grid::addr::a1;
    use officina_grid::Input;
    let s = d.sheet;
    let num = |d: &Doc, r: u32, c: u32| matches!(d.wb.get(CellAddr::new(s, r, c)), Value::Num(_));
    if sel.r0 == sel.r1 && sel.c0 == sel.c1 {
        let (r, c) = (sel.r0, sel.c0);
        let mut top = r;
        while top > 0 && r - top < MAX_SPLIT_ROWS && num(d, top - 1, c) {
            top -= 1;
        }
        let formula = if top < r {
            format!("=SUM({}:{})", a1(top, c), a1(r - 1, c))
        } else {
            let mut left = c;
            while left > 0 && c - left < MAX_SPLIT_ROWS && num(d, r, left - 1) {
                left -= 1;
            }
            if left == c {
                return Err("There are no numbers just above or left of this cell to sum.".into());
            }
            format!("=SUM({}:{})", a1(r, left), a1(r, c - 1))
        };
        return if d.type_into(r, c, &formula) { Ok(format!("{} is {}.", a1(r, c), &formula[1..])) } else { Err(d.status.clone()) };
    }
    if sel.r1 - sel.r0 >= MAX_SPLIT_ROWS || sel.c1 - sel.c0 >= MAX_SPLIT_COLS as u32 {
        return Err("Select fewer cells to sum.".into());
    }
    let below = sel.r1 + 1;
    if below >= officina_grid::addr::MAX_ROWS {
        return Err("There is no row under the selection for the totals.".into());
    }
    let mut busy = false;
    d.wb.each(&Rect { sheet: s, r0: below, c0: sel.c0, r1: below, c1: sel.c1 }, &mut |_, _, v| busy |= v != Value::Empty);
    if busy {
        return Err(format!("Row {} under the selection isn't empty; the totals go there.", below + 1));
    }
    let row: Vec<Input> = (sel.c0..=sel.c1).map(|c| Input::Formula(format!("=SUM({}:{})", a1(sel.r0, c), a1(sel.r1, c)))).collect();
    d.run("AutoSum", vec![GridOp::block(s, below, sel.c0, vec![row])]);
    Ok(format!("Totals in row {}.", below + 1))
}

/// A Go To reference: `B4`, `B4:D9`, `C:C`, `3:5`, any of them after
/// `Sheet2!` (quoted when the name needs it), or a defined name for one of
/// them. The sheet and the range there.
pub fn go_to_place(wb: &Workbook, own: u16, text: &str) -> Result<(u16, Rect), String> {
    use officina_grid::addr::{parse_a1, parse_col, parse_row, MAX_COLS, MAX_ROWS};
    let t = text.trim();
    if t.is_empty() {
        return Err("Type a cell, a range or a name.".into());
    }
    let (sheet, part) = match t.rsplit_once('!') {
        Some((name, part)) => {
            let name = name.trim_start_matches('=').trim();
            let name = name.strip_prefix('\'').and_then(|n| n.strip_suffix('\'')).map(|n| n.replace("''", "'")).unwrap_or_else(|| name.to_string());
            (wb.sheet_id(&name).ok_or_else(|| format!("There's no sheet called {name}."))?, part)
        }
        None => (own, t.trim_start_matches('=')),
    };
    let part = part.replace('$', "");
    let (a, b) = part.split_once(':').unwrap_or((&part, &part));
    let rect = if let (Some((r0, c0)), Some((r1, c1))) = (parse_a1(a), parse_a1(b)) {
        Some(Rect { sheet, r0: r0.min(r1), c0: c0.min(c1), r1: r0.max(r1), c1: c0.max(c1) })
    } else if let (true, Some(c0), Some(c1)) = (part.contains(':'), parse_col(a), parse_col(b)) {
        Some(Rect { sheet, r0: 0, c0: c0.min(c1), r1: MAX_ROWS - 1, c1: c0.max(c1) })
    } else if let (true, Some(r0), Some(r1)) = (part.contains(':'), parse_row(a), parse_row(b)) {
        Some(Rect { sheet, r0: r0.min(r1), c0: 0, r1: r0.max(r1), c1: MAX_COLS - 1 })
    } else {
        None
    };
    if wb.sheet(sheet).sheet_hidden {
        let name = wb.sheet_name(sheet);
        return Err(format!("{name} is hidden; Data ▸ Unhide {name} shows it."));
    }
    if let Some(r) = rect {
        return Ok((sheet, r));
    }
    // A defined name: what it refers to, written out, read the same way once.
    if !t.contains('!') {
        if let Some(def) = wb.names().get(own, t) {
            let src = officina_grid::formula::to_a1(&def.expr, 0, 0, &|id| wb.sheet_name(id));
            let src = src.trim_start_matches('=');
            if src != t && !src.contains(['(', ',', ' ']) {
                return go_to_place(wb, def.scope.unwrap_or(own), &if src.contains('!') { src.to_string() } else { format!("{}!{src}", quoted(&wb.sheet_name(def.scope.unwrap_or(own)))) });
            }
            return Err(format!("{t} isn't a range to go to."));
        }
    }
    Err(format!("There's no cell, range or name called {t}."))
}

fn quoted(name: &str) -> String {
    if name.chars().all(|c| c.is_alphanumeric() || c == '_') { name.to_string() } else { format!("'{}'", name.replace('\'', "''")) }
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

#[derive(Clone, Copy)]
enum FilterScope<'a> {
    Worksheet(&'a AutoFilter),
    Table(&'a Table),
}

impl FilterScope<'_> {
    fn rect(self) -> Rect {
        match self { Self::Worksheet(f) => f.rect, Self::Table(t) => t.filter_rect().unwrap() }
    }

    fn first(self) -> u32 {
        self.rect().r0 + match self { Self::Worksheet(_) => 1, Self::Table(t) => u32::from(t.header) }
    }

    fn offset(self) -> u32 {
        match self { Self::Worksheet(_) => 0, Self::Table(t) => t.rect.c0 }
    }

    fn hides(self, d: &Doc, row: u32) -> bool {
        let (rows, row) = match self {
            Self::Worksheet(_) => (d.wb.sheet(d.sheet).hidden.as_slice(), row),
            Self::Table(t) => (t.filter.as_ref().unwrap().hidden.as_slice(), row - t.rect.r0),
        };
        rows.get(rows.partition_point(|&(_, end)| end < row)).is_some_and(|&(start, _)| start <= row)
    }
}

impl<'a> FilterScope<'a> {
    fn conds(self) -> &'a [(u32, Cond)] {
        match self { Self::Worksheet(f) => &f.conds, Self::Table(t) => &t.filter.as_ref().unwrap().conds }
    }

    fn auto_filter(self) -> AutoFilter {
        match self { Self::Worksheet(f) => f.clone(), Self::Table(t) => t.auto_filter().unwrap() }
    }
}

fn filter_scope(d: &Doc, row: u32, col: u32) -> Option<FilterScope<'_>> {
    if let Some(table) = d.wb.table_at(d.sheet, row, col) {
        if table.filter.is_some() {
            return Some(FilterScope::Table(table));
        }
        if !d.wb.sheet(d.sheet).filter.as_ref().is_some_and(|f| f.rect.contains(CellAddr::new(d.sheet, row, col))) {
            return None;
        }
    }
    d.wb.sheet(d.sheet).filter.as_ref().map(FilterScope::Worksheet)
}

pub fn filter_header(d: &Doc, row: u32, col: u32) -> Option<u32> {
    let scope = filter_scope(d, row, col)?;
    Some(scope.rect().r0)
}

pub fn column_values(d: &Doc, row: u32, col: u32) -> Result<Vec<(String, u32, bool)>, String> {
    let scope = filter_scope(d, row, col).ok_or("There is no filter here.")?;
    let rect = scope.rect();
    let first = scope.first();
    if !(rect.c0..=rect.c1).contains(&col) {
        return Err("Put the cursor in a column of the filter.".into());
    }
    let all = d.wb.filter_values_from(&rect, first, col, MAX_VALUES + 1);
    if all.len() > MAX_VALUES {
        return Err(format!("This column has more than {MAX_VALUES} different values; use Show Only or Hide."));
    }
    let now = scope.conds().iter().find(|(c, cond)| *c + scope.offset() == col && !cond.is_expression()).map(|(_, c)| c);
    let key = officina_grid::eval::funcs::text_key;
    Ok(all
        .into_iter()
        .map(|(t, n)| {
            let on = match now {
                Some(Cond::Values(v)) => v.iter().any(|x| key(x) == key(&t)),
                // A rule: what it kept at the last application.
                Some(_) => (first..=rect.r1).any(|r| !scope.hides(d, r) && key(&d.wb.formatted(CellAddr::new(d.sheet, r, col)).text) == key(&t)),
                None => true,
            };
            (t, n, on)
        })
        .collect())
}

fn change_filter(d: &mut Doc, row: u32, col: u32, filter: AutoFilter) -> Result<String, String> {
    let scope = filter_scope(d, row, col).ok_or("There is no filter here.")?;
    let (op, hidden) = match scope {
        FilterScope::Worksheet(_) => {
            let state = d.wb.filtered(d.sheet, Some(filter));
            let hidden = hidden_in(&state, &scope.rect());
            (GridOp::Filter(state), hidden)
        }
        FilterScope::Table(t) => {
            let applied = d.wb.apply_table_filter(t, filter);
            let hidden = applied.hidden.iter().map(|(a, b)| b - a + 1).sum();
            let name = t.name.clone();
            let mut tables = d.wb.tables().to_vec();
            tables.iter_mut().find(|t| t.name == name).unwrap().filter = Some(applied);
            (GridOp::Tables(tables), hidden)
        }
    };
    d.run("Filter", vec![op]);
    Ok(if hidden == 0 { "The filter shows every row.".into() } else { format!("{hidden} rows hidden by the filter.") })
}

/// Replace this column's selection within its own filter, in one undo step.
pub fn keep_values(d: &mut Doc, row: u32, col: u32, keep: Vec<String>, every: bool) {
    let msg = (|| {
        let mut f = filter_scope(d, row, col).ok_or("There is no filter here.")?.auto_filter();
        let mut conds = f.conds.clone();
        conds.retain(|(c, cond)| *c != col || cond.is_expression());
        if !every {
            conds.push((col, Cond::Values(keep)));
            conds.sort_by_key(|(c, _)| *c);
        }
        if conds == f.conds {
            return Ok::<String, String>("The filter is as it was.".into());
        }
        f.conds = conds;
        change_filter(d, row, col, f)
    })();
    d.status = match msg { Ok(m) | Err(m) => m };
}

/// The conditions a filter column offers besides picking values (index 0).
pub const FILTER_CONDS: [&str; 16] = [
    "Values", "Equals", "Does not equal", "Greater than", "Greater than or equal to", "Less than", "Less than or equal to", "Between",
    "Top", "Bottom", "Top %", "Bottom %", "Begins with", "Ends with", "Contains", "Does not contain",
];

/// Condition `kind` of [`FILTER_CONDS`] with what was typed for it (`b` only for
/// Between). Numbers compare as numbers; other text as text, ignoring case.
pub fn filter_cond(kind: usize, a: &str, b: &str) -> Result<Cond, String> {
    use officina_grid::filter::{Rank, Test, TextOp, TextTest};
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() || (kind == 7 && b.is_empty()) {
        return Err("Type a value for the condition.".into());
    }
    let num = |t: &str| match officina_grid::Input::typed(t) {
        officina_grid::Input::Value(Value::Num(x)) => Some(x),
        _ => None,
    };
    let text = |op, value: &str| Cond::Custom(vec![Test::Text(TextTest { op, value: value.to_string(), case_sensitive: false })], true);
    Ok(match kind {
        1..=6 => {
            let op = ["=", "<>", ">", ">=", "<", "<="][kind - 1];
            match num(a) {
                Some(x) => Cond::Compare(vec![(op.to_string(), x)], true),
                None => text(TextOp::comparison(op).ok_or("Unknown comparison.")?, a),
            }
        }
        7 => match (num(a), num(b)) {
            (Some(x), Some(y)) => Cond::Compare(vec![(">=".into(), x.min(y)), ("<=".into(), x.max(y))], true),
            _ => return Err("Between takes two numbers.".into()),
        },
        8..=11 => {
            let percent = kind >= 10;
            Cond::Rank(Rank::parse(a, percent, kind == 9 || kind == 11).ok_or(if percent { "Type a whole percent from 1 to 100." } else { "Type a whole number of rows, 1 or more." })?)
        }
        12 => text(TextOp::Begins, a),
        13 => text(TextOp::Ends, a),
        14 => text(TextOp::Contains, a),
        15 => text(TextOp::NotContains, a),
        _ => return Err("Choose a condition.".into()),
    })
}

/// The condition and its typed values a column's filter shows in the panel,
/// when it is one the panel makes.
pub fn cond_fields(c: &Cond) -> Option<(usize, String, String)> {
    use officina_grid::filter::{Test, TextOp};
    let n = |x: f64| Value::Num(x).display();
    match c {
        Cond::Compare(v, true) if v.len() == 2 && v[0].0 == ">=" && v[1].0 == "<=" => Some((7, n(v[0].1), n(v[1].1))),
        Cond::Compare(v, _) if v.len() == 1 => {
            let k = ["=", "<>", ">", ">=", "<", "<="].iter().position(|o| *o == v[0].0)?;
            Some((k + 1, n(v[0].1), String::new()))
        }
        Cond::Rank(r) => Some((8 + 2 * usize::from(r.percent) + usize::from(r.bottom), r.limit().to_string(), String::new())),
        Cond::Custom(t, _) if t.len() == 1 => match &t[0] {
            Test::Text(t) if !t.case_sensitive => {
                let k = match t.op {
                    TextOp::Begins => 12,
                    TextOp::Ends => 13,
                    TextOp::Contains => 14,
                    TextOp::NotContains => 15,
                    op => ["=", "<>", ">", ">=", "<", "<="].iter().position(|o| Some(*o) == op.comparison_symbol())? + 1,
                };
                Some((k, t.value.clone(), String::new()))
            }
            _ => None,
        },
        _ => None,
    }
}

/// The panel's condition for column `col` of the filter at (`row`, `col`).
pub fn column_cond(d: &Doc, row: u32, col: u32) -> Option<(usize, String, String)> {
    let f = filter_scope(d, row, col)?.auto_filter();
    f.conds.iter().find(|(c, _)| *c == col).and_then(|(_, c)| cond_fields(c))
}

/// Puts `cond` in place of column `col`'s condition in the filter at (`row`, `col`).
pub fn set_condition(d: &mut Doc, row: u32, col: u32, cond: Cond) {
    let msg = (|| {
        let mut f = filter_scope(d, row, col).ok_or("There is no filter here.")?.auto_filter();
        let mut conds = f.conds.clone();
        conds.retain(|(c, cond)| *c != col || cond.is_expression());
        conds.push((col, cond));
        conds.sort_by_key(|(c, _)| *c);
        if conds == f.conds {
            return Ok::<String, String>("The filter is as it was.".into());
        }
        f.conds = conds;
        change_filter(d, row, col, f)
    })();
    d.status = match msg { Ok(m) | Err(m) => m };
}

/// The table at (`row`, `col`) with a total row under its data, or without
/// one. Turning it on needs the row under the table empty; its first column
/// says Total and each column of numbers gets `SUBTOTAL(109, …)`, a sum of the
/// rows the filter shows. Turning it off empties that row.
fn total_row(d: &mut Doc, row: u32, col: u32) -> Result<String, String> {
    use officina_grid::Input;
    let s = d.sheet;
    let t = d.wb.table_at(s, row, col).cloned().ok_or("The cursor isn't in a table.")?;
    let mut all = d.wb.tables().to_vec();
    let i = all.iter().position(|x| x.name == t.name).unwrap();
    if t.totals {
        let last = Rect { r0: t.rect.r1, ..t.rect };
        all[i].totals = false;
        all[i].rect.r1 -= 1;
        d.run("Total Row", vec![GridOp::clear(last), GridOp::Tables(all)]);
        return Ok(format!("{} has no total row now.", t.name));
    }
    let below = t.rect.r1 + 1;
    if below >= officina_grid::addr::MAX_ROWS {
        return Err("There is no row under the table for totals.".into());
    }
    let row_rect = Rect { r0: below, r1: below, ..t.rect };
    let mut busy = false;
    d.wb.each(&row_rect, &mut |_, _, v| busy |= v != Value::Empty);
    if busy {
        return Err(format!("Row {} under the table isn't empty; the totals go there.", below + 1));
    }
    let first = t.rect.r0 + u32::from(t.header);
    let cells: Vec<Input> = (t.rect.c0..=t.rect.c1).enumerate().map(|(k, c)| {
        let mut nums = false;
        if first <= t.rect.r1 {
            d.wb.each(&Rect { r0: first, c0: c, c1: c, ..t.rect }, &mut |_, _, v| nums |= matches!(v, Value::Num(_)));
        }
        if nums {
            Input::Formula(format!("=SUBTOTAL(109,{}[{}])", t.name, t.columns[k].chars().flat_map(|ch| if "[]#'".contains(ch) { vec!['\'', ch] } else { vec![ch] }).collect::<String>()))
        } else if k == 0 {
            Input::Value(Value::str("Total"))
        } else {
            Input::Empty
        }
    }).collect();
    all[i].totals = true;
    all[i].rect.r1 = below;
    d.run("Total Row", vec![GridOp::Tables(all), GridOp::block(s, below, t.rect.c0, vec![cells])]);
    Ok(format!("{} has a total row: column sums of the rows shown.", t.name))
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
            let kept_old = d.wb.sheet(s).hidden_widths.clone();
            let mut kept = kept_old.clone();
            if cmd == DataCmd::HideCols {
                // Remember the widths, to show them again as they were.
                for c in sel.c0..=sel.c1 {
                    let w = d.wb.sheet(s).col_sizes.size(c);
                    if w > 0.0 {
                        kept.retain(|k| k.0 != c);
                        kept.push((c, w));
                    }
                }
                over.extend((sel.c0..=sel.c1).map(|c| (c, 0.0)));
            } else {
                for &(c, w) in &old {
                    if inside(c) && w <= 0.0 {
                        if let Some(&(_, was)) = kept_old.iter().find(|k| k.0 == c) {
                            over.push((c, was));
                        }
                        kept.retain(|k| k.0 != c);
                    }
                }
            }
            over.sort_by_key(|o| o.0);
            if over == old {
                return Err(if cmd == DataCmd::HideCols { "Those columns are hidden already.".into() } else { "No hidden columns there. Select the columns around them.".into() });
            }
            let hidden = |v: &[(u32, f64)]| v.iter().filter(|o| o.1 <= 0.0).count();
            let n = hidden(&over).abs_diff(hidden(&old));
            kept.sort_by_key(|k| k.0);
            let mut ops = vec![GridOp::Sizes { sheet: s, rows: false, over }];
            if kept != kept_old {
                ops.push(GridOp::HiddenWidths { sheet: s, list: kept });
            }
            d.run(if cmd == DataCmd::HideCols { "Hide Columns" } else { "Unhide Columns" }, ops);
            Ok(if cmd == DataCmd::HideCols { format!("{n} columns hidden.") } else { format!("{n} columns shown.") })
        }
        DataCmd::PasteValues
        | DataCmd::PasteFormulas
        | DataCmd::PasteFormats
        | DataCmd::PasteTransposed
        | DataCmd::PasteSkipBlanks
        | DataCmd::PasteComments
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
                DataCmd::PasteComments => (of(What::Comments), "Paste Comments"),
                DataCmd::PasteSkipBlanks => (Special { skip_blanks: true, ..of(What::All) }, "Paste Skipping Blanks"),
                DataCmd::PasteAdd => (math(Arith::Add), "Paste and Add"),
                DataCmd::PasteSubtract => (math(Arith::Sub), "Paste and Subtract"),
                DataCmd::PasteMultiply => (math(Arith::Mul), "Paste and Multiply"),
                DataCmd::PasteDivide => (math(Arith::Div), "Paste and Divide"),
                _ => (Special { transpose: true, ..of(What::All) }, "Paste Transposed"),
            };
            paste_with(d, sel, sp, label)
        }
        DataCmd::AddCustomList => {
            if sel.r0 != sel.r1 && sel.c0 != sel.c1 {
                return Err("Select one row or one column of entries.".into());
            }
            let n = ((sel.r1 - sel.r0 + 1) as u64 * (sel.c1 - sel.c0 + 1) as u64).min(officina_grid::autofill::MAX_ENTRIES as u64 + 1) as u32;
            if n as usize > officina_grid::autofill::MAX_ENTRIES {
                return Err(format!("A list holds at most {} entries.", officina_grid::autofill::MAX_ENTRIES));
            }
            let entries: Vec<String> = (0..n).map(|k| if sel.r0 == sel.r1 { (sel.r0, sel.c0 + k) } else { (sel.r0 + k, sel.c0) }).map(|(r, c)| d.wb.formatted(CellAddr::new(s, r, c)).text).collect();
            let kept = crate::lists::add(entries)?;
            Ok(format!("Kept a list of {kept}: a fill from any of them goes on through the rest."))
        }
        DataCmd::ClearCustomLists => {
            crate::lists::clear()?;
            Ok("Custom lists forgotten.".into())
        }
        DataCmd::FlashFill => {
            let (ops, n) = officina_grid::flashfill::flash_fill(&d.wb, s, cursor.0, cursor.1)?;
            if !d.run_validated("Flash Fill", ops) {
                return Err(d.status.clone());
            }
            if !d.status.is_empty() {
                return Ok(d.status.clone());
            }
            Ok(if n == 1 { "Flash Fill filled 1 cell.".into() } else { format!("Flash Fill filled {n} cells.") })
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
            if d.refill(how) && d.status.is_empty() {
                Ok("Filled again.".into())
            } else {
                Ok(std::mem::take(&mut d.status))
            }
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
            // Frozen from the row and column at the top left of the view, as
            // in Excel: what is above and left of them is out of reach.
            let top = d.view_top;
            let (rows, cols) = match cmd {
                DataCmd::FreezePanes => cursor,
                DataCmd::FreezeTopRow => (top.0.saturating_add(1).min(officina_grid::addr::MAX_ROWS - 1), 0),
                _ => (0, 0),
            };
            let from = (if top.0 < rows { top.0 } else { 0 }, if top.1 < cols { top.1 } else { 0 });
            if (d.wb.sheet(s).freeze, d.wb.sheet(s).freeze_from) == ((rows, cols), from) {
                return Ok(String::new());
            }
            d.run(if (rows, cols) == (0, 0) { "Unfreeze Panes" } else { "Freeze Panes" }, vec![GridOp::Freeze { sheet: s, rows, cols, from }]);
            if from != (0, 0) {
                let span = |a: u32, b: u32, one: &str, many: &str, label: &dyn Fn(u32) -> String| if a + 1 == b { format!("{one} {}", label(a)) } else { format!("{many} {} to {}", label(a), label(b - 1)) };
                let col = |c: u32| officina_grid::addr::col_name(c);
                let row = |r: u32| (r + 1).to_string();
                let rs = (rows > 0).then(|| span(from.0, rows, "Row", "Rows", &row));
                let cs = (cols > 0).then(|| span(from.1, cols, "column", "columns", &col));
                let what = match (rs, cs) {
                    (Some(r), Some(c)) => format!("{r} and {c}"),
                    (Some(r), None) => r,
                    (None, Some(c)) => { let mut c = c; c[..1].make_ascii_uppercase(); c }
                    (None, None) => String::new(),
                };
                let many = rows.saturating_sub(from.0) + cols.saturating_sub(from.1) > 1;
                return Ok(format!("{what} {} in view; those above and left are out of reach until you unfreeze.", if many { "stay" } else { "stays" }));
            }
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
        DataCmd::RemoveDuplicates => {
            let t = target(d, sel, cursor).ok_or_else(need)?;
            let mut rect = t.rect;
            if t.header {
                rect.r0 += 1;
            }
            if rect.r0 > rect.r1 {
                return Err("There is nothing under the header to look through.".into());
            }
            if rect.r1 - rect.r0 >= MAX_DEDUP_ROWS {
                return Err(format!("Remove Duplicates looks through at most {MAX_DEDUP_ROWS} rows."));
            }
            if d.wb.sheet(s).filter.as_ref().is_some_and(|f| !f.conds.is_empty() && f.rect.r0 <= rect.r1 && f.rect.r1 >= rect.r0)
                || d.wb.sheet(s).table_hidden().iter().any(|&(a, b)| a <= rect.r1 && b >= rect.r0) {
                return Err("Show all rows first: the filter hides some of them.".into());
            }
            if !officina_grid::sort::merges_allow_sort(&d.wb, &rect) {
                return Err("Every row needs the same merged cells; unmerge them first.".into());
            }
            let (kept, dups) = duplicate_rows(&d.wb, &rect);
            if dups.is_empty() {
                return Ok(format!("No duplicate rows: all {} are different.", kept.len()));
            }
            let (n, left) = (dups.len(), kept.len());
            let tail = Rect { r0: rect.r0 + left as u32, ..rect };
            let ord: Vec<u32> = kept.into_iter().chain(dups).collect();
            let mut ops: Vec<GridOp> = officina_grid::sort::reorder_op(&d.wb, rect, &ord).into_iter().collect();
            ops.push(GridOp::clear(tail));
            d.run("Remove Duplicates", ops);
            Ok(format!("{} duplicate {} removed; {left} unique {} left.", n, if n == 1 { "row" } else { "rows" }, if left == 1 { "row" } else { "rows" }))
        }
        DataCmd::TextToColumns => text_to_columns(d, sel, cursor),
        DataCmd::TotalRow => total_row(d, row, col),
        DataCmd::AutoSum => auto_sum(d, sel),
        DataCmd::ClearFormats | DataCmd::ClearAll => {
            let r = Rect { sheet: s, r0: sel.r0, c0: sel.c0, r1: sel.r1, c1: sel.c1 };
            let all = cmd == DataCmd::ClearAll;
            let mut ops = vec![GridOp::style(r, officina_grid::style::Style::default())];
            if all {
                ops.push(GridOp::clear(r));
                let set: Vec<_> = comments_in(&d.wb, s, sel).map(|(r, c)| (r, c, None)).collect();
                if !set.is_empty() {
                    ops.push(GridOp::Comments { sheet: s, set });
                }
                let set: Vec<_> = links_in(&d.wb, s, sel).map(|(r, c)| (r, c, None)).collect();
                if !set.is_empty() {
                    ops.push(GridOp::Links { sheet: s, set });
                }
            }
            d.run(if all { "Clear All" } else { "Clear Formats" }, ops);
            Ok(if all { "Cleared: contents, formats, comments and links.".into() } else { "Formats cleared.".into() })
        }
        DataCmd::ClearComments => {
            let set: Vec<_> = comments_in(&d.wb, s, sel).map(|(r, c)| (r, c, None)).collect();
            if set.is_empty() {
                return Ok("No comments there.".into());
            }
            let n = set.len();
            d.run(if n == 1 { "Clear Comment" } else { "Clear Comments" }, vec![GridOp::Comments { sheet: s, set }]);
            Ok(if n == 1 { "Comment cleared.".into() } else { format!("{n} comments cleared.") })
        }
        // The window opens its bar.
        DataCmd::GoTo => Ok(String::new()),
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
                None => Err("This shift exceeds the sheet size or axis identity limits.".into()),
            }
        }
        DataCmd::InsertCellsDown | DataCmd::InsertCellsRight | DataCmd::DeleteCellsUp | DataCmd::DeleteCellsLeft => shift_cells(d, sel, cmd),
        DataCmd::CustomSort => Ok(String::new()),
        DataCmd::SortAsc | DataCmd::SortDesc => {
            let (rect, t) = sort_rect(d, sel, cursor)?;
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
            if let Some(table) = d.wb.table_at(s, row, col) {
                if !matches!(filter_scope(d, row, col), Some(FilterScope::Worksheet(_))) {
                    if table.filter_rect().is_none() {
                        return Err("The table has no rows to filter.".into());
                    }
                    let name = table.name.clone();
                    let mut all = d.wb.tables().to_vec();
                    let table = all.iter_mut().find(|t| t.name == name).unwrap();
                    let enabled = table.filter.is_none();
                    table.filter = enabled.then(Default::default);
                    d.run("Table Filter", vec![GridOp::Tables(all)]);
                    return Ok(format!("Filter {} for {name}.", if enabled { "on" } else { "off" }));
                }
            }
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
        DataCmd::KeepValue | DataCmd::HideValue | DataCmd::ClearColumn | DataCmd::ShowAll | DataCmd::ReapplyFilter => {
            let scope = filter_scope(d, row, col).ok_or("There is no filter here.")?;
            let first = scope.first();
            let mut f = scope.auto_filter();
            let mut conds = f.conds.clone();
            let text = d.wb.formatted(CellAddr::new(s, row, col)).text;
            match cmd {
                DataCmd::ReapplyFilter => {}
                DataCmd::ShowAll => conds.clear(),
                DataCmd::ClearColumn => conds.retain(|(c, cond)| *c != col || cond.is_expression()),
                _ => {
                    if !(first..=f.rect.r1).contains(&row) || !(f.rect.c0..=f.rect.c1).contains(&col) {
                        return Err("Put the cursor on a value under the filter's header.".into());
                    }
                    let keep: Vec<String> = if cmd == DataCmd::KeepValue {
                        vec![text.clone()]
                    } else {
                        // Every value the column shows now, but this one.
                        const MAX: usize = 10_000;
                        let all = d.wb.filter_values_from(&f.rect, first, col, MAX + 1);
                        if all.len() > MAX {
                            return Err("This column has too many different values to hide one; use Show Only.".into());
                        }
                        let now: Option<Vec<String>> = conds.iter().find(|(c, cond)| *c == col && !cond.is_expression()).and_then(|(_, c)| match c {
                            Cond::Values(v) => Some(v.clone()),
                            _ => None,
                        });
                        let key = officina_grid::eval::funcs::text_key(&text);
                        all.into_iter().map(|(t, _)| t).filter(|t| now.as_ref().is_none_or(|n| n.iter().any(|x| officina_grid::eval::funcs::text_key(x) == officina_grid::eval::funcs::text_key(t)))).filter(|t| officina_grid::eval::funcs::text_key(t) != key).collect()
                    };
                    conds.retain(|(c, cond)| *c != col || cond.is_expression());
                    conds.push((col, Cond::Values(keep)));
                    conds.sort_by_key(|(c, _)| *c);
                }
            }
            f.conds = conds;
            change_filter(d, row, col, f)
        }
        // The window opens its value list for this one.
        DataCmd::Author(_) | DataCmd::CondFormats | DataCmd::PivotFields | DataCmd::CompareWith | DataCmd::BrowseVersions | DataCmd::PrintComments | DataCmd::TrackChanges | DataCmd::ReviewTracked | DataCmd::MergeCopies | DataCmd::AddSheet | DataCmd::DuplicateSheet | DataCmd::DefineName | DataCmd::NameManager | DataCmd::HideSheet | DataCmd::UnhideSheet(_) | DataCmd::RenameSheet | DataCmd::DeleteSheet | DataCmd::MoveSheetLeft | DataCmd::MoveSheetRight | DataCmd::TabColor | DataCmd::Comment | DataCmd::NewThread | DataCmd::OpenLink | DataCmd::Link | DataCmd::ChooseFromList | DataCmd::Validation | DataCmd::CircleInvalid | DataCmd::RowHeight | DataCmd::ColWidth | DataCmd::PasteSpecial | DataCmd::CustomLists => Ok(String::new()),
        DataCmd::ChooseValues => column_values(d, row, col).map(|v| format!("{} values in column {}.", v.len(), officina_grid::addr::col_name(col))),
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
            let table = Table { name: name.as_str().into(), rect: t.rect, header: t.header, totals: false, columns, style: Some("TableStyleMedium2".into()), stripes: true, filter: None };
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
    fn a_selection_kept_as_a_list_fills_on() {
        let mut d = doc();
        for (r, w) in ["North", "South", "East", "West"].iter().enumerate() {
            d.type_into(r as u32 + 20, 0, w);
        }
        let col = CellRange { r0: 20, c0: 0, r1: 23, c1: 0 };
        run(&mut d, col, (20, 0), DataCmd::AddCustomList);
        assert_eq!(d.status, "Kept a list of 4: a fill from any of them goes on through the rest.");
        d.type_into(30, 5, "east");
        let r = |r0, r1| Rect { sheet: 0, r0, c0: 5, r1, c1: 5 };
        assert!(d.fill(r(30, 30), r(30, 32), false));
        assert_eq!((cell(&d, 31, 5), cell(&d, 32, 5)), ("west".to_string(), "north".to_string()));
        // A block, or one entry: refused.
        run(&mut d, CellRange { r0: 20, c0: 0, r1: 21, c1: 1 }, (20, 0), DataCmd::AddCustomList);
        assert_eq!(d.status, "Select one row or one column of entries.");
        run(&mut d, one(20, 0), (20, 0), DataCmd::AddCustomList);
        assert_eq!(d.status, "A list needs two different entries or more.");
        run(&mut d, one(20, 0), (20, 0), DataCmd::ClearCustomLists);
        assert!(officina_grid::autofill::custom_lists().is_empty());
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
        // With the view scrolled to row 11, column B: frozen from there.
        d.view_top = (10, 1);
        run(&mut d, one(12, 3), (12, 3), DataCmd::FreezePanes);
        assert_eq!((d.wb.sheet(0).freeze, d.wb.sheet(0).freeze_from), ((12, 3), (10, 1)));
        assert_eq!(d.status, "Rows 11 to 12 and columns B to C stay in view; those above and left are out of reach until you unfreeze.");
        run(&mut d, one(12, 3), (12, 3), DataCmd::Unfreeze);
        assert_eq!((d.wb.sheet(0).freeze, d.wb.sheet(0).freeze_from), ((0, 0), (0, 0)));
        // Freeze Top Row keeps the row at the top of the view.
        run(&mut d, one(2, 1), (2, 1), DataCmd::FreezeTopRow);
        assert_eq!((d.wb.sheet(0).freeze, d.wb.sheet(0).freeze_from), ((11, 0), (10, 0)));
        assert_eq!(d.status, "Row 11 stays in view; those above and left are out of reach until you unfreeze.");
        // The cursor above the view's top: frozen from the first row.
        d.undo();
        run(&mut d, one(5, 0), (5, 0), DataCmd::FreezePanes);
        assert_eq!((d.wb.sheet(0).freeze, d.wb.sheet(0).freeze_from), ((5, 0), (0, 0)));
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
