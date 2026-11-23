//! Independent table filters and a worksheet filter for the desktop gate.

use std::{env, fs, path::Path};

use officina_convert::{ods, xlsx};
use officina_grid::filter::{AutoFilter, Cond};
use officina_grid::tables::{Table, TableFilter};
use officina_grid::{CellAddr, Rect, Value, Workbook};

fn at(row: u32, col: u32) -> CellAddr {
    CellAddr::new(0, row, col)
}

fn table(wb: &mut Workbook, name: &str, row: u32, col: u32, statuses: &[&str], keep: &str) {
    let rect = Rect {
        sheet: 0,
        r0: row,
        r1: row + statuses.len() as u32,
        c0: col,
        c1: col + 1,
    };
    for (c, name) in [(col, "Item"), (col + 1, "Status")] {
        wb.set_value(at(row, c), Value::str(name));
    }
    for (offset, status) in (1..).zip(statuses) {
        wb.set_value(at(row + offset, col), Value::Num(f64::from(offset)));
        wb.set_value(at(row + offset, col + 1), Value::str(status));
    }
    let mut table = Table {
        name: name.into(),
        rect,
        header: true,
        totals: false,
        columns: vec!["Item".into(), "Status".into()],
        style: None,
        stripes: false,
        filter: None,
    };
    let filter = AutoFilter {
        rect,
        conds: vec![(col + 1, Cond::Values(vec![keep.into()]))],
    };
    table.filter = Some(wb.apply_table_filter(&table, filter));
    wb.add_table(table).unwrap();
}

fn fixture() -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("Orders").unwrap();
    table(
        &mut wb,
        "West",
        0,
        0,
        &["Open", "Closed", "Open", "Closed"],
        "Open",
    );
    table(
        &mut wb,
        "East",
        7,
        0,
        &["Ready", "Pending", "Ready", "Pending"],
        "Ready",
    );

    wb.set_value(at(14, 3), Value::str("Region"));
    wb.set_value(at(15, 3), Value::str("North"));
    wb.set_value(at(16, 3), Value::str("South"));
    let sheet_filter = AutoFilter {
        rect: Rect {
            sheet: 0,
            r0: 14,
            r1: 16,
            c0: 3,
            c1: 3,
        },
        conds: vec![(3, Cond::Values(vec!["North".into()]))],
    };
    wb.sheet_mut(0).hidden = vec![(20, 20)];
    wb.replace_filter(wb.filtered(0, Some(sheet_filter)));
    wb
}

fn write(wb: &Workbook, format: &str) -> Vec<u8> {
    match format {
        "ods" => ods::write(wb, Vec::new()).unwrap(),
        "xlsm" => xlsx::write_xlsm(wb, Vec::new()).unwrap(),
        "xlsx" => xlsx::write(wb, Vec::new()).unwrap(),
        _ => panic!("unknown workbook format {format}"),
    }
}

fn read(bytes: &[u8], format: &str) -> Workbook {
    let (wb, loaded) = match format {
        "ods" => ods::read(bytes).unwrap(),
        _ => xlsx::read(bytes).unwrap(),
    };
    let notes = loaded.report.human();
    assert!(!notes.iter().any(|n| n.contains("filter")), "{notes:?}");
    if format == "xlsm" {
        assert!(wb.macros().is_some());
        assert!(
            notes
                .iter()
                .any(|n| n.to_ascii_lowercase().contains("inert")),
            "{notes:?}"
        );
    }
    wb
}

fn table_state<'a>(wb: &'a Workbook, name: &str) -> &'a TableFilter {
    wb.table(name).unwrap().filter.as_ref().unwrap()
}

