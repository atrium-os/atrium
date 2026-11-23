//! Excel calendar groups and dynamic date rules for the Sheets desktop gate.

use std::{env, fs, path::Path};

use officina_convert::xlsx;
use officina_grid::eval::dates::DateSystem;
use officina_grid::filter::{
    AutoFilter, CalendarSelection, Cond, DateBounds, DateGroup, DynamicDate,
};
use officina_grid::style::Style;
use officina_grid::{CellAddr, Rect, Value, Workbook};

const RANGE: Rect = Rect {
    sheet: 0,
    r0: 0,
    c0: 0,
    r1: 9,
    c1: 2,
};

fn cell(row: u32, col: u32) -> CellAddr {
    CellAddr::new(0, row, col)
}

fn system(epoch: &str) -> DateSystem {
    match epoch {
        "1900" => DateSystem::Excel1900,
        "1904" => DateSystem::Excel1904,
        _ => panic!("unknown date system {epoch}"),
    }
}

fn date(system: DateSystem, iso: &str) -> Value {
    Value::Num(system.iso(iso).unwrap().0)
}

fn rules(today: f64) -> AutoFilter {
    let groups = [[2024, 2].as_slice(), &[2022, 4, 30, 9, 3, 15]]
        .into_iter()
        .map(|parts| DateGroup::new(parts).unwrap())
        .collect();
    let calendar = CalendarSelection {
        groups,
        values: vec![String::new(), "pending".into()],
        calendar: Some("gregorian".into()),
    };
    let today = DynamicDate::parse("today", DateBounds::new(today, today + 1.0)).unwrap();
    AutoFilter {
        rect: RANGE,
        conds: vec![(1, Cond::Calendar(calendar)), (2, Cond::DynamicDate(today))],
    }
}

fn source(system: DateSystem, today: f64) -> [(Value, Value); 9] {
    [
        (date(system, "2024-02-02"), Value::Num(today + 0.125)),
        (date(system, "2024-02-03"), Value::Num(today + 0.5)),
        (date(system, "2024-02-29"), Value::Num(today + 0.875)),
        (date(system, "2024-03-01"), Value::Num(today)),
        (date(system, "2024-02-01"), Value::Num(today - 1.0)),
        (Value::str("Pending"), Value::Num(today)),
        (Value::Empty, Value::Num(today)),
        (date(system, "2022-04-30T09:03:15"), Value::Num(today)),
        (date(system, "2024-02-02"), Value::Num(today)),
    ]
}

fn fixture(system: DateSystem, today: f64) -> Workbook {
    let mut book = Workbook::with_date_system(system);
    book.add_sheet("Deliveries").unwrap();
    for (col, header) in ["Item", "Ordered", "Delivered"].into_iter().enumerate() {
        book.set_value(cell(0, col as u32), Value::str(header));
    }
    let style = Style::default().with_num_fmt("yyyy-mm-dd");
    book.set_style(
        &Rect {
            sheet: 0,
            r0: 1,
            c0: 1,
            r1: 8,
            c1: 2,
        },
        style.clone(),
    );
    book.set_style(
        &Rect::cell(cell(8, 1)),
        style.clone().with_num_fmt("yyyy-mm-dd hh:mm:ss"),
    );
    book.set_style(&Rect::cell(cell(9, 2)), style);
    for (row, (ordered, delivered)) in (1..).zip(source(system, today)) {
        book.set_value(cell(row, 0), Value::str(&format!("Item {row}")));
        book.set_value(cell(row, 1), ordered);
        book.set_value(cell(row, 2), delivered);
    }
    book.set_value(cell(11, 0), Value::str("Manually hidden"));
    // Persist Excel's last evaluated rows independently from the clock.
    book.sheet_mut(0).hidden = vec![(4, 5), (9, 9), (11, 11)];
    book.sheet_mut(0).filter = Some(rules(today));
    book
}

fn read(bytes: &[u8]) -> Workbook {
    let (book, loaded) = xlsx::read(bytes).unwrap();
    assert!(loaded.report.is_empty(), "{:?}", loaded.report.human());
    book
}

