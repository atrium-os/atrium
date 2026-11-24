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

#[test]
fn a_workbook_past_the_budget_is_refused_while_loading() {
    use officina_grid::{Value, Workbook};
    let mut wb = Workbook::new();
    wb.add_sheet("Sparse").unwrap();
    // 300 columns of 8,192 numbers: about 20 MB open.
    for c in 0..300 {
        wb.set_numbers(0, c, 0, &vec![1.0; 8192]);
    }
    let file = officina_grid::native::save_to_vec(&wb);
    let full = wb.memory_bytes();
    assert!(full > 16 << 20, "{full} in memory");
    match officina_grid::native::load_within(&file, 4 << 20) {
        Err(officina_grid::native::LoadError::TooBig(b)) => assert!(b > 4 << 20 && b < full),
        other => panic!("{:?}", other.map(|w| w.memory_bytes())),
    }
    let back = officina_grid::native::load_within(&file, full + (1 << 20)).unwrap();
    assert_eq!(back.value(0, 8191, 299), Value::Num(1.0));
}

#[test]
fn sliced_groups_preserve_disjoint_dirty_runs_and_count_completed_cells() {
    const ROWS: u32 = 131_072;
    const STARTS: [u32; 3] = [1, 40_000, 105_000];
    const WRITES: u32 = 20_000;
    let mut wb = Workbook::new();
    wb.add_sheet("Sheet1").unwrap();
    wb.threads = 4;
    wb.set_numbers(0, 0, 0, &vec![1.0; ROWS as usize]);
    wb.fill_formula(0, 1, 0, ROWS - 1, "=ROUND(A1/3,2)").unwrap();
    wb.recalc();
    for start in STARTS {
        wb.set_numbers(0, 0, start, &vec![6.0; WRITES as usize]);
    }

    let mut computed = 0;
    for _ in 0..10_000 {
        if !wb.recalc_pending() { break; }
        computed += wb.recalc_for(Duration::from_millis(2)).0.cells;
    }
    assert!(!wb.recalc_pending());
    assert_eq!(computed, u64::from(WRITES) * STARTS.len() as u64);
    for row in 0..ROWS {
        let changed = STARTS.iter().any(|&start| (start..start + WRITES).contains(&row));
        assert_eq!(wb.value(0, row, 1), Value::Num(if changed { 2.0 } else { 0.33 }), "row {row}");
    }
}

#[test]
fn visible_rows_and_cross_sheet_precedents_advance_before_the_background() {
    use officina_grid::Rect;
    const ROWS: u32 = 350_000;
    let mut wb = Workbook::new();
    wb.add_sheet("Source").unwrap();
    wb.add_sheet("Report").unwrap();
    wb.set_value(CellAddr::new(0, 0, 0), Value::Num(1.0));
    wb.fill_formula(0, 1, 0, ROWS - 1, "=ROUND(ROW()/3,2)*$A$1").unwrap();
    wb.fill_formula(1, 0, 0, ROWS - 3, "=Source!B3+1").unwrap();
    let total = CellAddr::new(1, 0, 2);
    wb.set_formula(total, "=SUM(Source!B1:B350000)").unwrap();
    wb.recalc();
    let old_total = wb.get(total);
    wb.set_value(CellAddr::new(0, 0, 0), Value::Num(7.0));

    let budget = Duration::from_millis(8);
    let target = |row| Rect { sheet: 1, r0: row, c0: 0, r1: row + 15, c1: 0 };
    for row in [240_000, 310_000] {
        for _ in 0..6 {
            wb.recalc_for_view(budget, &[target(row), Rect::cell(total)]);
            if !wb.is_stale(1, row + 15, 0) { break; }
        }
        assert!(wb.is_stale(1, 160_000, 0), "the earlier offscreen row should stay pending");
        assert!(wb.is_stale(1, 0, 2), "the wide aggregate must wait for all of its inputs");
        assert_eq!(wb.get(total), old_total);
        for r in row..row + 16 {
            let want = ((f64::from(r + 3) / 3.0 * 100.0).round() / 100.0) * 7.0;
            assert!(!wb.is_stale(0, r + 2, 1), "cross-sheet precedent at {r}");
            assert!(!wb.is_stale(1, r, 0), "visible cell at {r}");
            assert_eq!(wb.value(0, r + 2, 1), Value::Num(want));
            assert_eq!(wb.value(1, r, 0), Value::Num(want + 1.0));
        }
    }
    wb.recalc();
    assert!(!wb.recalc_pending());
    let (Value::Num(before), Value::Num(after)) = (old_total, wb.get(total)) else { panic!() };
    assert!((after - before * 7.0).abs() < 1e-3);
}

#[test]
fn visible_priority_preserves_running_totals_in_both_directions_and_cycles() {
    use officina_grid::{ErrorKind, Rect};
    const ROWS: u32 = 512;
    let mut wb = Workbook::new();
    wb.add_sheet("Ordered").unwrap();
    wb.set_value(CellAddr::new(0, 0, 0), Value::Num(1.0));
    wb.set_formula(CellAddr::new(0, 0, 1), "=$A$1").unwrap();
    wb.fill_formula(0, 1, 1, ROWS - 1, "=B1+$A$1").unwrap();
    wb.set_formula(CellAddr::new(0, ROWS - 1, 2), "=$A$1").unwrap();
    wb.fill_formula(0, 2, 0, ROWS - 2, "=C2+$A$1").unwrap();
    wb.fill_formula(0, 3, 0, ROWS - 1, "=B1-C1").unwrap();
    wb.set_formula(CellAddr::new(0, 4, 4), "=F5+$A$1").unwrap();
    wb.set_formula(CellAddr::new(0, 4, 5), "=E5+1").unwrap();
    wb.recalc();
    wb.set_value(CellAddr::new(0, 0, 0), Value::Num(3.0));

    let view = [Rect { sheet: 0, r0: 400, c0: 1, r1: 415, c1: 3 }, Rect { sheet: 0, r0: 4, c0: 4, r1: 4, c1: 5 }];
    for _ in 0..100 {
        wb.recalc_for_view(Duration::from_millis(8), &view);
        for row in 400..=415 {
            if !wb.is_stale(0, row, 3) {
                assert_eq!(wb.value(0, row, 3), Value::Num(3.0 * (f64::from(row + 1) - f64::from(ROWS - row))));
            }
        }
        if !wb.recalc_pending() { break; }
    }
    assert!(!wb.recalc_pending());
    for row in 0..ROWS {
        assert_eq!(wb.value(0, row, 1), Value::Num(3.0 * f64::from(row + 1)));
        assert_eq!(wb.value(0, row, 2), Value::Num(3.0 * f64::from(ROWS - row)));
    }
    for col in [4, 5] {
        assert_eq!(wb.value(0, 4, col), Value::Err(ErrorKind::Circ));
    }
}
