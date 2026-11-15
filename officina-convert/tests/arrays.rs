//! Dynamic arrays through .xlsx and .ods (docs/spec/officina.md §16.31): spilling
//! formulas written as Excel 365 writes them (`t="array"`, `cm="1"`, the XLDAPR cell
//! metadata, `_xlfn.`/`_xlws.`/`_xlpm.` names) and read back; a sheet as Excel saves it,
//! with Excel's cached results, recalculated to the same values; formulas from before
//! dynamic arrays keeping their one value.

use officina_convert::zip::{Archive, ZipWriter};
use officina_convert::{ods, xlsx};
use officina_grid::{CellAddr, Value, Workbook};

fn book() -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("Data").unwrap();
    for (i, (n, t)) in [("3", "c"), ("1", "a"), ("2", "b")].iter().enumerate() {
        wb.set_input(CellAddr::new(0, i as u32, 0), n).unwrap();
        wb.set_input(CellAddr::new(0, i as u32, 1), t).unwrap();
    }
    let put = |wb: &mut Workbook, a: &str, f: &str| {
        let (r, c) = officina_grid::addr::parse_a1(a).unwrap();
        wb.set_input(CellAddr::new(0, r, c), f).unwrap();
    };
    put(&mut wb, "D1", "=SORT(A1:A3)");
    put(&mut wb, "E1", "=FILTER(B1:B3,A1:A3>1)");
    put(&mut wb, "F1", "=LET(x,A1:A2,HSTACK(x,x*10))");
    put(&mut wb, "H1", "=SUM(D1#)");
    put(&mut wb, "I1", "=MAP(A1:A3,LAMBDA(v,v+1))");
    put(&mut wb, "J2", "=@A1:A3*2");
    put(&mut wb, "K1", "=SEQUENCE(2,2)");
    wb.recalc();
    wb
}

fn part(file: &[u8], name: &str) -> String {
    let z = Archive::open(file).unwrap();
    let e = z.entries.iter().find(|e| e.name == name).unwrap_or_else(|| panic!("no {name}"));
    String::from_utf8(z.read(e).unwrap()).unwrap()
}

fn same_cells(a: &Workbook, b: &Workbook) {
    let (rows, cols) = a.extent(0);
    for r in 0..rows {
        for c in 0..cols {
            let at = CellAddr::new(0, r, c);
            assert_eq!(b.get(at), a.get(at), "value at {}", officina_grid::addr::a1(r, c));
            assert_eq!(b.formula_text(at), a.formula_text(at), "formula at {}", officina_grid::addr::a1(r, c));
        }
    }
}

