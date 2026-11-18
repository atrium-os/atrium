//! Merged cells through .xlsx and .ods and back, and (when LibreOffice is
//! installed) as LibreOffice reads our files and writes its own.

use officina_grid::addr::Rect;
use officina_grid::{CellAddr, Value, Workbook};

fn r(sheet: u16, r0: u32, c0: u32, r1: u32, c1: u32) -> Rect {
    Rect { sheet, r0, c0, r1, c1 }
}

fn book() -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("One").unwrap();
    wb.add_sheet("Two").unwrap();
    wb.set_value(CellAddr::new(0, 0, 0), Value::str("Quarterly report"));
    wb.set_value(CellAddr::new(0, 2, 1), Value::Num(42.0));
    wb.set_value(CellAddr::new(0, 9, 3), Value::str("after"));
    // A title across A1:D1, an empty block B5:C7, a value in B3:B4, one on sheet 2.
    wb.set_merges(0, vec![r(0, 0, 0, 0, 3), r(0, 4, 1, 6, 2), r(0, 2, 1, 3, 1)]);
    wb.set_merges(1, vec![r(1, 10, 2, 10, 5)]);
    wb
}

fn merges(wb: &Workbook) -> Vec<Vec<Rect>> {
    (0..wb.sheet_count() as u16).map(|s| wb.merges(s).to_vec()).collect()
}

#[test]
fn merged_cells_round_trip_through_xlsx_and_ods() {
    let wb = book();
    let want = merges(&wb);
    let x = officina_convert::xlsx::write(&wb, Vec::new()).unwrap();
    let (back, rep) = officina_convert::xlsx::read(&x).unwrap();
    assert_eq!(merges(&back), want);
    assert!(!format!("{rep:?}").contains("merged"), "{rep:?}");
    let o = officina_convert::ods::write(&wb, Vec::new()).unwrap();
    let (back, rep) = officina_convert::ods::read(&o).unwrap();
    assert_eq!(merges(&back), want);
    assert!(!format!("{rep:?}").contains("merged"), "{rep:?}");
    assert_eq!(back.value(0, 0, 0), Value::str("Quarterly report"));
    assert_eq!(back.value(0, 2, 1), Value::Num(42.0));
    assert_eq!(back.value(0, 9, 3), Value::str("after"));
}

/// LibreOffice opens our file and saves it as the other format; we read that.
#[test]
fn libreoffice_keeps_our_merged_cells() {
    if !has_soffice() {
        eprintln!("skipped: no LibreOffice");
        return;
    }
    let wb = book();
    let want = merges(&wb);
    let tmp = std::env::temp_dir().join(format!("officina-merges-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    for (ext, to, bytes) in [("xlsx", "ods", officina_convert::xlsx::write(&wb, Vec::new()).unwrap()), ("ods", "xlsx", officina_convert::ods::write(&wb, Vec::new()).unwrap())] {
        let src = tmp.join(format!("m.{ext}"));
        std::fs::write(&src, bytes).unwrap();
        let out = tmp.join(to);
        let st = std::process::Command::new("soffice")
            .args(["--headless", &format!("-env:UserInstallation=file://{}/profile", tmp.display()), "--convert-to", to, "--outdir"])
            .arg(&out)
            .arg(&src)
            .output()
            .unwrap();
        assert!(st.status.success(), "{st:?}");
        let data = std::fs::read(out.join(format!("m.{to}"))).unwrap();
        let back = if to == "ods" { officina_convert::ods::read(&data).unwrap().0 } else { officina_convert::xlsx::read(&data).unwrap().0 };
        assert_eq!(merges(&back), want, "{ext} -> LibreOffice -> {to}");
        assert_eq!(back.value(0, 9, 3), Value::str("after"), "{ext} -> {to}");
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

fn has_soffice() -> bool {
    std::process::Command::new("soffice").arg("--version").output().is_ok_and(|o| o.status.success())
}
