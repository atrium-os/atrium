//! XLSX manual and ODS value-sorted pivot checkpoints for the desktop gate.

use std::{env, fs, path::Path};

use officina_convert::zip::{Archive, ZipWriter};
use officina_convert::{ods, xlsx};
use officina_grid::pivot::{
    Agg, AxisSort, BaseItem, DataField, Pivot, PivotField, ReferenceCalc, ShowAs,
};
use officina_grid::{CellAddr, Rect, Value, Workbook};

fn at(row: u32, col: u32) -> CellAddr {
    CellAddr::new(0, row, col)
}

fn sort(format: &str) -> AxisSort {
    match format {
        "xlsx" => AxisSort::Manual(["C", "A", "B"].map(Value::str).to_vec()),
        "ods" => AxisSort::Data {
            caption: "Sales total".into(),
            descending: true,
        },
        _ => panic!("unexpected format {format}"),
    }
}

fn fixture(format: &str) -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("Sales").unwrap();
    wb.set_value(at(0, 0), Value::str("Store"));
    wb.set_value(at(0, 1), Value::str("Sales"));
    for (row, (store, amount)) in (1..).zip([("A", 25.0), ("B", 20.0), ("C", 30.0)]) {
        wb.set_value(at(row, 0), Value::str(store));
        wb.set_value(at(row, 1), Value::Num(amount));
    }
    let range = Rect {
        sheet: 0,
        r0: 0,
        c0: 0,
        r1: 3,
        c1: 1,
    };
    let mut pivot = Pivot::new("Ordered sales", range, at(0, 5));
    pivot.rows.push(PivotField {
        sort: sort(format),
        ..PivotField::new(0)
    });
    pivot.subtotals = false;
    let relative = |item| ShowAs::Reference {
        calc: ReferenceCalc::Difference,
        field: 0,
        item,
    };
    for (name, show_as) in [
        ("Previous", relative(BaseItem::Previous)),
        ("Next", relative(BaseItem::Next)),
        ("Running total", ShowAs::RunningTotal(0)),
        ("Sales total", ShowAs::Normal),
    ] {
        pivot.data.push(DataField {
            field: 1,
            agg: Agg::Sum,
            name: Some(name.into()),
            show_as,
        });
    }
    wb.add_pivot(pivot).unwrap();
    wb
}

fn xlsx_input(book: &Workbook) -> Vec<u8> {
    // Give the source workbook a different cache index order. A manual order
    // must follow the pivotField's items, which point into this cache.
    let bytes = xlsx::write(book, Vec::new()).unwrap();
    let zip = Archive::open(&bytes).unwrap();
    let mut out = ZipWriter::new(Vec::new());
    for entry in &zip.entries {
        let data = zip.read(entry).unwrap();
        let swap = match entry.name.as_str() {
            "xl/pivotCache/pivotCacheDefinition1.xml" => Some((
                r#"<s v="C"/><s v="A"/><s v="B"/>"#,
                r#"<s v="A"/><s v="B"/><s v="C"/>"#,
            )),
            "xl/pivotTables/pivotTable1.xml" => Some((
                r#"sortType="manual" defaultSubtotal="0"><items count="3"><item x="0"/><item x="1"/><item x="2"/>"#,
                r#"sortType="manual" defaultSubtotal="0"><items count="3"><item x="2"/><item x="0"/><item x="1"/>"#,
            )),
            _ => None,
        };
        if let Some((old, new)) = swap {
            let xml = String::from_utf8(data).unwrap();
            assert!(xml.contains(old), "{xml}");
            out.add(&entry.name, xml.replace(old, new).as_bytes())
                .unwrap();
        } else {
            out.add(&entry.name, &data).unwrap();
        }
    }
    out.finish().unwrap()
}

fn read(bytes: &[u8], format: &str) -> Workbook {
    let (wb, loaded) = match format {
        "xlsx" => xlsx::read(bytes).unwrap(),
        "ods" => ods::read(bytes).unwrap(),
        _ => panic!("unexpected format {format}"),
    };
    assert!(loaded.report.is_empty(), "{:?}", loaded.report);
    wb
}

