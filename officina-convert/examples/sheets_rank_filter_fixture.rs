//! Ranked filter fixtures and Save checkpoints for the Sheets desktop gate.
use std::{env, fs, path::Path};

use officina_convert::{ods, xlsx};
use officina_grid::filter::{AutoFilter, Cond, Rank};
use officina_grid::{CellAddr, Rect, Value, Workbook};

const RANGE: Rect = Rect {
    sheet: 0,
    r0: 0,
    c0: 0,
    r1: 7,
    c1: 1,
};
const SCORES: [f64; 6] = [9.0, 8.0, 8.0, 6.0, 4.0, 3.0];

fn score(row: u32) -> CellAddr {
    CellAddr::new(0, row, 1)
}

fn rule(format: &str) -> Rank {
    match format {
        "ods" => Rank::new(34, true, true).unwrap(),
        "xlsx" => Rank::new(2, false, false).unwrap(),
        _ => panic!("unexpected format {format}"),
    }
}

fn fixture(rank: Rank) -> Workbook {
    let mut book = Workbook::new();
    book.add_sheet("Scores").unwrap();
    book.set_value(CellAddr::new(0, 0, 0), Value::str("Entry"));
    book.set_value(score(0), Value::str("Score"));
    for (row, value) in (1..).zip(SCORES) {
        book.set_value(
            CellAddr::new(0, row, 0),
            Value::str(&format!("Entry {row}")),
        );
        book.set_value(score(row), Value::Num(value));
    }
    book.set_value(score(7), Value::str("n/a"));
    book.set_value(score(9), Value::str("Manually hidden"));
    book.sheet_mut(0).hidden = vec![(9, 9)];
    let state = book.filtered(
        0,
        Some(AutoFilter {
            rect: RANGE,
            conds: vec![(1, Cond::Rank(rank))],
        }),
    );
    book.replace_filter(state);
    book
}

fn read_foreign(bytes: &[u8], format: &str) -> Workbook {
    let (book, loaded) = match format {
        "xlsx" => xlsx::read(bytes).unwrap(),
        "ods" => ods::read(bytes).unwrap(),
        _ => panic!("unexpected file format {format}"),
    };
    let report = loaded.report;
    assert!(
        !report.lines().iter().any(|line| line.contains("filter")),
        "{report:?}"
    );
    book
}

fn check(file: &Path, state: &str, format: &str) {
    let bytes = fs::read(file).unwrap();
    let book = match file.extension().unwrap().to_str().unwrap() {
        "osheet" => officina_grid::native::load(&bytes).unwrap(),
        format => read_foreign(&bytes, format),
    };
    let (visible, edits): (&[u32], &[(u32, f64)]) = match (format, state) {
        ("xlsx", "initial") => (&[1, 2, 3], &[]),
        ("xlsx", "edited") => (&[2, 3], &[(1, 1.0)]),
        ("xlsx", "edited_again") => (&[3, 4], &[(1, 1.0), (2, 1.0)]),
        ("ods", "initial") => (&[4, 5, 6], &[]),
        ("ods", "edited") => (&[2, 3, 5, 6], &[(4, 9.0)]),
        ("ods", "edited_again") => (&[3, 5, 6], &[(4, 9.0), (2, 9.0)]),
        _ => panic!("unknown checkpoint {format} {state}"),
    };
    let sheet = book.sheet(0);
    assert_eq!(
        sheet.filter,
        Some(AutoFilter {
            rect: RANGE,
            conds: vec![(1, Cond::Rank(rule(format)))],
        })
    );
    let actual: Vec<_> = (1..=7).filter(|&row| !sheet.row_hidden(row)).collect();
    assert_eq!(actual, visible, "{format} {state}");
    assert!(!sheet.row_hidden(0));
    assert!(!sheet.row_hidden(8));
    assert!(sheet.row_hidden(9));
    assert_eq!(book.get(score(7)), Value::str("n/a"));
    assert_eq!(book.get(score(9)), Value::str("Manually hidden"));
    for (row, original) in (1..).zip(SCORES) {
        let expected = edits
            .iter()
            .find(|(r, _)| *r == row)
            .map_or(original, |(_, v)| *v);
        assert_eq!(
            book.get(score(row)),
            Value::Num(expected),
            "{format} row {row}"
        );
    }
}

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    match args.as_slice() {
        [command, dir] if command == "make" => {
            let dir = Path::new(dir);
            fs::write(
                dir.join("sales.ods"),
                ods::write(&fixture(rule("ods")), Vec::new()).unwrap(),
            )
            .unwrap();
            fs::write(
                dir.join("sales.xlsx"),
                xlsx::write(&fixture(rule("xlsx")), Vec::new()).unwrap(),
            )
            .unwrap();
        }
        [command, file, state, format] if command == "check" => {
            check(Path::new(file), state, format)
        }
        _ => panic!("usage: sheets_rank_filter_fixture make DIR | check FILE STATE FORMAT"),
    }
}

#[test]
fn each_rank_mode_round_trips_in_both_formats() {
    for (bottom, percent, keep) in [
        (false, false, vec![1, 2, 3]),
        (false, true, vec![1, 2, 3]),
        (true, false, vec![5, 6]),
        (true, true, vec![4, 5, 6]),
    ] {
        let rank = Rank::new(if percent { 34 } else { 2 }, percent, bottom).unwrap();
        let original = fixture(rank);
        for book in [
            read_foreign(&xlsx::write(&original, Vec::new()).unwrap(), "xlsx"),
            read_foreign(&ods::write(&original, Vec::new()).unwrap(), "ods"),
        ] {
            assert_eq!(book.filter_state(0), original.filter_state(0));
            let rows: Vec<_> = (1..=7)
                .filter(|&row| !book.sheet(0).row_hidden(row))
                .collect();
            assert_eq!(rows, keep);
            assert_eq!(
                book.filtered(0, book.sheet(0).filter.clone()),
                original.filter_state(0)
            );
        }
    }
}
