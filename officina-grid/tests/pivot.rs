//! Pivot tables (docs/spec/officina.md §16.32): the cells Excel's tabular layout
//! shows for the same source and fields, refresh, filters and the native file.

use officina_grid::pivot::{Agg, DataField, Pivot, PivotField, Role};
use officina_grid::{CellAddr, Rect, Value, Workbook};

fn book() -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("Data").unwrap();
    let rows = [
        ["Region", "Rep", "Quarter", "Units", "Price"],
        ["North", "Ana", "Q1", "10", "2.5"],
        ["North", "Ana", "Q2", "20", "3"],
        ["North", "Bo", "Q1", "5", "4"],
        ["South", "Cy", "Q1", "7", "1"],
        ["South", "Cy", "Q2", "3", "2"],
        ["East", "Di", "Q2", "8", "5"],
        ["north", "Ana", "Q1", "1", "1.5"],
    ];
    for (r, row) in rows.iter().enumerate() {
        for (c, v) in row.iter().enumerate() {
            wb.set_input(CellAddr::new(0, r as u32, c as u32), v).unwrap();
        }
    }
    wb.recalc();
    wb
}

fn src() -> Rect {
    Rect { sheet: 0, r0: 0, c0: 0, r1: 7, c1: 4 }
}

fn sum(field: u32) -> DataField {
    DataField { field, agg: Agg::Sum, name: None, show_as: Default::default() }
}

/// The output as text rows ("" for empty), trailing empties cut.
fn grid(wb: &Workbook, i: usize) -> Vec<Vec<String>> {
    let a = wb.pivot_shape(i).unwrap().area.unwrap();
    (a.r0..=a.r1)
        .map(|r| {
            let mut row: Vec<String> = (a.c0..=a.c1).map(|c| wb.get(CellAddr::new(a.sheet, r, c)).display()).collect();
            while row.last().is_some_and(|s| s.is_empty()) {
                row.pop();
            }
            row
        })
        .collect()
}

fn rows(t: &[&[&str]]) -> Vec<Vec<String>> {
    t.iter().map(|r| r.iter().map(|s| s.to_string()).collect()).collect()
}

#[test]
fn tabular_layout_as_excel_shows_it() {
    let mut wb = book();
    let mut p = Pivot::new("PivotTable1", src(), CellAddr::new(0, 0, 7));
    p.rows = vec![PivotField::new(0), PivotField::new(1)];
    p.cols = vec![PivotField::new(2)];
    p.data = vec![sum(3)];
    let i = wb.add_pivot(p).unwrap();
    assert_eq!(
        grid(&wb, i),
        rows(&[
            &["Sum of Units", "", "Quarter"],
            &["Region", "Rep", "Q1", "Q2", "Grand Total"],
            &["East", "Di", "", "8", "8"],
            &["East Total", "", "", "8", "8"],
            &["North", "Ana", "11", "20", "31"],
            &["", "Bo", "5", "", "5"],
            &["North Total", "", "16", "20", "36"],
            &["South", "Cy", "7", "3", "10"],
            &["South Total", "", "7", "3", "10"],
            &["Grand Total", "", "23", "31", "54"],
        ])
    );
    let at = |r, c| wb.pivot_at(CellAddr::new(0, r, c)).map(|x| x.1);
    assert_eq!(at(0, 7), Some(Role::Header));
    assert_eq!(at(2, 7), Some(Role::Label));
    assert_eq!(at(2, 10), Some(Role::Data));
    assert_eq!(at(3, 7), Some(Role::Total));
    assert_eq!(at(4, 11), Some(Role::Total));
    assert_eq!(at(9, 9), Some(Role::Total));
    assert_eq!(at(10, 7), None);
}

#[test]
fn several_values_counts_averages_and_no_subtotals() {
    let mut wb = book();
    let mut p = Pivot::new("P", src(), CellAddr::new(0, 0, 7));
    p.rows = vec![PivotField::new(0)];
    p.data = vec![sum(3), DataField { field: 4, agg: Agg::Average, name: Some("Mean price".into()), show_as: Default::default() }, DataField { field: 1, agg: Agg::Count, name: None, show_as: Default::default() }];
    let i = wb.add_pivot(p).unwrap();
    assert_eq!(
        grid(&wb, i),
        rows(&[
            &["", "Values"],
            &["Region", "Sum of Units", "Mean price", "Count of Rep"],
            &["East", "8", "5", "1"],
            &["North", "36", "2.75", "4"],
            &["South", "10", "1.5", "2"],
            &["Grand Total", "54", "2.71428571428571", "7"],
        ])
    );
    // Columns with several values: each item's values side by side, totals per value.
    let mut p = wb.pivots()[i].clone();
    p.cols = vec![PivotField::new(2)];
    p.data.truncate(2);
    p.subtotals = false;
    wb.set_pivot(i, p).unwrap();
    assert_eq!(
        grid(&wb, i)[..3],
        rows(&[
            &["", "Quarter", "Values"],
            &["", "Q1", "", "Q2", "", "Total Sum of Units", "Total Mean price"],
            &["Region", "Sum of Units", "Mean price", "Sum of Units", "Mean price"],
        ])[..]
    );
}