fn check(book: &Workbook, state: &str, format: &str) {
    assert_eq!(book.pivots(), fixture(format).pivots());
    let edited = matches!(state, "edited" | "refreshed");
    assert_eq!(
        book.get(at(1, 1)),
        Value::Num(if edited { 8.0 } else { 25.0 })
    );
    assert_eq!(
        book.get(at(1, 3)),
        if state == "initial" {
            Value::Empty
        } else {
            Value::Num(7.0)
        }
    );
    let expected = match (format, state) {
        ("xlsx", "refreshed") => [
            ("C", None, Some(22.0), 30.0, 30.0),
            ("A", Some(-22.0), Some(-12.0), 38.0, 8.0),
            ("B", Some(12.0), None, 58.0, 20.0),
        ],
        ("ods", "refreshed") => [
            ("C", None, Some(10.0), 30.0, 30.0),
            ("B", Some(-10.0), Some(12.0), 50.0, 20.0),
            ("A", Some(-12.0), None, 58.0, 8.0),
        ],
        (_, "initial" | "saved" | "edited") => [
            ("C", None, Some(5.0), 30.0, 30.0),
            ("A", Some(-5.0), Some(5.0), 55.0, 25.0),
            ("B", Some(-5.0), None, 75.0, 20.0),
        ],
        _ => panic!("unexpected checkpoint {format} {state}"),
    };
    for (row, (label, previous, next, running, actual)) in (2..).zip(expected) {
        let values = [
            Value::str(label),
            previous.map_or(Value::Empty, Value::Num),
            next.map_or(Value::Empty, Value::Num),
            Value::Num(running),
            Value::Num(actual),
        ];
        for (col, expected) in (5..).zip(values) {
            assert_eq!(
                book.get(at(row, col)),
                expected,
                "{format} {state} ({row}, {col})"
            );
        }
    }
}

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    match args.as_slice() {
        [cmd, dir] if cmd == "make" => {
            let dir = Path::new(dir);
            fs::write(dir.join("sales.xlsx"), xlsx_input(&fixture("xlsx"))).unwrap();
            fs::write(
                dir.join("sales.ods"),
                ods::write(&fixture("ods"), Vec::new()).unwrap(),
            )
            .unwrap();
        }
        [cmd, file, state, format] if cmd == "check" => {
            let path = Path::new(file);
            let bytes = fs::read(path).unwrap();
            let book = if path.extension().is_some_and(|ext| ext == "osheet") {
                officina_grid::native::load(&bytes).unwrap()
            } else {
                read(&bytes, format)
            };
            check(&book, state, format);
        }
        _ => panic!("usage: sheets_pivot_order_fixture make DIR | check FILE STATE FORMAT"),
    }
}

#[test]
fn calculations_follow_persisted_manual_or_recalculated_value_order() {
    use officina_core::history::Op;

    for format in ["xlsx", "ods"] {
        let write = |book: &Workbook| {
            if format == "xlsx" {
                xlsx::write(book, Vec::new()).unwrap()
            } else {
                ods::write(book, Vec::new()).unwrap()
            }
        };
        let original = fixture(format);
        let bytes = if format == "xlsx" {
            xlsx_input(&original)
        } else {
            write(&original)
        };
        let mut book = read(&bytes, format);
        check(&book, "initial", format);
        book.set_value(at(1, 3), Value::Num(7.0));
        let mut reopened = read(&write(&book), format);
        reopened =
            officina_grid::native::load(&officina_grid::native::save_to_vec(&reopened)).unwrap();
        check(&reopened, "saved", format);
        reopened.set_value(at(1, 1), Value::Num(8.0));
        check(&reopened, "edited", format);
        let op = reopened
            .pivot_op(Some(0), Some(reopened.pivots()[0].clone()))
            .unwrap();
        let undo = op.apply(&mut reopened);
        check(&read(&write(&reopened), format), "refreshed", format);
        let redo = officina_grid::ops::GridOp::decode(&undo.encode())
            .unwrap()
            .apply(&mut reopened);
        check(&reopened, "edited", format);
        redo.apply(&mut reopened);
        check(&reopened, "refreshed", format);
    }
}

#[test]
fn ods_preserves_explicit_sort_modes_and_directions() {
    for (sort, labels) in [
        (
            AxisSort::Manual(["B", "C", "A"].map(Value::str).to_vec()),
            ["B", "C", "A"],
        ),
        (AxisSort::Source, ["A", "B", "C"]),
        (AxisSort::Name { descending: false }, ["A", "B", "C"]),
        (AxisSort::Name { descending: true }, ["C", "B", "A"]),
        (
            AxisSort::Data {
                caption: "Sales total".into(),
                descending: false,
            },
            ["B", "A", "C"],
        ),
    ] {
        let mut book = fixture("ods");
        let mut pivot = book.pivots()[0].clone();
        pivot.rows[0].sort = sort.clone();
        book.set_pivot(0, pivot).unwrap();
        let mut book = read(&ods::write(&book, Vec::new()).unwrap(), "ods");
        assert_eq!(book.pivots()[0].rows[0].sort, sort);
        book.refresh_pivot(0).unwrap();
        let actual = [2, 3, 4].map(|row| book.get(at(row, 5)));
        assert_eq!(actual, labels.map(Value::str));
    }
}

#[test]
fn xlsx_manual_positions_survive_temporarily_missing_members() {
    let mut book = fixture("xlsx");
    // Remove the only record for the middle member, and introduce a new member.
    book.set_value(at(1, 0), Value::str("D"));
    book.refresh_pivot(0).unwrap();
    assert_eq!(
        [2, 3, 4].map(|row| book.get(at(row, 5))),
        ["C", "B", "D"].map(Value::str)
    );
    let mut book = read(&xlsx::write(&book, Vec::new()).unwrap(), "xlsx");
    book.set_value(at(1, 0), Value::str("A"));
    book.refresh_pivot(0).unwrap();
    assert_eq!(
        [2, 3, 4].map(|row| book.get(at(row, 5))),
        ["C", "A", "B"].map(Value::str)
    );
}
