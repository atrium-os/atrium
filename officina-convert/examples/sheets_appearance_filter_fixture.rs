//! Excel color and icon filters for the Sheets desktop gate.

use std::{env, fs, path::Path};

use officina_convert::xlsx;
use officina_grid::condfmt::{icon_set, Cfvo, CondFormat, Dxf, Rule, RuleKind};
use officina_grid::filter::{AppearanceFilter, AutoFilter, ColorFilter, Cond, IconFilter};
use officina_grid::style::{Font, Style};
use officina_grid::{CellAddr, Rect, Value, Workbook};

const FILL: u32 = 0x96D395;
const FONT: u32 = 0xB00055;
const RANGE: Rect = Rect {
    sheet: 0,
    r0: 0,
    c0: 0,
    r1: 9,
    c1: 3,
};
const SCORES: [[i32; 3]; 9] = [
    [8, 8, 8],
    [8, 8, 8],
    [8, 8, 8],
    [9, 9, 9],
    [2, 2, 8], // These colors come from the cell styles.
    [8, 8, 1],
    [2, 8, 8],
    [8, 2, 8],
    [8, 8, 0], // This cell has text and no icon.
];

fn at(row: u32, col: u32) -> CellAddr {
    CellAddr::new(0, row, col)
}

fn icon(kind: &str) -> u8 {
    icon_set(match kind {
        "base" => "3Arrows",
        "extended" => "3Stars",
        _ => panic!("unknown fixture {kind}"),
    })
    .unwrap()
}

fn filter(set: u8) -> AutoFilter {
    AutoFilter {
        rect: RANGE,
        conds: vec![
            (
                1,
                Cond::Appearance(AppearanceFilter::Color(ColorFilter {
                    cell: true,
                    rgb: Some(FILL),
                })),
            ),
            (
                2,
                Cond::Appearance(AppearanceFilter::Color(ColorFilter {
                    cell: false,
                    rgb: Some(FONT),
                })),
            ),
            (
                3,
                Cond::Appearance(AppearanceFilter::Icon(
                    IconFilter::new(set, Some(0)).unwrap(),
                )),
            ),
        ],
    }
}

fn fixture(set: u8) -> Workbook {
    let mut book = Workbook::new();
    book.add_sheet("Results").unwrap();
    for (col, text) in ["Item", "Fill", "Font", "Icon", "Limit"]
        .into_iter()
        .enumerate()
    {
        book.set_value(at(0, col as u32), Value::str(text));
    }
    for (row, scores) in (1..).zip(SCORES) {
        book.set_value(at(row, 0), Value::str(&format!("Item {row}")));
        for (col, score) in (1..).zip(scores) {
            let value = if score == 0 {
                Value::str("n/a")
            } else {
                Value::Num(f64::from(score))
            };
            book.set_value(at(row, col), value);
        }
    }
    book.set_value(at(0, 4), Value::Num(5.0));
    book.set_value(at(11, 0), Value::str("Manually hidden"));
    book.sheet_mut(0).hidden = vec![(11, 11)];
    book.set_style(
        &Rect::cell(at(5, 1)),
        Style {
            fill: Some(FILL),
            ..Style::default()
        },
    );
    book.set_style(
        &Rect::cell(at(5, 2)),
        Style {
            font: Font {
                color: Some(FONT),
                ..Font::default()
            },
            ..Style::default()
        },
    );
    for (col, formula, dxf) in [
        (
            1,
            "=B2>=$E$1",
            Dxf {
                fill: Some(FILL),
                ..Dxf::default()
            },
        ),
        (
            2,
            "=C2>=$E$1",
            Dxf {
                color: Some(FONT),
                ..Dxf::default()
            },
        ),
    ] {
        let kind = RuleKind::Expression(book.cond_formula(formula, 0, (1, col)).unwrap());
        book.add_cond_format(CondFormat {
            sheet: 0,
            ranges: vec![Rect {
                sheet: 0,
                r0: 1,
                r1: 9,
                c0: col,
                c1: col,
            }],
            rules: vec![Rule {
                kind,
                dxf,
                stop: false,
            }],
        })
        .unwrap();
    }
    book.add_cond_format(CondFormat {
        sheet: 0,
        ranges: vec![Rect {
            sheet: 0,
            r0: 1,
            r1: 9,
            c0: 3,
            c1: 3,
        }],
        rules: vec![Rule {
            kind: RuleKind::IconSet {
                set,
                points: [0.0, 4.0, 7.0].map(|v| (Cfvo::Num(v), true)).to_vec(),
                reverse: true,
                show_value: true,
                icons: None,
            },
            dxf: Dxf::default(),
            stop: false,
        }],
    })
    .unwrap();
    book.recalc();
    let state = book.filtered(0, Some(filter(set)));
    book.replace_filter(state);
    book
}

