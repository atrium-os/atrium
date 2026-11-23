//! Paired desktop fixtures for pivot display calculations, grouping, and refresh.

use std::{env, fs, path::Path};

use officina_convert::zip::{Archive, ZipWriter};
use officina_convert::{ods, xlsx};
use officina_grid::addr::MAX_COLS;
use officina_grid::pivot::{
    Agg, BaseItem, DataField, GroupField, GroupKind, ItemGroup, Pivot, PivotField, ReferenceCalc,
    ShowAs,
};
use officina_grid::{CellAddr, Rect, Value, Workbook};

fn at(row: u32, col: u32) -> CellAddr {
    CellAddr::new(0, row, col)
}

fn fixture(format: &str) -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("Sales").unwrap();
    wb.set_value(at(0, 0), Value::str("Store"));
    wb.set_value(at(0, 1), Value::str("Sales"));
    for (row, (store, sales)) in (1..).zip([("A", 10.0), ("B", 9.0), ("C", 30.0)]) {
        wb.set_value(at(row, 0), Value::str(store));
        wb.set_value(at(row, 1), Value::Num(sales));
    }
    let source = Rect {
        sheet: 0,
        r0: 0,
        c0: 0,
        r1: 3,
        c1: 1,
    };
    let mut pivot = Pivot::new("SalesPivot", source, at(0, 5));
    let (field, base) = if format == "ods" {
        pivot.groups.push(GroupField {
            field: MAX_COLS,
            source: 0,
            name: "Region".into(),
            kind: GroupKind::Named(vec![
                ItemGroup {
                    name: "North".into(),
                    members: vec!["A".into(), "B".into()],
                },
                ItemGroup {
                    name: "South".into(),
                    members: vec!["C".into()],
                },
            ]),
        });
        (MAX_COLS, "North")
    } else {
        (0, "A")
    };
    pivot.rows.push(PivotField::new(field));
    pivot.subtotals = false;
    let field_value = |name: &str, show_as| DataField {
        field: 1,
        agg: Agg::Sum,
        name: Some(name.into()),
        show_as,
    };
    pivot
        .data
        .push(field_value("Share", ShowAs::PercentOfTotal));
    pivot.data.push(field_value(
        "Change",
        ShowAs::Reference {
            calc: ReferenceCalc::Difference,
            field,
            item: BaseItem::Named(Value::str(base)),
        },
    ));
    if format == "xlsx" {
        pivot.data.push(field_value(
            "Rank",
            ShowAs::Rank {
                field,
                descending: true,
            },
        ));
    }
    wb.add_pivot(pivot).unwrap();
    wb
}