#[test]
fn filters_hidden_items_and_refresh() {
    let mut wb = book();
    let mut p = Pivot::new("P", src(), CellAddr::new(0, 10, 0));
    p.rows = vec![PivotField { hidden: vec![Value::str("BO")], ..PivotField::new(1) }];
    p.pages = vec![PivotField { hidden: vec![Value::str("Q2")], ..PivotField::new(2) }];
    p.data = vec![sum(3)];
    let i = wb.add_pivot(p).unwrap();
    assert_eq!(grid(&wb, i), rows(&[&["Quarter", "Q1"], &[], &["Rep", "Sum of Units"], &["Ana", "11"], &["Cy", "7"], &["Grand Total", "18"]]));
    // The source changes: nothing moves until a refresh, as in Excel.
    wb.set_input(CellAddr::new(0, 4, 3), "70").unwrap();
    wb.recalc();
    assert_eq!(wb.get(CellAddr::new(0, 14, 1)), Value::Num(7.0));
    assert!(wb.refresh_pivots().is_empty());
    assert_eq!(wb.get(CellAddr::new(0, 14, 1)), Value::Num(70.0));
    // A cell in the way of a bigger output refuses the refresh and keeps the old one.
    wb.set_input(CellAddr::new(0, 11, 3), "mine").unwrap();
    let mut p = wb.pivots()[i].clone();
    p.rows[0].hidden.clear();
    p.pages.clear();
    p.cols = vec![PivotField::new(2)];
    assert_eq!(wb.set_pivot(i, p), Err("there are cells in the way"));
    assert_eq!(wb.get(CellAddr::new(0, 10, 1)), Value::str("Q1"));
    // Removing it clears its cells only.
    wb.remove_pivot(i);
    assert!((10..16).all(|r| (0..2).all(|c| wb.get(CellAddr::new(0, r, c)).is_empty())));
    assert_eq!(wb.get(CellAddr::new(0, 11, 3)), Value::str("mine"));
}

#[test]
fn bad_pivots_are_refused() {
    let mut wb = book();
    let mut p = Pivot::new("P", src(), CellAddr::new(0, 2, 2));
    p.rows = vec![PivotField::new(0)];
    p.data = vec![sum(3)];
    assert_eq!(wb.add_pivot(p.clone()), Err("it would cover its own source"));
    p.dest = CellAddr::new(0, 0, 9);
    p.cols = vec![PivotField::new(0)];
    assert_eq!(wb.add_pivot(p.clone()), Err("a field on two axes"));
    p.cols = vec![PivotField::new(9)];
    assert_eq!(wb.add_pivot(p), Err("a field outside the source"));
    assert!(wb.pivots().is_empty());
}

#[test]
fn native_files_keep_pivots() {
    let mut wb = book();
    let mut p = Pivot::new("Sales by rep", src(), CellAddr::new(0, 0, 7));
    p.rows = vec![PivotField::new(0), PivotField { hidden: vec![Value::str("Bo"), Value::Num(3.0)], ..PivotField::new(1) }];
    p.cols = vec![PivotField::new(2)];
    p.data = vec![sum(3), DataField { field: 4, agg: Agg::Max, name: Some("Top".into()), show_as: Default::default() }];
    p.grand_col = false;
    let i = wb.add_pivot(p).unwrap();
    let back = officina_grid::native::load(&officina_grid::native::save_to_vec(&wb)).unwrap();
    assert_eq!(back.pivots(), wb.pivots());
    assert_eq!(back.pivot_shape(i), wb.pivot_shape(i));
    assert_eq!(grid(&back, i), grid(&wb, i));
}

