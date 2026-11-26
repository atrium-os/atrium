//! Themes, table styles and options, shapes and Camera pictures through
//! .xlsx and .ods (docs/spec/officina.md §16.165).

use officina_core::history::Op;
use officina_convert::{ods, xlsx};
use officina_grid::addr::Rect;
use officina_grid::charts::Anchor;
use officina_grid::look::{builtin_theme, Camera, Shape, TablePart, TableStyleDef, HEADER, ROW_STRIPE};
use officina_grid::ops::GridOp;
use officina_grid::tables::{Table, TableBands};
use officina_grid::{CellAddr, Value, Workbook};

fn book() -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("Data").unwrap();
    wb.add_sheet("Other sheet").unwrap();
    wb.set_value(CellAddr::new(0, 0, 0), Value::str("Name"));
    wb.set_value(CellAddr::new(0, 0, 1), Value::str("Score"));
    for r in 1..6 {
        wb.set_value(CellAddr::new(0, r, 0), Value::str(&format!("P{r}")));
        wb.set_value(CellAddr::new(0, r, 1), Value::Num(r as f64 * 3.0));
    }
    wb.theme_op(builtin_theme("Green").unwrap()).apply(&mut wb);
    let mut look = wb.workbook_look().clone();
    let mut parts = [None; 7];
    parts[HEADER] = Some(TablePart { fill: Some(0x1F4E78), text: Some(0xFFFFFF), bold: true, border: None });
    parts[ROW_STRIPE] = Some(TablePart { fill: Some(0xDDEBF7), ..Default::default() });
    look.table_styles.push(TableStyleDef { name: "Brand".into(), parts });
    let a = |sheet, r, c| Anchor { sheet, from: (r, c, 2.0, 3.0), to: (r + 4, c + 3, 10.0, 6.0) };
    look.shapes.push(Shape { name: "Callout 1".into(), anchor: a(0, 1, 4), geom: "wedgeRectCallout".into(), fill: Some(0xFFC000), line: Some(0x7F6000),
        text: "Best score\nso far".into(), text_color: Some(0x000000), text_size: 280, bold: true, alt: Some("A note on the best score".into()) });
    look.shapes.push(Shape { name: "Text 2".into(), anchor: a(1, 2, 1), geom: "rect".into(), fill: None, line: None,
        text: "Plain words & <marks>".into(), text_color: None, text_size: 220, bold: false, alt: None });
    look.cameras.push(Camera { name: "Camera 1".into(), anchor: a(1, 10, 1), source: Rect { sheet: 0, r0: 0, c0: 0, r1: 5, c1: 1 } });
    GridOp::Look(Box::new(look)).apply(&mut wb);
    wb.add_table(Table { name: "Scores".into(), rect: Rect { sheet: 0, r0: 0, c0: 0, r1: 5, c1: 1 }, header: true, totals: false,
        columns: vec!["Name".into(), "Score".into()], style: Some("Brand".into()), stripes: true,
        bands: TableBands { cols: true, first: false, last: true }, filter: None }).unwrap();
    assert_eq!(wb.workbook_look().shapes.len(), 2);
    wb
}

fn same_look(back: &Workbook, wb: &Workbook) {
    assert_eq!(back.workbook_look(), wb.workbook_look());
    let (t, u) = (&back.tables()[0], &wb.tables()[0]);
    assert_eq!((&t.style, t.stripes, t.bands), (&u.style, u.stripes, u.bands));
    assert_eq!(back.table_cell_look(t, 0, 0).unwrap().fill, Some(0x1F4E78));
}

#[test]
fn the_look_survives_xlsx_and_ods() {
    let wb = book();
    let x = xlsx::write(&wb, Vec::new()).unwrap();
    let (back, _) = xlsx::read(&x).unwrap();
    same_look(&back, &wb);
    let o = ods::write(&wb, Vec::new()).unwrap();
    let (back, _) = ods::read(&o).unwrap();
    same_look(&back, &wb);
    // A plain workbook carries no look either way.
    let mut plain = Workbook::new();
    plain.add_sheet("S").unwrap();
    assert!(xlsx::read(&xlsx::write(&plain, Vec::new()).unwrap()).unwrap().0.workbook_look().is_default());
    assert!(ods::read(&ods::write(&plain, Vec::new()).unwrap()).unwrap().0.workbook_look().is_default());
}

