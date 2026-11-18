//! How long one edit holds the UI thread: a huge sheet whose every row reads
//! one cell, and thousands of sheets that each read a cell on the first.
use std::time::Instant;

use atrium_sheets::bench::big_sheet;
use officina_grid::addr::CellAddr;
use officina_grid::ops::GridOp;
use officina_grid::Input;
use officina_grid::Value;

fn main() {
    let rows: u32 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(2_000_000);
    let t = Instant::now();
    let mut d = big_sheet(rows);
    println!("built {rows} rows x 10 in {:?}", t.elapsed());
    d.wb.fill_formula(0, 10, 0, rows - 1, "=B1*$M$1").unwrap();
    d.wb.recalc();
    let t = Instant::now();
    d.run("Edit", vec![GridOp::set(CellAddr::new(0, 0, 12), Input::Value(Value::Num(2.0)))]);
    println!("edit read by {rows} rows: UI thread held {:?}", t.elapsed());
    let t = Instant::now();
    d.run("Edit", vec![GridOp::set(CellAddr::new(0, 5, 1), Input::Value(Value::Num(3.0)))]);
    println!("edit read by 2 cells: {:?}", t.elapsed());

    let mut d = atrium_sheets::doc::Doc::new("many");
    let n = 4000;
    for i in 1..n {
        d.wb.add_sheet(&format!("S{i}")).unwrap();
    }
    for i in 1..n as u16 {
        d.wb.fill_formula(i, 0, 0, 99, "=Sheet1!$A$1+ROW()").unwrap();
        let v: Vec<f64> = (0..100).map(|r| r as f64).collect();
        d.wb.set_numbers(i, 1, 0, &v);
    }
    let t = Instant::now();
    d.wb.recalc();
    println!("{n} sheets first recalc {:?}", t.elapsed());
    let t = Instant::now();
    d.run("Edit", vec![GridOp::set(CellAddr::new(0, 0, 0), Input::Value(Value::Num(2.0)))]);
    println!("{n} sheets, edit read by all: {:?}", t.elapsed());
    let t = Instant::now();
    d.run("Edit", vec![GridOp::set(CellAddr::new(5, 0, 3), Input::Value(Value::Num(2.0)))]);
    println!("{n} sheets, lone edit: {:?}", t.elapsed());
}
