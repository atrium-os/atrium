//! Print settings stored as sheet-scoped names so they follow cell references
//! through structural edits and native Save and reopen.
use crate::formula::ast::{Coord::Abs, Expr, RangeKind};
use crate::Workbook;

pub const AREA: &str = "_xlnm.Print_Area";
pub const TITLES: &str = "_xlnm.Print_Titles";
// XLSX puts both axes in one name formula. Store them independently so the
// grid can rebase each range and reuse its existing name and journal format.
pub const ROWS: &str = "_xlnm.Print_Title_Rows";
pub const COLS: &str = "_xlnm.Print_Title_Columns";

pub fn is_title_name(name: &str) -> bool {
    [TITLES, ROWS, COLS]
        .iter()
        .any(|title| name.eq_ignore_ascii_case(title))
}

/// Absolute, sheet-local print rectangle, with exclusive end coordinates.
pub fn area(expr: &Expr, sheet: u16) -> Option<[u32; 4]> {
    match expr {
        Expr::Ref(r) if r.sheet.is_none_or(|s| s == sheet) => match (r.row, r.col) {
            (Abs(row), Abs(col)) => Some([row, col, row + 1, col + 1]),
            _ => None,
        },
        Expr::Range(r) if r.kind == RangeKind::Cells && r.sheet.is_none_or(|s| s == sheet) => {
            match (r.r0, r.c0, r.r1, r.c1) {
                (Abs(r0), Abs(c0), Abs(r1), Abs(c1)) => {
                    Some([r0.min(r1), c0.min(c1), r0.max(r1) + 1, c0.max(c1) + 1])
                }
                _ => None,
            }
        }
        _ => None,
    }
}

pub type Band = (u32, u32);

/// The axis name and absolute, half-open band represented by a title formula.
pub fn title(expr: &Expr, sheet: u16) -> Option<(&'static str, Band)> {
    let Expr::Range(r) = expr else { return None };
    if r.sheet.is_some_and(|s| s != sheet) {
        return None;
    }
    match (r.kind, r.r0, r.r1, r.c0, r.c1) {
        (RangeKind::Rows, Abs(a), Abs(b), _, _) => Some((ROWS, (a.min(b), a.max(b) + 1))),
        (RangeKind::Cols, _, _, Abs(a), Abs(b)) => Some((COLS, (a.min(b), a.max(b) + 1))),
        _ => None,
    }
}

pub fn titles(wb: &Workbook, sheet: u16) -> (Option<Band>, Option<Band>) {
    let band = |axis: &str| {
        [axis, TITLES].into_iter().find_map(|name| {
            let def = wb.names().get(sheet, name)?;
            let (kind, band) = title(&def.expr, sheet)?;
            (kind == axis).then_some(band)
        })
    };
    (band(ROWS), band(COLS))
}
