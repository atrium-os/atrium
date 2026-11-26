//! Protect Workbook (the structure lock) survives .xlsx and .ods, with an
//! Excel verifier kept for Excel.

use officina_convert::{ods, xlsx};
use officina_grid::protection::{ProtectionFormat, SheetProtection};
use officina_grid::Workbook;

#[test]
fn structure_protection_round_trips() {
    let mut wb = Workbook::new();
    wb.add_sheet("S").unwrap();
    let mut p = SheetProtection::new(ProtectionFormat::ExcelWorkbook);
    assert!(p.keep("workbookHashValue", "q0/Y+Hk="));
    assert!(p.keep("workbookAlgorithmName", "SHA-512"));
    wb.set_structure_protection(Some(p.clone()));
    let file = xlsx::write(&wb, Vec::new()).unwrap();
    let zip = officina_convert::zip::Archive::open(&file).unwrap();
    let text = String::from_utf8(zip.read_name("xl/workbook.xml").unwrap().unwrap()).unwrap();
    assert!(text.contains(r#"<workbookProtection lockStructure="1" workbookAlgorithmName="SHA-512" workbookHashValue="q0/Y+Hk="/><sheets>"#), "{text}");
    let (back, _) = xlsx::read(&file).unwrap();
    assert_eq!(back.structure_protection(), Some(&p));
    assert!(back.structure_protection().unwrap().has_password());

    // .ods: the lock goes; Excel's verifier doesn't belong there.
    let (back, _) = ods::read(&ods::write(&wb, Vec::new()).unwrap()).unwrap();
    assert!(back.structure_protected());
    assert!(!back.structure_protection().unwrap().has_password());

    let mut open = Workbook::new();
    open.add_sheet("S").unwrap();
    let (back, _) = xlsx::read(&xlsx::write(&open, Vec::new()).unwrap()).unwrap();
    assert!(!back.structure_protected());
    let (back, _) = ods::read(&ods::write(&open, Vec::new()).unwrap()).unwrap();
    assert!(!back.structure_protected());
}
