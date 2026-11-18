//! Recalculation in slices: each call stops near its budget, cells not yet done
//! say so, edits in between join the work, and the end is what one call gives.
use officina_grid::{CellAddr, Value, Workbook};
use std::time::{Duration, Instant};

fn book(rows: u32) -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("Sheet1").unwrap();
    let a: Vec<f64> = (0..rows).map(|r| (r % 97) as f64).collect();
    wb.set_numbers(0, 0, 0, &a);
    let b: Vec<f64> = (0..rows).map(|r| (r % 13) as f64).collect();
    wb.set_numbers(0, 1, 0, &b);
    wb.set_value(CellAddr::new(0, 0, 5), Value::Num(1.0));
    // Slow on purpose: each row walks column A (a range criterion isn't indexed).
    wb.fill_formula(0, 2, 0, rows - 1, &format!("=SUMIF($A$1:$A${rows},\">\"&A1,$B$1:$B${rows})*$F$1")).unwrap();
    // And a cheap one reading the slow one.
    wb.fill_formula(0, 3, 0, rows - 1, "=C1+1").unwrap();
    wb
}

fn values(wb: &Workbook, rows: u32) -> Vec<Value> {
    (0..rows).flat_map(|r| [wb.value(0, r, 2), wb.value(0, r, 3)]).collect()
}

#[test]
fn sliced_recalc_stops_in_time_and_ends_where_one_call_does() {
    let rows = 20_000;
    let mut want = book(rows);
    want.recalc();
    want.set_value(CellAddr::new(0, 0, 5), Value::Num(3.0));
    want.set_value(CellAddr::new(0, 7, 0), Value::Num(50.0));
    want.recalc();

    let mut wb = book(rows);
    wb.recalc();
    wb.set_value(CellAddr::new(0, 0, 5), Value::Num(3.0));
    let budget = Duration::from_millis(8);
    let mut calls = 0;
    let mut worst = Duration::ZERO;
    let mut saw_stale = false;
    loop {
        let t = Instant::now();
        let (_, done) = wb.recalc_for(budget);
        worst = worst.max(t.elapsed());
        calls += 1;
        if calls == 2 {
            // An edit while work is pending.
            wb.set_value(CellAddr::new(0, 7, 0), Value::Num(50.0));
        }
        if done && !wb.recalc_pending() {
            break;
        }
        saw_stale |= (0..rows).step_by(500).any(|r| wb.is_stale(0, r, 2) || wb.is_stale(0, r, 3));
        assert!(calls < 100_000);
    }
    eprintln!("{calls} calls, worst {worst:?}");
    assert!(calls > 3, "the work should take several slices");
    assert!(saw_stale);
    assert!(worst < budget * 4, "a call held {worst:?}");
    assert!(!wb.is_stale(0, 5, 2));
    assert_eq!(values(&wb, rows), values(&want, rows));
}
