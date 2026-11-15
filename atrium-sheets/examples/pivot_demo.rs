//! Writes a workbook (`.osheet`) showing O5g: sales rows and a pivot table of
//! them (region and rep down, quarter across, units summed).
//!
//!   cargo run --example pivot_demo -- OUT.osheet

use officina_grid::pivot::{Agg, DataField, Pivot, PivotField};
use officina_grid::{CellAddr, Rect, Workbook};

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "pivot.osheet".into());
    let mut wb = Workbook::new();
    wb.add_sheet("Sales").unwrap();
    let rows = [
        ("Region", "Rep", "Quarter", "Units"),
        ("North", "Ana", "Q1", "120"),
        ("North", "Ana", "Q2", "95"),
        ("North", "Bo", "Q1", "60"),
        ("North", "Bo", "Q3", "80"),
        ("South", "Cy", "Q1", "70"),
        ("South", "Cy", "Q2", "45"),
        ("South", "Fay", "Q3", "130"),
        ("East", "Di", "Q2", "88"),
        ("East", "Ed", "Q1", "40"),
        ("East", "Ed", "Q3", "52"),
        ("West", "Hal", "Q1", "150"),
        ("West", "Hal", "Q2", "110"),
    ];
    for (r, (a, b, c, d)) in rows.iter().enumerate() {
        for (k, v) in [a, b, c, d].iter().enumerate() {
            wb.set_input(CellAddr::new(0, r as u32, k as u32), v).unwrap();
        }
    }
    wb.recalc();
    let mut p = Pivot::new("PivotTable1", Rect { sheet: 0, r0: 0, c0: 0, r1: rows.len() as u32 - 1, c1: 3 }, CellAddr::new(0, 0, 5));
    p.rows = vec![PivotField::new(0), PivotField::new(1)];
    p.cols = vec![PivotField::new(2)];
    p.data = vec![DataField { field: 3, agg: Agg::Sum, name: None }];
    wb.add_pivot(p).unwrap();
    wb.recalc();
    std::fs::write(&out, officina_grid::native::save_to_vec(&wb)).unwrap();
    println!("wrote {out}");
}
