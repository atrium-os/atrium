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
