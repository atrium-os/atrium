//! Writes a small styled workbook (`.osheet`) to look at: number formats, a header
//! row, fills, borders, alignment.
//!
//!   cargo run --example styled_demo -- OUT.osheet

use officina_grid::addr::{CellAddr, Rect};
use officina_grid::style::{Edge, HAlign, Line, Style};
use officina_grid::{Value, Workbook};

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "styled.osheet".into());
    let mut wb = Workbook::new();
    wb.add_sheet("Sales").unwrap();
    let put = |wb: &mut Workbook, r: u32, c: u32, s: &str| wb.set_input(CellAddr::new(0, r, c), s).unwrap();
    for (c, h) in ["Region", "Opened", "Q1", "Q2", "Total", "Growth", "Status"].iter().enumerate() {
        put(&mut wb, 0, c as u32, h);
    }
    let regions = ["North", "South", "East", "West", "Central", "Islands"];
    for (i, r) in regions.iter().enumerate() {
        let row = i as u32 + 1;
        put(&mut wb, row, 0, r);
        wb.set_value(CellAddr::new(0, row, 1), Value::Num(45000.0 + 211.0 * i as f64));
        wb.set_value(CellAddr::new(0, row, 2), Value::Num(10_000.0 + 1_234.5 * i as f64));
        wb.set_value(CellAddr::new(0, row, 3), Value::Num(9_800.0 + 1_510.25 * i as f64 - 3000.0 * (i % 2) as f64));
        put(&mut wb, row, 4, &format!("=C{0}+D{0}", row + 1));
        put(&mut wb, row, 5, &format!("=D{0}/C{0}-1", row + 1));
        put(&mut wb, row, 6, &format!("=IF(F{0}<0,\"behind\",\"on track\")", row + 1));
    }
    let n = regions.len() as u32;
    put(&mut wb, n + 1, 0, "All");
    for c in ["C", "D", "E"] {
        let col = (c.as_bytes()[0] - b'A') as u32;
        put(&mut wb, n + 1, col, &format!("=SUM({c}2:{c}{})", n + 1));
    }
    wb.recalc();
    let rect = |r0, c0, r1, c1| Rect { sheet: 0, r0, c0, r1, c1 };
    let mut head = Style::default();
    head.font.bold = true;
    head.font.color = Some(0xFFFFFF);
    head.fill = Some(0x1F4E79);
    head.h_align = HAlign::Center;
    wb.set_style(&rect(0, 0, 0, 6), head);
    wb.set_style(&rect(1, 1, n, 1), Style::default().with_num_fmt("d mmm yyyy"));
    let money = Style::default().with_num_fmt("\"$\"#,##0.00");
    wb.set_style(&rect(1, 2, n, 4), money.clone());
    wb.set_style(&rect(1, 5, n, 5), Style::default().with_num_fmt("0.0%;[Red]-0.0%"));
    let mut status = Style::default();
    status.h_align = HAlign::Center;
    status.font.color = Some(0x375623);
    wb.set_style(&rect(1, 6, n, 6), status);
    let mut total = money.clone();
    total.font.bold = true;
    total.fill = Some(0xE2EFDA);
    total.borders[2] = Edge { line: Line::Thin, color: None };
    total.borders[3] = Edge { line: Line::Double, color: None };
    wb.set_style(&rect(n + 1, 2, n + 1, 4), total);
    let mut label = Style::default();
    label.font.bold = true;
    wb.set_style(&rect(n + 1, 0, n + 1, 0), label);
    wb.sheet_mut(0).col_sizes.set(1, 100.0);
    for c in 2..=4 {
        wb.sheet_mut(0).col_sizes.set(c, 90.0);
    }
    std::fs::write(&out, officina_grid::native::save_to_vec(&wb)).unwrap();
    println!("{out}");
}
