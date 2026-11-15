//! condfmt_lo: our conditional formats through LibreOffice and back.
//!
//! Our writer → LibreOffice opens the file and saves it (the same format, and our
//! `.ods` as `.xlsx`) → our reader: each cell should look the same. The `.ods` book
//! keeps to what LibreOffice can hold (the first matching condition of a format).
//! Needs `soffice` on PATH. `KEEP=1` keeps the files.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use officina_grid::condfmt::{Cfvo, CmpOp, CondFormat, Dxf, Rule, RuleKind, TextOp};
use officina_grid::{CellAddr, Rect, Workbook};

fn book(lo: bool) -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("Data").unwrap();
    for i in 0..10u32 {
        wb.set_input(CellAddr::new(0, i, 0), &(i + 1).to_string()).unwrap();
        wb.set_input(CellAddr::new(0, i, 1), ["apple", "pear", "fig", "apple", "kiwi"][i as usize % 5]).unwrap();
        wb.set_input(CellAddr::new(0, i, 2), &((i * 7) % 10).to_string()).unwrap();
    }
    wb.set_input(CellAddr::new(0, 3, 1), "").unwrap();
    wb.set_input(CellAddr::new(0, 5, 1), "=1/0").unwrap();
    wb.recalc();
    let col = |c: u32| Rect { sheet: 0, r0: 0, c0: c, r1: 9, c1: c };
    let red = Dxf { fill: Some(0xFFC7CE), color: Some(0x9C0006), ..Dxf::default() };
    let bold = Dxf { bold: Some(true), italic: Some(true), ..Dxf::default() };
    let green = Dxf { fill: Some(0xC6EFCE), ..Dxf::default() };
    let f = |wb: &Workbook, s: &str| wb.cond_formula(s, 0, (0, 0)).unwrap();
    let a = vec![
        Rule { kind: RuleKind::CellIs { op: CmpOp::Between, a: f(&wb, "2"), b: Some(f(&wb, "$A$4")) }, dxf: red.clone(), stop: true },
        Rule { kind: RuleKind::Expression(f(&wb, "MOD(A1,3)=0")), dxf: bold.clone(), stop: lo },
        Rule { kind: RuleKind::Top { n: 20, percent: true, bottom: false }, dxf: green.clone(), stop: lo },
        Rule { kind: RuleKind::Average { below: true, equal: false, std_dev: 0 }, dxf: bold.clone(), stop: lo },
    ];
    let b = vec![
        Rule { kind: RuleKind::Text { op: TextOp::Contains, text: Arc::from("pp") }, dxf: red.clone(), stop: lo },
        Rule { kind: RuleKind::Text { op: TextOp::EndsWith, text: Arc::from("g") }, dxf: green.clone(), stop: lo },
        Rule { kind: RuleKind::Errors(true), dxf: bold.clone(), stop: lo },
        Rule { kind: RuleKind::Blanks(true), dxf: green.clone(), stop: lo },
    ];
    let c = vec![
        Rule { kind: RuleKind::ColorScale(vec![(Cfvo::Min, 0xF8696B), (Cfvo::Percentile(50.0), 0xFFEB84), (Cfvo::Max, 0x63BE7B)]), dxf: Dxf::default(), stop: false },
        Rule { kind: RuleKind::Duplicates { unique: false }, dxf: bold.clone(), stop: lo },
    ];
    let d = vec![Rule { kind: RuleKind::DataBar { min: Cfvo::Min, max: Cfvo::Max, color: 0x638EC6 }, dxf: Dxf::default(), stop: false }];
    wb.add_cond_format(CondFormat { sheet: 0, ranges: vec![col(0)], rules: a }).unwrap();
    wb.add_cond_format(CondFormat { sheet: 0, ranges: vec![col(1)], rules: b }).unwrap();
    wb.add_cond_format(CondFormat { sheet: 0, ranges: vec![col(2)], rules: c }).unwrap();
    wb.add_cond_format(CondFormat { sheet: 0, ranges: vec![Rect { sheet: 0, r0: 0, c0: 3, r1: 9, c1: 3 }], rules: d }).unwrap();
    for i in 0..10u32 {
        wb.set_input(CellAddr::new(0, i, 3), &((i * 3) % 7 + 1).to_string()).unwrap();
    }
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
    for (from, to) in std::env::var("RUNS").map_or(vec![("xlsx".to_string(), "xlsx".to_string()), ("ods".into(), "ods".into()), ("ods".into(), "xlsx".into())], |r| r.split(',').map(|p| { let (a, b) = p.split_once('-').unwrap(); (a.to_string(), b.to_string()) }).collect()) {
        run(&from, &to);
    }
}

fn run(from: &str, to: &str) {
    let wb = book(from == "ods");
    let dir = std::env::temp_dir().join(format!("condfmt-lo-{from}-{to}-{}", std::process::id()));
    let (a, b) = (dir.join("ours"), dir.join("lo"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let x = a.join(format!("cf.{from}"));
    let bytes = if from == "ods" { officina_convert::ods::write(&wb, Vec::new()).unwrap() } else { officina_convert::xlsx::write(&wb, Vec::new()).unwrap() };
    std::fs::write(&x, bytes).unwrap();
    let (rb, loaded) = if to == "ods" {
        officina_convert::ods::read(&soffice(&dir, "ods:calc8", &x, &b)).unwrap()
    } else {
        officina_convert::xlsx::read(&soffice(&dir, "xlsx:Calc MS Excel 2007 XML", &x, &b)).unwrap()
    };
    println!("{from}→{to} report {:?}", loaded.report.human());
    let (mut same, mut all) = (0, 0);
    for r in 0..10 {
        for c in 0..4 {
            all += 1;
            let (p, q) = (wb.look(CellAddr::new(0, r, c)), rb.look(CellAddr::new(0, r, c)));
            // LibreOffice's bars and scales are its own sums: compare which cells have one.
            if p.dxf == q.dxf && p.scale.is_some() == q.scale.is_some() && p.bar.is_some() == q.bar.is_some() {
                same += 1;
                if p.scale != q.scale || p.bar.map(|b| (b.0 * 100.0).round()) != q.bar.map(|b| (b.0 * 100.0).round()) {
                    println!("  {r},{c}: scale/bar {:?}/{:?} vs {:?}/{:?}", p.scale, p.bar, q.scale, q.bar);
                }
            } else {
                println!("  {r},{c}: ours {p:?}\n        back {q:?}");
            }
        }
    }
    println!("{from}→{to} summary: {} formats, {} back; {same}/{all} cells look the same", wb.cond_formats().len(), rb.cond_formats().len());
    if std::env::var_os("KEEP").is_none() {
        let _ = std::fs::remove_dir_all(&dir);
    } else {
        println!("kept {}", dir.display());
    }
}
