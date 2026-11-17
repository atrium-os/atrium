//! Pivot tables through .xlsx and .ods (docs/spec/officina.md §16.32): written as
//! Excel and LibreOffice write them, read back the same, with the same cells.

use officina_convert::zip::Archive;
use officina_convert::{ods, xlsx};
use officina_grid::pivot::{Agg, DataField, Pivot, PivotField};
use officina_grid::{CellAddr, Rect, Value, Workbook};

pub fn book() -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("Data").unwrap();
    wb.add_sheet("Report").unwrap();
    let rows = [
        ["Region", "Rep", "Quarter", "Units", "Price"],
        ["North", "Ana", "Q1", "10", "2.5"],
        ["North", "Ana", "Q2", "20", "3"],
        ["North", "Bo", "Q1", "5", "4"],
        ["South", "Cy", "Q1", "7", "1"],
        ["South", "Cy", "Q2", "3", "2"],
        ["East", "Di", "Q2", "8", "5"],
        ["East", "Ed", "Q1", "", "1.5"],
    ];
    for (r, row) in rows.iter().enumerate() {
        for (c, v) in row.iter().enumerate() {
            if !v.is_empty() {
                wb.set_input(CellAddr::new(0, r as u32, c as u32), v).unwrap();
            }
        }
    }
    wb.recalc();
    let src = Rect { sheet: 0, r0: 0, c0: 0, r1: 7, c1: 4 };
    let mut p = Pivot::new("Units by region", src, CellAddr::new(1, 2, 1));
    p.rows = vec![PivotField::new(0), PivotField { field: 1, hidden: vec![Value::str("Bo")] }];
    p.cols = vec![PivotField::new(2)];
    p.data = vec![DataField { field: 3, agg: Agg::Sum, name: None }];
    wb.add_pivot(p).unwrap();
    let mut q = Pivot::new("Prices", src, CellAddr::new(0, 0, 7));
    q.rows = vec![PivotField::new(1)];
    q.pages = vec![PivotField { field: 0, hidden: vec![Value::str("South")] }];
    q.data = vec![DataField { field: 4, agg: Agg::Average, name: Some("Mean price".into()) }, DataField { field: 3, agg: Agg::Count, name: None }];
    q.subtotals = false;
    q.grand_col = false;
    wb.add_pivot(q).unwrap();
    wb.recalc();
    wb
}

fn part(file: &[u8], name: &str) -> String {
    let z = Archive::open(file).unwrap();
    let e = z.entries.iter().find(|e| e.name == name).unwrap_or_else(|| panic!("no {name}"));
    String::from_utf8(z.read(e).unwrap()).unwrap()
}

fn cells(wb: &Workbook) -> Vec<(u16, u32, u32, Value)> {
    let mut v = Vec::new();
    for s in 0..wb.sheet_count() as u16 {
        for r in 0..30 {
            for c in 0..12 {
                let x = wb.get(CellAddr::new(s, r, c));
                if !x.is_empty() {
                    v.push((s, r, c, x));
                }
            }
        }
    }
    v
}

#[test]
fn xlsx_round_trip() {
    let wb = book();
    let file = xlsx::write(&wb, Vec::new()).unwrap();
    let t = part(&file, "xl/pivotTables/pivotTable1.xml");
    assert!(t.contains(r#"<location ref="B3:F12" firstHeaderRow="1" firstDataRow="2" firstDataCol="2"/>"#), "{t}");
    assert!(t.contains(r#"<pivotField axis="axisRow" compact="0" outline="0" showAll="0"><items count="6"><item x="0"/><item x="1" h="1"/>"#), "{t}");
    assert!(t.contains(r#"<rowFields count="2"><field x="0"/><field x="1"/></rowFields><colFields count="1"><field x="2"/></colFields><dataFields count="1"><dataField name="Sum of Units" fld="3" baseField="0" baseItem="0"/>"#), "{t}");
    let c = part(&file, "xl/pivotCache/pivotCacheDefinition1.xml");
    assert!(c.contains(r#"<worksheetSource ref="A1:E8" sheet="Data"/>"#), "{c}");
    assert!(c.contains(r#"<cacheField name="Region" numFmtId="0"><sharedItems count="3"><s v="East"/><s v="North"/><s v="South"/></sharedItems></cacheField>"#), "{c}");
    assert!(c.contains(r#"<cacheField name="Units" numFmtId="0"><sharedItems containsSemiMixedTypes="0" containsString="0" containsNumber="1" containsInteger="1" minValue="3" maxValue="20" containsBlank="1"/>"#), "{c}");
    let t2 = part(&file, "xl/pivotTables/pivotTable2.xml");
    assert!(t2.contains(r#"<pageFields count="1"><pageField fld="0" hier="-1"/></pageFields>"#), "{t2}");
    assert!(t2.contains(r#"<dataField name="Mean price" fld="4" subtotal="average" baseField="0" baseItem="0"/>"#), "{t2}");
    assert!(part(&file, "xl/workbook.xml").contains(r#"<pivotCaches><pivotCache cacheId="1" r:id="rIdPc1"/><pivotCache cacheId="2" r:id="rIdPc2"/></pivotCaches>"#));
    let (back, loaded) = xlsx::read(&file).unwrap();
    assert!(loaded.report.is_empty(), "{:?}", loaded.report.human());
    assert_eq!(back.pivots(), wb.pivots());
    assert_eq!(back.pivot_shape(0), wb.pivot_shape(0));
    assert_eq!(back.pivot_shape(1), wb.pivot_shape(1));
    assert_eq!(cells(&back), cells(&wb));
}

#[test]
fn xlsx_compact_round_trip() {
    let mut wb = book();
    let mut p = wb.pivots()[0].clone();
    p.compact = true;
    wb.set_pivot(0, p).unwrap();
    let file = xlsx::write(&wb, Vec::new()).unwrap();
    let t = part(&file, "xl/pivotTables/pivotTable1.xml");
    assert!(!t.contains(r#"compact="0""#) && t.contains(r#"<pivotField axis="axisRow" showAll="0">"#), "{t}");
    assert!(part(&file, "xl/pivotTables/pivotTable2.xml").contains(r#"compact="0""#));
    let (back, loaded) = xlsx::read(&file).unwrap();
    assert!(loaded.report.is_empty(), "{:?}", loaded.report.human());
    assert_eq!(back.pivots(), wb.pivots());
    assert_eq!(back.pivot_shape(0), wb.pivot_shape(0));
    assert!(!wb.pivot_shape(0).unwrap().indents.is_empty());
    assert_eq!(cells(&back), cells(&wb));
}

#[test]
fn ods_round_trip() {
    let wb = book();
    let file = ods::write(&wb, Vec::new()).unwrap();
    let (back, loaded) = ods::read(&file).unwrap();
    assert!(loaded.report.is_empty(), "{:?}", loaded.report.human());
    assert_eq!(back.pivots(), wb.pivots());
    assert_eq!(back.pivot_shape(0), wb.pivot_shape(0));
    assert_eq!(cells(&back), cells(&wb));
    // Compact form, as LibreOffice marks it.
    let mut wb = wb;
    let mut p = wb.pivots()[0].clone();
    p.compact = true;
    wb.set_pivot(0, p).unwrap();
    let file = ods::write(&wb, Vec::new()).unwrap();
    let (back, _) = ods::read(&file).unwrap();
    assert!(back.pivots()[0].compact && !back.pivots()[1].compact);
    assert_eq!(back.pivots(), wb.pivots());
    assert_eq!(cells(&back), cells(&wb));
}
