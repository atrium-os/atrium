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
    wb.validations()
        .iter()
        .map(|v| {
            let f = |e: &Option<std::sync::Arc<officina_grid::formula::Expr>>| e.as_ref().map(|e| wb.cond_formula_text(e, v.origin())).unwrap_or_default();
            let op = if matches!(v.allow, Allow::List | Allow::Custom) { CmpOp::Between } else { v.op };
            format!("{:?} {:?} {:?} {} {} blank={} dd={} {:?} {:?}/{:?} {:?}", v.ranges.iter().map(|r| (r.r0, r.c0, r.r1, r.c1)).collect::<Vec<_>>(), v.allow, op, f(&v.f1), f(&v.f2), v.allow_blank, v.dropdown, v.style, v.prompt, v.error, (v.show_prompt, v.show_error))
        })
        .collect()
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