fn check(book: &Workbook, state: &str, system: DateSystem, today: f64) {
    let (visible, edited): (&[u32], bool) = match state {
        "initial" | "saved" => (&[1, 2, 3, 6, 7, 8], false),
        "edited" => (&[1, 2, 3, 6, 7, 8], true),
        "reapplied" => (&[3, 6, 7, 8], true),
        _ => panic!("unknown checkpoint {state}"),
    };
    assert_eq!(book.date_system(), system);
    let sheet = book.sheet(0);
    assert_eq!(sheet.filter, Some(rules(today)));
    let actual: Vec<_> = (1..=9).filter(|&r| !sheet.row_hidden(r)).collect();
    assert_eq!(actual, visible, "{system:?} {state}");
    assert!(!sheet.row_hidden(0));
    assert!(!sheet.row_hidden(10));
    assert!(sheet.row_hidden(11));
    assert_eq!(book.get(cell(11, 0)), Value::str("Manually hidden"));
    assert_eq!(
        book.get(cell(1, 3)),
        if state == "initial" {
            Value::Empty
        } else {
            Value::Num(8.0)
        }
    );
    let replacement = date(system, "1904-01-09");
    for (row, (mut ordered, mut delivered)) in (1..).zip(source(system, today)) {
        if edited && row == 1 {
            ordered = replacement.clone();
        }
        if edited && row == 2 {
            delivered = replacement.clone();
        }
        assert_eq!(book.get(cell(row, 1)), ordered, "ordered row {row}");
        assert_eq!(book.get(cell(row, 2)), delivered, "delivered row {row}");
    }
}

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    match args.as_slice() {
        [command, dir] if command == "make" => {
            for epoch in ["1900", "1904"] {
                let system = system(epoch);
                let book = fixture(system, system.now().floor());
                fs::write(
                    Path::new(dir).join(format!("dates-{epoch}.xlsx")),
                    xlsx::write(&book, Vec::new()).unwrap(),
                )
                .unwrap();
            }
        }
        [command, file, state, epoch] if command == "check" => {
            let bytes = fs::read(file).unwrap();
            let book = if file.ends_with(".osheet") {
                officina_grid::native::load(&bytes).unwrap()
            } else {
                read(&bytes)
            };
            let system = system(epoch);
            check(&book, state, system, system.now().floor());
        }
        _ => panic!("usage: sheets_date_filter_fixture make DIR | check FILE STATE EPOCH"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use officina_convert::zip::{Archive, ZipWriter};
    use officina_core::history::Op;
    use officina_grid::{native, ops::GridOp};

    fn write(book: &Workbook) -> Vec<u8> {
        xlsx::write(book, Vec::new()).unwrap()
    }

    #[test]
    fn both_epochs_keep_live_rules_through_native_undo_and_foreign_save() {
        for system in [DateSystem::Excel1900, DateSystem::Excel1904] {
            let today = system.now().floor();
            let bytes = write(&fixture(system, today));
            let mut book = read(&bytes);
            check(&book, "initial", system, today);
            assert_eq!(
                book.filtered(0, book.sheet(0).filter.clone()).hidden,
                book.sheet(0).hidden
            );
            book.set_value(cell(1, 3), Value::Num(8.0));
            book = read(&write(&book));
            check(&book, "saved", system, today);
            book.set_value(cell(1, 1), date(system, "1904-01-09"));
            book.set_value(cell(2, 2), date(system, "1904-01-09"));
            book = native::load(&native::save_to_vec(&book)).unwrap();
            check(&book, "edited", system, today);

            let op = GridOp::Filter(book.filtered(0, book.sheet(0).filter.clone()));
            let undo = GridOp::decode(&op.encode()).unwrap().apply(&mut book);
            check(&read(&write(&book)), "reapplied", system, today);
            let redo = undo.apply(&mut book);
            check(&book, "edited", system, today);
            redo.apply(&mut book);
            check(&book, "reapplied", system, today);
        }
    }

    #[test]
    fn stored_iso_bounds_survive_save_and_reapply_uses_the_current_date() {
        let system = DateSystem::Excel1904;
        let today = system.now().floor();
        let bytes = write(&fixture(system, today));
        let archive = Archive::open(&bytes).unwrap();
        let mut out = ZipWriter::new(Vec::new());
        for entry in &archive.entries {
            let mut bytes = archive.read(entry).unwrap();
            if entry.name == "xl/worksheets/sheet1.xml" {
                let xml = String::from_utf8(bytes).unwrap();
                let range = format!(
                    r#"<dynamicFilter type="today" val="{today}" maxVal="{}"/>"#,
                    today + 1.0
                );
                assert!(xml.contains(&range), "{xml}");
                bytes = xml.replace(&range, r#"<dynamicFilter type="today" valIso="2024-02-29T00:00:00" maxValIso="2024-03-01T00:00:00"/>"#).into_bytes();
            }
            out.add(&entry.name, &bytes).unwrap();
        }
        let mut book = read(&out.finish().unwrap());
        let old_day = system.date(2024.0, 2.0, 29.0).unwrap();
        assert_eq!(book.sheet(0).filter, Some(rules(old_day)));
        book = read(&write(&book));
        assert_eq!(book.sheet(0).filter, Some(rules(old_day)));
        assert_eq!(book.sheet(0).hidden, vec![(4, 5), (9, 9), (11, 11)]);
        let reapplied = book.filtered(0, book.sheet(0).filter.clone());
        assert_eq!(reapplied.filter, Some(rules(today)));
        assert_eq!(reapplied.hidden, book.sheet(0).hidden);
    }
}
