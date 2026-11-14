//! data_demo: a sheet made with the Data menu's commands (a table, a filter with a
//! hidden value, a chart) and macros kept inert, saved as an `.osheet` to look at.
//!
//!   cargo run --example data_demo -- out.osheet

use atrium_sheets::data::{run, DataCmd};
use atrium_sheets::doc::Doc;
use officina_grid::charts::ChartKind;
use officina_grid::macros::*;
use pergola::widgets::CellRange;

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "data-demo.osheet".into());
    let mut csv = String::from("Region,Q1,Q2,Q3,,Item,Kind,Stock\n");
    let regions = ["North", "South", "East", "West", "Central", "Coast", "Hills", "Islands"];
    let items = [("Pear", "fruit", 12), ("Kale", "leaf", 4), ("Fig", "fruit", 9), ("Leek", "root", 15), ("Plum", "fruit", 3), ("Beet", "root", 7), ("Chard", "leaf", 11), ("Lime", "fruit", 6)];
    for i in 0..8 {
        let (it, k, n) = items[i];
        csv.push_str(&format!("{},{},{},{},,{it},{k},{n}\n", regions[i], 1100 + i * 317 % 900, 900 + i * 211 % 700, 1300 + i * 97 % 500));
    }
    let mut d = Doc::from_csv("demo.csv", csv.as_bytes()).unwrap().0;
    let one = |r, c| CellRange { r0: r, c0: c, r1: r, c1: c };
    run(&mut d, one(1, 1), (1, 1), DataCmd::FormatAsTable);
    println!("{}", d.status);
    run(&mut d, one(1, 1), (1, 1), DataCmd::Chart(ChartKind::Column));
    println!("{}", d.status);
    run(&mut d, one(1, 5), (1, 5), DataCmd::Filter);
    println!("{}", d.status);
    // Cursor on a "root" cell: hide roots.
    run(&mut d, one(4, 6), (4, 6), DataCmd::HideValue);
    println!("{}", d.status);
    d.wb.set_macros(Macros {
        family: MacroFamily::Ooxml,
        parts: vec![MacroPart { path: "xl/vbaProject.bin".into(), media_type: "application/vnd.ms-office.vbaProject".into(), data: vec![0; 64].into() }],
        workbook_code: None,
        sheet_codes: vec![],
    })
    .unwrap();
    std::fs::write(&out, d.write_native(Vec::new()).unwrap()).unwrap();
    println!("wrote {out}");
}
