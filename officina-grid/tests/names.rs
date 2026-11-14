//! Defined names: evaluation, dependencies through names, redefinition, scope, cycles,
//! and the native file.

use officina_grid::{CellAddr, Value, Workbook};

fn a(s: u16, cell: &str) -> CellAddr {
    let (r, c) = officina_grid::addr::parse_a1(cell).unwrap();
    CellAddr::new(s, r, c)
}

fn book() -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("Data").unwrap();
    wb.add_sheet("Calc").unwrap();
    for i in 0..10 {
        wb.set_value(a(0, &format!("A{}", i + 1)), Value::Num(i as f64 + 1.0));
    }
    wb.define_name("Sales", None, "=Data!$A$1:$A$10").unwrap();
    wb.define_name("Rate", None, "=0.2").unwrap();
    wb.define_name("Taxed", None, "=SUM(Sales)*(1+Rate)").unwrap();
    wb.set_formula(a(1, "A1"), "=SUM(Sales)").unwrap();
    wb.set_formula(a(1, "A2"), "=Taxed").unwrap();
    wb.set_formula(a(1, "A3"), "=Nope+1").unwrap();
    wb.recalc();
    wb
}

#[test]
fn names_evaluate_and_track_their_cells() {
    let mut wb = book();
    assert_eq!(wb.get(a(1, "A1")), Value::Num(55.0));
    assert_eq!(wb.get(a(1, "A2")), Value::Num(66.0));
    assert_eq!(wb.get(a(1, "A3")).display(), "#NAME?");
    // A cell the name covers changes: formulas using it recalculate.
    wb.set_value(a(0, "A10"), Value::Num(20.0));
    wb.recalc();
    assert_eq!(wb.get(a(1, "A1")), Value::Num(65.0));
    assert_eq!(wb.get(a(1, "A2")), Value::Num(78.0));
    // Redefining a name, and defining a missing one.
    wb.define_name("Rate", None, "=0.5").unwrap();
    wb.define_name("Nope", None, "=Data!$A$2").unwrap();
    wb.recalc();
    assert_eq!(wb.get(a(1, "A2")), Value::Num(97.5));
    assert_eq!(wb.get(a(1, "A3")), Value::Num(3.0));
    // Removing it.
    wb.remove_name("Nope", None).unwrap();
    wb.recalc();
    assert_eq!(wb.get(a(1, "A3")).display(), "#NAME?");
    // Now the redefined range's new cells count.
    wb.define_name("Sales", None, "=Data!$A$1:$A$2").unwrap();
    wb.recalc();
    assert_eq!(wb.get(a(1, "A1")), Value::Num(3.0));
    wb.set_value(a(0, "A2"), Value::Num(10.0));
    wb.recalc();
    assert_eq!(wb.get(a(1, "A1")), Value::Num(11.0));
}

#[test]
fn sheet_scope_cycles_and_relative_names() {
    let mut wb = book();
    wb.define_name("Rate", Some(1), "=1").unwrap();
    wb.set_formula(a(0, "B1"), "=Rate").unwrap();
    wb.recalc();
    assert_eq!(wb.get(a(1, "A2")), Value::Num(110.0), "Calc sees its own Rate");
    assert_eq!(wb.get(a(0, "B1")), Value::Num(0.2), "Data sees the workbook's");
    // A cycle of names is #NAME?, not a hang.
    wb.define_name("Ping", None, "=Pong+1").unwrap();
    wb.define_name("Pong", None, "=Ping+1").unwrap();
    wb.set_formula(a(1, "B1"), "=Ping").unwrap();
    wb.recalc();
    assert_eq!(wb.get(a(1, "B1")).display(), "#NAME?");
    // Relative: written as if in A1, so =B1 is "the cell to the right".
    wb.define_name("Right", None, "=B1").unwrap();
    wb.fill_formula(0, 2, 0, 9, "=Right*2").unwrap();
    wb.recalc();
    assert_eq!(wb.get(a(0, "C5")), Value::Num(0.0), "D5 is empty");
    wb.set_value(a(0, "D5"), Value::Num(5.0));
    wb.recalc();
    assert_eq!(wb.get(a(0, "C5")), Value::Num(10.0), "D5 through the name");
    assert!(wb.define_name("A1", None, "=1").is_err());
    assert!(wb.define_name("bad name", None, "=1").is_err());
}

#[test]
fn names_in_the_native_file() {
    let mut wb = book();
    wb.define_name("Local", Some(1), "=Data!$A$3*2").unwrap();
    let file = officina_grid::native::save_to_vec(&wb);
    let mut back = officina_grid::native::load(&file).unwrap();
    assert_eq!(back.names().len(), 4);
    assert_eq!(back.name_text(back.names().get(1, "local").unwrap()), "=Data!$A$3*2");
    assert_eq!(back.names().get(0, "Local"), None);
    back.set_value(a(0, "A1"), Value::Num(101.0));
    back.recalc();
    assert_eq!(back.get(a(1, "A1")), Value::Num(155.0));
}
