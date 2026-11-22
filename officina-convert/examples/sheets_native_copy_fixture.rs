//! Mixed XLSX and ODS workbooks and checkpoints for the native Save As gate.

use std::{env, fs, path::Path};

use officina_convert::{ods, xlsx};
use officina_grid::comments::{Comment, CommentRun, CommentStyle};
use officina_grid::condfmt::{CmpOp, CondFormat, Dxf, Rule, RuleKind};
use officina_grid::links::Link;
use officina_grid::style::Style;
use officina_grid::validation::{Allow, ErrorStyle, Message, Validation};
use officina_grid::{CellAddr, Rect, Value, Workbook};

const INPUT: CellAddr = CellAddr {
    sheet: 0,
    row: 1,
    col: 1,
};
const TOTAL: CellAddr = CellAddr {
    sheet: 0,
    row: 1,
    col: 2,
};
const RATE: CellAddr = CellAddr {
    sheet: 1,
    row: 0,
    col: 0,
};
const NOTE: &str = "Rate confirmed";
const FORMULA: &str = "=B2*Rate+Rates!$A$1";
const CONDITION: &str = "$B2>=7";
const BASE: u32 = 0xddebf7;
const HIGHLIGHT: u32 = 0xc6efce;
const MERGE: Rect = Rect {
    sheet: 0,
    r0: 0,
    c0: 3,
    r1: 0,
    c1: 4,
};

fn fixture(units: f64) -> Workbook {
    let mut book = Workbook::new();
    book.add_sheet("Orders").unwrap();
    book.add_sheet("Rates").unwrap();
    for (col, text) in ["Item", "Units", "Total", "Reference"]
        .into_iter()
        .enumerate()
    {
        book.set_value(CellAddr::new(0, 0, col as u32), Value::str(text));
    }
    book.set_value(CellAddr::new(0, 1, 0), Value::str("Paper"));
    book.set_value(INPUT, Value::Num(units));
    book.set_value(RATE, Value::Num(3.0));
    book.define_name("Rate", None, "=Rates!$A$1").unwrap();
    book.set_formula(TOTAL, FORMULA).unwrap();
    book.set_style(
        &Rect::cell(TOTAL),
        Style {
            fill: Some(BASE),
            ..Style::default().with_num_fmt("0.00")
        },
    );
    book.set_merges(0, vec![MERGE]);
    book.sheet_mut(0).freeze = (1, 1);
    book.sheet_mut(0).col_sizes.set(3, 115.0);
    book.set_comments(
        0,
        vec![(
            1,
            2,
            Some(Comment::with_runs(
                "Casey",
                NOTE,
                [CommentRun {
                    start: 0,
                    end: 4,
                    style: CommentStyle {
                        bold: true,
                        ..CommentStyle::default()
                    },
                }],
            )),
        )],
    );
    book.set_value(CellAddr::new(0, 1, 3), Value::str("Rate source"));
    book.set_links(0, vec![(1, 3, Link::new("#Rates!A1"))]);

    let condition = book.cond_formula(CONDITION, 0, (1, 2)).unwrap();
    book.add_cond_format(CondFormat {
        sheet: 0,
        ranges: vec![Rect::cell(TOTAL)],
        rules: vec![Rule {
            kind: RuleKind::Expression(condition),
            dxf: Dxf {
                fill: Some(HIGHLIGHT),
                ..Dxf::default()
            },
            stop: true,
        }],
    })
    .unwrap();
    let bound = |text| Some(book.validation_formula(text, 0, (1, 1)).unwrap());
    let mut rule = Validation::new(
        0,
        vec![Rect::cell(INPUT)],
        Allow::Whole,
        CmpOp::Between,
        bound("1"),
        bound("9"),
    );
    rule.allow_blank = false;
    rule.dropdown = false;
    rule.error = Message::new("Units", "Enter a whole number from 1 to 9.");
    book.add_validation(rule).unwrap();

    book.recalc();
    book
}

