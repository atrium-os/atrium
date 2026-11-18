//! Formulas whose cost grows with the square of the rows: how long they hold the
//! UI thread when an input changes.
use std::time::Instant;

use atrium_sheets::bench::big_sheet;
use officina_grid::addr::CellAddr;
use officina_grid::ops::GridOp;
use officina_grid::{Input, Value};

fn main() {
    let rows: u32 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(100_000);
    for (name, f) in [("running SUM", "=SUM($B$1:B1)"), ("SUMIF", "=SUMIF(C:C,C1,B:B)"), ("VLOOKUP", "=VLOOKUP(A1,A:B,2,FALSE)"), ("COUNTIF", "=COUNTIF(C:C,C1)")] {
        let mut d = big_sheet(rows);
        let t = Instant::now();
        d.wb.fill_formula(0, 10, 0, rows - 1, f).unwrap();
        d.wb.recalc();
        let first = t.elapsed();
        let t = Instant::now();
        d.run("Edit", vec![GridOp::set(CellAddr::new(0, 0, 1), Input::Value(Value::Num(2.0)))]);
        d.run("Edit", vec![GridOp::set(CellAddr::new(0, 0, 2), Input::Value(Value::Num(7.0)))]);
        println!("{name} x {rows}: fill {first:?}, two edits {:?}", t.elapsed());
    }
}
