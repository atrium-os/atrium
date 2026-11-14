//! styles_lo: our `.xlsx` styles through LibreOffice and back.
//!
//!   cargo run --release --example styles_lo
//!
//! A sheet of cells, each with its own style (number formats, fonts, fills, borders,
//! alignment), goes out with our writer; LibreOffice opens it and saves it as `.xlsx`
//! again (its own styles part: its own fonts, fills, theme and format codes); our
//! reader opens that. For every cell, what it shows and its look must be what we
//! wrote. Differences are printed with a summary line.

use std::process::Command;

use officina_grid::addr::{CellAddr, Rect};
use officina_grid::style::{Edge, HAlign, Line, Style, VAlign};
use officina_grid::{Value, Workbook};

fn main() {
    let mut cases: Vec<(Style, f64)> = Vec::new();
    for (code, v) in [("General", 1234.5), ("0.00", 2.675), ("#,##0", 1234567.0), ("0%", 0.256), ("0.00E+00", 12345.0), ("# ?/?", 1.5), ("yyyy-mm-dd", 46340.0), ("d-mmm-yy", 46340.0), ("h:mm AM/PM", 0.65), ("[h]:mm:ss", 1.5), ("\"$\"#,##0.00;[Red]-\"$\"#,##0.00", -12.5), ("0.0,,\"M\"", 12345678.0), ("@", 5.0), ("dddd", 46340.0), ("#,##0_);(#,##0)", -1234.0)] {
        cases.push((Style::default().with_num_fmt(code), v));
    }
    let mut s = Style::default();
    s.font.bold = true;
    cases.push((s.clone(), 1.0));
    s.font.italic = true;
    s.font.underline = true;
    cases.push((s.clone(), 2.0));
    let mut s = Style::default();
    s.font.strike = true;
    s.font.color = Some(0xC00000);
    s.font.size = Some(14 * 20);
    s.font.name = Some("DejaVu Serif".into());
    cases.push((s, 3.0));
    for fill in [0xFFFF00, 0xDDEBF7, 0x000000] {
        let mut s = Style::default();
        s.fill = Some(fill);
        cases.push((s, 4.0));
    }
    for line in [Line::Thin, Line::Medium, Line::Thick, Line::Double, Line::Dashed, Line::Dotted, Line::Hair] {
        let mut s = Style::default();
        s.borders = [Edge { line, color: Some(0x1F4E79) }, Edge { line, color: None }, Edge::default(), Edge { line, color: Some(0xFF0000) }];
        cases.push((s, 5.0));
    }
    for (h, v) in [(HAlign::Left, VAlign::Top), (HAlign::Center, VAlign::Center), (HAlign::Right, VAlign::Bottom), (HAlign::Justify, VAlign::Bottom)] {
        let mut s = Style::default();
        s.h_align = h;
        s.v_align = v;
        s.wrap = h == HAlign::Justify;
        s.indent = if h == HAlign::Left { 2 } else { 0 };
        cases.push((s, 6.0));
    }
    let mut wb = Workbook::new();
    wb.add_sheet("S").unwrap();
    for (i, (st, v)) in cases.iter().enumerate() {
        let a = CellAddr::new(0, i as u32, 1);
        wb.set_value(a, Value::Num(*v));
        wb.set_style(&Rect { sheet: 0, r0: a.row, c0: 1, r1: a.row, c1: 1 }, st.clone());
    }
    // A whole column styled.
    wb.set_style(&Rect { sheet: 0, r0: 0, c0: 3, r1: 1_048_575, c1: 3 }, cases[2].0.clone());
    let dir = std::env::temp_dir().join(format!("styles-lo-{}", std::process::id()));
    let (a, b) = (dir.join("ours"), dir.join("lo"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let ours = a.join("s.xlsx");
    std::fs::write(&ours, officina_convert::xlsx::write(&wb, Vec::new()).unwrap()).unwrap();
    let st = Command::new("soffice").args(["--headless", "--norestore", "--convert-to", "xlsx:Calc MS Excel 2007 XML", "--outdir"]).arg(&b).arg(&ours).env("HOME", &dir).output().expect("soffice");
    let lo = b.join("s.xlsx");
    if !lo.exists() {
        eprintln!("LibreOffice did not convert:\n{}", String::from_utf8_lossy(&st.stderr));
        std::process::exit(1);
    }
    let (back, loaded) = officina_convert::xlsx::read(&std::fs::read(&lo).unwrap()).unwrap();
    let mut same = 0;
    for (i, (want, _)) in cases.iter().enumerate() {
        let a = CellAddr::new(0, i as u32, 1);
        let (w, g) = (wb.formatted(a).text, back.formatted(a).text);
        let mut got = back.style(a).clone();
        // The code may be spelled differently; what it shows is the test.
        got.num_fmt = want.num_fmt.clone();
        if w == g && got == *want {
            same += 1;
        } else {
            println!("row {i}: shows {w:?} → {g:?} (code {:?} → {:?})", want.num_fmt, back.style(a).num_fmt);
            if got != *want {
                println!("    wrote {want:?}\n    read  {got:?}");
            }
        }
    }
    let col_ok = back.style(CellAddr::new(0, 500_000, 3)).num_fmt == cases[2].0.num_fmt;
    println!("report: {:?}", loaded.report.human());
    println!("summary: {} styled cells; same after LibreOffice: {same}; whole-column style kept: {col_ok}", cases.len());
    let _ = std::fs::remove_dir_all(&dir);
}