fn expected(format: &str, state: &str) -> f64 {
    match (format, state) {
        ("ods", "initial") => 3.0,
        ("ods", "copy") => 6.0,
        ("ods", "edited") => 8.0,
        ("xlsx", "initial") => 4.0,
        ("xlsx", "copy") => 7.0,
        ("xlsx", "edited") => 5.0,
        _ => panic!("unknown checkpoint {format} {state}"),
    }
}

fn check(file: &Path, format: &str, state: &str) {
    let bytes = fs::read(file).unwrap();
    let mut book = match file.extension().unwrap().to_str().unwrap() {
        "ods" => ods::read(&bytes).unwrap().0,
        "xlsx" => xlsx::read(&bytes).unwrap().0,
        "osheet" => officina_grid::native::load(&bytes).unwrap(),
        _ => panic!("unexpected extension"),
    };
    let units = expected(format, state);
    assert_eq!(book.sheet_count(), 2);
    assert_eq!(book.sheet_name(0), "Orders");
    assert_eq!(book.sheet_name(1), "Rates");
    assert_eq!(book.sheet(0).freeze, (1, 1));
    assert_eq!(book.merges(0), &[MERGE]);
    assert!((book.sheet(0).col_sizes.size(3) - 115.0).abs() <= 2.0);
    assert_eq!(book.get(RATE), Value::Num(3.0));
    assert_eq!(book.get(INPUT), Value::Num(units));
    assert_eq!(book.formula_text(TOTAL).as_deref(), Some(FORMULA));
    assert!(book.names().get(0, "Rate").is_some());
    // Native copies must already have the refreshed formula result in their saved bytes.
    let total = units * 3.0 + 3.0;
    assert_eq!(book.get(TOTAL), Value::Num(total));
    assert_eq!(book.formatted(TOTAL).text, format!("{total:.2}"));
    assert_eq!(book.style(TOTAL).fill, Some(BASE));

    let [format] = book.cond_formats() else {
        panic!("expected one conditional format")
    };
    assert_eq!(format.ranges, [Rect::cell(TOTAL)]);
    let [rule] = format.rules.as_slice() else {
        panic!("expected one condition")
    };
    let RuleKind::Expression(expr) = &rule.kind else {
        panic!("expected a formula condition")
    };
    assert_eq!(book.cond_formula_text(expr, (1, 2)), CONDITION);
    assert_eq!(
        book.look(TOTAL).dxf.fill,
        (units >= 7.0).then_some(HIGHLIGHT)
    );

    let note = book.comment(0, 1, 2).unwrap();
    assert_eq!(note.author.as_ref(), "Casey");
    assert_eq!(note.text.as_ref(), NOTE);
    assert!(note
        .runs()
        .iter()
        .any(|r| r.start == 0 && r.end == 4 && r.style.bold));
    assert_eq!(book.link(0, 1, 3).unwrap().place(&book, 0), Some(RATE));

    let rule = book.validation_at(INPUT).unwrap();
    assert_eq!(
        (rule.allow, rule.op, rule.style),
        (Allow::Whole, CmpOp::Between, ErrorStyle::Stop)
    );
    assert!(rule.show_error && !rule.allow_blank);
    assert_eq!(
        rule.error,
        Message::new("Units", "Enter a whole number from 1 to 9.")
    );
    assert!(book.check_cell(INPUT).is_ok());
    for invalid in [0.0, 1.5, 10.0] {
        book.set_value(INPUT, Value::Num(invalid));
        assert!(book.check_cell(INPUT).is_err());
    }
}

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    match args.as_slice() {
        [command, dir] if command == "make" => {
            let dir = Path::new(dir);
            fs::write(
                dir.join("orders.ods"),
                ods::write(&fixture(expected("ods", "initial")), Vec::new()).unwrap(),
            )
            .unwrap();
            fs::write(
                dir.join("sales.xlsx"),
                xlsx::write(&fixture(expected("xlsx", "initial")), Vec::new()).unwrap(),
            )
            .unwrap();
        }
        [command, file, format, state] if command == "check" => {
            check(Path::new(file), format, state)
        }
        _ => panic!("usage: sheets_native_copy_fixture make DIR | check FILE FORMAT STATE"),
    }
}
