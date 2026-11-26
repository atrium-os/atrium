//! Manual calculation (`calcPr calcMode`) and the Watch Window's cells
//! (`<cellWatches>`) survive .xlsx, together with iterative calculation.

use officina_convert::xlsx;
use officina_grid::book::Iteration;
use officina_grid::Workbook;

fn part(file: &[u8], name: &str) -> String {
    let zip = officina_convert::zip::Archive::open(file).unwrap();
    let e = zip.entries.iter().find(|e| e.name == name).unwrap();
    String::from_utf8(zip.read(e).unwrap()).unwrap()
}

#[test]
fn manual_calculation_and_watches_round_trip_in_xlsx() {
    let mut wb = Workbook::new();
    wb.add_sheet("One").unwrap();
    wb.add_sheet("Two").unwrap();
    wb.set_manual_calc(true);
    assert!(wb.set_iteration(Some(Iteration { count: 50, change: 0.01 })));
    assert!(wb.set_watches(1, vec![(2, 1), (0, 0)]));
    let file = xlsx::write(&wb, Vec::new()).unwrap();
    let book = part(&file, "xl/workbook.xml");
    assert!(book.contains(r#"<calcPr calcMode="manual" iterate="1" iterateCount="50" iterateDelta="0.01"/>"#), "{book}");
    let sheet = part(&file, "xl/worksheets/sheet2.xml");
    assert!(sheet.contains(r#"<cellWatches><cellWatch r="B3"/><cellWatch r="A1"/></cellWatches>"#), "{sheet}");
    let (back, rep) = xlsx::read(&file).unwrap();
    assert!(back.manual_calc(), "{rep:?}");
    assert_eq!(back.iteration(), Some(Iteration { count: 50, change: 0.01 }));
    assert_eq!(back.sheet(1).watches(), [(2, 1), (0, 0)]);
    assert!(back.sheet(0).watches().is_empty());

    // Automatic is Excel's default: no calcMode written, none read.
    wb.set_manual_calc(false);
    assert!(wb.set_iteration(None));
    let file = xlsx::write(&wb, Vec::new()).unwrap();
    assert!(!part(&file, "xl/workbook.xml").contains("calcPr"));
    assert!(!xlsx::read(&file).unwrap().0.manual_calc());
}
