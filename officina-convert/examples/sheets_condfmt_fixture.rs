//! Conditional formatting fixtures and visual checkpoints for the Sheets desktop gate.

use std::{env, fs, path::Path};

use navigator_image::image::{decode_unaddressed, Ceilings};
use officina_convert::zip::{Archive, ZipWriter};
use officina_convert::{ods, xlsx};
use officina_grid::condfmt::{CondFormat, Dxf, Rule, RuleKind};
use officina_grid::style::Style;
use officina_grid::{CellAddr, Rect, Value, Workbook};

const FIRST: u32 = 0xffc7ce;
const SECOND: u32 = 0xbdd7ee;
const BASE: u32 = 0xffeb9c;
const FORMULAS: [&str; 2] = ["AND($B2>=$E$2,$C2>0)", "$B2>=4"];
const TARGET: Rect = Rect {
    sheet: 0,
    r0: 1,
    c0: 3,
    r1: 4,
    c1: 3,
};

fn at(row: u32, col: u32) -> CellAddr {
    CellAddr::new(0, row, col)
}

fn fixture() -> Workbook {
    let mut book = Workbook::new();
    book.add_sheet("Scores").unwrap();
    for (col, text) in ["Input", "Score", "Enabled", "Result", "Limit"]
        .into_iter()
        .enumerate()
    {
        book.set_value(at(0, col as u32), Value::str(text));
    }
    for (row, input) in (1..=4).zip([4.0, 3.0, 1.0, 4.0]) {
        book.set_value(at(row, 0), Value::Num(input));
        book.set_formula(at(row, 1), &format!("=A{}*2", row + 1))
            .unwrap();
        book.set_value(at(row, 2), Value::Num(if row == 4 { 0.0 } else { 1.0 }));
        book.set_value(at(row, 3), Value::str("Check"));
    }
    book.set_value(at(1, 4), Value::Num(8.0));
    book.set_style(
        &TARGET,
        Style {
            fill: Some(BASE),
            ..Style::default()
        },
    );
    let rule = |index, fill, stop, bold| Rule {
        kind: RuleKind::Expression(book.cond_formula(FORMULAS[index], 0, (1, 3)).unwrap()),
        dxf: Dxf {
            fill: Some(fill),
            bold,
            ..Dxf::default()
        },
        stop,
    };
    book.add_cond_format(CondFormat {
        sheet: 0,
        ranges: vec![TARGET],
        rules: vec![
            rule(0, FIRST, true, None),
            rule(1, SECOND, false, Some(true)),
        ],
    })
    .unwrap();
    book.recalc();
    book
}

