//! Hidden rows and columns through .xlsx and .ods and back, and (when
//! LibreOffice is installed) as LibreOffice reads our files and writes its own.

use officina_grid::column::Sizes;
use officina_grid::{CellAddr, Value, Workbook};

fn book() -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("Hidden").unwrap();
    for r in 0..8 {
        for c in 0..6 {
            wb.set_value(CellAddr::new(0, r, c), Value::Num((r * 10 + c) as f64));
        }
    }
    // Rows 3-4 and 7, columns B and D-E hidden; C wider.
    wb.sheet_mut(0).hidden = vec![(2, 3), (6, 6)];
    wb.sheet_mut(0).col_sizes = Sizes::from_overrides(64.0, vec![(1, 0.0), (2, 100.0), (3, 0.0), (4, 0.0)]);
    // B was 150 px wide before it was hidden; D and E had the default.
    wb.sheet_mut(0).hidden_widths = vec![(1, 150.0)];
    wb
}

/// Hidden rows, hidden columns, and columns shown.
fn all(wb: &Workbook) -> (Vec<(u32, u32)>, Vec<u32>, Vec<u32>) {
    let sh = wb.sheet(0);
    let cols = (0..6).filter(|&c| sh.col_sizes.size(c) <= 0.0).collect();
    let shown = (0..6).filter(|&c| sh.col_sizes.size(c) > 0.0).collect();
    (sh.hidden.clone(), cols, shown)
}

/// The width hidden column B will show again at, as wide (to within 2 px)
/// against shown column C as it was (files keep widths in characters or
/// inches, and LibreOffice measures characters a little wider).
fn b_kept(wb: &Workbook) -> bool {
    let sh = wb.sheet(0);
    let scale = sh.col_sizes.size(2) / 100.0;
    sh.hidden_widths.iter().any(|&(c, w)| c == 1 && (w - 150.0 * scale).abs() <= 2.0)
}

#[test]
fn hidden_rows_and_columns_round_trip() {
    let wb = book();
    let x = officina_convert::xlsx::write(&wb, Vec::new()).unwrap();
    let (back, rep) = officina_convert::xlsx::read(&x).unwrap();
    assert_eq!(all(&back), all(&wb));
    assert!(b_kept(&back), "{:?}", back.sheet(0).hidden_widths);
    assert!(!format!("{rep:?}").contains("hidden"), "{rep:?}");
    let o = officina_convert::ods::write(&wb, Vec::new()).unwrap();
    let (back, rep) = officina_convert::ods::read(&o).unwrap();
    assert_eq!(all(&back), all(&wb));
    assert!(b_kept(&back), "{:?}", back.sheet(0).hidden_widths);
    assert!(!format!("{rep:?}").contains("hidden"), "{rep:?}");
}

#[test]
fn libreoffice_keeps_our_hidden_rows_and_columns() {
    if !std::process::Command::new("soffice").arg("--version").output().is_ok_and(|o| o.status.success()) {
        eprintln!("skipped: no LibreOffice");
        return;
    }
    let wb = book();
    let tmp = std::env::temp_dir().join(format!("officina-hidden-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    for (ext, to, bytes) in [("xlsx", "ods", officina_convert::xlsx::write(&wb, Vec::new()).unwrap()), ("ods", "xlsx", officina_convert::ods::write(&wb, Vec::new()).unwrap())] {
        let src = tmp.join(format!("h.{ext}"));
        std::fs::write(&src, bytes).unwrap();
        let out = tmp.join(to);
        let st = std::process::Command::new("soffice")
            .args(["--headless", &format!("-env:UserInstallation=file://{}/profile", tmp.display()), "--convert-to", to, "--outdir"])
            .arg(&out)
            .arg(&src)
            .output()
            .unwrap();
        assert!(st.status.success(), "{st:?}");
        let data = std::fs::read(out.join(format!("h.{to}"))).unwrap();
        let back = if to == "ods" { officina_convert::ods::read(&data).unwrap().0 } else { officina_convert::xlsx::read(&data).unwrap().0 };
        assert_eq!(all(&back), all(&wb), "{ext} -> LibreOffice -> {to}");
        assert!(b_kept(&back), "{ext} -> LibreOffice -> {to}: {:?}", back.sheet(0).hidden_widths);
    }
    let _ = std::fs::remove_dir_all(&tmp);
}
