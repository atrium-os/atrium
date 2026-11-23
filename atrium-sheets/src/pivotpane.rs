//! The pivot field list (docs/spec/officina.md §16.61): a pivot's source fields,
//! where each one is (rows, columns, filters, values), and the changes the pane
//! makes, each one undo step through [`Workbook::pivot_op`].
//!
//! The pane is drawn by [`crate::app`]; this is its plain, testable model.

use officina_grid::pivot::{Agg, DataField, Pivot, PivotField};
use officina_grid::{CellAddr, Value, Workbook};

use crate::doc::Doc;

/// Where a field is in a pivot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Area {
    Rows,
    Columns,
    Filters,
    Values,
}

impl Area {
    pub const ALL: [Area; 4] = [Area::Rows, Area::Columns, Area::Filters, Area::Values];
    pub fn name(self) -> &'static str {
        match self {
            Area::Rows => "Rows",
            Area::Columns => "Columns",
            Area::Filters => "Filters",
            Area::Values => "Values",
        }
    }
}

/// The pane: which pivot, which of its source fields is picked, and why the last
/// change was refused.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PivotPane {
    pub open: bool,
    /// Into the workbook's pivots.
    pub pivot: usize,
    /// A source field, from the source's first column.
    pub picked: Option<u32>,
    pub error: String,
}

/// The pivot's fields: each source column's name and where it is (values first
/// found wins when a field is also summed).
pub fn fields(wb: &Workbook, p: &Pivot) -> Vec<(u32, String, Option<Area>)> {
    let width = p.source.c1 - p.source.c0 + 1;
    (0..width).chain(p.groups.iter().map(|group| group.field).filter(|&field| field >= width))
        .map(|field| (field, wb.pivot_field_name(p, field), area_of(p, field))).collect()
}

/// Where field `f` is.
pub fn area_of(p: &Pivot, f: u32) -> Option<Area> {
    if p.rows.iter().any(|x| x.field == f) {
        Some(Area::Rows)
    } else if p.cols.iter().any(|x| x.field == f) {
        Some(Area::Columns)
    } else if p.pages.iter().any(|x| x.field == f) {
        Some(Area::Filters)
    } else if p.data.iter().any(|x| x.field == f) {
        Some(Area::Values)
    } else {
        None
    }
}

/// Whether most of a source column's cells are numbers (so it is summed, not
/// counted, when it goes to Values).
fn numeric(wb: &Workbook, p: &Pivot, f: u32) -> bool {
    let s = p.source;
    let n = (s.r0 + 1..=s.r1).filter(|&r| matches!(wb.get(CellAddr::new(s.sheet, r, s.c0 + f)), Value::Num(_))).count();
    n * 2 > (s.r1 - s.r0) as usize
}

fn axis_mut(p: &mut Pivot, a: Area) -> Option<&mut Vec<PivotField>> {
    match a {
        Area::Rows => Some(&mut p.rows),
        Area::Columns => Some(&mut p.cols),
        Area::Filters => Some(&mut p.pages),
        Area::Values => None,
    }
}

/// Field `f` taken out of every area; its hidden items kept, for an axis it goes back to.
fn take(p: &mut Pivot, f: u32) -> Vec<Value> {
    let mut hidden = Vec::new();
    for v in [&mut p.rows, &mut p.cols, &mut p.pages] {
        if let Some(i) = v.iter().position(|x| x.field == f) {
            hidden = v.remove(i).hidden;
        }
    }
    p.data.retain(|d| d.field != f);
    hidden
}

/// Runs `f` on a copy of pivot `i` and puts it in place (one undo step), or says why not.
fn change(d: &mut Doc, i: usize, f: impl FnOnce(&Workbook, &mut Pivot)) -> Result<(), String> {
    let mut p = d.wb.pivots().get(i).ok_or("That pivot table is gone.")?.clone();
    let before = p.clone();
    f(&d.wb, &mut p);
    if p == before {
        return Ok(());
    }
    if p.data.is_empty() && p.rows.is_empty() && p.cols.is_empty() {
        return Err("A pivot table needs at least one field in Rows, Columns or Values.".into());
    }
    let op = d.wb.pivot_op(Some(i), Some(p)).map_err(|e| format!("Couldn't change the pivot table: {e}."))?;
    d.run("Change Pivot Table", vec![op]);
    Ok(())
}

