//! Desktop checkpoints for calculated, grouped and chained pivot fields.

use std::{env, fs, path::Path};

use officina_convert::{ods, xlsx};
use officina_grid::addr::MAX_COLS;
use officina_grid::pivot::{
    Agg, CalculatedField, DataField, GroupField, GroupKind, ItemGroup, Pivot, PivotField,
};
use officina_grid::{CellAddr, Rect, Value, Workbook};

const INPUT: Rect = Rect {
    sheet: 0,
    r0: 0,
    c0: 0,
    r1: 4,
    c1: 2,
};
const GROUP: u32 = MAX_COLS;
const DERIVED: u32 = MAX_COLS + 1;

fn at(row: u32, col: u32) -> CellAddr {
    CellAddr::new(0, row, col)
}

fn named(fields: &[(&str, &[&str])]) -> GroupKind {
    GroupKind::Named(
        fields
            .iter()
            .map(|(name, members)| ItemGroup {
                name: (*name).into(),
                members: members.iter().map(|name| (*name).into()).collect(),
            })
            .collect(),
    )
}

fn fixture(kind: &str) -> Workbook {
    let mut book = Workbook::new();
    book.add_sheet("Sales").unwrap();
    for (col, header) in ["Region", "Net", "Unit Cost"].into_iter().enumerate() {
        book.set_value(at(0, col as u32), Value::str(header));
    }
    for (row, (region, net, cost)) in (1..).zip([
        ("North", 4.0, 2.0),
        ("West", 6.0, 3.0),
        ("South", 10.0, 2.0),
        ("North", 8.0, 4.0),
    ]) {
        book.set_value(at(row, 0), Value::str(region));
        book.set_value(at(row, 1), Value::Num(net));
        book.set_value(at(row, 2), Value::Num(cost));
    }
    let mut pivot = Pivot::new("Regional analysis", INPUT, at(0, 5));
    pivot.subtotals = false;
    let mappings = [
        ("North", "Domestic"),
        ("West", "Domestic"),
        ("South", "Export"),
    ];
    let kind_group = if kind == "ods" {
        named(&[("Domestic", &["North", "West"]), ("Export", &["South"])])
    } else {
        GroupKind::Discrete(
            mappings
                .map(|(key, label)| (Value::str(key), Value::str(label)))
                .to_vec(),
        )
    };
    pivot.groups.push(GroupField {
        field: GROUP,
        source: 0,
        name: "Zone".into(),
        kind: kind_group,
    });
    if kind == "ods" {
        pivot.groups.push(GroupField {
            field: DERIVED,
            source: GROUP,
            name: "Channel".into(),
            kind: named(&[("Managed", &["Domestic"]), ("Partner", &["Export"])]),
        });
        pivot.rows.push(PivotField::new(DERIVED));
        pivot.data.push(DataField {
            field: 1,
            agg: Agg::Auto,
            name: Some("Recorded Net".into()),
            show_as: Default::default(),
        });
    } else {
        pivot.calculations.push(CalculatedField {
            field: DERIVED,
            name: "Efficiency".into(),
            formula: "(Net/'Unit Cost')".into(),
        });
        pivot.rows.push(PivotField::new(GROUP));
        pivot.data.push(DataField {
            field: DERIVED,
            agg: Agg::Sum,
            name: Some("Net per Cost".into()),
            show_as: Default::default(),
        });
    }
    book.add_pivot(pivot).unwrap();

    if kind == "xlsx" {
        let mut bins = Pivot::new("Net brackets", INPUT, at(0, 9));
        bins.subtotals = false;
        bins.groups.push(GroupField {
            field: 1,
            source: 1,
            name: "Net".into(),
            kind: GroupKind::NumericBins {
                start: 0.0,
                end: 10.0,
                step: 5.0,
                items: ["<0", "0-4", "5-10", ">10"].map(Value::str).to_vec(),
            },
        });
        bins.rows.push(PivotField::new(1));
        bins.data.push(DataField {
            field: 1,
            agg: Agg::Sum,
            name: None,
            show_as: Default::default(),
        });
        book.add_pivot(bins).unwrap();
    }
    book
}

fn write(book: &Workbook, kind: &str) -> Vec<u8> {
    match kind {
        "ods" => ods::write(book, Vec::new()).unwrap(),
        "xlsx" => xlsx::write(book, Vec::new()).unwrap(),
        _ => panic!("unknown format {kind}"),
    }
}

