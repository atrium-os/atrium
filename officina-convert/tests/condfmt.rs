//! Conditional formats through .xlsx and .ods (docs/spec/officina.md §16.31): every
//! rule kind written as Excel writes it and read back the same; a sheet's rules as
//! Excel saves them picking the cells Excel picks.

use officina_convert::zip::{Archive, ZipWriter};
use officina_convert::{ods, xlsx};
use officina_grid::condfmt::{Cfvo, CmpOp, CondFormat, Dxf, Rule, RuleKind, TextOp};
use officina_grid::{CellAddr, Rect, Workbook};
use std::sync::Arc;

fn book() -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("Data").unwrap();
    for i in 0..10u32 {
        wb.set_input(CellAddr::new(0, i, 0), &(i + 1).to_string()).unwrap();
        wb.set_input(CellAddr::new(0, i, 1), ["apple", "pear", "fig", "apple", "kiwi"][i as usize % 5]).unwrap();
    }
    wb.recalc();
    let col = |c: u32| Rect { sheet: 0, r0: 0, c0: c, r1: 9, c1: c };
    let red = Dxf { fill: Some(0xFFC7CE), color: Some(0x9C0006), ..Dxf::default() };
    let bold = Dxf { bold: Some(true), italic: Some(false), num_fmt: Some(Arc::from("0.0%")), ..Dxf::default() };
    let f = |wb: &Workbook, s: &str| wb.cond_formula(s, 0, (0, 0)).unwrap();
    let rules_a = vec![
        Rule { kind: RuleKind::CellIs { op: CmpOp::Between, a: f(&wb, "2"), b: Some(f(&wb, "$A$4")) }, dxf: red.clone(), stop: true },
        Rule { kind: RuleKind::Expression(f(&wb, "MOD(A1,2)=0")), dxf: bold.clone(), stop: false },
        Rule { kind: RuleKind::Top { n: 20, percent: true, bottom: true }, dxf: red.clone(), stop: false },
        Rule { kind: RuleKind::Average { below: true, equal: true, std_dev: 1 }, dxf: bold.clone(), stop: false },
        Rule { kind: RuleKind::ColorScale(vec![(Cfvo::Min, 0xF8696B), (Cfvo::Percentile(50.0), 0xFFEB84), (Cfvo::Max, 0x63BE7B)]), dxf: Dxf::default(), stop: false },
        Rule { kind: RuleKind::DataBar { min: Cfvo::Num(0.0), max: Cfvo::Formula(f(&wb, "MAX($A$1:$A$10)")), color: 0x638EC6 }, dxf: Dxf::default(), stop: false },
    ];
    wb.add_cond_format(CondFormat { sheet: 0, ranges: vec![col(0)], rules: rules_a }).unwrap();
    let rules_b = vec![
        Rule { kind: RuleKind::Text { op: TextOp::Contains, text: Arc::from("p\"l") }, dxf: red.clone(), stop: false },
        Rule { kind: RuleKind::Text { op: TextOp::BeginsWith, text: Arc::from("ki") }, dxf: bold.clone(), stop: false },
        Rule { kind: RuleKind::Duplicates { unique: false }, dxf: red.clone(), stop: false },
        Rule { kind: RuleKind::Blanks(false), dxf: bold.clone(), stop: false },
        Rule { kind: RuleKind::Errors(true), dxf: red, stop: false },
    ];
    wb.add_cond_format(CondFormat { sheet: 0, ranges: vec![col(1), Rect { sheet: 0, r0: 12, c0: 1, r1: 12, c1: 3 }], rules: rules_b }).unwrap();
    wb
}

fn part(file: &[u8], name: &str) -> String {
    let z = Archive::open(file).unwrap();
    let e = z.entries.iter().find(|e| e.name == name).unwrap_or_else(|| panic!("no {name}"));
    String::from_utf8(z.read(e).unwrap()).unwrap()
}

