//! Frozen panes through .xlsx and .ods and back, and (when LibreOffice is
//! installed) as LibreOffice reads our files and writes its own.

use officina_grid::{CellAddr, Value, Workbook};

fn book() -> Workbook {
    let mut wb = Workbook::new();
    for n in ["Rows", "Both", "Cols", "None", "Scrolled"] {
        wb.add_sheet(n).unwrap();
        wb.set_value(CellAddr::new(wb.sheet_count() as u16 - 1, 0, 0), Value::str(n));
    }
    wb.sheet_mut(0).freeze = (1, 0);
    wb.sheet_mut(1).freeze = (2, 3);
    wb.sheet_mut(2).freeze = (0, 1);
    // Frozen with the view at row 11 and column B: rows 11-12, column B.
    wb.sheet_mut(4).freeze = (12, 2);
    wb.sheet_mut(4).freeze_from = (10, 1);
    wb
}

fn all(wb: &Workbook) -> Vec<((u32, u32), (u32, u32))> {
    (0..wb.sheet_count() as u16).map(|s| (wb.sheet(s).freeze, wb.sheet(s).freeze_from)).collect()
}

#[test]
fn frozen_panes_round_trip_through_xlsx_and_ods() {
    let wb = book();
    let x = officina_convert::xlsx::write(&wb, Vec::new()).unwrap();
    let (back, rep) = officina_convert::xlsx::read(&x).unwrap();
    assert_eq!(all(&back), all(&wb));
    assert!(!format!("{rep:?}").contains("panes"), "{rep:?}");
    let o = officina_convert::ods::write(&wb, Vec::new()).unwrap();
    let (back, _) = officina_convert::ods::read(&o).unwrap();
    assert_eq!(all(&back), all(&wb));
}

#[test]
fn each_sheets_zoom_round_trips_through_xlsx_ods_and_the_native_file() {
    let mut wb = book();
    wb.sheet_mut(1).zoom = 150;
    wb.sheet_mut(3).zoom = 75;
    let zooms = |wb: &Workbook| (0..wb.sheet_count() as u16).map(|s| wb.sheet(s).zoom).collect::<Vec<_>>();
    let x = officina_convert::xlsx::write(&wb, Vec::new()).unwrap();
    let (back, _) = officina_convert::xlsx::read(&x).unwrap();
    assert_eq!(zooms(&back), vec![100, 150, 100, 75, 100]);
    assert_eq!(all(&back), all(&wb), "frozen panes stay with a zoom");
    let o = officina_convert::ods::write(&wb, Vec::new()).unwrap();
    let (back, _) = officina_convert::ods::read(&o).unwrap();
    assert_eq!(zooms(&back), vec![100, 150, 100, 75, 100]);
    assert_eq!(all(&back), all(&wb), "frozen panes stay with a zoom in .ods");
    let n = officina_grid::native::save_to_vec(&wb);
    let back = officina_grid::native::load(&n).unwrap();
    assert_eq!(zooms(&back), vec![100, 150, 100, 75, 100]);
}

/// LibreOffice opens our file with a view (tests/lo_view.py, under Xvfb)
/// and saves it as the other format; we read that. Headless conversion has
/// no view and drops view settings, so this needs Xvfb and Python's `uno`.
#[test]
fn libreoffice_keeps_our_frozen_panes() {
    let have = |cmd: &str, args: &[&str]| std::process::Command::new(cmd).args(args).output().is_ok_and(|o| o.status.success());
    if !have("soffice", &["--version"]) || !have("xvfb-run", &["-a", "true"]) || !have("python3", &["-c", "import uno"]) {
        eprintln!("skipped: no LibreOffice, Xvfb or Python uno");
        return;
    }
    let wb = book();
    let tmp = std::env::temp_dir().join(format!("officina-freeze-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/lo_view.py");
    for (i, (ext, to, filter, bytes)) in [
        ("xlsx", "ods", "calc8", officina_convert::xlsx::write(&wb, Vec::new()).unwrap()),
        ("ods", "xlsx", "Calc MS Excel 2007 XML", officina_convert::ods::write(&wb, Vec::new()).unwrap()),
    ]
    .into_iter()
    .enumerate()
    {
        let src = tmp.join(format!("f.{ext}"));
        let dst = tmp.join(format!("back.{to}"));
        std::fs::write(&src, bytes).unwrap();
        let port = (20000 + (std::process::id() % 20000) * 2 + i as u32).to_string();
        let st = std::process::Command::new("xvfb-run")
            .args(["-a", "python3"])
            .arg(&script)
            .args([src.to_str().unwrap(), dst.to_str().unwrap(), filter, tmp.join("profile").to_str().unwrap(), &port])
            .output()
            .unwrap();
        assert!(st.status.success(), "{st:?}");
        let data = std::fs::read(&dst).unwrap();
        let back = if to == "ods" { officina_convert::ods::read(&data).unwrap().0 } else { officina_convert::xlsx::read(&data).unwrap().0 };
        assert_eq!(all(&back), all(&wb), "{ext} -> LibreOffice -> {to}");
    }
    let _ = std::fs::remove_dir_all(&tmp);
}
