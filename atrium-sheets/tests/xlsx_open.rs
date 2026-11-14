//! Opening an `.xlsx` the way the app does: through the converter process.

use std::path::PathBuf;
use std::process::Command;

use atrium_sheets::files::load;
use officina_grid::addr::CellAddr;
use officina_grid::{Value, Workbook};

fn converter() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../officina-convert");
    let st = Command::new(env!("CARGO")).args(["build", "-q", "--bin", "officina-convert"]).current_dir(&dir).status().unwrap();
    assert!(st.success());
    let target = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or_else(|| dir.join("target"));
    target.join("debug/officina-convert")
}

#[test]
fn an_xlsx_opens_through_the_converter() {
    std::env::set_var("OFFICINA_CONVERT", converter());
    let mut wb = Workbook::new();
    wb.add_sheet("Q1").unwrap();
    for r in 0..100u32 {
        wb.set_value(CellAddr::new(0, r, 0), Value::Num(r as f64));
    }
    wb.fill_formula(0, 1, 0, 99, "=A1*A1").unwrap();
    wb.recalc();
    let file = officina_convert::xlsx::write(&wb, Vec::new()).unwrap();
    let d = load("q1.xlsx", &file).unwrap();
    assert_eq!(d.wb.get(CellAddr::new(0, 99, 1)), Value::Num(9801.0));
    assert_eq!(d.wb.formula_text(CellAddr::new(0, 9, 1)).as_deref(), Some("=A10*A10"));
    assert!(d.status.starts_with("Opened “q1.xlsx”"), "{}", d.status);
    // A damaged file is refused with a reason, and nothing crashes here.
    let e = load("bad.xlsx", &file[..file.len() / 2]).err().unwrap();
    assert!(e.contains("couldn't be opened"), "{e}");
}
