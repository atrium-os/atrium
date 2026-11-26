//! A sheet protected here (a password verifier and allowances) and a
//! sheet's hidden zero values survive .xlsx and .ods, and the password
//! still unprotects the sheet read back.

use officina_convert::{ods, protect, xlsx};
use officina_grid::protection::{ProtectionFormat, SheetProtection};
use officina_grid::Workbook;

fn book() -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("S").unwrap();
    wb.add_sheet("T").unwrap();
    let mut p = SheetProtection::new(ProtectionFormat::Excel);
    for (k, v) in protect::verifier("pass word", &[9; 16]) {
        assert!(p.keep(k, &v));
    }
    assert!(p.keep("formatCells", "0"));
    wb.sheet_mut(0).protection = Some(p);
    wb.sheet_mut(1).show_zeros = false;
    wb
}

#[test]
fn protection_and_zero_values_round_trip_in_xlsx() {
    let file = xlsx::write(&book(), Vec::new()).unwrap();
    let zip = officina_convert::zip::Archive::open(&file).unwrap();
    let sheet = |n: &str| String::from_utf8(zip.read(zip.entries.iter().find(|e| e.name == n).unwrap()).unwrap()).unwrap();
    let one = sheet("xl/worksheets/sheet1.xml");
    assert!(one.contains(r#"algorithmName="SHA-512""#) && one.contains(r#"formatCells="0""#) && one.contains(r#"spinCount="100000""#), "{one}");
    assert!(sheet("xl/worksheets/sheet2.xml").contains(r#"showZeros="0""#));
    let (back, rep) = xlsx::read(&file).unwrap();
    let p = back.sheet(0).protection.clone().expect("still protected");
    assert_eq!(protect::check(&p, "pass word"), Ok(true), "{rep:?}");
    assert_eq!(protect::check(&p, "password"), Ok(false));
    assert!(p.allows("formatCells") && !p.allows("formatRows"));
    assert!(back.sheet(0).show_zeros && !back.sheet(1).show_zeros);
}

#[test]
fn zero_values_round_trip_in_ods() {
    let file = ods::write(&book(), Vec::new()).unwrap();
    let (back, _) = ods::read(&file).unwrap();
    assert!(back.sheet(0).show_zeros && !back.sheet(1).show_zeros);
}

#[test]
fn workbook_and_range_passwords_round_trip_in_xlsx() {
    use officina_grid::sheetareas::EditRange;
    let mut wb = book();
    let mut p = SheetProtection::new(ProtectionFormat::ExcelWorkbook);
    for (k, v) in protect::workbook_verifier("books", &[3; 16]) {
        assert!(p.keep(k, &v));
    }
    wb.set_structure_protection(Some(p));
    let er = EditRange { title: "Inputs".into(), areas: vec![(1, 1, 4, 1)], password: Some(protect::range_verifier("in", &[5; 16])) };
    assert!(wb.set_edit_ranges(0, vec![er]));
    let file = xlsx::write(&wb, Vec::new()).unwrap();
    let (back, rep) = xlsx::read(&file).unwrap();
    let s = back.structure_protection().cloned().expect("structure still protected");
    assert_eq!(protect::check(&s, "books"), Ok(true), "{rep:?}");
    assert_eq!(protect::check(&s, "Books"), Ok(false));
    let v = back.sheet(0).edit_ranges()[0].password.clone().expect("range keeps its password");
    assert_eq!(protect::check_range(&v, "in"), Ok(true));
    assert_eq!(protect::check_range(&v, "out"), Ok(false));
}