/// Puts field `f` of pivot `i` in `to` (last there), or out of the pivot (`None`).
pub fn place(d: &mut Doc, i: usize, f: u32, to: Option<Area>) -> Result<(), String> {
    if f >= officina_grid::addr::MAX_COLS && to == Some(Area::Values) {
        return Err("Use the source field to add values for this group.".into());
    }
    change(d, i, |wb, p| {
        if area_of(p, f) == to {
            return;
        }
        let hidden = take(p, f);
        match to {
            Some(Area::Values) => {
                let agg = if numeric(wb, p, f) { Agg::Sum } else { Agg::Count };
                p.data.push(DataField { field: f, agg, name: None, show_as: Default::default() });
            }
            Some(a) => axis_mut(p, a).unwrap().push(PivotField { field: f, hidden }),
            None => {}
        }
    })
}

/// Moves field `f` one place earlier (`up`) or later in its area.
pub fn shift(d: &mut Doc, i: usize, f: u32, up: bool) -> Result<(), String> {
    change(d, i, |_, p| {
        let Some(a) = area_of(p, f) else { return };
        let swap = |len: usize, k: usize| -> Option<(usize, usize)> {
            let j = if up { k.checked_sub(1)? } else { k + 1 };
            (j < len).then_some((k, j))
        };
        match axis_mut(p, a) {
            Some(v) => {
                let k = v.iter().position(|x| x.field == f).unwrap();
                if let Some((k, j)) = swap(v.len(), k) {
                    v.swap(k, j);
                }
            }
            None => {
                let k = p.data.iter().position(|x| x.field == f).unwrap();
                if let Some((k, j)) = swap(p.data.len(), k) {
                    p.data.swap(k, j);
                }
            }
        }
    })
}

/// Value field `k` of pivot `i` summed up by `agg` (its own caption dropped, so it
/// reads "<Agg> of <field>").
pub fn set_agg(d: &mut Doc, i: usize, k: usize, agg: Agg) -> Result<(), String> {
    change(d, i, |_, p| {
        if let Some(x) = p.data.get_mut(k) {
            if x.agg != agg {
                x.agg = agg;
                x.name = None;
            }
        }
    })
}

/// Which total the pane's switches change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Totals {
    GrandRow,
    GrandCol,
    Subtotals,
    /// Compact form (else tabular).
    Compact,
}

pub fn toggle(d: &mut Doc, i: usize, t: Totals) -> Result<(), String> {
    change(d, i, |_, p| {
        let b = match t {
            Totals::GrandRow => &mut p.grand_row,
            Totals::GrandCol => &mut p.grand_col,
            Totals::Subtotals => &mut p.subtotals,
            Totals::Compact => &mut p.compact,
        };
        *b = !*b;
    })
}

/// An area's fields as one line ("Rows: Region, Product").
pub fn area_line(wb: &Workbook, p: &Pivot, a: Area) -> String {
    let names: Vec<String> = match a {
        Area::Rows => p.rows.iter().map(|x| wb.pivot_field_name(p, x.field)).collect(),
        Area::Columns => p.cols.iter().map(|x| wb.pivot_field_name(p, x.field)).collect(),
        Area::Filters => p.pages.iter().map(|x| wb.pivot_field_name(p, x.field)).collect(),
        Area::Values => p.data.iter().map(|x| wb.data_caption(p, x)).collect(),
    };
    format!("{}: {}", a.name(), if names.is_empty() { "—".to_string() } else { names.join(", ") })
}

