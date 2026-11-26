//! Allow Edit Ranges (`<protectedRanges>`) and ignored errors
//! (`<ignoredErrors>`) survive .xlsx, an Excel password verifier kept as is.

use officina_convert::xlsx;
use officina_grid::sheetareas::{EditRange, Ignored, INCONSISTENT, NUMBER_AS_TEXT};
use officina_grid::Workbook;

#[test]
fn edit_ranges_and_ignored_errors_round_trip_in_xlsx() {
    let mut wb = Workbook::new();
    wb.add_sheet("S").unwrap();
    let ranges = vec![
        EditRange { title: "Inputs & notes".into(), areas: vec![(1, 1, 8, 1), (0, 3, 0, 3)], password: None },
        EditRange { title: "Locked".into(), areas: vec![(9, 0, 9, 4)], password: Some("algorithmName=SHA-512\nhashValue=abc=\nsaltValue=xyz==\nspinCount=100000".into()) },
    ];
    assert!(wb.set_edit_ranges(0, ranges.clone()));
    let ignored = vec![Ignored { r0: 0, c0: 0, r1: 4, c1: 0, rules: NUMBER_AS_TEXT }, Ignored { r0: 6, c0: 2, r1: 6, c1: 2, rules: INCONSISTENT | 1 }];
    assert!(wb.set_ignored_errors(0, ignored.clone()));
    let file = xlsx::write(&wb, Vec::new()).unwrap();
    let zip = officina_convert::zip::Archive::open(&file).unwrap();
    let e = zip.entries.iter().find(|e| e.name == "xl/worksheets/sheet1.xml").unwrap();
    let text = String::from_utf8(zip.read(e).unwrap()).unwrap();
    assert!(text.contains(r#"<protectedRange sqref="B2:B9 D1" name="Inputs &amp; notes"/>"#), "{text}");
    assert!(text.contains(r#"<protectedRange sqref="A10:E10" name="Locked" algorithmName="SHA-512" hashValue="abc=" saltValue="xyz==" spinCount="100000"/>"#), "{text}");
    assert!(text.contains(r#"<ignoredError sqref="C7" evalError="1" formula="1"/>"#), "{text}");
    let (back, rep) = xlsx::read(&file).unwrap();
    assert_eq!(back.sheet(0).edit_ranges(), ranges.as_slice(), "{rep:?}");
    assert_eq!(back.sheet(0).ignored_errors(), ignored.as_slice());
}