#[test]
fn excel_parts_say_what_excel_reads() {
    let wb = book();
    let x = xlsx::write(&wb, Vec::new()).unwrap();
    let z = officina_convert::zip::Archive::open(&x).unwrap();
    let part = |n: &str| String::from_utf8(z.read_name(n).unwrap().unwrap()).unwrap();
    let theme = part("xl/theme/theme1.xml");
    assert!(theme.contains(r#"name="Green""#) && theme.contains(&format!("{:06X}", wb.theme().colors[4])), "{theme}");
    let styles = part("xl/styles.xml");
    assert!(styles.contains(r#"<tableStyle name="Brand" pivot="0" count="2"><tableStyleElement type="headerRow""#), "{styles}");
    let table = part("xl/tables/table1.xml");
    assert!(table.contains(r#"showFirstColumn="0" showLastColumn="1" showRowStripes="1" showColumnStripes="1""#), "{table}");
    let d = part("xl/drawings/drawing1.xml");
    assert!(d.contains(r#"<a:prstGeom prst="wedgeRectCallout">"#) && d.contains("<a:t>so far</a:t>") && d.contains(r#"sz="1400" b="1""#), "{d}");
    assert!(part("xl/drawings/drawing2.xml").contains(r#"txBox="1""#));
}

#[test]
fn shapes_from_other_programs_read_with_their_styles() {
    // An Excel-style shape: colours from the shape's style references.
    let mut wb = Workbook::new();
    wb.add_sheet("S").unwrap();
    let x = xlsx::write(&wb, Vec::new()).unwrap();
    let z = officina_convert::zip::Archive::open(&x).unwrap();
    let mut out = officina_convert::zip::ZipWriter::new(Vec::new());
    for e in &z.entries {
        let mut data = z.read(e).unwrap();
        if e.name == "xl/worksheets/sheet1.xml" {
            data = String::from_utf8(data).unwrap().replace("</worksheet>", r#"<drawing r:id="rIdD"/></worksheet>"#).into_bytes();
        }
        if e.name == "[Content_Types].xml" {
            data = String::from_utf8(data).unwrap().replace("</Types>", r#"<Override PartName="/xl/drawings/drawing1.xml" ContentType="application/vnd.openxmlformats-officedocument.drawing+xml"/></Types>"#).into_bytes();
        }
        out.add(&e.name, &data).unwrap();
    }
    out.add("xl/worksheets/_rels/sheet1.xml.rels", br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rIdD" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#).unwrap();
    out.add("xl/drawings/drawing1.xml", br#"<?xml version="1.0"?><xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
<xdr:twoCellAnchor><xdr:from><xdr:col>1</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>2</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from><xdr:to><xdr:col>4</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>8</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:to>
<xdr:sp macro="" textlink=""><xdr:nvSpPr><xdr:cNvPr id="2" name="Oval 1"/><xdr:cNvSpPr/></xdr:nvSpPr><xdr:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/></a:xfrm><a:prstGeom prst="ellipse"><a:avLst/></a:prstGeom></xdr:spPr>
<xdr:style><a:lnRef idx="2"><a:schemeClr val="accent1"><a:shade val="50000"/></a:schemeClr></a:lnRef><a:fillRef idx="1"><a:schemeClr val="accent2"/></a:fillRef><a:effectRef idx="0"><a:schemeClr val="accent1"/></a:effectRef><a:fontRef idx="minor"><a:schemeClr val="lt1"/></a:fontRef></xdr:style>
<xdr:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang="en-US" sz="1800"/><a:t>Hello</a:t></a:r></a:p></xdr:txBody></xdr:sp><xdr:clientData/></xdr:twoCellAnchor>
<xdr:twoCellAnchor><xdr:from><xdr:col>1</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>2</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from><xdr:to><xdr:col>4</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>8</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:to>
<xdr:grpSp><xdr:sp><xdr:spPr><a:prstGeom prst="rect"/></xdr:spPr></xdr:sp></xdr:grpSp><xdr:clientData/></xdr:twoCellAnchor></xdr:wsDr>"#).unwrap();
    let (back, out) = xlsx::read(&out.finish().unwrap()).unwrap();
    let s = &back.workbook_look().shapes;
    assert_eq!(s.len(), 1, "the grouped shape is dropped: {s:?}");
    let o = &s[0];
    assert_eq!((&*o.name, &*o.geom, &*o.text, o.fill, o.line, o.text_color, o.text_size), ("Oval 1", "ellipse", "Hello", Some(0xED7D31), Some(0x4472C4), Some(0xFFFFFF), 360));
    assert_eq!((o.anchor.from.0, o.anchor.from.1, o.anchor.to.0, o.anchor.to.1), (2, 1, 8, 4));
    assert!(format!("{:?}", out.report).contains("grouped shapes"), "{:?}", out.report);
}

#[test]
fn hostile_look_parts_are_bounded() {
    // Our extension with an unknown sheet, a bad preset and a bad colour: each dropped alone.
    let wb = book();
    let o = ods::write(&wb, Vec::new()).unwrap();
    let z = officina_convert::zip::Archive::open(&o).unwrap();
    let mut out = officina_convert::zip::ZipWriter::new(Vec::new());
    for e in &z.entries {
        let mut data = z.read(e).unwrap();
        if e.name == "content.xml" {
            let s = String::from_utf8(data).unwrap();
            let bad = r#"<officina:camera officina:name="Lost" officina:sheet="Nowhere" officina:from="0 0 0 0" officina:to="1 1 0 0" officina:source-sheet="Data" officina:source="0 0 1 1"/><officina:camera officina:name="Huge" officina:sheet="Data" officina:from="0 0 0 0" officina:to="1 1 0 0" officina:source-sheet="Data" officina:source="0 0 999999 999"/><officina:table-style officina:name="TableStyleMedium2"/><officina:table-style officina:name="Odd"><officina:part officina:kind="headerRow" officina:fill="zzzzzz" officina:text="none" officina:bold="0" officina:border="none"/></officina:table-style></officina:look>"#;
            data = s.replacen("</officina:look>", bad, 1).into_bytes();
        }
        out.add(&e.name, &data).unwrap();
    }
    let (back, rep) = ods::read(&out.finish().unwrap()).unwrap();
    assert_eq!(back.workbook_look(), wb.workbook_look());
    assert!(format!("{:?}", rep.report).contains("invalid or excess"), "{:?}", rep.report);
}

#[test]
fn libreoffice_reads_our_shapes_and_theme() {
    if !std::process::Command::new("soffice").arg("--version").output().is_ok_and(|o| o.status.success()) {
        eprintln!("skipped: no LibreOffice");
        return;
    }
    let wb = book();
    let tmp = std::env::temp_dir().join(format!("officina-look-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    for (ext, bytes) in [("xlsx", xlsx::write(&wb, Vec::new()).unwrap()), ("ods", ods::write(&wb, Vec::new()).unwrap())] {
        let src = tmp.join(format!("in.{ext}"));
        std::fs::write(&src, bytes).unwrap();
        let out = tmp.join(format!("out-{ext}"));
        let st = std::process::Command::new("soffice")
            .args(["--headless", &format!("-env:UserInstallation=file://{}/profile", tmp.display()), "--convert-to", ext, "--outdir"])
            .arg(&out)
            .arg(&src)
            .output()
            .unwrap();
        assert!(st.status.success(), "{st:?}");
        let again = std::fs::read(out.join(format!("in.{ext}"))).unwrap();
        let back = if ext == "xlsx" { xlsx::read(&again).unwrap().0 } else { ods::read(&again).unwrap().0 };
        let s = &back.workbook_look().shapes;
        let callout = s.iter().find(|x| &*x.geom == "wedgeRectCallout").unwrap_or_else(|| panic!("{ext}: {s:?}"));
        assert_eq!(&*callout.text, "Best score\nso far", "{ext}");
        assert_eq!(callout.fill, Some(0xFFC000), "{ext}");
        assert!(s.iter().any(|x| &*x.text == "Plain words & <marks>"), "{ext}: {s:?}");
        if ext == "xlsx" {
            assert_eq!(back.theme().colors[4], wb.theme().colors[4]);
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
}
