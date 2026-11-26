//! `write PATH.xlsx|PATH.ods`: a workbook with superscript, subscript, double underline and
//! the Fill, Justify and Distributed alignments, an accounting underline and
//! document properties, and each border line with diagonals (column B),
//! for LibreOffice to convert.
//! `read PATH.ods|PATH.xlsx`: prints what each of its first four cells has
//! and the document properties,
//! to compare with what was written.
use officina_grid::addr::{CellAddr, Rect};
use officina_grid::style::{HAlign, Line, Script, Style};

/// Each border line, under one cell of column B each.
const LINES: [Line; 13] = [Line::Thin, Line::Medium, Line::Thick, Line::Double, Line::Dashed, Line::Dotted, Line::Hair, Line::MediumDashed, Line::DashDot, Line::MediumDashDot, Line::DashDotDot, Line::MediumDashDotDot, Line::SlantDashDot];
use officina_grid::{Value, Workbook};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (mode, path) = (args.get(1).expect("write or read"), args.get(2).expect("path"));
    if mode == "read" {
        let bytes = std::fs::read(path).unwrap();
        let wb = if path.ends_with(".ods") { officina_convert::ods::read(&bytes).unwrap().0 } else { officina_convert::xlsx::read(&bytes).unwrap().0 };
        for r in 0..4 {
            let st = wb.style(CellAddr::new(0, r, 0));
            println!("{r}: {:?} underline={} double={} accounting={} {:?}", st.font.script, st.font.underline, st.font.double, st.font.accounting, st.h_align);
        }
        let p = wb.properties();
        println!("properties: {:?}", (0..6).map(|i| p.get(i)).collect::<Vec<_>>());
        for r in 0..LINES.len() as u32 {
            let st = wb.style(CellAddr::new(0, r, 1));
            println!("border {r}: {:?} {:?} diagonals {:?}", st.borders[3].line, st.borders[3].color, st.diagonals.map(|e| e.line));
        }
        return;
    }
    let mut wb = Workbook::new();
    wb.add_sheet("S").unwrap();
    for (i, (script, double, align)) in [(Script::Super, false, HAlign::Fill), (Script::Sub, true, HAlign::Justify), (Script::Normal, true, HAlign::Distributed)].into_iter().enumerate() {
        let mut st = Style::default();
        st.font.script = script;
        st.font.underline = double;
        st.font.double = double;
        st.h_align = align;
        let r = i as u32;
        wb.set_value(CellAddr::new(0, r, 0), Value::str("x2"));
        wb.set_style(&Rect { sheet: 0, r0: r, c0: 0, r1: r, c1: 0 }, st);
    }
    let mut acc = Style::default();
    acc.font.underline = true;
    acc.font.accounting = true;
    wb.set_value(CellAddr::new(0, 3, 0), Value::Num(5.0));
    wb.set_style(&Rect { sheet: 0, r0: 3, c0: 0, r1: 3, c1: 0 }, acc);
    let mut p = officina_grid::docprops::DocProps::default();
    for (i, v) in ["Budget 2027", "Spending", "Ana Rao", "budget, plan", "Finance", "First draft"].into_iter().enumerate() {
        p.set(i, v);
    }
    wb.set_properties(p);
    for (i, line) in LINES.into_iter().enumerate() {
        let mut st = Style::default();
        st.borders[3] = officina_grid::style::Edge { line, color: Some(0x1F4E79) };
        st.diagonals[i % 2] = officina_grid::style::Edge { line: Line::Thin, color: Some(0xC62828) };
        wb.set_value(CellAddr::new(0, i as u32, 1), Value::Num(i as f64));
        wb.set_style(&Rect { sheet: 0, r0: i as u32, c0: 1, r1: i as u32, c1: 1 }, st);
    }
    let bytes = if path.ends_with(".ods") { officina_convert::ods::write(&wb, Vec::new()).unwrap() } else { officina_convert::xlsx::write(&wb, Vec::new()).unwrap() };
    std::fs::write(path, bytes).unwrap();
}