fn check(wb: &Workbook, stage: &str) {
    let (west, west_rule): (&[(u32, u32)], &[_]) = match stage {
        "initial" | "saved" | "edited" => {
            (&[(2, 2), (4, 4)], &[(1, Cond::Values(vec!["Open".into()]))])
        }
        "reapplied" => (&[(1, 2), (4, 4)], &[(1, Cond::Values(vec!["Open".into()]))]),
        "cleared" => (&[], &[]),
        _ => panic!("unknown stage {stage}"),
    };
    let state = table_state(wb, "West");
    assert_eq!(state.conds, west_rule);
    assert_eq!(state.hidden, west);
    let east = table_state(wb, "East");
    assert_eq!(east.conds, vec![(1, Cond::Values(vec!["Ready".into()]))]);
    assert_eq!(east.hidden, vec![(2, 2), (4, 4)]);
    assert_eq!(
        wb.sheet(0).filter.as_ref().unwrap().conds,
        vec![(3, Cond::Values(vec!["North".into()]))]
    );
    assert_eq!(wb.sheet(0).hidden, vec![(16, 16), (20, 20)]);
    assert_eq!(
        wb.get(at(1, 4)),
        if stage == "initial" {
            Value::Empty
        } else {
            Value::Num(8.0)
        }
    );
    let edited = matches!(stage, "edited" | "reapplied" | "cleared");
    assert_eq!(
        wb.get(at(1, 1)),
        Value::str(if edited { "no" } else { "Open" })
    );
    for r in 0..=20 {
        let want = west.iter().any(|&(a, b)| (a..=b).contains(&r)) || [9, 11, 16, 20].contains(&r);
        assert_eq!(wb.sheet(0).row_hidden(r), want, "{stage} row {r}");
    }
}

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    match args.as_slice() {
        [command, dir, macro_source] if command == "make" => {
            let source = fs::read(macro_source).unwrap();
            let mut macros = xlsx::read(&source).unwrap().0.macros().unwrap().clone();
            macros.sheet_codes.retain(|(sheet, _)| *sheet == 0);
            for format in ["ods", "xlsm", "xlsx"] {
                let mut wb = fixture();
                if format == "xlsm" {
                    wb.set_macros(macros.clone()).unwrap();
                }
                fs::write(
                    Path::new(dir).join(format!("tables.{format}")),
                    write(&wb, format),
                )
                .unwrap();
            }
        }
        [command, file, stage, format] if command == "check" => {
            let bytes = fs::read(file).unwrap();
            let wb = if file.ends_with(".osheet") {
                officina_grid::native::load(&bytes).unwrap()
            } else {
                read(&bytes, format)
            };
            check(&wb, stage);
        }
        _ => panic!(
            "usage: sheets_table_filter_fixture make DIR VBA_SOURCE | check FILE STAGE FORMAT"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use officina_core::history::Op;
    use officina_grid::{native, ops::GridOp};

    #[test]
    fn native_and_foreign_filters_keep_their_scope() {
        for format in ["ods", "xlsx"] {
            let mut wb = read(&write(&fixture(), format), format);
            check(&wb, "initial");
            wb.set_value(at(1, 4), Value::Num(8.0));
            wb = read(&write(&wb, format), format);
            check(&wb, "saved");
            wb.set_value(at(1, 1), Value::str("no"));
            check(&wb, "edited");

            let mut tables = wb.tables().to_vec();
            let west = tables
                .iter_mut()
                .find(|t| t.name.as_ref() == "West")
                .unwrap();
            west.filter = Some(wb.apply_table_filter(west, west.auto_filter().unwrap()));
            let op = GridOp::decode(&GridOp::Tables(tables).encode()).unwrap();
            op.apply(&mut wb);
            check(&wb, "reapplied");
            check(
                &native::load(&native::save_to_vec(&wb)).unwrap(),
                "reapplied",
            );
            check(&read(&write(&wb, format), format), "reapplied");

            let mut tables = wb.tables().to_vec();
            tables
                .iter_mut()
                .find(|t| t.name.as_ref() == "West")
                .unwrap()
                .filter = Some(TableFilter::default());
            let undo = GridOp::Tables(tables).apply(&mut wb);
            check(&wb, "cleared");
            undo.apply(&mut wb);
            check(&wb, "reapplied");
        }
    }

    #[test]
    fn overlapping_row_masks_are_independent_even_after_export() {
        for format in ["ods", "xlsx"] {
            let mut wb = Workbook::new();
            wb.add_sheet("Orders").unwrap();
            table(&mut wb, "West", 0, 0, &["Open", "Closed", "Closed"], "Open");
            table(
                &mut wb,
                "East",
                0,
                3,
                &["Pending", "Pending", "Ready"],
                "Ready",
            );
            // Both tables hide row 2, and each hides another row separately.
            wb = read(&write(&wb, format), format);
            assert_eq!(wb.sheet(0).hidden_rows(), vec![(1, 3)]);
            wb.set_table_filter("West", Some(TableFilter::default()))
                .unwrap();
            assert_eq!(wb.sheet(0).hidden_rows(), vec![(1, 2)]);
            wb.set_table_filter("East", Some(TableFilter::default()))
                .unwrap();
            assert!(wb.sheet(0).hidden_rows().is_empty());
        }
    }
}
