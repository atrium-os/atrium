//! Iterative calculation settings survive .xlsx and .ods.

use officina_convert::{ods, xlsx};
use officina_grid::book::Iteration;
use officina_grid::{CellAddr, Value, Workbook};

fn book(it: Option<Iteration>) -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("S").unwrap();
    wb.set_formula(CellAddr::new(0, 0, 0), "=B1/2+1").unwrap();
    wb.set_formula(CellAddr::new(0, 0, 1), "=A1").unwrap();
    assert!(wb.set_iteration(it));
    wb.recalc();
    wb
}

#[test]
fn iteration_settings_round_trip_in_xlsx_and_ods() {
    let it = Some(Iteration { count: 250, change: 0.0005 });
    let wb = book(it);
    let file = xlsx::write(&wb, Vec::new()).unwrap();
    let xml = officina_convert::zip::Archive::open(&file).unwrap();
    let part = xml.entries.iter().find(|e| e.name == "xl/workbook.xml").unwrap();
    let text = String::from_utf8(xml.read(part).unwrap()).unwrap();
    assert!(text.contains(r#"<calcPr iterate="1" iterateCount="250" iterateDelta="0.0005"/>"#), "{text}");
    let (mut back, _) = xlsx::read(&file).unwrap();
    assert_eq!(back.iteration(), it);
    back.recalc();
    assert!(matches!(back.get(CellAddr::new(0, 0, 0)), Value::Num(x) if (x - 2.0).abs() < 0.01));

    let file = ods::write(&wb, Vec::new()).unwrap();
    let (back, _) = ods::read(&file).unwrap();
    assert_eq!(back.iteration(), it);

    let off = book(None);
    let (back, _) = xlsx::read(&xlsx::write(&off, Vec::new()).unwrap()).unwrap();
    assert_eq!(back.iteration(), None);
    let (back, _) = ods::read(&ods::write(&off, Vec::new()).unwrap()).unwrap();
    assert_eq!(back.iteration(), None);
}
