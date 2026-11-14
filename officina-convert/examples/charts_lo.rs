//! charts_lo: our `.xlsx` and `.ods` charts through LibreOffice and back.
//!
//! Our writer → LibreOffice opens the file and saves the same format again → our reader:
//! each chart's kind, grouping, series ranges, titles and legend should come back.
//! Needs `soffice` on PATH. `KEEP=1` keeps the files.

use std::path::Path;
use std::process::Command;

use officina_grid::addr::{CellAddr, Rect};
use officina_grid::charts::*;
use officina_grid::{Value, Workbook};

fn book() -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("Data").unwrap();
    wb.add_sheet("Charts here").unwrap();
    wb.set_value(CellAddr::new(0, 0, 0), Value::str("Month"));
    wb.set_value(CellAddr::new(0, 0, 1), Value::str("Sales"));
    wb.set_value(CellAddr::new(0, 0, 2), Value::str("Costs"));
    for r in 1..=12u32 {
        wb.set_value(CellAddr::new(0, r, 0), Value::str(&format!("M{r}")));
        wb.set_value(CellAddr::new(0, r, 1), Value::Num((r * 7 % 11) as f64 + 1.0));
        wb.set_value(CellAddr::new(0, r, 2), Value::Num((r * 5 % 7) as f64 + 0.5));
    }
    let col = |c: u32| Rect { sheet: 0, r0: 1, c0: c, r1: 12, c1: c };
    let ser = |c: u32, kind| Series { name: Some(ChartText::Cell(CellAddr::new(0, 0, c))), cats: Some(col(0)), vals: col(c), kind };
    let kinds = [
        (ChartKind::Column, Grouping::Standard),
        (ChartKind::Column, Grouping::Stacked),
        (ChartKind::Bar, Grouping::Percent),
        (ChartKind::Line, Grouping::Standard),
        (ChartKind::Area, Grouping::Stacked),
        (ChartKind::Scatter, Grouping::Standard),
        (ChartKind::Pie, Grouping::Standard),
        (ChartKind::Doughnut, Grouping::Standard),
    ];
    for (i, (kind, grouping)) in kinds.into_iter().enumerate() {
        let series = if kind.round() {
            vec![ser(1, None)]
        } else if kind == ChartKind::Scatter {
            vec![Series { name: Some(ChartText::Cell(CellAddr::new(0, 0, 2))), cats: Some(col(1)), vals: col(2), kind: None }]
        } else {
            vec![ser(1, None), ser(2, None)]
        };
        let r = 20 * i as u32;
        wb.add_chart(Chart {
            kind,
            grouping,
            title: Some(ChartText::Lit(format!("{kind:?} {grouping:?}").into())),
            series,
            legend: Some(if i % 2 == 0 { LegendPos::Right } else { LegendPos::Bottom }),
            x_title: (!kind.round()).then(|| "Month".into()),
            y_title: (!kind.round()).then(|| "Amount".into()),
            anchor: Anchor { sheet: 1, from: (r, 1, 0.0, 0.0), to: (r + 18, 8, 0.0, 0.0) },
        })
        .unwrap();
    }
    // A combo: columns with a line.
    wb.add_chart(Chart {
        kind: ChartKind::Column,
        grouping: Grouping::Standard,
        title: None,
        series: vec![ser(1, None), ser(2, Some(ChartKind::Line))],
        legend: Some(LegendPos::Top),
        x_title: None,
        y_title: None,
        anchor: Anchor { sheet: 1, from: (200, 1, 0.0, 0.0), to: (218, 8, 0.0, 0.0) },
    })
    .unwrap();
    wb.recalc();
    wb
}

fn soffice(dir: &Path, filter: &str, input: &Path, out: &Path) -> Vec<u8> {
    let st = Command::new("soffice").args(["--headless", "--norestore", "--convert-to", filter, "--outdir"]).arg(out).arg(input).env("HOME", dir).output().expect("soffice");
    let ext = filter.split(':').next().unwrap();
    let p = out.join(input.file_stem().unwrap()).with_extension(ext);
    std::fs::read(&p).unwrap_or_else(|e| panic!("soffice gave no {}: {e}\n{}", p.display(), String::from_utf8_lossy(&st.stderr)))
}

fn main() {
    for fmt in std::env::var("FMT").map_or(vec!["xlsx".to_string(), "ods".to_string()], |f| vec![f]) {
        run(&fmt);
    }
}

fn run(fmt: &str) {
    let wb = book();
    let dir = std::env::temp_dir().join(format!("charts-lo-{fmt}-{}", std::process::id()));
    let (a, b) = (dir.join("ours"), dir.join("lo"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let x = a.join(format!("charts.{fmt}"));
    let (rb, loaded) = if fmt == "ods" {
        std::fs::write(&x, officina_convert::ods::write(&wb, Vec::new()).unwrap()).unwrap();
        officina_convert::ods::read(&soffice(&dir, "ods:calc8", &x, &b)).unwrap()
    } else {
        std::fs::write(&x, officina_convert::xlsx::write(&wb, Vec::new()).unwrap()).unwrap();
        officina_convert::xlsx::read(&soffice(&dir, "xlsx:Calc MS Excel 2007 XML", &x, &b)).unwrap()
    };
    println!("report {:?}", loaded.report.human());
    let mut same = 0;
    for (i, c) in wb.charts().iter().enumerate() {
        match rb.charts().get(i) {
            Some(d) if d == c => same += 1,
            Some(d) => {
                // Anchors move by LibreOffice's rounding: compare the rest.
                let mut d2 = d.clone();
                d2.anchor = c.anchor;
                if d2 == *c {
                    same += 1;
                    println!("chart {i}: anchor {:?} → {:?}", c.anchor, d.anchor);
                } else {
                    println!("chart {i}:\n  ours {c:?}\n  back {d:?}");
                }
            }
            None => println!("chart {i}: missing"),
        }
    }
    // LibreOffice's own drawing of the first data, as a check that it read the values.
    let pdf = soffice(&dir, "pdf", &x, &b);
    println!("{fmt} summary: {} charts, {same} same after LibreOffice; LibreOffice drew a {} byte PDF", wb.charts().len(), pdf.len());
    if std::env::var_os("KEEP").is_none() {
        let _ = std::fs::remove_dir_all(&dir);
    } else {
        println!("kept {}", dir.display());
    }
}
