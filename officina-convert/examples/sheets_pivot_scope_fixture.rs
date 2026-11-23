//! Desktop checks for Excel value sorting by totals and selected members.

use std::{env, fs, path::Path};

use officina_convert::xlsx;
use officina_convert::zip::{Archive, ZipWriter};
use officina_grid::pivot::{
    Agg, AxisSort, BaseItem, DataField, Pivot, PivotField, ReferenceCalc, ShowAs,
};
use officina_grid::{CellAddr, Rect, Value, Workbook};

const BASE: [[f64; 3]; 3] = [[1.0, 7.0, 8.0], [2.0, 14.0, 2.0], [5.0, 4.0, 4.0]];
const REGIONS: [&str; 3] = ["A", "B", "C"];
const PERIODS: [&str; 3] = ["X", "Y", "Z"];
const VALUE_FIELDS: u32 = 8;

fn at(row: u32, col: u32) -> CellAddr {
    CellAddr::new(0, row, col)
}

fn sorting(selected: bool, axis: u32) -> AxisSort {
    AxisSort::Scoped {
        data: 1,
        descending: true,
        members: if selected {
            vec![(1 - axis, Value::str(if axis == 0 { "X" } else { "A" }))]
        } else {
            Vec::new()
        },
    }
}

fn fixture(selected: bool) -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("Sales").unwrap();
    for (col, caption) in (0..).zip(["Region", "Period", "Sales", "Costs"]) {
        wb.set_value(at(0, col), Value::str(caption));
    }
    for (r, region) in REGIONS.iter().enumerate() {
        for (c, period) in PERIODS.iter().enumerate() {
            let row = 1 + (r * 3 + c) as u32;
            wb.set_value(at(row, 0), Value::str(region));
            wb.set_value(at(row, 1), Value::str(period));
            wb.set_value(at(row, 2), Value::Num(BASE[r][c]));
            wb.set_value(at(row, 3), Value::Num(100.0 - BASE[r][c]));
        }
    }
    let source = Rect {
        sheet: 0,
        r0: 0,
        c0: 0,
        r1: 9,
        c1: 3,
    };
    let mut pivot = Pivot::new("Sorted sales", source, at(0, 5));
    pivot.rows.push(PivotField {
        sort: sorting(selected, 0),
        ..PivotField::new(0)
    });
    pivot.cols.push(PivotField {
        sort: sorting(selected, 1),
        ..PivotField::new(1)
    });
    // Put the sorting measure second. Costs would order the members in reverse.
    let mut add = |name: &str, field, show_as| {
        pivot.data.push(DataField {
            field,
            agg: Agg::Sum,
            name: Some(name.into()),
            show_as,
        });
    };
    add("Costs", 3, ShowAs::Normal);
    add("Sales", 2, ShowAs::Normal);
    for (field, axis) in [(0, "row"), (1, "col")] {
        for (name, item) in [("Prev", BaseItem::Previous), ("Next", BaseItem::Next)] {
            add(
                &format!("{name} {axis}"),
                2,
                ShowAs::Reference {
                    calc: ReferenceCalc::Difference,
                    field,
                    item,
                },
            );
        }
        add(&format!("Run {axis}"), 2, ShowAs::RunningTotal(field));
    }
    pivot.subtotals = false;
    wb.add_pivot(pivot).unwrap();
    wb
}

fn rewrite_pivot(bytes: &[u8], patch: impl Fn(&str) -> String) -> Vec<u8> {
    let zip = Archive::open(bytes).unwrap();
    let mut out = ZipWriter::new(Vec::new());
    for entry in &zip.entries {
        let data = zip.read(entry).unwrap();
        if entry.name == "xl/pivotTables/pivotTable1.xml" {
            out.add(
                &entry.name,
                patch(std::str::from_utf8(&data).unwrap()).as_bytes(),
            )
            .unwrap();
        } else {
            out.add(&entry.name, &data).unwrap();
        }
    }
    out.finish().unwrap()
}

