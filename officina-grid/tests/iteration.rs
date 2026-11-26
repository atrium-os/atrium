use officina_core::history::History;
use officina_grid::book::Iteration;
use officina_grid::ops::GridOp;
use officina_grid::value::ErrorKind;
use officina_grid::{CellAddr, Value, Workbook};

fn book() -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("S").unwrap();
    wb
}
fn a(r: u32, c: u32) -> CellAddr {
    CellAddr::new(0, r, c)
}
fn num(wb: &Workbook, at: CellAddr) -> f64 {
    match wb.get(at) {
        Value::Num(x) => x,
        other => panic!("{at:?} is {other:?}"),
    }
}

#[test]
fn a_cycle_is_circ_until_iteration_turns_on_then_converges() {
    let mut wb = book();
    // x = x / 2 + 1 through two cells: settles at 2.
    wb.set_formula(a(0, 0), "=B1/2+1").unwrap();
    wb.set_formula(a(0, 1), "=A1").unwrap();
    wb.set_formula(a(0, 2), "=A1*10").unwrap();
    wb.recalc();
    assert_eq!(wb.get(a(0, 0)), Value::Err(ErrorKind::Circ));
    assert_eq!(wb.get(a(0, 2)), Value::Err(ErrorKind::Circ));

    let mut h: History<GridOp> = History::new();
    let op = GridOp::decode(&GridOp::Iteration(Some(Iteration::DEFAULT)).encode()).unwrap();
    h.run(&mut wb, "Iterative Calculation", vec![op]);
    wb.recalc();
    assert!((num(&wb, a(0, 0)) - 2.0).abs() <= 0.002, "{:?}", wb.get(a(0, 0)));
    assert!((num(&wb, a(0, 2)) - 20.0).abs() <= 0.02, "the cell after the cycle reads its result");

    // Saved and loaded with the workbook.
    let back = officina_grid::native::load(&officina_grid::native::save_to_vec(&wb)).unwrap();
    assert_eq!(back.iteration(), Some(Iteration::DEFAULT));

    h.undo(&mut wb);
    wb.recalc();
    assert_eq!(wb.iteration(), None);
    assert_eq!(wb.get(a(0, 0)), Value::Err(ErrorKind::Circ));
}

#[test]
fn a_counter_runs_the_set_number_of_times_each_recalculation() {
    let mut wb = book();
    assert!(wb.set_iteration(Some(Iteration { count: 7, change: 0.001 })));
    wb.set_formula(a(0, 0), "=A1+1").unwrap();
    wb.recalc();
    assert_eq!(num(&wb, a(0, 0)), 7.0);
    wb.mark_all(None);
    wb.recalc();
    assert_eq!(num(&wb, a(0, 0)), 14.0, "the next recalculation starts from the last value");
    assert!(!wb.set_iteration(Some(Iteration { count: 0, change: 0.001 })));
    assert!(!wb.set_iteration(Some(Iteration { count: 5, change: f64::NAN })));
    assert!(GridOp::decode(&[28, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).is_err());
}