fn xlsx_with_out_of_order_blocks(mut book: Workbook) -> Vec<u8> {
    // Physical order differs from priority order in some Excel files.
    let mut formats = book.cond_formats().to_vec();
    let first = formats.pop().unwrap();
    let blocks = first
        .rules
        .iter()
        .rev()
        .map(|rule| CondFormat {
            sheet: first.sheet,
            ranges: first.ranges.clone(),
            rules: vec![rule.clone()],
        })
        .collect();
    book.replace_cond_formats(blocks);
    let bytes = xlsx::write(&book, Vec::new()).unwrap();
    let archive = Archive::open(&bytes).unwrap();
    let mut out = ZipWriter::new(Vec::new());
    for entry in &archive.entries {
        let mut data = archive.read(entry).unwrap();
        if entry.name == "xl/worksheets/sheet1.xml" {
            data = String::from_utf8(data)
                .unwrap()
                .replace(r#"priority="1""#, r#"priority="first""#)
                .replace(r#"priority="2""#, r#"priority="1""#)
                .replace(r#"priority="first""#, r#"priority="2""#)
                .into_bytes();
        }
        out.add(&entry.name, &data).unwrap();
    }
    out.finish().unwrap()
}

fn fills(state: &str) -> [u32; 4] {
    match state {
        "initial" => [FIRST, SECOND, BASE, SECOND],
        "threshold" => [FIRST, FIRST, BASE, SECOND],
        "edited" => [SECOND, FIRST, BASE, SECOND],
        "reopened-edit" => [SECOND, SECOND, BASE, SECOND],
        _ => panic!("unknown state {state}"),
    }
}

fn check(file: &Path, state: &str) {
    let bytes = fs::read(file).unwrap();
    let mut book = match file.extension().unwrap().to_str().unwrap() {
        "xlsx" => xlsx::read(&bytes).unwrap().0,
        "ods" => ods::read(&bytes).unwrap().0,
        "osheet" => officina_grid::native::load(&bytes).unwrap(),
        _ => panic!("unexpected extension"),
    };
    book.recalc();
    let (input, limit) = match state {
        "initial" => (4.0, 8.0),
        "threshold" => (4.0, 6.0),
        "edited" => (2.0, 6.0),
        "reopened-edit" => (2.0, 8.0),
        _ => panic!("unknown state {state}"),
    };
    assert_eq!(book.get(at(1, 0)), Value::Num(input));
    assert_eq!(book.get(at(1, 4)), Value::Num(limit));
    let rules: Vec<_> = book
        .cond_formats()
        .iter()
        .flat_map(|format| {
            assert_eq!(format.ranges, [TARGET]);
            format.rules.iter()
        })
        .collect();
    assert_eq!(rules.len(), FORMULAS.len());
    for (rule, expected) in rules.iter().zip(FORMULAS) {
        let RuleKind::Expression(expr) = &rule.kind else {
            panic!("expected a formula rule")
        };
        assert_eq!(book.cond_formula_text(expr, (1, 3)), expected);
    }
    assert!(rules[0].stop);
    for ((row, score), fill) in (1..=4).zip([input * 2.0, 6.0, 2.0, 8.0]).zip(fills(state)) {
        assert!(
            book.group_at(0, row, 1).is_some(),
            "B{} lost its formula",
            row + 1
        );
        assert_eq!(book.get(at(row, 1)), Value::Num(score));
        let result = at(row, 3);
        assert_eq!(book.style(result).fill, Some(BASE));
        let look = book.look(result).dxf;
        assert_eq!(look.fill, (fill != BASE).then_some(fill), "D{}", row + 1);
        // The lower-priority bold must not leak into the first rule's fill.
        assert_eq!(look.bold, (fill == SECOND).then_some(true), "D{}", row + 1);
    }
}

fn check_screen(file: &Path, state: &str) {
    let pixels = decode_unaddressed(&fs::read(file).unwrap(), Ceilings::default()).unwrap();
    assert_eq!((pixels.width, pixels.height), (1280, 720));
    // Inside D2:D5, clear of grid lines and the text in each cell.
    let x = 275;
    for ((row, y), fill) in (2..=5).zip([132, 152, 172, 192]).zip(fills(state)) {
        let offset = (y * pixels.width as usize + x) * 4;
        let shown = &pixels.rgba[offset..offset + 3];
        let expected = [(fill >> 16) as u8, (fill >> 8) as u8, fill as u8];
        assert!(
            shown.iter().zip(expected).all(|(a, b)| a.abs_diff(b) <= 2),
            "D{row} in {state}: pixel {shown:?}, expected {expected:?}"
        );
    }
}

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    match args.as_slice() {
        [command, dir] if command == "make" => {
            let dir = Path::new(dir);
            let book = fixture();
            fs::write(
                dir.join("sales.ods"),
                ods::write(&book, Vec::new()).unwrap(),
            )
            .unwrap();
            fs::write(dir.join("sales.xlsx"), xlsx_with_out_of_order_blocks(book)).unwrap();
        }
        [command, file, state] if command == "check" => check(Path::new(file), state),
        [command, file, state] if command == "screen" => check_screen(Path::new(file), state),
        _ => panic!("usage: sheets_condfmt_fixture make DIR | (check | screen) FILE STATE"),
    }
}