fn read(bytes: &[u8]) -> Workbook {
    let (book, loaded) = xlsx::read(bytes).unwrap();
    assert!(loaded.report.is_empty(), "{:?}", loaded.report.human());
    book
}

fn check(book: &Workbook, state: &str, set: u8) {
    let (edited, visible): (bool, &[u32]) = match state {
        "initial" | "saved" => (false, &[1, 2, 3, 4, 5]),
        "edited" => (true, &[1, 2, 3, 4, 5]),
        "reapplied" => (true, &[4, 5]),
        _ => panic!("unknown checkpoint {state}"),
    };
    let sheet = book.sheet(0);
    assert_eq!(sheet.filter, Some(filter(set)));
    let actual: Vec<_> = (1..=9).filter(|&r| !sheet.row_hidden(r)).collect();
    assert_eq!(actual, visible, "{state}");
    assert!(!sheet.row_hidden(0));
    assert!(!sheet.row_hidden(10));
    assert!(sheet.row_hidden(11));
    assert_eq!(book.get(at(11, 0)), Value::str("Manually hidden"));
    assert_eq!(
        book.get(at(1, 4)),
        if state == "initial" {
            Value::Empty
        } else {
            Value::Num(8.0)
        }
    );
    for (row, scores) in (1..).zip(SCORES) {
        for (col, score) in (1..).zip(scores) {
            let expected = if score == 0 {
                Value::str("n/a")
            } else if edited && row <= 3 && row == col {
                Value::Num(1.0)
            } else {
                Value::Num(f64::from(score))
            };
            assert_eq!(
                book.get(at(row, col)),
                expected,
                "{state} row {row}, column {col}"
            );
        }
    }
}

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    match args.as_slice() {
        [command, dir] if command == "make" => {
            for (file, kind) in [("01-base.xlsx", "base"), ("02-extended.xlsx", "extended")] {
                fs::write(
                    Path::new(dir).join(file),
                    xlsx::write(&fixture(icon(kind)), Vec::new()).unwrap(),
                )
                .unwrap();
            }
        }
        [command, file, state, kind] if command == "check" => {
            let bytes = fs::read(file).unwrap();
            let book = if file.ends_with(".osheet") {
                officina_grid::native::load(&bytes).unwrap()
            } else {
                read(&bytes)
            };
            check(&book, state, icon(kind));
        }
        _ => panic!("usage: sheets_appearance_filter_fixture make DIR | check FILE STATE KIND"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use officina_core::history::Op;
    use officina_grid::{native, ops::GridOp};

    fn reopen(book: &Workbook) -> Workbook {
        read(&xlsx::write(book, Vec::new()).unwrap())
    }

    fn change_xml(book: &Workbook, part: &str, change: impl Fn(&str) -> String) -> Vec<u8> {
        use officina_convert::zip::{Archive, ZipWriter};
        let bytes = xlsx::write(book, Vec::new()).unwrap();
        let archive = Archive::open(&bytes).unwrap();
        let mut out = ZipWriter::new(Vec::new());
        for entry in &archive.entries {
            let bytes = archive.read(entry).unwrap();
            if entry.name == part {
                out.add(
                    &entry.name,
                    change(std::str::from_utf8(&bytes).unwrap()).as_bytes(),
                )
                .unwrap();
            } else {
                out.add(&entry.name, &bytes).unwrap();
            }
        }
        out.finish().unwrap()
    }

    #[test]
    fn default_fill_font_and_icon_are_distinct_from_explicit_appearance() {
        let set = icon("base");
        for (col, rule, keeps) in [
            (
                1,
                AppearanceFilter::Color(ColorFilter {
                    cell: true,
                    rgb: None,
                }),
                7,
            ),
            (
                2,
                AppearanceFilter::Color(ColorFilter {
                    cell: false,
                    rgb: None,
                }),
                8,
            ),
            (
                3,
                AppearanceFilter::Icon(IconFilter::new(set, None).unwrap()),
                9,
            ),
        ] {
            let mut book = fixture(set);
            let filter = AutoFilter {
                rect: RANGE,
                conds: vec![(col, Cond::Appearance(rule))],
            };
            book.replace_filter(book.filtered(0, Some(filter.clone())));
            let reopened = reopen(&book);
            assert_eq!(reopened.sheet(0).filter, Some(filter));
            assert_eq!(reopened.filter_state(0), book.filter_state(0));
            let visible: Vec<_> = (1..=9)
                .filter(|&row| !reopened.sheet(0).row_hidden(row))
                .collect();
            assert_eq!(visible, vec![keeps]);
        }
    }

    #[test]
    fn reads_indexed_filter_dxf_and_preserves_static_visibility_when_reference_is_missing() {
        let set = icon("base");
        let book = fixture(set);
        let bytes = change_xml(&book, "xl/styles.xml", |xml| {
            let from = format!(r#"<fgColor rgb="FF{FILL:06X}"/>"#);
            assert!(xml.contains(&from));
            xml.replace(&from, r#"<fgColor indexed="0"/>"#).replace("</styleSheet>", &format!(
                r#"<colors><indexedColors><rgbColor rgb="FF{FILL:06X}"/></indexedColors></colors></styleSheet>"#
            ))
        });
        let indexed = read(&bytes);
        check(&indexed, "initial", set);
        assert_eq!(
            indexed.filtered(0, Some(filter(set))).hidden,
            book.sheet(0).hidden
        );

        let bytes = change_xml(&book, "xl/worksheets/sheet1.xml", |xml| {
            assert!(xml.contains(r#"<colorFilter dxfId="2""#));
            xml.replace(r#"<colorFilter dxfId="2""#, r#"<colorFilter dxfId="999""#)
        });
        let (reopened, loaded) = xlsx::read(&bytes).unwrap();
        assert!(loaded
            .report
            .human()
            .iter()
            .any(|note| note.contains("unsupported color or icon filter")));
        assert!(reopened.sheet(0).filter.is_none());
        assert_eq!(reopened.sheet(0).hidden, book.sheet(0).hidden);
    }

    #[test]
    fn office_2010_icon_choice_and_formula_threshold_override_the_fallback() {
        let set = icon("extended");
        let book = fixture(set);
        let bytes = change_xml(&book, "xl/worksheets/sheet1.xml", |xml| {
            assert!(xml.contains(r#"<x14:iconFilter iconSet="3Stars" iconId="0"/>"#));
            let point = r#"<x14:cfvo type="num" gte="1"><xm:f>7</xm:f></x14:cfvo>"#;
            assert!(xml.contains(point));
            xml.replace("<mc:Fallback/>", r#"<mc:Fallback><customFilters><customFilter operator="equal" val="0"/></customFilters></mc:Fallback>"#)
                .replace(point, r#"<x14:cfvo type="formula" gte="1"><xm:f>IF($E$1&gt;4,7,3)</xm:f></x14:cfvo>"#)
        });
        let mut book = read(&bytes);
        check(&book, "initial", set);
        assert_eq!(book.look(at(1, 3)).icon, Some((set, 0)));
        book.set_value(at(1, 3), Value::Num(4.0));
        book.recalc();
        assert!(book
            .filtered(0, Some(filter(set)))
            .hidden
            .iter()
            .any(|&(lo, hi)| (lo..=hi).contains(&1)));
        book.set_value(at(0, 4), Value::Num(4.0));
        book.recalc();
        assert!(!book
            .filtered(0, Some(filter(set)))
            .hidden
            .iter()
            .any(|&(lo, hi)| (lo..=hi).contains(&1)));
    }

    #[test]
    fn edited_colors_and_icons_change_membership_only_on_reapply() {
        for set in [icon("base"), icon("extended")] {
            check_edit_cycle(set);
        }
    }

    fn check_edit_cycle(set: u8) {
        let mut book = reopen(&fixture(set));
        check(&book, "initial", set);
        book.set_value(at(1, 4), Value::Num(8.0));
        book = reopen(&book);
        check(&book, "saved", set);
        for col in 1..=3 {
            book.set_value(at(col, col), Value::Num(1.0));
        }
        book.recalc();
        book = native::load(&native::save_to_vec(&book)).unwrap();
        check(&book, "edited", set);

        // Take the first three rows individually to verify each rule independently.
        for (col, cond) in filter(set).conds {
            let state = book.filtered(
                0,
                Some(AutoFilter {
                    rect: RANGE,
                    conds: vec![(col, cond)],
                }),
            );
            for row in 1..=3 {
                assert_eq!(
                    state
                        .hidden
                        .iter()
                        .any(|&(lo, hi)| (lo..=hi).contains(&row)),
                    row == col
                );
            }
        }

        let op = GridOp::Filter(book.filtered(0, Some(filter(set))));
        let undo = GridOp::decode(&op.encode()).unwrap().apply(&mut book);
        check(&reopen(&book), "reapplied", set);
        let redo = undo.apply(&mut book);
        check(&book, "edited", set);
        redo.apply(&mut book);
        check(&book, "reapplied", set);
    }
}
