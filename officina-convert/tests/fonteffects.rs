//! Superscript, subscript, double underline and the Fill, Justify and
//! Distributed alignments survive `.xlsx` and `.ods`.

use officina_convert::{ods, xlsx};
use officina_grid::addr::{CellAddr, Rect};
use officina_grid::style::{HAlign, Script, Style};
use officina_grid::{Value, Workbook};

fn book() -> (Workbook, Vec<Style>) {
    let mut wb = Workbook::new();
    wb.add_sheet("S").unwrap();
    let mut styles = Vec::new();
    for (i, (script, double, align)) in [(Script::Super, false, HAlign::Fill), (Script::Sub, true, HAlign::Justify), (Script::Normal, true, HAlign::Distributed)].into_iter().enumerate() {
        let mut st = Style::default();
        st.font.script = script;
        st.font.underline = double;
        st.font.double = double;
        st.h_align = align;
        let r = i as u32;
        wb.set_value(CellAddr::new(0, r, 0), Value::str("x2"));
        wb.set_style(&Rect { sheet: 0, r0: r, c0: 0, r1: r, c1: 0 }, st.clone());
        styles.push(st);
    }
    (wb, styles)
}

fn check(back: &Workbook, styles: &[Style], what: &str) {
    for (i, want) in styles.iter().enumerate() {
        let got = back.style(CellAddr::new(0, i as u32, 0));
        assert_eq!((got.font.script, got.font.underline, got.font.double, got.h_align), (want.font.script, want.font.underline, want.font.double, want.h_align), "{what} row {i}");
    }
}

#[test]
fn font_effects_round_trip_in_xlsx() {
    let (wb, styles) = book();
    let file = xlsx::write(&wb, Vec::new()).unwrap();
    let zip = officina_convert::zip::Archive::open(&file).unwrap();
    let xml = String::from_utf8(zip.read(zip.entries.iter().find(|e| e.name == "xl/styles.xml").unwrap()).unwrap()).unwrap();
    assert!(xml.contains(r#"<vertAlign val="superscript"/>"#) && xml.contains(r#"<u val="double"/>"#), "{xml}");
    let (back, _) = xlsx::read(&file).unwrap();
    check(&back, &styles, "xlsx");
}

#[test]
fn font_effects_round_trip_in_ods() {
    let (wb, styles) = book();
    let file = ods::write(&wb, Vec::new()).unwrap();
    let (back, _) = ods::read(&file).unwrap();
    check(&back, &styles, "ods");
}
