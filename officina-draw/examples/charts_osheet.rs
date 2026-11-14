//! charts_osheet: a native sheet with data and three charts in view, to look at in
//! Sheets (`OPEN=… BENCH=none sh scripts/sheets-vm.sh`). Argument: the output path.

use officina_grid::addr::{CellAddr, Rect};
use officina_grid::charts::*;
use officina_grid::{Value, Workbook};

fn main() {
    let out = std::env::args().nth(1).expect("output path");
    let mut wb = Workbook::new();
    wb.add_sheet("Sheet1").unwrap();
    for (i, h) in ["Month", "Sales", "Costs"].iter().enumerate() {
        wb.set_value(CellAddr::new(0, 0, i as u32), Value::str(h));
    }
    for r in 1..=12u32 {
        wb.set_value(CellAddr::new(0, r, 0), Value::str(&format!("M{r}")));
        wb.set_value(CellAddr::new(0, r, 1), Value::Num((r * 7 % 11) as f64 + 1.0));
        wb.set_formula(CellAddr::new(0, r, 2), &format!("=B{}*0.6+1", r + 1)).unwrap();
    }
    wb.recalc();
    let col = |c: u32| Rect { sheet: 0, r0: 1, c0: c, r1: 12, c1: c };
    let ser = |c: u32, kind| Series { name: Some(ChartText::Cell(CellAddr::new(0, 0, c))), cats: Some(col(0)), vals: col(c), kind };
    let mk = |kind, series, from: (u32, u32), to: (u32, u32), title: &str| Chart {
        kind,
        grouping: Grouping::Standard,
        title: Some(ChartText::Lit(title.into())),
        series,
        legend: Some(LegendPos::Bottom),
        x_title: None,
        y_title: None,
        anchor: Anchor { sheet: 0, from: (from.0, from.1, 0.0, 0.0), to: (to.0, to.1, 0.0, 0.0) },
    };
    wb.add_chart(mk(ChartKind::Column, vec![ser(1, None), ser(2, Some(ChartKind::Line))], (1, 4), (16, 10), "Sales and costs")).unwrap();
    wb.add_chart(mk(ChartKind::Pie, vec![ser(1, None)], (17, 4), (32, 8), "Sales by month")).unwrap();
    wb.add_chart(mk(ChartKind::Area, vec![ser(2, None)], (17, 8), (32, 13), "Costs")).unwrap();
    let mut f = std::fs::File::create(&out).unwrap();
    officina_grid::native::save(&wb, &mut f).unwrap();
}
