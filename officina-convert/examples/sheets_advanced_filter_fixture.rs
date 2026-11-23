//! Nested OpenDocument and appearance-based Excel filters for the desktop gate.

use std::{env, fs, path::Path};

use officina_convert::{ods, xlsx};
use officina_grid::condfmt::{icon_set, Cfvo, CondFormat, Dxf, Rule, RuleKind};
use officina_grid::eval::dates::DateSystem;
use officina_grid::filter::{
    AppearanceFilter, AutoFilter, CalendarSelection, Cond, DateGroup, FilterExpr, IconFilter,
};
use officina_grid::style::{PatternFill, Style};
use officina_grid::{CellAddr, Rect, Value, Workbook};

const RANGE: Rect = Rect {
    sheet: 0,
    r0: 0,
    c0: 0,
    r1: 7,
    c1: 3,
};
const ODS_SCORES: [[f64; 3]; 7] = [
    [8., 1., 8.],
    [1., 8., 8.],
    [1., 8., 8.],
    [8., 8., 8.],
    [1., 8., 1.],
    [1., 1., 8.],
    [8., 1., 1.],
];

fn at(row: u32, col: u32) -> CellAddr {
    CellAddr::new(0, row, col)
}

fn number(iso: &str) -> Value {
    Value::Num(DateSystem::Excel1900.iso(iso).unwrap().0)
}

fn pattern(name: &str) -> PatternFill {
    PatternFill::new(name, Some(0x90D290), Some(0xF1F9F1)).unwrap()
}

fn selected_icon() -> (u8, u8) {
    (icon_set("3Stars").unwrap(), 1)
}

fn filter(kind: &str) -> AutoFilter {
    let conds = if kind == "ods" {
        let leaf = |col| FilterExpr::Column(col, Cond::Compare(vec![(">=".into(), 5.0)], true));
        let expr = FilterExpr::Any(vec![leaf(1), FilterExpr::All(vec![leaf(2), leaf(3)])]);
        vec![(0, Cond::Expression(Box::new(expr)))]
    } else {
        let (set, index) = selected_icon();
        vec![
            (
                1,
                Cond::Calendar(CalendarSelection {
                    calendar: Some("thai".into()),
                    groups: vec![DateGroup::new(&[2024, 2]).unwrap()],
                    values: vec![],
                }),
            ),
            (
                2,
                Cond::Appearance(AppearanceFilter::Pattern(pattern("darkGrid"))),
            ),
            (
                3,
                Cond::Appearance(AppearanceFilter::Icon(
                    IconFilter::new(set, Some(index)).unwrap(),
                )),
            ),
        ]
    };
    AutoFilter { rect: RANGE, conds }
}

fn fixture(kind: &str) -> Workbook {
    let mut book = Workbook::new();
    book.add_sheet("Sales").unwrap();
    for (col, label) in ["Item", "Date or score", "Fill score", "Icon score"]
        .into_iter()
        .enumerate()
    {
        book.set_value(at(0, col as u32), Value::str(label));
    }
    for row in 1..=7 {
        book.set_value(at(row, 0), Value::Num(f64::from(row)));
        if kind == "ods" {
            for (col, score) in (1..).zip(ODS_SCORES[row as usize - 1]) {
                book.set_value(at(row, col), Value::Num(score));
            }
        } else {
            book.set_value(at(row, 1), number("2024-02-02"));
            book.set_value(at(row, 2), Value::Num(if row >= 5 { 2.0 } else { 8.0 }));
            book.set_value(at(row, 3), Value::Num(if row == 6 { 1.0 } else { 8.0 }));
        }
    }
    book.set_value(at(9, 0), Value::str("Manually hidden"));
    book.sheet_mut(0).hidden = vec![(9, 9)];
    if kind == "xlsx" {
        book.set_style(
            &Rect {
                r0: 1,
                c0: 1,
                c1: 1,
                ..RANGE
            },
            Style::default().with_num_fmt("yyyy-mm-dd"),
        );
        for (row, name) in [(5, "darkTrellis"), (6, "darkGrid"), (7, "darkGrid")] {
            let pattern = pattern(name);
            book.set_style(
                &Rect::cell(at(row, 2)),
                Style {
                    fill: pattern.preview(),
                    pattern: Some(pattern),
                    ..Style::default()
                },
            );
        }
        let hatch = pattern("darkGrid");
        let formula = book.cond_formula("=C2>=5", 0, (1, 2)).unwrap();
        book.add_cond_format(CondFormat {
            sheet: 0,
            ranges: vec![Rect {
                r0: 1,
                c0: 2,
                c1: 2,
                ..RANGE
            }],
            rules: vec![Rule {
                kind: RuleKind::Expression(formula),
                dxf: Dxf {
                    fill: hatch.preview(),
                    pattern: Some(hatch),
                    ..Dxf::default()
                },
                stop: false,
            }],
        })
        .unwrap();
        book.add_cond_format(CondFormat {
            sheet: 0,
            ranges: vec![Rect {
                r0: 1,
                c0: 3,
                c1: 3,
                ..RANGE
            }],
            rules: vec![Rule {
                kind: RuleKind::IconSet {
                    set: icon_set("3Arrows").unwrap(),
                    points: [0., 4., 6., 7.].map(|n| (Cfvo::Num(n), true)).to_vec(),
                    reverse: false,
                    show_value: true,
                    icons: Some(vec![
                        None,
                        Some((icon_set("4Arrows").unwrap(), 3)),
                        Some((icon_set("3Flags").unwrap(), 1)),
                        Some(selected_icon()),
                    ]),
                },
                dxf: Dxf::default(),
                stop: false,
            }],
        })
        .unwrap();
    }
    book.recalc();
    book.replace_filter(book.filtered(0, Some(filter(kind))));
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
    assert!(
        loaded
            .report
            .human()
            .iter()
            .all(|note| note.contains("patterned fills shown solid")),
        "{:?}",
        loaded.report.human()
    );
    book
}

