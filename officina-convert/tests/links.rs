//! Hyperlinks through .xlsx and .ods and back, and through LibreOffice.

use officina_grid::links::Link;
use officina_grid::{CellAddr, Value, Workbook};

fn book() -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("One").unwrap();
    wb.add_sheet("Two").unwrap();
    for (r, c, t) in [(0, 0, "Site"), (1, 0, "Mail us"), (2, 1, "See totals"), (3, 0, "Search")] {
        wb.set_value(CellAddr::new(0, r, c), Value::str(t));
    }
    wb.set_links(0, vec![
        (0, 0, Link::new("https://example.org/")),
        (1, 0, Link::new("mailto:team@example.org")),
        (2, 1, Link::new("#Two!B4")),
        (3, 0, Link::new("https://example.org/find?q=a&b=c")),
    ]);
    wb
}

fn all(wb: &Workbook) -> Vec<(u32, u32, String)> {
    wb.links(0).iter().map(|(&(r, c), l)| (r, c, l.target.to_string())).collect()
}

#[test]
fn links_round_trip_through_xlsx_and_ods() {
    let wb = book();
    let want = all(&wb);
    let x = officina_convert::xlsx::write(&wb, Vec::new()).unwrap();
    let (back, rep) = officina_convert::xlsx::read(&x).unwrap();
    assert_eq!(all(&back), want);
    assert!(!format!("{rep:?}").contains("hyperlink"), "{rep:?}");
    let o = officina_convert::ods::write(&wb, Vec::new()).unwrap();
    let (back, _) = officina_convert::ods::read(&o).unwrap();
    assert_eq!(all(&back), want);
    assert_eq!(back.value(0, 2, 1), Value::str("See totals"));
    assert_eq!(back.link(0, 2, 1).unwrap().place(&back, 0), Some(CellAddr::new(1, 3, 1)));
}

#[test]
fn libreoffice_keeps_our_links() {
    if !std::process::Command::new("soffice").arg("--version").output().is_ok_and(|o| o.status.success()) {
        eprintln!("skipped: no LibreOffice");
        return;
    }
    let wb = book();
    let want = all(&wb);
    let tmp = std::env::temp_dir().join(format!("officina-links-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    for (ext, to, bytes) in [("xlsx", "ods", officina_convert::xlsx::write(&wb, Vec::new()).unwrap()), ("ods", "xlsx", officina_convert::ods::write(&wb, Vec::new()).unwrap())] {
        let src = tmp.join(format!("l.{ext}"));
        std::fs::write(&src, bytes).unwrap();
        let out = tmp.join(to);
        let st = std::process::Command::new("soffice")
            .args(["--headless", &format!("-env:UserInstallation=file://{}/profile", tmp.display()), "--convert-to", to, "--outdir"])
            .arg(&out)
            .arg(&src)
            .output()
            .unwrap();
        assert!(st.status.success(), "{st:?}");
        let data = std::fs::read(out.join(format!("l.{to}"))).unwrap();
        let back = if to == "ods" { officina_convert::ods::read(&data).unwrap().0 } else { officina_convert::xlsx::read(&data).unwrap().0 };
        assert_eq!(all(&back), want, "{ext} -> LibreOffice -> {to}");
    }
    let _ = std::fs::remove_dir_all(&tmp);
}
