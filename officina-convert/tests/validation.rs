//! Data validation rules through .xlsx and .ods and back, and (when
//! LibreOffice is installed) as LibreOffice reads our files and writes its own.

use officina_grid::addr::Rect;
use officina_grid::condfmt::CmpOp;
use officina_grid::validation::{Allow, ErrorStyle, Message, Validation};
use officina_grid::{CellAddr, Value, Workbook};

fn book() -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("Data").unwrap();
    wb.add_sheet("Lists").unwrap();
    for (i, s) in ["North", "South", "East"].iter().enumerate() {
        wb.set_value(CellAddr::new(1, i as u32, 0), Value::str(s));
    }
    wb.set_value(CellAddr::new(0, 0, 0), Value::str("Region"));
    let r = |r0, c0, r1, c1| Rect { sheet: 0, r0, c0, r1, c1 };
    let f = |wb: &Workbook, s: &str, at: (u32, u32)| Some(wb.validation_formula(s, 0, at).unwrap());
    let mut list = Validation::new(0, vec![r(1, 0, 20, 0)], Allow::List, CmpOp::Between, f(&wb, "Lists!$A$1:$A$3", (1, 0)), None);
    list.prompt = Message::new("Region", "Pick a region.");
    list.error = Message::new("Not a region", "Choose one from the list.");
    let literal = Validation::new(0, vec![r(1, 1, 20, 1)], Allow::List, CmpOp::Between, f(&wb, "\"Yes,No\"", (1, 1)), None);
    let mut whole = Validation::new(0, vec![r(1, 2, 20, 2), r(30, 2, 30, 2)], Allow::Whole, CmpOp::Between, f(&wb, "1", (1, 2)), f(&wb, "100", (1, 2)));
    whole.style = ErrorStyle::Warning;
    let decimal = Validation::new(0, vec![r(1, 3, 20, 3)], Allow::Decimal, CmpOp::GreaterOrEqual, f(&wb, "0.5", (1, 3)), None);
    let mut length = Validation::new(0, vec![r(1, 4, 20, 4)], Allow::TextLength, CmpOp::LessOrEqual, f(&wb, "10", (1, 4)), None);
    length.style = ErrorStyle::Information;
    length.allow_blank = false;
    let custom = Validation::new(0, vec![r(1, 5, 20, 5)], Allow::Custom, CmpOp::Between, f(&wb, "F2>E2", (1, 5)), None);
    let date = Validation::new(0, vec![r(1, 6, 20, 6)], Allow::Date, CmpOp::Greater, f(&wb, "DATE(2026,1,1)", (1, 6)), None);
    for v in [list, literal, whole, decimal, length, custom, date] {
        wb.add_validation(v).unwrap();
    }
    wb.recalc();
    wb
}

/// The rules as text: what each is over, allows, says, and its formulas.
fn all(wb: &Workbook) -> Vec<String> {
    let mut rules: Vec<_> = wb.validations()
        .iter()
        .map(|v| {
            let f = |e: &Option<std::sync::Arc<officina_grid::formula::Expr>>| e.as_ref().map(|e| wb.cond_formula_text(e, v.origin())).unwrap_or_default();
            let op = if matches!(v.allow, Allow::List | Allow::Custom) { CmpOp::Between } else { v.op };
            format!("{:?} {:?} {:?} {} {} blank={} dd={} {:?} {:?}/{:?} {:?}", v.ranges.iter().map(|r| (r.r0, r.c0, r.r1, r.c1)).collect::<Vec<_>>(), v.allow, op, f(&v.f1), f(&v.f2), v.allow_blank, v.dropdown, v.style, v.prompt, v.error, (v.show_prompt, v.show_error))
        })
        .collect();
    // Excel stores extension rules separately from standard rules.
    rules.sort();
    rules
}

#[test]
fn validation_round_trips_through_xlsx_and_ods() {
    let wb = book();
    let x = officina_convert::xlsx::write(&wb, Vec::new()).unwrap();
    let (back, rep) = officina_convert::xlsx::read(&x).unwrap();
    assert_eq!(all(&back), all(&wb));
    assert!(!format!("{rep:?}").contains("validation"), "{rep:?}");
    assert_eq!(back.list_items(CellAddr::new(0, 4, 0)), vec![Value::str("North"), Value::str("South"), Value::str("East")]);
    let o = officina_convert::ods::write(&wb, Vec::new()).unwrap();
    let (back, rep) = officina_convert::ods::read(&o).unwrap();
    assert_eq!(all(&back), all(&wb));
    assert!(!format!("{rep:?}").contains("validation"), "{rep:?}");
}

