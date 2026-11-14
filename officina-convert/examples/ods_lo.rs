//! ods_lo: `.ods` against LibreOffice, both directions.
//!
//!   cargo run --release --example ods_lo
//!
//! One workbook: officina-grid's conformance fixture and its formulas (each with the
//! result Excel shows), plus a column of styled cells (number formats, fonts, fills,
//! borders, alignment).
//!
//! - In: our `.xlsx` writer → LibreOffice saves `.ods` (its own OpenFormula, data
//!   styles, automatic styles, repeats) → our `.ods` reader. Every formula must
//!   recalculate to Excel's result; every styled cell must show and look the same.
//! - Out: our `.ods` writer → LibreOffice saves `.xlsx` → our `.xlsx` reader (already
//!   checked against LibreOffice by lo_check and styles_lo). Same checks.
//!
//! Needs `soffice` on PATH. Prints the differences and a summary line per direction.

use std::path::Path;
use std::process::Command;

use officina_grid::addr::{CellAddr, Rect};
use officina_grid::filter::{AutoFilter, Cond};
use officina_grid::style::{Edge, HAlign, Line, Style, VAlign};
use officina_grid::{Value, Workbook};

const COL: u32 = 25; // Z, as in tests/conformance.rs
const SCOL: u32 = 1;
/// Formulas using defined names (the last on the second sheet, where `Local` is).
const NAMED: [&str; 4] = ["=COUNTIF(Fruit,\"a*\")", "=SUM(A1:A5)*Rate", "=Doubled+1", "=Local"]; // styled cells, column B of the third sheet

fn cases() -> Vec<(String, String)> {
    let src = include_str!("../../officina-grid/tests/conformance.rs");
    let unq = |s: &str| s.replace("\\\"", "\"").replace("\\\\", "\\");
    let mut out = Vec::new();
    for line in src.lines() {
        let l = line.trim();
        let Some(rest) = l.strip_prefix("(\"") else { continue };
        let Some(mid) = find_sep(rest) else { continue };
        let f = &rest[..mid];
        let tail = &rest[mid + 4..];
        let Some(end) = tail.rfind("\")") else { continue };
        out.push((unq(f), unq(&tail[..end])));
    }
    out
}

