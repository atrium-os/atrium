//! Sheets' tab colours through .xlsx and .ods and back.

use officina_grid::Workbook;

fn book() -> Workbook {
    let mut wb = Workbook::new();
    for n in ["Plain", "Green", "Red"] {
        wb.add_sheet(n).unwrap();
    }
    wb.sheet_mut(1).tab_color = Some(0x2E7D32);
    wb.sheet_mut(2).tab_color = Some(0xC62828);
    wb
}

fn colors(wb: &Workbook) -> Vec<Option<u32>> {
    (0..wb.sheet_count() as u16).map(|s| wb.sheet(s).tab_color).collect()
}

#[test]
fn tab_colours_round_trip_through_xlsx_and_ods() {
    let wb = book();
    let x = officina_convert::xlsx::write(&wb, Vec::new()).unwrap();
    let (back, _) = officina_convert::xlsx::read(&x).unwrap();
    assert_eq!(colors(&back), [None, Some(0x2E7D32), Some(0xC62828)]);
    let o = officina_convert::ods::write(&wb, Vec::new()).unwrap();
    let (back, _) = officina_convert::ods::read(&o).unwrap();
    assert_eq!(colors(&back), [None, Some(0x2E7D32), Some(0xC62828)]);
}

#[test]
fn a_theme_tab_colour_is_read() {
    // Excel writes theme colours with a tint as often as rgb.
    let wb = book();
    let x = officina_convert::xlsx::write(&wb, Vec::new()).unwrap();
    let z = officina_convert::zip::Archive::open(&x).unwrap();
    let mut w = officina_convert::zip::ZipWriter::new(Vec::new());
    for e in &z.entries {
        let mut d = z.read(e).unwrap();
        if e.name == "xl/worksheets/sheet2.xml" {
            let t = String::from_utf8(d).unwrap().replace(r#"<tabColor rgb="FF2E7D32"/>"#, r#"<tabColor theme="4" tint="0"/>"#);
            d = t.into_bytes();
        }
        w.add(&e.name, &d).unwrap();
    }
    let (back, _) = officina_convert::xlsx::read(&w.finish().unwrap()).unwrap();
    // No theme part: Office's default theme, accent 1.
    let t = back.sheet(1).tab_color;
    assert!(t.is_some() && t != Some(0x2E7D32), "{t:x?}");
    assert_eq!(back.sheet(2).tab_color, Some(0xC62828));
}

#[test]
fn hidden_sheets_stay_hidden_through_xlsx_and_ods() {
    let mut wb = book();
    wb.sheet_mut(1).sheet_hidden = true;
    let hidden = |wb: &Workbook| (0..wb.sheet_count() as u16).map(|s| wb.sheet(s).sheet_hidden).collect::<Vec<_>>();
    let x = officina_convert::xlsx::write(&wb, Vec::new()).unwrap();
    let (back, _) = officina_convert::xlsx::read(&x).unwrap();
    assert_eq!(hidden(&back), [false, true, false]);
    assert_eq!(colors(&back), [None, Some(0x2E7D32), Some(0xC62828)]);
    let o = officina_convert::ods::write(&wb, Vec::new()).unwrap();
    let (back, _) = officina_convert::ods::read(&o).unwrap();
    assert_eq!(hidden(&back), [false, true, false]);
    assert_eq!(colors(&back), [None, Some(0x2E7D32), Some(0xC62828)]);
    // Excel's very hidden reads as hidden.
    let z = officina_convert::zip::Archive::open(&x).unwrap();
    let mut w = officina_convert::zip::ZipWriter::new(Vec::new());
    for e in &z.entries {
        let mut d = z.read(e).unwrap();
        if e.name == "xl/workbook.xml" {
            let t = String::from_utf8(d).unwrap();
            assert!(t.contains(r#"state="hidden""#), "{t}");
            d = t.replace(r#"state="hidden""#, r#"state="veryHidden""#).into_bytes();
        }
        w.add(&e.name, &d).unwrap();
    }
    let (back, _) = officina_convert::xlsx::read(&w.finish().unwrap()).unwrap();
    assert_eq!(hidden(&back), [false, true, false]);
}
