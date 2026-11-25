use officina_core::history::Op;
use officina_grid::filter::{AutoFilter, FilterState};
use officina_grid::ops::GridOp;
use officina_grid::outline::{Group, Outline};
use officina_grid::shift::Axis;
use officina_grid::{CellAddr, Rect, Value, Workbook};

#[test]
fn subtotal_reacts_to_visibility_and_keeps_local_dependents_live() {
    let mut wb = Workbook::new();
    wb.add_sheet("Source").unwrap();
    wb.add_sheet("Totals").unwrap();
    for (row, value) in [(1, 10.0), (2, 20.0), (3, 30.0)] {
        wb.set_value(CellAddr::new(0, row, 0), Value::Num(value));
    }
    wb.set_formula(CellAddr::new(0, 4, 0), "=SUBTOTAL(9,A2:A4)")
        .unwrap();
    wb.set_formula(CellAddr::new(1, 0, 0), "=SUBTOTAL(9,Source!A2:A5)")
        .unwrap();
    wb.set_formula(CellAddr::new(1, 1, 0), "=SUBTOTAL(109,Source!A2:A5)")
        .unwrap();
    wb.set_formula(CellAddr::new(1, 2, 0), "=A2*2").unwrap();
    assert_totals(&mut wb, [60.0, 60.0, 120.0]);

    let visibility = FilterState {
        sheet: 0,
        filter: Some(AutoFilter {
            rect: Rect {
                sheet: 0,
                r0: 0,
                r1: 2,
                c0: 0,
                c1: 0,
            },
            conds: vec![],
        }),
        hidden: vec![(2, 3)],
    };
    let undo = GridOp::Filter(visibility).apply(&mut wb);
    assert!(wb.recalc_pending());
    assert_totals(&mut wb, [40.0, 10.0, 20.0]);
    undo.apply(&mut wb);
    assert_totals(&mut wb, [60.0, 60.0, 120.0]);

    let outline = Outline::new(
        vec![Group {
            start: 1,
            end: 2,
            collapsed: true,
        }],
        Axis::Rows,
    )
    .unwrap();
    let undo = GridOp::Outline {
        sheet: 0,
        axis: Axis::Rows,
        outline,
    }
    .apply(&mut wb);
    assert_totals(&mut wb, [60.0, 30.0, 60.0]);
    undo.apply(&mut wb);
    assert_totals(&mut wb, [60.0, 60.0, 120.0]);

    let undo = GridOp::Sizes {
        sheet: 0,
        rows: true,
        over: vec![(3, 0.0)],
    }
    .apply(&mut wb);
    assert_totals(&mut wb, [60.0, 30.0, 60.0]);
    undo.apply(&mut wb);
    assert_totals(&mut wb, [60.0, 60.0, 120.0]);

    wb.set_value(CellAddr::new(0, 2, 0), Value::Num(7.0));
    assert_totals(&mut wb, [47.0, 47.0, 94.0]);
}

fn assert_totals(workbook: &mut Workbook, expected: [f64; 3]) {
    workbook.recalc();
    for (row, expected) in expected.into_iter().enumerate() {
        assert_eq!(workbook.value(1, row as u32, 0), Value::Num(expected));
    }
}

#[test]
fn table_filters_invalidate_cross_sheet_subtotals() {
    use officina_grid::filter::Cond;
    use officina_grid::tables::Table;

    let mut wb = Workbook::new();
    wb.add_sheet("Source").unwrap();
    wb.add_sheet("Totals").unwrap();
    let rect = Rect {
        sheet: 0,
        r0: 0,
        r1: 3,
        c0: 0,
        c1: 0,
    };
    wb.set_value(CellAddr::new(0, 0, 0), Value::str("Amount"));
    wb.set_numbers(0, 0, 1, &[10.0, 20.0, 30.0]);
    wb.add_table(Table {
        name: "Amounts".into(),
        rect,
        header: true,
        totals: false,
        columns: vec!["Amount".into()],
        style: None,
        stripes: false,
        filter: None,
    })
    .unwrap();
    wb.set_formula(CellAddr::new(1, 0, 0), "=SUBTOTAL(9,Amounts[Amount])")
        .unwrap();
    wb.set_formula(CellAddr::new(1, 1, 0), "=SUBTOTAL(109,Amounts[Amount])")
        .unwrap();
    wb.set_formula(CellAddr::new(1, 2, 0), "=A2*2").unwrap();
    assert_totals(&mut wb, [60.0, 60.0, 120.0]);

    let filtered = wb.apply_table_filter(
        wb.table("Amounts").unwrap(),
        AutoFilter {
            rect,
            conds: vec![(0, Cond::Values(vec!["20".into()]))],
        },
    );
    let previous = wb.set_table_filter("Amounts", Some(filtered)).unwrap();
    assert_totals(&mut wb, [20.0, 20.0, 40.0]);
    wb.set_table_filter("Amounts", previous).unwrap();
    assert_totals(&mut wb, [60.0, 60.0, 120.0]);
}

#[test]
fn aggregate_follows_its_options_and_recalculates_when_rows_hide() {
    let mut wb = Workbook::new();
    wb.add_sheet("Source").unwrap();
    wb.add_sheet("Totals").unwrap();
    for (row, value) in [(1, 10.0), (2, 20.0), (3, 30.0)] {
        wb.set_value(CellAddr::new(0, row, 0), Value::Num(value));
    }
    // A nested AGGREGATE counts as a subtotal for the options that skip those.
    wb.set_formula(CellAddr::new(0, 4, 0), "=AGGREGATE(9,4,A2:A4)").unwrap();
    // 5: skip hidden rows only; 4: skip nothing; 1: skip hidden rows and nested totals.
    wb.set_formula(CellAddr::new(1, 0, 0), "=AGGREGATE(9,5,Source!A2:A5)").unwrap();
    wb.set_formula(CellAddr::new(1, 1, 0), "=AGGREGATE(9,4,Source!A2:A5)").unwrap();
    wb.set_formula(CellAddr::new(1, 2, 0), "=AGGREGATE(9,1,Source!A2:A5)").unwrap();
    assert_totals(&mut wb, [120.0, 120.0, 60.0]);

    let undo = GridOp::Sizes { sheet: 0, rows: true, over: vec![(3, 0.0)] }.apply(&mut wb);
    assert_totals(&mut wb, [90.0, 120.0, 30.0]);
    undo.apply(&mut wb);
    assert_totals(&mut wb, [120.0, 120.0, 60.0]);
}
