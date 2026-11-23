//! Excel 97–2003 .xls, plain and protected; protected .xlsx and .ods, saved here and opened through the converter
//! process, as the app does; a protected sheet's journal and restore.

use std::path::PathBuf;
use std::process::Command;

use atrium_sheets::doc::{self, Doc};
use atrium_sheets::files::load_with;
use officina_convert::launch::{protect, ConvertError};
use officina_grid::addr::CellAddr;
use officina_grid::Value;

fn converter() -> PathBuf {
    // The FreeBSD guest runs the worker cross-built and staged by the host.
    if let Some(staged) = std::env::var_os("OFFICINA_CONVERT") {
        return staged.into();
    }
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../officina-convert");
    let st = Command::new(env!("CARGO")).args(["build", "-q", "--release", "--bin", "officina-convert"]).current_dir(&dir).status().unwrap();
    assert!(st.success());
    let target = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or_else(|| dir.join("target"));
    target.join("release/officina-convert")
}

#[test]
fn protected_sheets_open_only_with_their_password() {
    std::env::set_var("OFFICINA_CONVERT", converter());
    let (d, _) = Doc::from_csv("q.csv", b"item,price\nsecret widget,12\n").unwrap();
    for (name, kind, file) in [("q.xlsx", "xlsx", officina_convert::xlsx::write(&d.wb, Vec::new()).unwrap()), ("q.ods", "ods", officina_convert::ods::write(&d.wb, Vec::new()).unwrap())] {
        let file = protect(kind, &file, "pa55").unwrap();
        assert!(!file.windows(6).any(|w| w == b"widget"), "{name}");
        assert_eq!(load_with(name, &file, None).err(), Some(ConvertError::NeedsPassword), "{name}");
        assert_eq!(load_with(name, &file, Some("pass")).err(), Some(ConvertError::WrongPassword), "{name}");
        let back = load_with(name, &file, Some("pa55")).unwrap();
        assert_eq!(back.wb.get(CellAddr::new(0, 1, 1)), Value::Num(12.0), "{name}");
        assert!(back.protection.is_some());
    }
}

#[test]
fn xls_files_open_through_the_converter() {
    std::env::set_var("OFFICINA_CONVERT", converter());
    let plain = officina_convert::xls::sample::workbook(None);
    let d = load_with("old.xls", &plain, None).unwrap();
    assert_eq!(d.wb.get(CellAddr::new(0, 0, 1)), Value::str("hello"));
    assert_eq!(d.wb.get(CellAddr::new(0, 0, 2)), Value::Num(42.0));
    assert!(d.protection.is_none());
    let locked = officina_convert::xls::sample::workbook(Some("pa55"));
    assert_eq!(load_with("old.xls", &locked, None).err(), Some(ConvertError::NeedsPassword));
    assert_eq!(load_with("old.xls", &locked, Some("pass")).err(), Some(ConvertError::WrongPassword));
    let back = load_with("old.xls", &locked, Some("pa55")).unwrap();
    assert_eq!(back.wb.formula_text(CellAddr::new(0, 0, 2)).as_deref(), Some("=A1*21"));
    assert_eq!(atrium_sheets::files::format_of("Old.XLS"), atrium_sheets::files::Format::Xls);
}

#[test]
fn a_protected_sheet_keeps_nothing_in_the_clear() {
    let dir = std::env::temp_dir().join(format!("atrium-sheets-protected-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (mut d, _) = Doc::from_csv("q.csv", b"item\nsecret widget\n").unwrap();
    d.autosave_to(&dir).unwrap();
    d.set_password(Some("pa55")).unwrap();
    d.type_into(2, 0, "another gadget");
    d.autosave().unwrap().sync().unwrap();
    for e in std::fs::read_dir(&dir).unwrap().flatten() {
        let b = std::fs::read(e.path()).unwrap();
        assert!(!b.windows(6).any(|w| w == b"widget" || w == b"gadget"), "{:?}", e.path());
    }
    let native = d.stored_native().unwrap();
    assert_eq!(load_with("q.osheet", &native, Some("nope")).err(), Some(ConvertError::WrongPassword));
    assert_eq!(load_with("q.osheet", &native, Some("pa55")).unwrap().wb.get(CellAddr::new(0, 2, 0)), Value::str("another gadget"));
    // A new process restores it with the password (the lock goes with this
    // one's handle, so restore from a copy).
    std::mem::forget(d);
    let copy = dir.with_extension("copy");
    let _ = std::fs::remove_dir_all(&copy);
    std::fs::create_dir_all(&copy).unwrap();
    for e in std::fs::read_dir(&dir).unwrap().flatten() {
        if !e.file_name().to_string_lossy().contains("lock") {
            std::fs::copy(e.path(), copy.join(e.file_name())).unwrap();
        }
    }
    assert!(doc::recovery_locked(&copy));
    assert_eq!(doc::recover(&copy).err().unwrap().kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(doc::recover_with(&copy, Some("x")).err().unwrap().kind(), std::io::ErrorKind::PermissionDenied);
    let (r, rec) = doc::recover_with(&copy, Some("pa55")).unwrap();
    assert_eq!(r.wb.get(CellAddr::new(0, 2, 0)), Value::str("another gadget"));
    assert_eq!(rec.actions, 1);
    r.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
