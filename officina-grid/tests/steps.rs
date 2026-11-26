//! Evaluate Formula's steps (docs/spec/officina.md §16.142).

use officina_grid::eval::steps::evaluate_steps;
use officina_grid::{CellAddr, Workbook};

fn book() -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("S").unwrap();
    for (r, c, t) in [(0, 0, "2"), (1, 0, "3"), (2, 0, "4"), (0, 1, "10")] {
        wb.set_input(CellAddr::new(0, r, c), t).unwrap();
    }
    wb
}

fn steps(wb: &mut Workbook, f: &str) -> Vec<String> {
    let at = CellAddr::new(0, 5, 5);
    wb.set_formula(at, f).unwrap();
    wb.recalc();
    evaluate_steps(wb, at).unwrap()
}

#[test]
fn a_formula_is_worked_out_innermost_part_first() {
    let mut wb = book();
    assert_eq!(steps(&mut wb, "=A1+B1*2"), ["=A1+B1*2", "=2+B1*2", "=2+10*2", "=2+20", "=22"]);
    assert_eq!(steps(&mut wb, "=SUM(A1:A3)/(B1-6)"), ["=SUM(A1:A3)/(B1-6)", "=SUM({2;3;4})/(B1-6)", "=9/(B1-6)", "=9/(10-6)", "=9/4", "=2.25"]);
    // IF goes whole (it evaluates only one branch), as do reference readers.
    assert_eq!(steps(&mut wb, "=IF(A1>1,\"big\",1/0)&\"!\""), ["=IF(A1>1,\"big\",1/0)&\"!\"", "=\"big\"&\"!\"", "=\"big!\""]);
    assert_eq!(steps(&mut wb, "=ROW(A3)+1"), ["=ROW(A3)+1", "=3+1", "=4"]);
    // An empty cell isn't replaced by a value; its parent goes whole.
    assert_eq!(steps(&mut wb, "=C1&\"x\""), ["=C1&\"x\"", "=\"x\""]);
    // Errors flow through as values.
    assert_eq!(steps(&mut wb, "=A1/0+1"), ["=A1/0+1", "=2/0+1", "=#DIV/0!+1", "=#DIV/0!"]);
    // No formula: none.
    assert!(evaluate_steps(&wb, CellAddr::new(0, 0, 0)).is_none());
}

#[test]
fn every_formula_ends_on_its_cells_value() {
    let mut wb = book();
    for f in ["=AVERAGE(A1:A3)*2", "=TEXT(A1,\"0.00\")&LEN(\"abc\")", "=MAX(A1:B3)-MIN(A1:A3)", "=SUMPRODUCT(A1:A3,A1:A3)", "=XLOOKUP(3,A1:A3,A1:A3)+1"] {
        let s = steps(&mut wb, f);
        let shown = wb.formatted(CellAddr::new(0, 5, 5)).text;
        let last = s.last().unwrap().trim_start_matches('=').trim_matches('"').to_string();
        assert_eq!(last, shown, "{f}: {s:?}");
    }
}
