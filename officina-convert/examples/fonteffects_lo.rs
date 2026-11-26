//! `write PATH.xlsx|PATH.ods`: a workbook with superscript, subscript, double underline and
//! the Fill, Justify and Distributed alignments, an accounting underline and
//! document properties, for LibreOffice to convert.
//! `read PATH.ods|PATH.xlsx`: prints what each of its first four cells has
//! and the document properties,
//! to compare with what was written.
use officina_grid::addr::{CellAddr, Rect};
use officina_grid::style::{HAlign, Script, Style};
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
    let bytes = if path.ends_with(".ods") { officina_convert::ods::write(&wb, Vec::new()).unwrap() } else { officina_convert::xlsx::write(&wb, Vec::new()).unwrap() };
    std::fs::write(path, bytes).unwrap();
}
