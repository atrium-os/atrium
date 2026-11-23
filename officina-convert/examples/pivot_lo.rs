//! pivot_lo: our pivot tables through LibreOffice and back.
//!
//! Our writer → LibreOffice opens the file and saves it (the same format, and the
//! other one) → our reader: each pivot's definition should come back, and after a
//! refresh by us its cells should be what ours were. Needs `soffice` on PATH.
//! `KEEP=1` keeps the files; `RUNS=xlsx-xlsx,ods-ods` picks the runs.

use std::path::Path;
use std::process::Command;

use officina_grid::pivot::{Agg, DataField, Pivot, PivotField};
use officina_grid::{CellAddr, Rect, Value, Workbook};

fn book() -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("Data").unwrap();
    wb.add_sheet("Report").unwrap();
    let rows = [
        ["Region", "Rep", "Quarter", "Units", "Price"],
        ["North", "Ana", "Q1", "10", "2.5"],
        ["North", "Ana", "Q2", "20", "3"],
        ["North", "Bo", "Q1", "5", "4"],
        ["South", "Cy", "Q1", "7", "1"],
        ["South", "Cy", "Q2", "3", "2"],
        ["East", "Di", "Q2", "8", "5"],
        ["East", "Ed", "Q1", "12", "1.5"],
        ["West", "Fay", "Q3", "4", "6"],
    ];
    for (r, row) in rows.iter().enumerate() {
        for (c, v) in row.iter().enumerate() {
            wb.set_input(CellAddr::new(0, r as u32, c as u32), v).unwrap();
        }
    }
    wb.recalc();
    let src = Rect { sheet: 0, r0: 0, c0: 0, r1: 8, c1: 4 };
    let mut p = Pivot::new("Units by region", src, CellAddr::new(1, 2, 1));
    p.rows = vec![PivotField::new(0), PivotField { field: 1, hidden: vec![Value::str("Bo")] }];
    p.cols = vec![PivotField::new(2)];
    p.data = vec![DataField { field: 3, agg: Agg::Sum, name: None, show_as: Default::default() }];
    // COMPACT=1: this one in compact form.
    p.compact = std::env::var_os("COMPACT").is_some();
    wb.add_pivot(p).unwrap();
    let mut q = Pivot::new("Prices", src, CellAddr::new(0, 0, 7));
    q.rows = vec![PivotField::new(1)];
    q.pages = vec![PivotField { field: 0, hidden: vec![Value::str("South")] }];
    q.data = vec![DataField { field: 4, agg: Agg::Average, name: Some("Mean price".into()), show_as: Default::default() }, DataField { field: 3, agg: Agg::Max, name: None, show_as: Default::default() }];
    q.subtotals = false;
    wb.add_pivot(q).unwrap();
    let mut r = Pivot::new("Count", src, CellAddr::new(1, 20, 1));
    r.cols = vec![PivotField::new(0)];
    r.rows = vec![PivotField::new(2)];
    r.data = vec![DataField { field: 1, agg: Agg::Count, name: None, show_as: Default::default() }];
    r.grand_col = false;
    wb.add_pivot(r).unwrap();
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
    let runs = std::env::var("RUNS").unwrap_or_else(|_| "xlsx-xlsx,ods-ods,xlsx-ods,ods-xlsx".into());
    for run in runs.split(',') {
        let (a, b) = run.split_once('-').unwrap();
        go(a, b);
    }
}

fn go(from: &str, to: &str) {
    let wb = book();
    let dir = std::env::temp_dir().join(format!("pivot-lo-{from}-{to}-{}", std::process::id()));
    let (a, b) = (dir.join("ours"), dir.join("lo"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let x = a.join(format!("pv.{from}"));
    let bytes = if from == "ods" { officina_convert::ods::write(&wb, Vec::new()).unwrap() } else { officina_convert::xlsx::write(&wb, Vec::new()).unwrap() };
    std::fs::write(&x, bytes).unwrap();
    let (mut rb, loaded) = if to == "ods" { officina_convert::ods::read(&soffice(&dir, "ods:calc8", &x, &b)).unwrap() } else { officina_convert::xlsx::read(&soffice(&dir, "xlsx:Calc MS Excel 2007 XML", &x, &b)).unwrap() };
    println!("{from}→{to} report {:?}", loaded.report.human());
    let mut same = 0;
    for (i, p) in wb.pivots().iter().enumerate() {
        match rb.pivots().iter().find(|q| q.name == p.name) {
            Some(q) if q == p => same += 1,
            Some(q) => println!("  pivot {i}:\n    ours {p:?}\n    back {q:?}"),
            None => println!("  pivot {i} ({}) missing", p.name),
        }
    }
    // LibreOffice lays its output out its own way: refreshed by us, the cells match.
    let bad = rb.refresh_pivots();
    let mut cells_same = 0;
    for (i, p) in wb.pivots().iter().enumerate() {
        let Some(j) = rb.pivots().iter().position(|q| q.name == p.name) else { continue };
        let area = wb.pivot_shape(i).unwrap().area.unwrap();
        let ok = rb.pivot_shape(j).unwrap().area == Some(area) && (area.r0..=area.r1).all(|r| (area.c0..=area.c1).all(|c| wb.get(CellAddr::new(area.sheet, r, c)) == rb.get(CellAddr::new(area.sheet, r, c))));
        if ok {
            cells_same += 1;
        } else {
            println!("  pivot {i}: cells differ after refresh");
        }
    }
    println!("{from}→{to} summary: {} pivots, {same} same definition, {cells_same} same cells after refresh; refresh refused {bad:?}", wb.pivots().len());
    if std::env::var_os("KEEP").is_none() {
        let _ = std::fs::remove_dir_all(&dir);
    } else {
        println!("kept {}", dir.display());
    }
}