/// LibreOffice reads our .xlsx and writes .ods, and reads our .ods and writes
/// .xlsx: the rules come back the same.
#[test]
fn libreoffice_keeps_our_validation() {
    if !std::process::Command::new("soffice").arg("--version").output().is_ok_and(|o| o.status.success()) {
        eprintln!("skipped: no LibreOffice");
        return;
    }
    let wb = book();
    let want = all(&wb);
    let tmp = std::env::temp_dir().join(format!("officina-validation-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    for (ext, to, bytes) in [("xlsx", "ods", officina_convert::xlsx::write(&wb, Vec::new()).unwrap()), ("ods", "xlsx", officina_convert::ods::write(&wb, Vec::new()).unwrap())] {
        let src = tmp.join(format!("v.{ext}"));
        std::fs::write(&src, bytes).unwrap();
        let out = tmp.join(to);
        let st = std::process::Command::new("soffice")
            .args(["--headless", &format!("-env:UserInstallation=file://{}/profile", tmp.display()), "--convert-to", to, "--outdir"])
            .arg(&out)
            .arg(&src)
            .output()
            .unwrap();
        assert!(st.status.success(), "{st:?}");
        let data = std::fs::read(out.join(format!("v.{to}"))).unwrap();
        let (back, rep) = if to == "ods" { officina_convert::ods::read(&data).map(|(w, l)| (w, format!("{:?}", l))).unwrap() } else { officina_convert::xlsx::read(&data).map(|(w, r)| (w, format!("{r:?}"))).unwrap() };
        assert_eq!(all(&back), want, "{ext} -> LibreOffice -> {to}: {rep}");
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn excel_extension_validation_survives_normal_and_macro_enabled_save() {
    use officina_convert::xlsx;
    use officina_convert::zip::{Archive, ZipWriter};
    use officina_grid::macros::{MacroFamily, MacroPart, Macros};

    let mut wb = book();
    // Only standard rules in the source before inserting the Excel XML.
    let rules = wb.validations()[1..].to_vec();
    wb.replace_validations(rules);
    wb.set_value(CellAddr::new(1, 0, 1), Value::Num(2.0));
    wb.set_value(CellAddr::new(1, 1, 1), Value::Num(8.0));
    let bytes = xlsx::write(&wb, Vec::new()).unwrap();
    let archive = Archive::open(&bytes).unwrap();
    let mut source = ZipWriter::new(Vec::new());
    for part in &archive.entries {
        let mut data = archive.read(part).unwrap();
        if part.name == "xl/worksheets/sheet1.xml" {
            let xml = String::from_utf8(data).unwrap();
            data = xml.replace("</worksheet>", concat!(
                r#"<extLst><ext uri="{CCE6A557-97BC-4B89-ADB6-D9C93CAAB3DF}""#,
                r#" xmlns:v="http://schemas.microsoft.com/office/spreadsheetml/2009/9/main""#,
                r#" xmlns:m="http://schemas.microsoft.com/office/excel/2006/main">"#,
                r#"<v:dataValidations count="3">"#,
                r#"<v:dataValidation type="list" allowBlank="1" showInputMessage="1" showErrorMessage="1" promptTitle="Region" prompt="Pick a region" errorTitle="Unknown region" error="Use the list"><v:formula1><m:f>Lists!$A$1:$A$3</m:f></v:formula1><m:sqref>H1 H3</m:sqref></v:dataValidation>"#,
                r#"<v:dataValidation type="whole" operator="between" showErrorMessage="1"><v:formula1><m:f>Lists!$B$1</m:f></v:formula1><v:formula2><m:f>Lists!$B$2</m:f></v:formula2><m:sqref>I1:I2</m:sqref></v:dataValidation>"#,
                r#"<v:dataValidation type="whole" showErrorMessage="1"><v:formula1><m:f>0</m:f></v:formula1><m:sqref>J1</m:sqref></v:dataValidation>"#,
                "</v:dataValidations></ext></extLst></worksheet>"
            )).into_bytes();
        }
        source.add(&part.name, &data).unwrap();
    }
    let (mut wb, report) = xlsx::read(&source.finish().unwrap()).unwrap();
    assert_eq!(report.report.lines(), ["dropped\t1\tdata validation rules without their required formulas\t"]);
    assert!(wb.validation_at(CellAddr::new(0, 0, 9)).is_none());
    let check = |wb: &mut Workbook| {
        for row in [0, 2] {
            let a = CellAddr::new(0, row, 7);
            let list = wb.validation_at(a).unwrap();
            assert!(list.allow_blank && list.show_prompt && list.show_error && list.dropdown);
            assert_eq!(list.prompt, Message::new("Region", "Pick a region"));
            assert_eq!(list.error, Message::new("Unknown region", "Use the list"));
            assert_eq!(wb.list_items(a), ["North", "South", "East"].map(Value::str));
            wb.set_value(a, Value::str("West"));
            assert!(wb.check_cell(a).is_err());
            wb.set_value(a, Value::str("South"));
            assert!(wb.check_cell(a).is_ok());
        }
        let a = CellAddr::new(0, 1, 8);
        for (num, ok) in [(1.0, false), (2.0, true), (8.0, true), (9.0, false)] {
            wb.set_value(a, Value::Num(num));
            assert_eq!(wb.check_cell(a).is_ok(), ok);
        }
    };
    check(&mut wb);
    let wanted = all(&wb);
    wb.set_macros(Macros {
        family: MacroFamily::Ooxml,
        parts: vec![MacroPart {
            path: "xl/vbaProject.bin".into(),
            media_type: "application/vnd.ms-office.vbaProject".into(),
            data: vec![1, 2, 3].into(),
        }],
        workbook_code: None,
        sheet_codes: Vec::new(),
    }).unwrap();
    for (bytes, macro_enabled) in [(xlsx::write(&wb, Vec::new()).unwrap(), false), (xlsx::write_xlsm(&wb, Vec::new()).unwrap(), true)] {
        let archive = Archive::open(&bytes).unwrap();
        let xml = String::from_utf8(archive.read_name("xl/worksheets/sheet1.xml").unwrap().unwrap()).unwrap();
        assert!(xml.contains("<dataValidations count=\"6\">"));
        assert!(xml.contains("<x14:dataValidations count=\"2\">"));
        assert!(xml.contains("<xm:sqref>H1 H3</xm:sqref>"));
        assert!(xml.contains("<x14:formula2><xm:f>Lists!$B$2</xm:f></x14:formula2>"));
        let (mut back, loaded) = xlsx::read(&bytes).unwrap();
        assert_eq!(all(&back), wanted);
        assert_eq!(back.macros(), if macro_enabled { wb.macros() } else { None });
        assert_eq!(loaded.report.lines().len(), usize::from(macro_enabled));
        if macro_enabled {
            assert!(loaded.report.lines()[0].starts_with("inert\t"));
        }
        check(&mut back);
    }
}