fn xlsx_input(book: &Workbook) -> Vec<u8> {
    // Excel addresses a named base by its pivot-item position. Deliberately use
    // an order different from sharedItems to catch lookups into the wrong list.
    let bytes = xlsx::write(book, Vec::new()).unwrap();
    let zip = Archive::open(&bytes).unwrap();
    let mut out = ZipWriter::new(Vec::new());
    for entry in &zip.entries {
        let data = zip.read(entry).unwrap();
        if entry.name == "xl/pivotTables/pivotTable1.xml" {
            let xml = String::from_utf8(data).unwrap();
            let items = r#"<item x="0"/><item x="1"/><item x="2"/>"#;
            assert!(xml.contains(items));
            let xml = xml
                .replace(items, r#"<item x="1"/><item x="0"/><item x="2"/>"#)
                .replace(
                    r#"showDataAs="difference" baseField="0" baseItem="0""#,
                    r#"showDataAs="difference" baseField="0" baseItem="1""#,
                );
            out.add(&entry.name, xml.as_bytes()).unwrap();
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
    assert!(
        !loaded
            .report
            .lines()
            .iter()
            .any(|note| note.contains("pivot")),
        "{:?}",
        loaded.report
    );
    wb
}

fn check(book: &Workbook, state: &str, format: &str) {
    assert_eq!(book.pivots(), fixture(format).pivots());
    let changed = matches!(state, "edited" | "refreshed");
    assert_eq!(
        book.get(at(1, 1)),
        Value::Num(if changed { 8.0 } else { 10.0 })
    );
    assert_eq!(
        book.get(at(1, 3)),
        if state == "initial" {
            Value::Empty
        } else {
            Value::Num(7.0)
        }
    );

    let refreshed = state == "refreshed";
    let divisor = if refreshed { 47.0 } else { 49.0 };
    let rows = match format {
        "ods" => vec![
            ("North", if refreshed { 17.0 } else { 19.0 }, None),
            ("South", 30.0, Some(if refreshed { 13.0 } else { 11.0 })),
        ],
        "xlsx" => vec![
            ("A", if refreshed { 8.0 } else { 10.0 }, None),
            ("B", 9.0, Some(if refreshed { 1.0 } else { -1.0 })),
            ("C", 30.0, Some(if refreshed { 22.0 } else { 20.0 })),
        ],
        _ => panic!("unexpected format {format}"),
    };
    for (i, (label, share, change)) in rows.iter().enumerate() {
        let row = 2 + i as u32;
        assert_eq!(book.get(at(row, 5)), Value::str(label));
        assert_eq!(book.get(at(row, 6)), Value::Num(share / divisor));
        assert!(book.formatted(at(row, 6)).text.ends_with('%'));
        assert_eq!(
            book.get(at(row, 7)),
            change.map_or(Value::Empty, Value::Num)
        );
        if format == "xlsx" {
            let ranks = if refreshed {
                [3.0, 2.0, 1.0]
            } else {
                [2.0, 3.0, 1.0]
            };
            assert_eq!(book.get(at(row, 8)), Value::Num(ranks[i]));
        }
    }
    let last = 2 + rows.len() as u32;
    assert_eq!(book.get(at(last, 5)), Value::str("Grand Total"));
    assert_eq!(book.get(at(last, 6)), Value::Num(1.0));
}

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    match args.as_slice() {
        [cmd, dir] if cmd == "make" => {
            let dir = Path::new(dir);
            fs::write(
                dir.join("sales.ods"),
                ods::write(&fixture("ods"), Vec::new()).unwrap(),
            )
            .unwrap();
            fs::write(dir.join("sales.xlsx"), xlsx_input(&fixture("xlsx"))).unwrap();
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
        _ => panic!("usage: sheets_pivot_display_fixture make DIR | check FILE STATE FORMAT"),
    }
}

#[test]
fn paired_pivots_survive_native_and_foreign_saves_and_refresh() {
    for format in ["ods", "xlsx"] {
        let write = |wb: &Workbook| match format {
            "ods" => ods::write(wb, Vec::new()).unwrap(),
            _ => xlsx::write(wb, Vec::new()).unwrap(),
        };
        let original = fixture(format);
        let bytes = if format == "xlsx" {
            xlsx_input(&original)
        } else {
            write(&original)
        };
        let mut loaded = read(&bytes, format);
        check(&loaded, "initial", format);
        loaded.set_value(at(1, 3), Value::Num(7.0));
        let mut reopened = read(&write(&loaded), format);
        reopened =
            officina_grid::native::load(&officina_grid::native::save_to_vec(&reopened)).unwrap();
        check(&reopened, "saved", format);
        reopened.set_value(at(1, 1), Value::Num(8.0));
        check(&reopened, "edited", format);
        use officina_core::history::Op;
        let pivot = reopened.pivots()[0].clone();
        let refresh = reopened.pivot_op(Some(0), Some(pivot)).unwrap();
        let undo = refresh.apply(&mut reopened);
        check(&read(&write(&reopened), format), "refreshed", format);
        let redo = officina_grid::ops::GridOp::decode(&undo.encode())
            .unwrap()
            .apply(&mut reopened);
        check(&reopened, "edited", format);
        redo.apply(&mut reopened);
        check(&reopened, "refreshed", format);
    }
}