#[test]
fn xlsx_writes_dynamic_arrays_as_excel_does_and_reads_them_back() {
    let wb = book();
    assert_eq!(wb.value(0, 2, 3), Value::Num(3.0), "SORT spilled");
    assert_eq!(wb.value(0, 0, 7), Value::Num(6.0));
    let file = xlsx::write(&wb, Vec::new()).unwrap();
    let sheet = part(&file, "xl/worksheets/sheet1.xml");
    assert!(sheet.contains(r#"<c r="D1" cm="1"><f t="array" ref="D1:D3">_xlfn._xlws.SORT(A1:A3)</f><v>1</v></c>"#), "{sheet}");
    assert!(sheet.contains(r#"<f t="array" ref="E1:E2">_xlfn._xlws.FILTER(B1:B3,A1:A3&gt;1)</f>"#), "{sheet}");
    assert!(sheet.contains(r#"_xlfn.LET(_xlpm.x,A1:A2,_xlfn.HSTACK(_xlpm.x,_xlpm.x*10))"#), "{sheet}");
    assert!(sheet.contains(r#"<f>SUM(_xlfn.ANCHORARRAY(D1))</f>"#), "{sheet}");
    assert!(sheet.contains(r#"_xlfn.MAP(A1:A3,_xlfn.LAMBDA(_xlpm.v,_xlpm.v+1))"#), "{sheet}");
    // A formula from before dynamic arrays is written as one.
    assert!(sheet.contains(r#"<c r="J2"><f>A1:A3*2</f><v>2</v></c>"#), "{sheet}");
    // Spilled cells are plain values.
    assert!(sheet.contains(r#"<c r="D2"><v>2</v></c>"#), "{sheet}");
    assert!(part(&file, "[Content_Types].xml").contains("/xl/metadata.xml"));
    assert!(part(&file, "xl/_rels/workbook.xml.rels").contains("sheetMetadata"));
    assert!(part(&file, "xl/metadata.xml").contains(r#"fDynamic="1""#));

    let (back, loaded) = xlsx::read(&file).unwrap();
    same_cells(&wb, &back);
    assert_eq!(loaded.differ, 0, "{:?}", loaded.report.human());
    assert!(loaded.report.is_empty(), "{:?}", loaded.report.human());
    assert!(back.spill_area(CellAddr::new(0, 0, 3)).is_some_and(|a| a.rows() == 3));
}

/// Sheet XML as Excel 365 saves it, with its cached results: dynamic formulas (one
/// blocked by a value, giving #SPILL!), a Ctrl+Shift+Enter array, and formulas from
/// before dynamic arrays that intersect.
const EXCEL_SHEET: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:xr="http://schemas.microsoft.com/office/spreadsheetml/2014/revision"><dimension ref="A1:L4"/><sheetData>
<row r="1"><c r="A1"><v>3</v></c><c r="B1" t="str"><v>c</v></c><c r="D1" cm="1"><f t="array" ref="D1:D3">_xlfn._xlws.SORT(A1:A3,1,-1)</f><v>3</v></c><c r="E1" cm="1"><f t="array" ref="E1:F2">_xlfn.LET(_xlpm.n,2,_xlfn.SEQUENCE(_xlpm.n,_xlpm.n,0))</f><v>0</v></c><c r="F1"><v>1</v></c><c r="H1" t="e" cm="1"><f t="array" ref="H1">_xlfn.SEQUENCE(3)</f><v>#SPILL!</v></c><c r="J1"><f>A1:A3*10</f><v>30</v></c><c r="K1"><f t="array" ref="K1:K2">A1:A2+1</f><v>4</v></c><c r="L1" t="str"><f>_xlfn.TEXTJOIN("-",TRUE,_xlfn._xlws.SORT(B1:B3))</f><v>a-b-c</v></c></row>
<row r="2"><c r="A2"><v>1</v></c><c r="B2" t="str"><v>a</v></c><c r="D2"><v>2</v></c><c r="E2"><v>2</v></c><c r="F2"><v>3</v></c><c r="H2" t="str"><v>in the way</v></c><c r="J2"><f>A1:A3*10</f><v>10</v></c><c r="K2"><v>2</v></c><c r="L2"><f>SUM(_xlfn.ANCHORARRAY(D1))</f><v>6</v></c></row>
<row r="3"><c r="A3"><v>2</v></c><c r="B3" t="str"><v>b</v></c><c r="D3"><v>1</v></c><c r="J3"><f>A1:A3*10</f><v>20</v></c><c r="L3"><f>_xlfn.REDUCE(0,A1:A3,_xlfn.LAMBDA(_xlpm.a,_xlpm.v,_xlpm.a+_xlpm.v*_xlpm.v))</f><v>14</v></c></row>
<row r="4"><c r="J4" t="e"><f>A1:A3*10</f><v>#VALUE!</v></c></row>
</sheetData></worksheet>"#;

#[test]
fn a_sheet_as_excel_saves_it_recalculates_to_excels_results() {
    let mut wb = Workbook::new();
    wb.add_sheet("Data").unwrap();
    wb.set_input(CellAddr::new(0, 0, 0), "=SEQUENCE(2)").unwrap();
    wb.recalc();
    let ours = xlsx::write(&wb, Vec::new()).unwrap();
    let z = Archive::open(&ours).unwrap();
    let mut w = ZipWriter::new(Vec::new());
    for e in &z.entries {
        let data = if e.name == "xl/worksheets/sheet1.xml" { EXCEL_SHEET.as_bytes().to_vec() } else { z.read(e).unwrap() };
        w.add(&e.name, &data).unwrap();
    }
    let file = w.finish().unwrap();
    let (back, loaded) = xlsx::read(&file).unwrap();
    assert_eq!(loaded.differ, 0, "{:?}", loaded.report.human());
    assert_eq!(loaded.formulas, 11);
    let v = |a: &str| {
        let (r, c) = officina_grid::addr::parse_a1(a).unwrap();
        back.value(0, r, c).display()
    };
    let col = |a: &[&str]| a.iter().map(|x| v(x)).collect::<Vec<_>>().join(",");
    assert_eq!(col(&["D1", "D2", "D3"]), "3,2,1");
    assert_eq!(col(&["E1", "F1", "E2", "F2"]), "0,1,2,3");
    assert_eq!(col(&["H1", "H2", "H3"]), "#SPILL!,in the way,");
    assert_eq!(col(&["J1", "J2", "J3", "J4"]), "30,10,20,#VALUE!");
    assert_eq!(col(&["K1", "K2"]), "4,2");
    assert_eq!(col(&["L1", "L2", "L3"]), "a-b-c,6,14");
    // Formulas from before dynamic arrays read with their `@`, as Excel 365 shows them.
    assert_eq!(back.formula_text(CellAddr::new(0, 0, 9)).as_deref(), Some("=@A1:A3*10"));
    assert_eq!(back.formula_text(CellAddr::new(0, 0, 3)).as_deref(), Some("=SORT(A1:A3,1,-1)"));
    // Clearing what was in the way lets the blocked one spill.
    let mut back = back;
    back.set_input(CellAddr::new(0, 1, 7), "").unwrap();
    back.recalc();
    assert_eq!(col_of(&back, 7), "1,2,3");
}

fn col_of(wb: &Workbook, c: u32) -> String {
    (0..3).map(|r| wb.value(0, r, c).display()).collect::<Vec<_>>().join(",")
}

#[test]
fn ods_writes_spills_as_matrix_formulas_and_reads_them_back() {
    let wb = book();
    let file = ods::write(&wb, Vec::new()).unwrap();
    let content = part(&file, "content.xml");
    assert!(content.contains(r#"table:formula="of:=COM.MICROSOFT.SORT([.A1:.A3])" table:number-matrix-columns-spanned="1" table:number-matrix-rows-spanned="3""#), "{content}");
    // No `A1#` in OpenFormula: the range it spills over.
    assert!(content.contains(r#"of:=SUM([.$D$1:.$D$3])"#), "{content}");
    // No `@` either: a plain formula intersects there anyway.
    assert!(content.contains(r#"of:=[.A1:.A3]*2""#), "{content}");
    let (back, loaded) = ods::read(&file).unwrap();
    assert_eq!(loaded.differ, 0, "{:?}", loaded.report.human());
    let (rows, cols) = wb.extent(0);
    for r in 0..rows {
        for c in 0..cols {
            let at = CellAddr::new(0, r, c);
            assert_eq!(back.get(at), wb.get(at), "value at {}", officina_grid::addr::a1(r, c));
        }
    }
    assert_eq!(back.formula_text(CellAddr::new(0, 1, 9)).as_deref(), Some("=@A1:A3*2"));
    assert!(back.spill_area(CellAddr::new(0, 0, 3)).is_some_and(|a| a.rows() == 3));
}
