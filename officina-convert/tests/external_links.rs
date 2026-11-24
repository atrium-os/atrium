use officina_convert::xlsx;
use officina_convert::zip::{Archive, ZipWriter};
use officina_grid::{CellAddr, ErrorKind, Value, Workbook};

fn linked_workbook() -> Vec<u8> {
    let mut wb = Workbook::new();
    wb.add_sheet("Main").unwrap();
    let original = xlsx::write(&wb, Vec::new()).unwrap();
    let archive = Archive::open(&original).unwrap();
    let mut output = ZipWriter::new(Vec::new());
    for part in &archive.entries {
        let data = archive.read(part).unwrap();
        match part.name.as_str() {
            "xl/workbook.xml" => {
                let xml = String::from_utf8(data).unwrap();
                let xml = xml.replace("</workbook>", concat!(
                    r#"<externalReferences><externalReference r:id="foreign"/><externalReference r:id="embedded"/></externalReferences>"#,
                    r#"<definedNames><definedName name="Linked">'[2]O''Brien'!$A$1</definedName>"#,
                    r#"<definedName name="Uncached" localSheetId="0">[2]Elsewhere!$A$1</definedName></definedNames></workbook>"#,
                ));
                output.add(&part.name, xml.as_bytes()).unwrap();
            }
            "xl/_rels/workbook.xml.rels" => {
                let xml = String::from_utf8(data).unwrap();
                let xml = xml.replace("</Relationships>", concat!(
                    r#"<Relationship Id="foreign" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/externalLink" Target="https://example.invalid/source.xlsx" TargetMode="External"/>"#,
                    r#"<Relationship Id="embedded" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/externalLink" Target="externalLinks/data.xml"/></Relationships>"#,
                ));
                output.add(&part.name, xml.as_bytes()).unwrap();
            }
            "xl/worksheets/sheet1.xml" => {
                output.add(&part.name, br#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>
                    <row r="1"><c r="A1"><f>'[2]O''Brien'!A1</f><v>5</v></c>
                    <c r="B1"><f>'[2]O''Brien'!$A$1</f></c><c r="C1"><f>A1+B1+Linked</f><v>29</v></c>
                    <c r="D1" t="str"><f>"[2]Elsewhere!A1"</f><v>[2]Elsewhere!A1</v></c>
                    <c r="E1" t="e"><f>Uncached</f><v>#REF!</v></c>
                    <c r="F1"><f>[1]Main!A1</f></c><c r="G1"><f>'[2]O''Brien'!$B$1</f></c>
                    <c r="H1" t="b"><f t="array" ref="H1:H2">[2]SomeSheet!A1:A2</f><v>1</v></c></row>
                    <row r="2"><c r="A2"><f t="shared" ref="A2:A3" si="1">[2]SomeSheet!A2+3</f><v>8</v></c>
                    <c r="B2"><f t="shared" ref="B2:B3" si="2">'[2]O''Brien'!$A$1</f></c>
                    <c r="H2" t="b"><v>0</v></c></row>
                    <row r="3"><c r="A3" t="e"><f t="shared" si="1"/><v>#N/A</v></c>
                    <c r="B3"><f t="shared" si="2"/></c></row>
                    </sheetData></worksheet>"#).unwrap();
            }
            _ => output.add(&part.name, &data).unwrap(),
        }
    }
    output.add("xl/externalLinks/data.xml", br#"<externalLink xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
        <externalBook r:id="remote"><sheetNames><sheetName val="O'Brien"/></sheetNames><sheetDataSet>
        <sheetData sheetId="0"><row r="1"><cell r="A1">
        <v>12</v></cell><cell r="B1" t="str"><v>A&amp;B</v></cell></row></sheetData>
        </sheetDataSet></externalBook></externalLink>"#).unwrap();
    output.finish().unwrap()
}

#[test]
fn cached_external_values_survive_and_local_formulas_keep_calculating() {
    let (mut wb, out) = xlsx::read(&linked_workbook()).unwrap();
    let at = |row, col| CellAddr::new(0, row, col);
    for (row, col, value) in [
        (0, 0, Value::Num(5.0)),
        (0, 1, Value::Num(12.0)),
        (0, 2, Value::Num(29.0)),
        (0, 3, Value::str("[2]Elsewhere!A1")),
        (0, 4, Value::Err(ErrorKind::Ref)),
        (0, 5, Value::Err(ErrorKind::Ref)),
        (0, 6, Value::str("A&B")),
        (0, 7, Value::Bool(true)),
        (1, 0, Value::Num(8.0)),
        (1, 1, Value::Num(12.0)),
        (1, 7, Value::Bool(false)),
        (2, 0, Value::Err(ErrorKind::NA)),
        (2, 1, Value::Num(12.0)),
    ] {
        assert_eq!(wb.get(at(row, col)), value, "at {row}, {col}");
    }
    for (row, col) in [
        (0, 0),
        (0, 1),
        (0, 5),
        (0, 6),
        (0, 7),
        (1, 0),
        (1, 1),
        (2, 0),
        (2, 1),
    ] {
        assert!(wb.group_at(0, row, col).is_none());
    }
    for col in [2, 3, 4] {
        assert!(wb.group_at(0, 0, col).is_some());
    }
    let notes = out.report.human().join("\n");
    assert!(notes.contains("linked workbook formulas kept as cached values"));
    assert!(notes.contains("linked workbook formulas without a usable cached result"));
    assert!(notes.contains("linked workbook names kept as cached constants"));
    wb.set_value(at(0, 0), Value::Num(15.0));
    wb.recalc();
    assert_eq!(wb.get(at(0, 2)), Value::Num(39.0));

    let saved = xlsx::write(&wb, Vec::new()).unwrap();
    let package = Archive::open(&saved).unwrap();
    assert!(package
        .entries
        .iter()
        .all(|part| !part.name.contains("externalLinks/")));
    let (mut reopened, _) = xlsx::read(&saved).unwrap();
    assert_eq!(reopened.get(at(0, 2)), Value::Num(39.0));
    assert_eq!(reopened.get(at(0, 6)), Value::str("A&B"));
    assert_eq!(reopened.get(at(2, 0)), Value::Err(ErrorKind::NA));
    reopened.set_value(at(0, 0), Value::Num(25.0));
    reopened.recalc();
    assert_eq!(reopened.get(at(0, 2)), Value::Num(49.0));
}