#[test]
fn edits_undo_and_journal() {
    use officina_core::history::Op;
    use officina_grid::ops::GridOp;
    let mut wb = book();
    let before: Vec<Value> = (0..12).flat_map(|r| (7..12).map(move |c| (r, c))).map(|(r, c)| wb.get(CellAddr::new(0, r, c))).collect();
    let mut p = Pivot::new(&wb.new_pivot_name(), src(), CellAddr::new(0, 0, 7));
    assert_eq!(&*p.name, "PivotTable1");
    p.rows = vec![PivotField::new(0)];
    p.data = vec![sum(3)];
    let op = wb.pivot_op(None, Some(p.clone())).unwrap();
    // Through the journal's bytes.
    let op = GridOp::decode(&op.encode()).unwrap();
    let undo = op.apply(&mut wb);
    wb.recalc();
    assert_eq!(wb.get(CellAddr::new(0, 2, 8)), Value::Num(36.0));
    assert_eq!(wb.pivot_at(CellAddr::new(0, 4, 7)).map(|x| x.1), Some(Role::Total));
    // Change it: a column field; the output grows.
    p.cols = vec![PivotField::new(2)];
    let redo = wb.pivot_op(Some(0), Some(p)).unwrap().apply(&mut wb);
    assert_eq!(wb.get(CellAddr::new(0, 1, 9)), Value::str("Q2"));
    redo.apply(&mut wb);
    assert_eq!(wb.get(CellAddr::new(0, 1, 9)), Value::Empty);
    assert_eq!(wb.get(CellAddr::new(0, 2, 8)), Value::Num(36.0));
    undo.apply(&mut wb);
    assert!(wb.pivots().is_empty());
    let after: Vec<Value> = (0..12).flat_map(|r| (7..12).map(move |c| (r, c))).map(|(r, c)| wb.get(CellAddr::new(0, r, c))).collect();
    assert_eq!(after, before);
    // Removing.
    let mut p = Pivot::new("P", src(), CellAddr::new(0, 0, 7));
    p.rows = vec![PivotField::new(0)];
    p.data = vec![sum(3)];
    wb.add_pivot(p).unwrap();
    wb.pivot_op(Some(0), None).unwrap().apply(&mut wb);
    assert!(wb.pivots().is_empty() && wb.get(CellAddr::new(0, 0, 7)).is_empty());
}

#[test]
fn compact_layout_as_excel_shows_it() {
    let mut wb = book();
    let mut p = Pivot::new("PivotTable1", src(), CellAddr::new(0, 0, 7));
    p.rows = vec![PivotField::new(0), PivotField::new(1)];
    p.cols = vec![PivotField::new(2)];
    p.data = vec![sum(3)];
    p.compact = true;
    let i = wb.add_pivot(p.clone()).unwrap();
    assert_eq!(
        grid(&wb, i),
        rows(&[
            &["Sum of Units", "Column Labels"],
            &["Row Labels", "Q1", "Q2", "Grand Total"],
            &["East", "", "8", "8"],
            &["Di", "", "8", "8"],
            &["North", "16", "20", "36"],
            &["Ana", "11", "20", "31"],
            &["Bo", "5", "", "5"],
            &["South", "7", "3", "10"],
            &["Cy", "7", "3", "10"],
            &["Grand Total", "23", "31", "54"],
        ])
    );
    let sh = wb.pivot_shape(i).unwrap().clone();
    let ind = |r| sh.indent(CellAddr::new(0, r, 7));
    assert_eq!((ind(2), ind(3), ind(5), ind(9), ind(0)), ((0, true), (1, false), (1, false), (0, false), (0, false)));
    assert_eq!(ind(3), (1, false));
    assert_eq!(wb.pivot_at(CellAddr::new(0, 9, 8)).map(|x| x.1), Some(Role::Total));
    // No subtotals: outer rows carry no values. One row field and no columns.
    p.subtotals = false;
    wb.set_pivot(i, p.clone()).unwrap();
    assert_eq!(grid(&wb, i)[2], ["East"]);
    p.rows = vec![PivotField::new(0)];
    p.cols.clear();
    wb.set_pivot(i, p.clone()).unwrap();
    assert_eq!(grid(&wb, i), rows(&[&["Row Labels", "Sum of Units"], &["East", "8"], &["North", "36"], &["South", "10"], &["Grand Total", "54"]]));
    // The native file keeps the form.
    let mut out = Vec::new();
    officina_grid::native::save(&wb, &mut out).unwrap();
    let back = officina_grid::native::load(&out).unwrap();
    assert!(back.pivots()[0].compact);
}