fn xlsx_input(selected: bool) -> Vec<u8> {
    let bytes = xlsx::write(&fixture(selected), Vec::new()).unwrap();
    if !selected {
        return bytes;
    }
    rewrite_pivot(&bytes, |xml| {
        // Offset both selected members from shared item 0 to pivot item 2.
        // The scope must resolve through the pivot field's item indirection.
        let before = r#"<item x="0"/><item x="1"/><item x="2"/>"#;
        let after = r#"<item x="1"/><item x="2"/><item x="0"/>"#;
        assert_eq!(xml.matches(before).count(), 2);
        let mut xml = xml.replace(before, after);
        for field in [0, 1] {
            let needle = format!(r#"<reference field="{field}" count="1" selected="0"><x v="0"/>"#);
            let replacement = needle.replace(r#"v="0""#, r#"v="2""#);
            assert!(xml.contains(&needle));
            xml = xml.replace(&needle, &replacement);
        }
        xml
    })
}

fn read(bytes: &[u8]) -> Workbook {
    let (wb, loaded) = xlsx::read(bytes).unwrap();
    assert!(loaded.report.is_empty(), "{:?}", loaded.report);
    wb
}

fn check(book: &Workbook, state: &str, selected: bool) {
    assert_eq!(book.pivots(), fixture(selected).pivots());
    let edited = matches!(state, "edited" | "refreshed");
    assert_eq!(
        book.get(at(1, 2)),
        Value::Num(if edited { 40.0 } else { 1.0 })
    );
    assert_eq!(
        book.get(at(1, 4)),
        if state == "initial" {
            Value::Empty
        } else {
            Value::Num(7.0)
        }
    );
    let refreshed = state == "refreshed";
    let (rows, cols) = match (selected, refreshed) {
        (false, false) => ([1, 0, 2], [1, 2, 0]),
        (false, true) => ([0, 1, 2], [0, 1, 2]),
        (true, false) => ([2, 1, 0], [2, 1, 0]),
        (true, true) => ([0, 2, 1], [0, 2, 1]),
    };
    let mut amounts = BASE;
    if refreshed {
        amounts[0][0] = 40.0;
    }
    for (i, r) in rows.into_iter().enumerate() {
        let row = i as u32 + 3;
        assert_eq!(
            book.get(at(row, 5)),
            Value::str(REGIONS[r]),
            "{state} row order"
        );
        for (j, c) in cols.into_iter().enumerate() {
            let col = 6 + VALUE_FIELDS * j as u32;
            if i == 0 {
                assert_eq!(
                    book.get(at(1, col)),
                    Value::str(PERIODS[c]),
                    "{state} column order"
                );
            }
            let value = amounts[r][c];
            let relative = |other: Option<usize>, by_row| {
                other.map_or(Value::Empty, |other| {
                    Value::Num(
                        value
                            - if by_row {
                                amounts[other][c]
                            } else {
                                amounts[r][other]
                            },
                    )
                })
            };
            let expected = [
                Value::Num(100.0 - BASE[r][c]),
                Value::Num(value),
                relative(i.checked_sub(1).map(|i| rows[i]), true),
                relative(rows.get(i + 1).copied(), true),
                Value::Num(rows[..=i].iter().map(|&row| amounts[row][c]).sum()),
                relative(j.checked_sub(1).map(|j| cols[j]), false),
                relative(cols.get(j + 1).copied(), false),
                Value::Num(cols[..=j].iter().map(|&col| amounts[r][col]).sum()),
            ];
            for (offset, expected) in (0..).zip(expected) {
                assert_eq!(
                    book.get(at(row, col + offset)),
                    expected,
                    "{state} selected={selected} row={row} column={}",
                    col + offset
                );
            }
        }
    }
}

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    match args.as_slice() {
        [cmd, dir] if cmd == "make" => {
            let dir = Path::new(dir);
            fs::write(dir.join("grand.xlsx"), xlsx_input(false)).unwrap();
            fs::write(dir.join("member.xlsx"), xlsx_input(true)).unwrap();
        }
        [cmd, file, state, kind] if cmd == "check" => {
            let path = Path::new(file);
            let bytes = fs::read(path).unwrap();
            let book = if path.extension().is_some_and(|ext| ext == "osheet") {
                officina_grid::native::load(&bytes).unwrap()
            } else {
                read(&bytes)
            };
            check(&book, state, kind == "member");
        }
        _ => panic!("usage: sheets_pivot_scope_fixture make DIR | check FILE STATE grand|member"),
    }
}

#[test]
fn grand_total_and_member_scopes_survive_xlsx_and_native_history() {
    use officina_core::history::Op;
    for selected in [false, true] {
        let mut wb = read(&xlsx_input(selected));
        check(&wb, "initial", selected);
        wb.set_value(at(1, 4), Value::Num(7.0));
        let mut wb = read(&xlsx::write(&wb, Vec::new()).unwrap());
        wb = officina_grid::native::load(&officina_grid::native::save_to_vec(&wb)).unwrap();
        check(&wb, "saved", selected);
        wb.set_value(at(1, 2), Value::Num(40.0));
        check(&wb, "edited", selected);
        let op = wb.pivot_op(Some(0), Some(wb.pivots()[0].clone())).unwrap();
        let undo = op.apply(&mut wb);
        check(
            &read(&xlsx::write(&wb, Vec::new()).unwrap()),
            "refreshed",
            selected,
        );
        let redo = officina_grid::ops::GridOp::decode(&undo.encode())
            .unwrap()
            .apply(&mut wb);
        check(&wb, "edited", selected);
        redo.apply(&mut wb);
        check(&wb, "refreshed", selected);
    }
}

#[test]
fn a_sort_member_missing_from_the_source_keeps_its_identity() {
    let mut wb = read(&xlsx_input(true));
    for row in [1, 4, 7] {
        wb.set_value(at(row, 1), Value::str("W"));
    }
    wb.refresh_pivot(0).unwrap();
    // All keys for the X column are missing, so regions fall back to label order.
    assert_eq!(
        [3, 4, 5].map(|row| wb.get(at(row, 5))),
        REGIONS.map(Value::str)
    );
    let mut wb = read(&xlsx::write(&wb, Vec::new()).unwrap());
    assert_eq!(wb.pivots()[0].rows[0].sort, sorting(true, 0));
    for row in [1, 4, 7] {
        wb.set_value(at(row, 1), Value::str("X"));
    }
    wb.refresh_pivot(0).unwrap();
    check(&wb, "initial", true);
}

#[test]
fn cache_index_scopes_use_shared_item_indices_and_unknown_scopes_are_reported() {
    let input = xlsx_input(true);
    let cached = rewrite_pivot(&input, |xml| {
        let mut xml = xml.replace(
            r#"<pivotArea dataOnly="0""#,
            r#"<pivotArea cacheIndex="1" dataOnly="0""#,
        );
        for field in [0, 1] {
            let old = format!(r#"<reference field="{field}" count="1" selected="0"><x v="2"/>"#);
            xml = xml.replace(&old, &old.replace(r#"v="2""#, r#"v="0""#));
        }
        xml
    });
    check(&read(&cached), "initial", true);
    let unknown = rewrite_pivot(&input, |xml| {
        xml.replace(
            r#"<reference field="1" count="1""#,
            r#"<reference field="1" byPosition="1" count="1""#,
        )
    });
    let (wb, loaded) = xlsx::read(&unknown).unwrap();
    assert!(
        loaded
            .report
            .human()
            .iter()
            .any(|note| note.contains("unresolved pivot value sort scope"))
    );
    assert_eq!(
        wb.pivots()[0].rows[0].sort,
        AxisSort::Name { descending: true }
    );
    assert_eq!(wb.pivots()[0].cols[0].sort, sorting(true, 1));
}

#[test]
fn nested_scopes_resolve_in_axis_order_and_keep_the_measure_when_reordered() {
    let mut wb = fixture(true);
    let mut pivot = wb.pivots()[0].clone();
    pivot.source.c1 = 4;
    wb.set_value(at(0, 4), Value::str("Market"));
    for row in 1..=9 {
        wb.set_value(at(row, 4), Value::str("Domestic"));
    }
    pivot.cols.insert(0, PivotField::new(4));
    pivot.rows[0].sort = AxisSort::Scoped {
        data: 1,
        descending: true,
        members: vec![(4, Value::str("Domestic")), (1, Value::str("X"))],
    };
    pivot.swap_data_fields(0, 1);
    pivot.remove_data_field(1); // Remove Costs. Sales stays the sort measure.
    wb.set_pivot(0, pivot.clone()).unwrap();
    let mut wb = read(&xlsx::write(&wb, Vec::new()).unwrap());
    assert_eq!(wb.pivots()[0], pivot);
    wb.set_value(at(1, 2), Value::Num(40.0));
    wb.refresh_pivot(0).unwrap();
    let body = wb.pivot_shape(0).unwrap().header_rows;
    assert_eq!(
        [0, 1, 2].map(|offset| wb.get(at(body + offset, 5))),
        ["A", "C", "B"].map(Value::str)
    );
    pivot.remove_data_field(0);
    assert_eq!(pivot.rows[0].sort, AxisSort::Default);
    assert_eq!(pivot.cols[1].sort, AxisSort::Default);
}

#[test]
fn grand_total_sort_translates_to_ods_and_back_in_ascending_order() {
    let mut wb = fixture(false);
    let mut pivot = wb.pivots()[0].clone();
    for field in pivot.rows.iter_mut().chain(&mut pivot.cols) {
        field.sort = AxisSort::Scoped {
            data: 1,
            descending: false,
            members: vec![],
        };
    }
    wb.set_pivot(0, pivot.clone()).unwrap();
    let bytes = officina_convert::ods::write(&wb, Vec::new()).unwrap();
    let (wb, loaded) = officina_convert::ods::read(&bytes).unwrap();
    assert!(loaded.report.is_empty(), "{:?}", loaded.report);
    assert_eq!(
        wb.pivots()[0].rows[0].sort,
        AxisSort::Data {
            caption: "Sales".into(),
            descending: false,
        }
    );
    let mut wb = read(&xlsx::write(&wb, Vec::new()).unwrap());
    assert_eq!(wb.pivots()[0], pivot);
    wb.refresh_pivot(0).unwrap();
    assert_eq!(
        [3, 4, 5].map(|row| wb.get(at(row, 5))),
        ["C", "A", "B"].map(Value::str)
    );
    assert_eq!(
        [6, 14, 22].map(|col| wb.get(at(1, col))),
        ["X", "Z", "Y"].map(Value::str)
    );
}