/// The pivot whose output or source holds the cursor.
pub fn pivot_here(wb: &Workbook, sheet: u16, row: u32, col: u32) -> Option<usize> {
    if let Some((i, _)) = wb.pivot_at(CellAddr::new(sheet, row, col)) {
        return Some(i);
    }
    wb.pivots().iter().position(|p| p.source.sheet == sheet && (p.source.r0..=p.source.r1).contains(&row) && (p.source.c0..=p.source.c1).contains(&col))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Region, Product, Units, Price; a pivot of units by region.
    fn doc() -> Doc {
        let (mut d, _) = Doc::from_csv("t.csv", b"Region,Product,Units,Price\nNorth,Pear,3,1.5\nSouth,Fig,10,2\nNorth,Fig,7,2\nSouth,Pear,1,1.5\n").unwrap();
        d.wb.recalc();
        crate::data::run(&mut d, pergola::widgets::CellRange::of((0, 0), (0, 0)), (0, 0), crate::data::DataCmd::InsertPivot);
        assert_eq!(d.wb.pivots().len(), 1, "{}", d.status);
        d
    }

    fn cell(d: &Doc, r: u32, c: u32) -> String {
        d.wb.formatted(CellAddr::new(0, r, c)).text
    }

    #[test]
    fn fields_move_between_areas_as_undo_steps() {
        let mut d = doc();
        let p = d.wb.pivots()[0].clone();
        let (r0, c0) = (p.dest.row, p.dest.col);
        let f = fields(&d.wb, &p);
        assert_eq!(f.iter().map(|x| x.1.as_str()).collect::<Vec<_>>(), ["Region", "Product", "Units", "Price"]);
        assert_eq!(f.iter().map(|x| x.2).collect::<Vec<_>>(), [Some(Area::Rows), None, Some(Area::Values), Some(Area::Values)]);
        assert_eq!(area_line(&d.wb, &p, Area::Values), "Values: Sum of Units, Sum of Price");
        // Product across the columns.
        place(&mut d, 0, 1, Some(Area::Columns)).unwrap();
        assert_eq!(area_of(&d.wb.pivots()[0], 1), Some(Area::Columns));
        // Price out; Units counted instead of summed.
        place(&mut d, 0, 3, None).unwrap();
        set_agg(&mut d, 0, 0, Agg::Count).unwrap();
        let p = d.wb.pivots()[0].clone();
        assert_eq!(area_line(&d.wb, &p, Area::Values), "Values: Count of Units");
        assert_eq!(cell(&d, r0, c0), "Count of Units");
        // Region to the filters leaves Product's columns and the counts.
        place(&mut d, 0, 0, Some(Area::Filters)).unwrap();
        assert_eq!(area_line(&d.wb, &d.wb.pivots()[0], Area::Filters), "Filters: Region");
        // Region (text) to Values is counted.
        place(&mut d, 0, 0, Some(Area::Values)).unwrap();
        assert_eq!(d.wb.pivots()[0].data[1].agg, Agg::Count);
        shift(&mut d, 0, 0, true).unwrap();
        assert_eq!(d.wb.pivots()[0].data[0].field, 0);
        toggle(&mut d, 0, Totals::GrandCol).unwrap();
        assert!(!d.wb.pivots()[0].grand_col);
        toggle(&mut d, 0, Totals::Compact).unwrap();
        assert!(d.wb.pivots()[0].compact);
        for _ in 0..8 {
            assert!(d.undo());
        }
        assert_eq!(d.wb.pivots()[0], fields_pivot_start(&doc()));
    }

    fn fields_pivot_start(d: &Doc) -> Pivot {
        d.wb.pivots()[0].clone()
    }

    #[test]
    fn rows_reorder_and_empty_pivots_are_refused() {
        let mut d = doc();
        place(&mut d, 0, 1, Some(Area::Rows)).unwrap();
        assert_eq!(d.wb.pivots()[0].rows.iter().map(|x| x.field).collect::<Vec<_>>(), [0, 1]);
        shift(&mut d, 0, 1, true).unwrap();
        assert_eq!(d.wb.pivots()[0].rows.iter().map(|x| x.field).collect::<Vec<_>>(), [1, 0]);
        let p = d.wb.pivots()[0].clone();
        // Product is now the first label column, its first item under it.
        let h = (p.dest.row..p.dest.row + 4).find(|&r| cell(&d, r, p.dest.col) == "Product").unwrap();
        assert_eq!(cell(&d, h + 1, p.dest.col), "Fig");
        // Compact form puts both row fields in one column.
        toggle(&mut d, 0, Totals::Compact).unwrap();
        let p = d.wb.pivots()[0].clone();
        let h = (p.dest.row..p.dest.row + 4).find(|&r| cell(&d, r, p.dest.col) == "Row Labels").unwrap();
        assert_eq!((cell(&d, h + 1, p.dest.col), cell(&d, h + 2, p.dest.col)), ("Fig".into(), "North".into()));
        toggle(&mut d, 0, Totals::Compact).unwrap();
        // Moving the first one up again changes nothing.
        let before = d.wb.pivots()[0].clone();
        shift(&mut d, 0, 1, true).unwrap();
        assert_eq!(d.wb.pivots()[0], before);
        for f in [0, 1, 2] {
            place(&mut d, 0, f, None).unwrap();
        }
        assert!(place(&mut d, 0, 3, None).unwrap_err().contains("at least one field"));
        assert_eq!(d.wb.pivots()[0].data.len(), 1);
        assert_eq!(pivot_here(&d.wb, 0, 2, 1), Some(0));
        assert_eq!(pivot_here(&d.wb, 0, 40, 40), None);
    }
}