fn read(bytes: &[u8], kind: &str) -> Workbook {
    let (book, loaded) = match kind {
        "ods" => ods::read(bytes).unwrap(),
        "xlsx" => xlsx::read(bytes).unwrap(),
        _ => panic!("unknown format {kind}"),
    };
    assert!(loaded.report.is_empty(), "{:?}", loaded.report.human());
    book
}

fn check(book: &Workbook, phase: &str, kind: &str) {
    let edited = matches!(phase, "edited" | "refreshed");
    let refreshed = phase == "refreshed";
    assert_eq!(
        book.get(at(1, 3)),
        if phase == "initial" {
            Value::Empty
        } else {
            Value::Num(7.0)
        }
    );
    assert_eq!(
        book.get(at(1, 1)),
        Value::Num(if edited { 8.0 } else { 4.0 })
    );
    let pivot = book.pivots().first().unwrap();
    let (labels, results) = if kind == "ods" {
        let channel = pivot
            .groups
            .iter()
            .find(|g| g.name.as_ref() == "Channel")
            .unwrap();
        let zone = pivot
            .groups
            .iter()
            .find(|g| g.name.as_ref() == "Zone")
            .unwrap();
        assert_eq!(channel.source, zone.field);
        assert_eq!(pivot.rows[0].field, channel.field);
        assert_eq!(pivot.data[0].agg, Agg::Auto);
        let sum = if refreshed { 22.0 } else { 18.0 };
        (
            ["Managed", "Partner", "Grand Total"],
            [sum, 10.0, sum + 10.0],
        )
    } else {
        assert_eq!(book.pivots().len(), 2);
        assert_eq!(pivot.calculations[0].formula.as_ref(), "(Net/'Unit Cost')");
        assert_eq!(pivot.data[0].field, pivot.calculations[0].field);
        let some = if refreshed { 22.0 } else { 18.0 };
        if refreshed {
            assert_eq!(book.get(at(1, 9)), Value::str("5-10"));
            assert_eq!(book.get(at(1, 10)), Value::Num(32.0));
            assert_eq!(book.get(at(2, 9)), Value::str("Grand Total"));
        } else {
            assert_eq!(book.get(at(1, 9)), Value::str("0-4"));
            assert_eq!(book.get(at(1, 10)), Value::Num(4.0));
            assert_eq!(book.get(at(2, 9)), Value::str("5-10"));
            assert_eq!(book.get(at(2, 10)), Value::Num(24.0));
        }
        (
            ["Domestic", "Export", "Grand Total"],
            [some / 9.0, 5.0, (some + 10.0) / 11.0],
        )
    };
    for (row, (label, result)) in (1..).zip(labels.into_iter().zip(results)) {
        assert_eq!(book.get(at(row, 5)), Value::str(label));
        let Value::Num(actual) = book.get(at(row, 6)) else {
            panic!("missing {label} for {phase}")
        };
        assert!(
            (actual - result).abs() < 1e-10,
            "{phase} {label}: {actual} != {result}"
        );
    }
}

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    match args.as_slice() {
        [command, dir] if command == "make" => {
            for kind in ["ods", "xlsx"] {
                fs::write(
                    Path::new(dir).join(format!("sales.{kind}")),
                    write(&fixture(kind), kind),
                )
                .unwrap();
            }
        }
        [command, file, phase, kind] if command == "check" => {
            let bytes = fs::read(file).unwrap();
            let book = if file.ends_with(".osheet") {
                officina_grid::native::load(&bytes).unwrap()
            } else {
                read(&bytes, kind)
            };
            check(&book, phase, kind);
        }
        _ => panic!("usage: sheets_pivot_sources_fixture make DIR | check FILE PHASE FORMAT"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use officina_core::history::Op;
    use officina_grid::native;

    fn refresh(book: &mut Workbook, i: usize) {
        book.pivot_op(Some(i), Some(book.pivots()[i].clone()))
            .unwrap()
            .apply(book);
    }

    #[test]
    fn grouped_and_calculated_pivots_reopen_before_and_after_refresh() {
        for kind in ["ods", "xlsx"] {
            let reopen = |book: &Workbook| read(&write(book, kind), kind);
            let mut book = reopen(&fixture(kind));
            check(&book, "initial", kind);
            book.set_value(at(1, 3), Value::Num(7.0));
            book = reopen(&book);
            check(&book, "saved", kind);
            book.set_value(at(1, 1), Value::Num(8.0));
            book = native::load(&native::save_to_vec(&book)).unwrap();
            book = reopen(&book);
            check(&book, "edited", kind);
            for i in 0..book.pivots().len() {
                refresh(&mut book, i);
            }
            check(&reopen(&book), "refreshed", kind);
        }
    }

    #[test]
    fn cascaded_groups_and_automatic_aggregate_follow_edited_source_members() {
        let mut book = read(&write(&fixture("ods"), "ods"), "ods");
        book.set_value(at(2, 1), Value::str("Pending"));
        book.set_value(at(2, 0), Value::str("South"));
        refresh(&mut book, 0);
        assert_eq!(book.get(at(1, 6)), Value::Num(2.0));
        assert_eq!(book.get(at(2, 6)), Value::Num(2.0));
        assert_eq!(book.get(at(3, 6)), Value::Num(4.0));
        let reopened = read(&write(&book, "ods"), "ods");
        assert_eq!(reopened.pivots()[0].data[0].agg, Agg::Auto);
        assert_eq!(reopened.get(at(3, 6)), Value::Num(4.0));
    }

    #[test]
    fn chained_calculations_use_subtotal_inputs_and_refresh_is_undoable() {
        let mut book = fixture("xlsx");
        let outer = MAX_COLS + 2;
        let mut pivot = book.pivots()[0].clone();
        pivot.calculations.push(CalculatedField {
            field: outer,
            name: "Score".into(),
            formula: "Efficiency^2 + 5%".into(),
        });
        pivot.data[0].field = outer;
        book.pivot_op(Some(0), Some(pivot))
            .unwrap()
            .apply(&mut book);
        book = read(&write(&book, "xlsx"), "xlsx");
        let old = book.get(at(3, 6));
        assert_eq!(old, Value::Num((28.0f64 / 11.0).powi(2) + 0.05));
        book.set_value(at(1, 1), Value::Num(8.0));
        let undo = book
            .pivot_op(Some(0), Some(book.pivots()[0].clone()))
            .unwrap()
            .apply(&mut book);
        let new = book.get(at(3, 6));
        assert_eq!(new, Value::Num((32.0f64 / 11.0).powi(2) + 0.05));
        let redo = undo.apply(&mut book);
        assert_eq!(book.get(at(3, 6)), old);
        redo.apply(&mut book);
        assert_eq!(book.get(at(3, 6)), new);
    }

    #[test]
    fn edited_nonnumeric_source_does_not_replace_saved_numeric_bins() {
        let mut book = fixture("xlsx");
        book.set_value(at(1, 1), Value::str("Pending"));
        let book = read(&write(&book, "xlsx"), "xlsx");
        let bins = book.pivots()[1].clone();
        assert!(book.pivot_op(Some(1), Some(bins)).is_err());
        assert_eq!(book.get(at(1, 10)), Value::Num(4.0));
        assert_eq!(book.get(at(2, 10)), Value::Num(24.0));
    }

    #[test]
    fn unsupported_formula_and_circular_group_keep_the_saved_cells() {
        use officina_convert::zip::{Archive, ZipWriter};
        for (kind, part, old, new) in [
            (
                "ods",
                "content.xml",
                "table:source-field-name=\"Zone\"><table:data-pilot-group",
                "table:source-field-name=\"Channel\"><table:data-pilot-group",
            ),
            (
                "xlsx",
                "xl/pivotCache/pivotCacheDefinition1.xml",
                "(Net/'Unit Cost')",
                "SUM(Net)",
            ),
        ] {
            let bytes = write(&fixture(kind), kind);
            let parts = Archive::open(&bytes).unwrap();
            let mut out = ZipWriter::new(Vec::new());
            for entry in &parts.entries {
                let mut data = parts.read(entry).unwrap();
                if entry.name == part {
                    let xml = String::from_utf8(data).unwrap();
                    assert!(xml.contains(old), "fixture missing {old}");
                    data = xml.replacen(old, new, 1).into_bytes();
                }
                out.add(&entry.name, &data).unwrap();
            }
            let bytes = out.finish().unwrap();
            let (book, loaded) = if kind == "ods" {
                ods::read(&bytes).unwrap()
            } else {
                xlsx::read(&bytes).unwrap()
            };
            assert!(loaded
                .report
                .human()
                .iter()
                .any(|s| s.contains("cells kept as values")));
            assert!(book
                .pivots()
                .iter()
                .all(|p| p.name.as_ref() != "Regional analysis"));
            assert_eq!(
                book.get(at(1, 5)),
                Value::str(if kind == "ods" { "Managed" } else { "Domestic" })
            );
        }
    }
}