fn find_sep(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = 0;
    while i + 3 < b.len() {
        if b[i] == b'\\' {
            i += 2;
            continue;
        }
        if &b[i..i + 4] == b"\", \"" {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn styles() -> Vec<(Style, Value)> {
    let mut cases: Vec<(Style, Value)> = Vec::new();
    for (code, v) in [("General", 1234.5), ("0.00", 2.675), ("#,##0", 1234567.0), ("#,##0.00", -1234.5), ("0%", 0.256), ("0.00%", 0.256), ("0.00E+00", 12345.0), ("# ?/?", 1.5), ("yyyy-mm-dd", 46340.0), ("d-mmm-yy", 46340.0), ("dd/mm/yyyy hh:mm", 46340.25), ("h:mm AM/PM", 0.65), ("[h]:mm:ss", 1.5), ("\"$\"#,##0.00;[Red]-\"$\"#,##0.00", -12.5), ("0.0,,\"M\"", 12345678.0), ("dddd", 46340.0), ("#,##0_);(#,##0)", -1234.0), ("mmmm yyyy", 46340.0)] {
        cases.push((Style::default().with_num_fmt(code), Value::Num(v)));
    }
    cases.push((Style::default().with_num_fmt("@"), Value::Str("00123".into())));
    cases.push((Style::default(), Value::Str("  two  spaces\tand a tab\nand a line".into())));
    cases.push((Style::default(), Value::Bool(true)));
    let mut s = Style::default();
    s.font.bold = true;
    cases.push((s.clone(), Value::Num(1.0)));
    s.font.italic = true;
    s.font.underline = true;
    cases.push((s.clone(), Value::Num(2.0)));
    let mut s = Style::default();
    s.font.strike = true;
    s.font.color = Some(0xC00000);
    s.font.size = Some(14 * 20);
    s.font.name = Some("DejaVu Serif".into());
    cases.push((s, Value::Num(3.0)));
    for fill in [0xFFFF00, 0xDDEBF7, 0x000000] {
        let mut s = Style::default();
        s.fill = Some(fill);
        cases.push((s, Value::Num(4.0)));
    }
    for line in [Line::Thin, Line::Medium, Line::Thick, Line::Double, Line::Dashed, Line::Dotted] {
        let mut s = Style::default();
        s.borders = [Edge { line, color: Some(0x1F4E79) }, Edge { line, color: None }, Edge::default(), Edge { line, color: Some(0xFF0000) }];
        cases.push((s, Value::Num(5.0)));
    }
    for (h, v) in [(HAlign::Left, VAlign::Top), (HAlign::Center, VAlign::Center), (HAlign::Right, VAlign::Bottom), (HAlign::Justify, VAlign::Bottom)] {
        let mut s = Style::default();
        s.h_align = h;
        s.v_align = v;
        s.wrap = h == HAlign::Justify;
        cases.push((s, Value::Num(6.0)));
    }
    cases
}

fn fixture(cases: &[(String, String)], st: &[(Style, Value)]) -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("S").unwrap();
    wb.add_sheet("Other sheet").unwrap();
    wb.add_sheet("Styles").unwrap();
    let set = |wb: &mut Workbook, a: &str, s: &str| {
        let (r, c) = officina_grid::addr::parse_a1(a).unwrap();
        wb.set_input(CellAddr::new(0, r, c), s).unwrap();
    };
    for (i, v) in ["1", "2", "3", "4", "5"].iter().enumerate() {
        set(&mut wb, &format!("A{}", i + 1), v);
    }
    for (i, v) in ["apple", "Banana", "cherry", "apple pie", "date"].iter().enumerate() {
        set(&mut wb, &format!("B{}", i + 1), v);
    }
    set(&mut wb, "D1", "10");
    set(&mut wb, "D2", "x");
    set(&mut wb, "D4", "20");
    set(&mut wb, "E1", "=1/0");
    wb.set_input(CellAddr::new(1, 0, 0), "7").unwrap();
    // Defined names: a range, a constant, a formula, one local to a sheet.
    wb.define_name("Fruit", None, "=S!$B$1:$B$5").unwrap();
    wb.define_name("Rate", None, "=0.2").unwrap();
    wb.define_name("Doubled", None, "=SUM(S!$A$1:$A$5)*2").unwrap();
    wb.define_name("Local", Some(1), "=S!$A$3*10").unwrap();
    for (k, f) in NAMED.iter().enumerate() {
        wb.set_input(CellAddr::new(if k == 3 { 1 } else { 0 }, 60 + k as u32, 27), f).unwrap();
    }
    for (i, (f, _)) in cases.iter().enumerate() {
        wb.set_input(CellAddr::new(0, 100 + i as u32, COL), f).unwrap();
    }
    // An auto-filter on the second sheet (C10:D16): fruit in a list, count over 3;
    // and rows 30-31 hidden by hand.
    for (r, (f, n)) in [("Fruit", 0.0), ("apple", 3.0), ("Pear", 10.0), ("apple", 7.0), ("fig", 1.0), ("pear", 4.0), ("kiwi", 9.0)].iter().enumerate() {
        wb.set_input(CellAddr::new(1, 9 + r as u32, 2), f).unwrap();
        if r > 0 {
            wb.set_value(CellAddr::new(1, 9 + r as u32, 3), Value::Num(*n));
        } else {
            wb.set_input(CellAddr::new(1, 9, 3), "Count").unwrap();
        }
    }
    wb.sheet_mut(1).hidden = vec![(29, 30)];
    let f = AutoFilter { rect: Rect { sheet: 1, r0: 9, c0: 2, r1: 15, c1: 3 }, conds: vec![(2, Cond::Values(vec!["pear".into(), "apple".into()])), (3, Cond::Compare(vec![(">".into(), 3.0)], true))] };
    let fst = wb.filtered(1, Some(f));
    wb.replace_filter(fst);
    for (i, (s, v)) in st.iter().enumerate() {
        let a = CellAddr::new(2, i as u32, SCOL);
        wb.set_value(a, v.clone());
        wb.set_style(&Rect { sheet: 2, r0: a.row, c0: SCOL, r1: a.row, c1: SCOL }, s.clone());
    }
    // A whole column styled.
    wb.set_style(&Rect { sheet: 2, r0: 0, c0: 3, r1: 1_048_575, c1: 3 }, st[2].0.clone());
    wb.recalc();
    wb
}

fn soffice(dir: &Path, filter: &str, input: &Path, out: &Path) -> Vec<u8> {
    let st = Command::new("soffice").args(["--headless", "--norestore", "--convert-to", filter, "--outdir"]).arg(out).arg(input).env("HOME", dir).output().expect("soffice");
    let ext = filter.split(':').next().unwrap();
    let f = out.join(input.with_extension(ext).file_name().unwrap());
    std::fs::read(&f).unwrap_or_else(|_| {
        eprintln!("LibreOffice did not convert {}:\n{}", input.display(), String::from_utf8_lossy(&st.stderr));
        std::process::exit(1)
    })
}

/// Compares `back` with `wb`; prints the differences, returns the summary.
fn compare(what: &str, wb: &Workbook, back: &Workbook, cases: &[(String, String)], st: &[(Style, Value)]) -> String {
    let mut f_ok = 0;
    let mut src_ok = 0;
    for (i, (f, excel)) in cases.iter().enumerate() {
        let a = CellAddr::new(0, 100 + i as u32, COL);
        let got = back.get(a).display();
        if got == *excel {
            f_ok += 1;
        } else {
            println!("  {what}: {f:<45} shows {got}, Excel {excel}");
        }
        let src = back.formula_text(a).unwrap_or_default();
        if src.eq_ignore_ascii_case(f) {
            src_ok += 1;
        } else {
            println!("  {what}: formula {f} came back as {src}");
        }
    }
    let mut s_ok = 0;
    for (i, (want, _)) in st.iter().enumerate() {
        let a = CellAddr::new(2, i as u32, SCOL);
        let (w, g) = (wb.formatted(a).text, back.formatted(a).text);
        let mut got = back.style(a).clone();
        got.num_fmt = want.num_fmt.clone();
        let same_v = wb.get(a) == back.get(a);
        if w == g && got == *want && same_v {
            s_ok += 1;
        } else {
            println!("  {what}: styled row {i}: shows {w:?} → {g:?} (code {:?} → {:?}); value {:?} → {:?}", want.num_fmt, back.style(a).num_fmt, wb.get(a), back.get(a));
            if got != *want {
                println!("      wrote {want:?}\n      read  {got:?}");
            }
        }
    }
    let col_ok = back.style(CellAddr::new(2, 500_000, 3)).num_fmt == st[2].0.num_fmt;
    let mut kept = true;
    for r in 0..5 {
        for c in [0u32, 1] {
            let a = CellAddr::new(0, r, c);
            kept &= back.get(a) == wb.get(a);
        }
    }
    kept &= back.get(CellAddr::new(1, 0, 0)) == wb.get(CellAddr::new(1, 0, 0));
    let rows_ok = back.sheet(1).hidden == wb.sheet(1).hidden;
    if !rows_ok {
        println!("  {what}: hidden rows {:?} → {:?}", wb.sheet(1).hidden, back.sheet(1).hidden);
    }
    let filter_ok = back.sheet(1).filter == wb.sheet(1).filter;
    if !filter_ok {
        println!("  {what}: filter {:?}\n      → {:?}", wb.sheet(1).filter, back.sheet(1).filter);
    }
    let mut n_ok = 0;
    for (k, f) in NAMED.iter().enumerate() {
        let a = CellAddr::new(if k == 3 { 1 } else { 0 }, 60 + k as u32, 27);
        if back.get(a) == wb.get(a) && back.formula_text(a).as_deref() == Some(f) {
            n_ok += 1;
        } else {
            println!("  {what}: {f} shows {:?} (want {:?}), formula {:?}", back.get(a), wb.get(a), back.formula_text(a));
        }
    }
    format!("{what}: {} formulas, {f_ok} show Excel's result, {src_ok} same source; {} styled cells, {s_ok} same; whole-column style {col_ok}; fixture values {kept}; {n_ok}/{} name formulas, {} names; hidden rows kept {rows_ok}, filter kept {filter_ok}", cases.len(), st.len(), NAMED.len(), back.names().len())
}

fn main() {
    let cases = cases();
    assert!(cases.len() >= 150, "read {} cases", cases.len());
    let st = styles();
    let wb = fixture(&cases, &st);
    let dir = std::env::temp_dir().join(format!("ods-lo-{}", std::process::id()));
    let (a, b) = (dir.join("ours"), dir.join("lo"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let mut summary = Vec::new();

    // In.
    let x = a.join("in.xlsx");
    std::fs::write(&x, officina_convert::xlsx::write(&wb, Vec::new()).unwrap()).unwrap();
    let ods = soffice(&dir, "ods", &x, &b);
    let (back, loaded) = officina_convert::ods::read(&ods).unwrap_or_else(|e| panic!("our reader on LibreOffice's .ods: {e}"));
    println!("in: report {:?}; reader differential {} of {} formulas", loaded.report.human(), loaded.differ, loaded.formulas);
    summary.push(compare("in", &wb, &back, &cases, &st));

    // Out.
    {
        let o = a.join("out.ods");
        std::fs::write(&o, officina_convert::ods::write(&wb, Vec::new()).unwrap()).unwrap();
        let xl = soffice(&dir, "xlsx:Calc MS Excel 2007 XML", &o, &b);
        let (back, loaded) = officina_convert::xlsx::read(&xl).unwrap();
        println!("out: report {:?}", loaded.report.human());
        summary.push(compare("out", &wb, &back, &cases, &st));
    }
    for s in summary {
        println!("summary {s}");
    }
    if std::env::var_os("KEEP").is_none() {
        let _ = std::fs::remove_dir_all(&dir);
    } else {
        println!("kept {}", dir.display());
    }
}