#[test]
fn xlsx_round_trip_of_every_rule_kind() {
    let wb = book();
    let file = xlsx::write(&wb, Vec::new()).unwrap();
    let sheet = part(&file, "xl/worksheets/sheet1.xml");
    assert!(sheet.contains(r#"<conditionalFormatting sqref="A1:A10"><cfRule type="cellIs" dxfId="0" priority="1" stopIfTrue="1" operator="between"><formula>2</formula><formula>$A$4</formula></cfRule>"#), "{sheet}");
    assert!(sheet.contains(r#"<cfRule type="containsText" dxfId="0" priority="7" operator="containsText" text="p&quot;l"><formula>NOT(ISERROR(SEARCH("p""l",B1)))</formula></cfRule>"#), "{sheet}");
    assert!(sheet.contains(r#"<cfRule type="colorScale" priority="5"><colorScale><cfvo type="min"/><cfvo type="percentile" val="50"/><cfvo type="max"/><color rgb="FFF8696B"/>"#), "{sheet}");
    assert!(sheet.contains(r#"sqref="B1:B10 B13:D13""#), "{sheet}");
    let styles = part(&file, "xl/styles.xml");
    assert!(styles.contains(r#"<dxfs count="2"><dxf><font><color rgb="FF9C0006"/></font><fill><patternFill patternType="solid"><fgColor rgb="FFFFC7CE"/><bgColor rgb="FFFFC7CE"/></patternFill></fill></dxf>"#), "{styles}");
    let (back, loaded) = xlsx::read(&file).unwrap();
    assert!(loaded.report.is_empty(), "{:?}", loaded.report.human());
    assert_eq!(back.cond_formats(), wb.cond_formats());
    for r in 0..13 {
        for c in 0..4 {
            assert_eq!(back.look(CellAddr::new(0, r, c)), wb.look(CellAddr::new(0, r, c)), "look at {r},{c}");
        }
    }
}

/// Rules as Excel 2016 saves them (Home › Conditional Formatting), over A1:A6 =
/// 5, 12, 7, 12, 30, 1: what Excel shows is in the comments.
const EXCEL_SHEET: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>
<row r="1"><c r="A1"><v>5</v></c></row><row r="2"><c r="A2"><v>12</v></c></row><row r="3"><c r="A3"><v>7</v></c></row>
<row r="4"><c r="A4"><v>12</v></c></row><row r="5"><c r="A5"><v>30</v></c></row><row r="6"><c r="A6"><v>1</v></c></row>
</sheetData>
<conditionalFormatting sqref="A1:A6"><cfRule type="duplicateValues" dxfId="0" priority="3"/><cfRule type="cellIs" dxfId="0" priority="1" operator="greaterThan"><formula>20</formula></cfRule></conditionalFormatting>
<conditionalFormatting sqref="A1:A6"><cfRule type="top10" dxfId="0" priority="2" bottom="1" rank="1"/></conditionalFormatting>
<conditionalFormatting sqref="A1:A6"><cfRule type="dataBar" priority="4"><dataBar><cfvo type="min"/><cfvo type="max"/><color rgb="FF638EC6"/></dataBar><extLst><ext uri="{B025F937-C7B1-47D3-B67F-A62EFF666E3E}" xmlns:x14="http://schemas.microsoft.com/office/spreadsheetml/2009/9/main"><x14:id>{1}</x14:id></ext></extLst></cfRule></conditionalFormatting>
<pageMargins left="0.7" right="0.7" top="0.75" bottom="0.75" header="0.3" footer="0.3"/>
<extLst><ext uri="{78C0D931-6437-407d-A8EE-F0AAD7539E65}" xmlns:x14="http://schemas.microsoft.com/office/spreadsheetml/2009/9/main"><x14:conditionalFormattings><x14:conditionalFormatting xmlns:xm="http://schemas.microsoft.com/office/excel/2006/main"><x14:cfRule type="dataBar" id="{1}"><x14:dataBar minLength="0" maxLength="100"><x14:cfvo type="autoMin"/><x14:cfvo type="autoMax"/></x14:dataBar></x14:cfRule><xm:sqref>A1:A6</xm:sqref></x14:conditionalFormatting></x14:conditionalFormattings></ext></extLst>
</worksheet>"#;

#[test]
fn rules_as_excel_saves_them_pick_what_excel_shows() {
    // Our file for the rest of the package (its styles have one red dxf).
    let mut wb = Workbook::new();
    wb.add_sheet("S").unwrap();
    wb.add_cond_format(CondFormat { sheet: 0, ranges: vec![Rect { sheet: 0, r0: 0, c0: 0, r1: 0, c1: 0 }], rules: vec![Rule { kind: RuleKind::Blanks(true), dxf: Dxf { fill: Some(0xFFC7CE), ..Dxf::default() }, stop: false }] }).unwrap();
    let ours = xlsx::write(&wb, Vec::new()).unwrap();
    let z = Archive::open(&ours).unwrap();
    let mut w = ZipWriter::new(Vec::new());
    for e in &z.entries {
        let data = if e.name == "xl/worksheets/sheet1.xml" { EXCEL_SHEET.as_bytes().to_vec() } else { z.read(e).unwrap() };
        w.add(&e.name, &data).unwrap();
    }
    let (back, loaded) = xlsx::read(&w.finish().unwrap()).unwrap();
    // The 2010 extension repeats the data bar: noted, not doubled.
    assert_eq!(loaded.report.human().len(), 1, "{:?}", loaded.report.human());
    let red: Vec<u32> = (0..6).filter(|&r| back.look(CellAddr::new(0, r, 0)).dxf.fill == Some(0xFFC7CE)).map(|r| r + 1).collect();
    // Duplicates (12, 12), over 20 (30), the bottom one (1).
    assert_eq!(red, [2, 4, 5, 6]);
    // Priority order across formats: greater-than first, top-10 second, duplicates third.
    let kinds: Vec<&str> = back.cond_formats().iter().flat_map(|f| f.rules.iter()).map(|r| match r.kind {
        RuleKind::CellIs { .. } => "cellIs",
        RuleKind::Duplicates { .. } => "dup",
        RuleKind::Top { .. } => "top",
        RuleKind::DataBar { .. } => "bar",
        _ => "?",
    }).collect();
    assert_eq!(kinds, ["cellIs", "dup", "top", "bar"]);
    assert_eq!(back.look(CellAddr::new(0, 4, 0)).bar.map(|b| b.0), Some(1.0));
}

/// LibreOffice applies the first condition of a format that matches and no other,
/// next to its colour scales and data bars: rules written that way come back the same.
#[test]
fn ods_round_trip_keeps_what_opendocument_holds() {
    let mut wb = book();
    let mut formats = wb.cond_formats().to_vec();
    while !wb.cond_formats().is_empty() {
        wb.remove_cond_format(0);
    }
    for f in &mut formats {
        f.rules.sort_by_key(|r| !matches!(r.kind, RuleKind::ColorScale(_) | RuleKind::DataBar { .. }));
        for r in &mut f.rules {
            r.stop = !matches!(r.kind, RuleKind::ColorScale(_) | RuleKind::DataBar { .. });
        }
        wb.add_cond_format(f.clone()).unwrap();
    }
    let file = ods::write(&wb, Vec::new()).unwrap();
    let content = part(&file, "content.xml");
    assert!(content.contains(r#"<calcext:condition calcext:apply-style-name="CF1" calcext:value="between(2,[.$A$4])" calcext:base-cell-address="Data.A1"/>"#), "{content}");
    assert!(content.contains(r#"calcext:value="contains-text(&quot;p&quot;&quot;l&quot;)""#), "{content}");
    assert!(part(&file, "styles.xml").contains(r##"<style:style style:name="CF1" style:family="table-cell" style:parent-style-name="Default"><style:table-cell-properties fo:background-color="#ffc7ce"/><style:text-properties fo:color="#9c0006"/></style:style>"##));
    let (back, loaded) = ods::read(&file).unwrap();
    assert!(loaded.report.is_empty(), "{:?}", loaded.report.human());
    assert_eq!(loaded.differ, 0);
    assert_eq!(back.cond_formats().len(), 2);
    for r in 0..13 {
        for c in 0..4 {
            assert_eq!(back.look(CellAddr::new(0, r, c)), wb.look(CellAddr::new(0, r, c)), "look at {r},{c}");
        }
    }
}