fn check(book: &Workbook, phase: &str, kind: &str) {
    let edited = matches!(phase, "edited" | "reapplied");
    let visible = if phase == "reapplied" {
        vec![4, 7]
    } else {
        vec![1, 2, 3, 4, 7]
    };
    assert_eq!(book.sheet(0).filter, Some(filter(kind)));
    assert_eq!(
        (1..=7)
            .filter(|&r| !book.sheet(0).row_hidden(r))
            .collect::<Vec<_>>(),
        visible,
        "{kind} {phase}"
    );
    assert!(!book.sheet(0).row_hidden(8));
    assert!(book.sheet(0).row_hidden(9));
    assert_eq!(
        book.get(at(1, 4)),
        if phase == "initial" {
            Value::Empty
        } else {
            Value::Num(8.0)
        }
    );
    for col in 1..=3 {
        let value = if kind == "xlsx" && col == 1 {
            number(if edited { "1904-01-09" } else { "2024-02-02" })
        } else {
            Value::Num(if edited { 1.0 } else { 8.0 })
        };
        assert_eq!(book.get(at(col, col)), value, "{kind} {phase}, col {col}");
    }
    if kind == "xlsx" {
        assert_eq!(book.style(at(5, 2)).pattern, Some(pattern("darkTrellis")));
        assert_eq!(book.look(at(4, 2)).dxf.pattern, Some(pattern("darkGrid")));
        assert_eq!(
            book.look(at(3, 3)).icon,
            if edited { None } else { Some(selected_icon()) }
        );
        assert_eq!(book.look(at(7, 3)).icon, Some(selected_icon()));
        assert_eq!(book.look(at(6, 3)).icon, None);
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
        _ => panic!("usage: sheets_advanced_filter_fixture make DIR | check FILE PHASE FORMAT"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use officina_core::history::Op;
    use officina_grid::{native, ops::GridOp};

    #[test]
    fn nested_named_ods_filter_tracks_columns_at_a_nonzero_offset() {
        use officina_grid::{
            shift::{shift_op, Axis},
            tables::Table,
        };

        let mut book = fixture("ods");
        let filter = book.sheet_mut(0).filter.take().unwrap();
        let table = Table {
            name: "SalesData".into(),
            rect: RANGE,
            header: true,
            totals: false,
            columns: ["Item", "Date or score", "Fill score", "Icon score"]
                .map(Into::into)
                .to_vec(),
            style: None,
            stripes: false,
            filter: None,
        };
        let mut table = table;
        table.filter = Some(book.apply_table_filter(&table, filter));
        book.add_table(table).unwrap();

        // Insert before the table, and then between its condition columns.
        shift_op(&book, 0, Axis::Cols, 0, 1)
            .unwrap()
            .apply(&mut book);
        shift_op(&book, 0, Axis::Cols, 3, 1)
            .unwrap()
            .apply(&mut book);
        let before = book.table("SalesData").unwrap().clone();
        assert_eq!((before.rect.c0, before.rect.c1), (1, 5));
        let mut book = read(&write(&book, "ods"), "ods");
        let table = book.table("SalesData").unwrap().clone();
        assert_eq!(before.filter, table.filter);
        book.set_value(at(2, 4), Value::Num(1.0));
        let rect = table.filter_rect().unwrap();
        let conds = table
            .filter
            .as_ref()
            .unwrap()
            .conds
            .iter()
            .map(|(col, cond)| (rect.c0 + col, cond.clone()))
            .collect();
        let updated = book.apply_table_filter(&table, AutoFilter { rect, conds });
        assert_eq!(updated.hidden, vec![(2, 2), (5, 6)]);
    }

    #[test]
    fn advanced_filters_survive_foreign_and_native_save_reapply_and_undo() {
        for kind in ["ods", "xlsx"] {
            let reopen = |book: &Workbook| read(&write(book, kind), kind);
            let mut book = reopen(&fixture(kind));
            check(&book, "initial", kind);
            book.set_value(at(1, 4), Value::Num(8.0));
            book = reopen(&book);
            check(&book, "saved", kind);
            for col in 1..=3 {
                let value = if kind == "xlsx" && col == 1 {
                    number("1904-01-09")
                } else {
                    Value::Num(1.0)
                };
                book.set_value(at(col, col), value);
            }
            book.recalc();
            book = native::load(&native::save_to_vec(&book)).unwrap();
            book = reopen(&book);
            check(&book, "edited", kind);
            let apply = GridOp::Filter(book.filtered(0, Some(filter(kind))));
            let undo = GridOp::decode(&apply.encode()).unwrap().apply(&mut book);
            check(&reopen(&book), "reapplied", kind);
            let redo = undo.apply(&mut book);
            check(&book, "edited", kind);
            redo.apply(&mut book);
            check(&book, "reapplied", kind);
        }
    }
}
