use officina_convert::{
    xlsx,
    zip::{Archive, ZipWriter},
};
use officina_core::history::Op;
use officina_grid::{
    addr::CellAddr,
    native,
    ops::GridOp,
    outline::{Group, Outline},
    shift::Axis,
    Value, Workbook,
};

fn group(start: u32, end: u32, collapsed: bool) -> Group {
    Group {
        start,
        end,
        collapsed,
    }
}

fn apply(wb: &mut Workbook, axis: Axis, groups: Vec<Group>) {
    GridOp::Outline {
        sheet: 0,
        axis,
        outline: Outline::new(groups, axis).unwrap(),
    }
    .apply(wb);
}

fn data() -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("Data").unwrap();
    wb.add_sheet("Totals").unwrap();
    for r in 1..=6 {
        wb.set_value(CellAddr::new(0, r, 0), Value::Num(f64::from(r)));
    }
    for (row, formula) in [
        "=SUBTOTAL(9,Data!A2:A7)",
        "=SUBTOTAL(109,Data!A2:A7)",
        "=A1=A2",
    ]
    .iter()
    .enumerate()
    {
        wb.set_formula(CellAddr::new(1, row as u32, 0), formula)
            .unwrap();
    }
    wb.recalc();
    wb
}

fn totals(wb: &Workbook, all: f64, visible: f64) {
    for (r, v) in [
        Value::Num(all),
        Value::Num(visible),
        Value::Bool(all == visible),
    ]
    .into_iter()
    .enumerate()
    {
        let addr = CellAddr::new(1, r as u32, 0);
        assert!(wb.formula_text(addr).is_some());
        assert_eq!(wb.get(addr), v);
    }
}

fn edit_xml(file: &[u8], edit: impl Fn(String) -> String) -> Vec<u8> {
    let original = Archive::open(file).unwrap();
    let mut out = ZipWriter::new(Vec::new());
    for entry in &original.entries {
        let mut bytes = original.read(entry).unwrap();
        if entry.name == "xl/worksheets/sheet1.xml" {
            bytes = edit(String::from_utf8(bytes).unwrap()).into_bytes();
        }
        out.add(&entry.name, &bytes).unwrap();
    }
    out.finish().unwrap()
}

#[test]
fn nested_row_groups_survive_both_saves_and_expand_independently() {
    let mut wb = data();
    // Include empty detail rows and an empty summary row.
    let outer = group(1, 9, false);
    let inner = group(2, 4, true);
    apply(&mut wb, Axis::Rows, vec![outer, inner]);
    wb.recalc();
    totals(&wb, 21.0, 12.0);
    let file = xlsx::write(&wb, Vec::new()).unwrap();
    let xml = Archive::open(&file)
        .unwrap()
        .read_name("xl/worksheets/sheet1.xml")
        .unwrap()
        .unwrap();
    let xml = String::from_utf8(xml).unwrap();
    assert!(xml.contains(r#"<row r="6" outlineLevel="1" collapsed="1""#));
    assert!(xml.contains(r#"<row r="10" outlineLevel="1"/>"#));
    let imported = xlsx::read(&file).unwrap().0;
    let native = native::load(&native::save_to_vec(&imported)).unwrap();
    let mut reopened = xlsx::read(&xlsx::write(&native, Vec::new()).unwrap())
        .unwrap()
        .0;
    assert_eq!(
        reopened.sheet(0).outline(Axis::Rows).groups(),
        &[outer, inner]
    );
    totals(&reopened, 21.0, 12.0);
    apply(&mut reopened, Axis::Rows, vec![outer, group(2, 4, false)]);
    reopened.recalc();
    totals(&reopened, 21.0, 21.0);
    reopened.set_value(CellAddr::new(0, 2, 0), Value::Num(8.0));
    reopened.recalc();
    totals(&reopened, 27.0, 27.0);
}

#[test]
fn summary_above_keeps_filtered_rows_hidden_after_expansion() {
    let mut wb = data();
    apply(&mut wb, Axis::Rows, vec![group(2, 4, true)]);
    let file = xlsx::write(&wb, Vec::new()).unwrap();
    let source = edit_xml(&file, |xml| {
        xml.replace("<dimension ", r#"<sheetPr><outlinePr summaryBelow="0"/></sheetPr><dimension "#)
            .replace(r#" collapsed="1""#, "")
            .replace(r#"<row r="2""#, r#"<row r="2" collapsed="1""#)
            .replace("</sheetData>", r#"</sheetData><autoFilter ref="A2:A6"><filterColumn colId="0"><customFilters><customFilter operator="notEqual" val="3"/></customFilters></filterColumn></autoFilter>"#)
    });
    let mut imported = xlsx::read(&source).unwrap().0;
    assert_eq!(
        imported.sheet(0).outline(Axis::Rows).groups(),
        &[group(2, 4, true)]
    );
    totals(&imported, 18.0, 12.0);
    assert!(!xlsx::supports_outlines(&imported));
    apply(&mut imported, Axis::Rows, vec![group(2, 4, false)]);
    imported.recalc();
    totals(&imported, 18.0, 18.0);
    assert!(imported.sheet(0).row_hidden(3));
    assert!(!imported.sheet(0).row_hidden(2));
}

#[test]
fn unrepresentable_outlines_require_native_save() {
    let mut wb = data();
    for (axis, groups) in [
        (Axis::Rows, vec![group(0, 1, true), group(2, 3, false)]),
        (Axis::Rows, vec![group(0, 4, true), group(1, 4, false)]),
        (Axis::Cols, vec![group(0, 1, false)]),
    ] {
        apply(&mut wb, Axis::Rows, vec![]);
        apply(&mut wb, axis, groups);
        assert!(xlsx::write(&wb, Vec::new()).is_err());
        assert!(xlsx::write_xlsm(&wb, Vec::new()).is_err());
    }
}

#[test]
fn expanded_group_keeps_manually_hidden_rows_and_invalid_depth_keeps_visibility() {
    let mut wb = data();
    apply(&mut wb, Axis::Rows, vec![group(2, 4, false)]);
    let file = xlsx::write(&wb, Vec::new()).unwrap();
    let source = edit_xml(&file, |xml| {
        xml.replace(
            r#"r="3" outlineLevel="1""#,
            r#"r="3" outlineLevel="1" hidden="1""#,
        )
    });
    let imported = xlsx::read(&source).unwrap().0;
    for saved in [
        xlsx::write(&imported, Vec::new()).unwrap(),
        xlsx::write_xlsm(&imported, Vec::new()).unwrap(),
    ] {
        let reopened = xlsx::read(&saved).unwrap().0;
        assert_eq!(
            reopened.sheet(0).outline(Axis::Rows).groups(),
            &[group(2, 4, false)]
        );
        totals(&reopened, 21.0, 19.0);
        assert!(reopened.sheet(0).row_hidden(2));
    }

    let invalid = edit_xml(&source, |xml| {
        xml.replace(r#"outlineLevel="1""#, r#"outlineLevel="8""#)
    });
    let (reopened, loaded) = xlsx::read(&invalid).unwrap();
    assert!(reopened.sheet(0).outline(Axis::Rows).groups().is_empty());
    assert!(reopened.sheet(0).row_hidden(2));
    assert!(format!("{:?}", loaded.report).contains("row outline beyond"));
}
