//! Writes a workbook (`.osheet`) showing O5f: conditional formats (a colour scale,
//! data bars, top 3, text and formula rules) beside spilled arrays (`SORT`,
//! `UNIQUE`, `SEQUENCE`, `LET`, `LAMBDA`).
//!
//!   cargo run --example condfmt_demo -- OUT.osheet

use std::sync::Arc;

use officina_grid::addr::{CellAddr, Rect};
use officina_grid::condfmt::{Cfvo, CmpOp, CondFormat, Dxf, Rule, RuleKind, TextOp};
use officina_grid::style::{HAlign, Style};
use officina_grid::Workbook;

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "condfmt.osheet".into());
    let mut wb = Workbook::new();
    wb.add_sheet("Sales").unwrap();
    let put = |wb: &mut Workbook, r: u32, c: u32, s: &str| wb.set_input(CellAddr::new(0, r, c), s).unwrap();
    for (c, h) in ["Rep", "Region", "Units", "Margin", "Trend", "", "Regions", "Top units", "Squares", "Doubled"].iter().enumerate() {
        put(&mut wb, 0, c as u32, h);
    }
    let reps = [("Ana", "North", 120, 0.31), ("Bo", "South", 85, 0.12), ("Cy", "East", 140, 0.27), ("Di", "North", 60, -0.05), ("Ed", "West", 175, 0.35), ("Fay", "South", 95, 0.18), ("Gus", "East", 40, 0.02), ("Hal", "West", 150, 0.22), ("Ivy", "North", 110, 0.15), ("Jo", "Central", 70, 0.09)];
    for (i, (n, reg, u, m)) in reps.iter().enumerate() {
        let r = i as u32 + 1;
        put(&mut wb, r, 0, n);
        put(&mut wb, r, 1, reg);
        put(&mut wb, r, 2, &u.to_string());
        put(&mut wb, r, 3, &m.to_string());
        put(&mut wb, r, 4, &format!("=C{}", r + 1));
    }
    put(&mut wb, 1, 6, "=SORT(UNIQUE(B2:B11))");
    put(&mut wb, 1, 7, "=TAKE(SORT(C2:C11,,-1),3)");
    put(&mut wb, 1, 8, "=SEQUENCE(5)^2");
    put(&mut wb, 1, 9, "=MAP(I2#,LAMBDA(x,LET(d,x*2,d+1)))");
    wb.recalc();
    let rect = |r0, c0, r1, c1| Rect { sheet: 0, r0, c0, r1, c1 };
    let mut head = Style::default();
    head.font.bold = true;
    head.font.color = Some(0xFFFFFF);
    head.fill = Some(0x1F4E79);
    head.h_align = HAlign::Center;
    wb.set_style(&rect(0, 0, 0, 4), head.clone());
    wb.set_style(&rect(0, 6, 0, 9), head);
    wb.set_style(&rect(1, 3, 10, 3), Style::default().with_num_fmt("0%"));
    let f = |wb: &Workbook, s: &str, o: (u32, u32)| wb.cond_formula(s, 0, o).unwrap();
    let red = Dxf { fill: Some(0xFFC7CE), color: Some(0x9C0006), ..Dxf::default() };
    let green = Dxf { fill: Some(0xC6EFCE), color: Some(0x006100), bold: Some(true), ..Dxf::default() };
    let rules = |kind, dxf| Rule { kind, dxf, stop: false };
    wb.add_cond_format(CondFormat { sheet: 0, ranges: vec![rect(1, 2, 10, 2)], rules: vec![rules(RuleKind::Top { n: 3, percent: false, bottom: false }, green.clone())] }).unwrap();
    wb.add_cond_format(CondFormat { sheet: 0, ranges: vec![rect(1, 3, 10, 3)], rules: vec![rules(RuleKind::ColorScale(vec![(Cfvo::Min, 0xF8696B), (Cfvo::Percentile(50.0), 0xFFEB84), (Cfvo::Max, 0x63BE7B)]), Dxf::default())] }).unwrap();
    wb.add_cond_format(CondFormat { sheet: 0, ranges: vec![rect(1, 4, 10, 4)], rules: vec![rules(RuleKind::DataBar { min: Cfvo::Num(0.0), max: Cfvo::Max, color: 0x638EC6 }, Dxf::default())] }).unwrap();
    wb.add_cond_format(CondFormat { sheet: 0, ranges: vec![rect(1, 1, 10, 1)], rules: vec![rules(RuleKind::Text { op: TextOp::Contains, text: Arc::from("north") }, Dxf { bold: Some(true), color: Some(0x1F4E79), ..Dxf::default() })] }).unwrap();
    wb.add_cond_format(CondFormat { sheet: 0, ranges: vec![rect(1, 0, 10, 0)], rules: vec![rules(RuleKind::Expression(f(&wb, "$D2<0.1", (1, 0))), red.clone())] }).unwrap();
    wb.add_cond_format(CondFormat { sheet: 0, ranges: vec![rect(1, 8, 5, 9)], rules: vec![rules(RuleKind::CellIs { op: CmpOp::Greater, a: f(&wb, "10", (1, 8)), b: None }, green)] }).unwrap();
    std::fs::write(&out, officina_grid::native::save_to_vec(&wb)).unwrap();
    println!("wrote {out}");
}
