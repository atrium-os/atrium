//! What-If scenarios survive .xlsx, and a malformed list is dropped with a note.

use officina_convert::xlsx;
use officina_grid::scenario::Scenario;
use officina_grid::Workbook;

#[test]
fn scenarios_round_trip_in_xlsx() {
    let mut wb = Workbook::new();
    wb.add_sheet("S").unwrap();
    let list = vec![
        Scenario { name: "Best & <worst>".into(), comment: "by \"me\"".into(), cells: vec![(0, 1, "12".into()), (1, 1, "a<b".into())] },
        Scenario { name: "Now".into(), comment: String::new(), cells: vec![(0, 1, "10".into())] },
    ];
    assert!(wb.set_scenarios(0, list.clone()));
    let file = xlsx::write(&wb, Vec::new()).unwrap();
    let zip = officina_convert::zip::Archive::open(&file).unwrap();
    let part = zip.entries.iter().find(|e| e.name == "xl/worksheets/sheet1.xml").unwrap();
    let text = String::from_utf8(zip.read(part).unwrap()).unwrap();
    assert!(text.contains(r#"<scenario name="Now" count="1"><inputCells r="B1" val="10"/></scenario>"#), "{text}");
    let (back, _) = xlsx::read(&file).unwrap();
    assert_eq!(back.sheet(0).scenarios(), &list[..]);

    // The same name twice: the list is refused and the report says so.
    let twice = text.replace(r#"name="Now""#, r#"name="NOW""#).replace("Best &amp; &lt;worst&gt;", "now");
    let mut parts: Vec<(String, Vec<u8>)> = zip.entries.iter().map(|e| (e.name.clone(), zip.read(e).unwrap())).collect();
    parts.iter_mut().find(|p| p.0 == "xl/worksheets/sheet1.xml").unwrap().1 = twice.into_bytes();
    let mut w = officina_convert::zip::ZipWriter::new(Vec::new());
    for (name, data) in &parts {
        w.add(name, data).unwrap();
    }
    let rebuilt = w.finish().unwrap();
    let (back, rep) = xlsx::read(&rebuilt).unwrap();
    assert!(back.sheet(0).scenarios().is_empty());
    assert!(format!("{rep:?}").contains("scenarios with no name, a name twice"), "{rep:?}");
}
