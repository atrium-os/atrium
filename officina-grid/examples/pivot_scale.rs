//! pivot_scale: a pivot of 1,000,000 rows (region × rep down, quarter across,
//! units summed), timed.
//!
//!   cargo run --release --example pivot_scale [ROWS]

use officina_grid::pivot::{Agg, DataField, Pivot, PivotField};
use officina_grid::{CellAddr, Rect, Value, Workbook};
use std::time::Instant;

fn main() {
    let n: u32 = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(1_000_000);
    let mut wb = Workbook::new();
    wb.add_sheet("Data").unwrap();
    for (c, h) in ["Region", "Rep", "Quarter", "Units"].iter().enumerate() {
        wb.set_value(CellAddr::new(0, 0, c as u32), Value::str(h));
    }
    let regions: Vec<Value> = (0..8).map(|i| Value::str(&format!("Region {i}"))).collect();
    let reps: Vec<Value> = (0..200).map(|i| Value::str(&format!("Rep {i}"))).collect();
    let qs: Vec<Value> = (1..=4).map(|i| Value::str(&format!("Q{i}"))).collect();
    let mut x: u64 = 12345;
    for r in 1..=n {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let rep = (x >> 33) as usize % 200;
        wb.set_value(CellAddr::new(0, r, 0), regions[rep % 8].clone());
        wb.set_value(CellAddr::new(0, r, 1), reps[rep].clone());
        wb.set_value(CellAddr::new(0, r, 2), qs[(x >> 20) as usize % 4].clone());
        wb.set_value(CellAddr::new(0, r, 3), Value::Num(((x >> 40) % 100) as f64));
    }
    let mut p = Pivot::new("Big", Rect { sheet: 0, r0: 0, c0: 0, r1: n, c1: 3 }, CellAddr::new(0, 0, 6));
    p.rows = vec![PivotField::new(0), PivotField::new(1)];
    p.cols = vec![PivotField::new(2)];
    p.data = vec![DataField { field: 3, agg: Agg::Sum, name: None, show_as: Default::default() }];
    let t = Instant::now();
    let i = wb.add_pivot(p).unwrap();
    let ms = t.elapsed().as_secs_f64() * 1e3;
    let a = wb.pivot_shape(i).unwrap().area.unwrap();
    println!("pivot of {n} rows: {:.0} ms, output {} × {}", ms, a.r1 - a.r0 + 1, a.c1 - a.c0 + 1);
}
