//! CSV and TSV encoding and quoting fixtures for the Sheets desktop gate.
use std::{env, fs, path::Path};

use officina_grid::csv::{self, CsvOptions};
use officina_grid::value::ErrorKind;
use officina_grid::{CellAddr, Value, Workbook};

fn separator(format: &str) -> char {
    match format {
        "csv" => ',',
        "tsv" => '\t',
        _ => panic!("unknown format {format}"),
    }
}

fn heading(format: &str) -> &'static str {
    // Unquoted decoy delimiters outnumber the actual separators in row one.
    if format == "csv" {
        "A;B;C;D;E"
    } else {
        "A,B,C,D,E"
    }
}

fn text(format: &str, result: u32) -> String {
    let s = separator(format);
    let title = heading(format);
    format!(
        "{title}{s}Qty{s}Calc{s}Literal\r\n\"café{s} € \"\"first\"\"\r\nsecond{s} line\"{s}7{s}{result}{s}=1+2\r\n\" padded \"{s}1{s}TRUE{s}#N/A\r\n"
    )
}

fn source(format: &str, encoding: &str) -> Vec<u8> {
    let text = text(format, 0);
    match encoding {
        "utf8" => [b"\xef\xbb\xbf".as_slice(), text.as_bytes()].concat(),
        "ansi" => text
            .chars()
            .map(|ch| match ch {
                'é' => 0xe9,
                '€' => 0x80,
                ascii if ascii.is_ascii() => ascii as u8,
                _ => panic!("character outside the fixture encoding"),
            })
            .collect(),
        _ => panic!("unknown encoding {encoding}"),
    }
}

fn make(dir: &Path) {
    for (file, format, encoding) in [
        ("01-utf8.csv", "csv", "utf8"),
        ("02-utf8.tsv", "tsv", "utf8"),
        ("03-ansi.csv", "csv", "ansi"),
        ("04-ansi.tsv", "tsv", "ansi"),
    ] {
        fs::write(dir.join(file), source(format, encoding)).unwrap();
    }
}

fn check(path: &Path, state: &str, format: &str, encoding: &str) {
    let saved = match state {
        "initial" => false,
        "saved" => true,
        _ => panic!("unknown checkpoint {state}"),
    };
    let bytes = fs::read(path).unwrap();
    let book = if path.extension().unwrap() == "osheet" {
        officina_grid::native::load(&bytes).unwrap()
    } else {
        let expected = if saved {
            text(format, 21).into_bytes()
        } else {
            source(format, encoding)
        };
        assert_eq!(
            bytes, expected,
            "file encoding, separators, quoting or saved results differ"
        );
        let mut book = Workbook::new();
        book.add_sheet("Table").unwrap();
        let stats = csv::read(&mut book, 0, &bytes, CsvOptions::default()).unwrap();
        assert_eq!(
            (stats.rows, stats.cols, stats.separator),
            (3, 4, separator(format) as u8)
        );
        assert_eq!(stats.latin, !saved && encoding == "ansi");
        book
    };
    let s = separator(format);
    let multiline = format!("café{s} € \"first\"\r\nsecond{s} line");
    let check_value = |row, col, value| {
        assert_eq!(
            book.get(CellAddr::new(0, row, col)),
            value,
            "row {row}, column {col}"
        );
    };
    assert_eq!(book.extent(0), (3, 4));
    assert!(book.groups().next().is_none());
    check_value(0, 0, Value::str(heading(format)));
    check_value(0, 1, Value::str("Qty"));
    check_value(0, 2, Value::str("Calc"));
    check_value(0, 3, Value::str("Literal"));
    check_value(1, 0, Value::str(&multiline));
    check_value(1, 1, Value::Num(7.0));
    check_value(1, 2, Value::Num(if saved { 21.0 } else { 0.0 }));
    check_value(1, 3, Value::str("=1+2"));
    check_value(2, 0, Value::str(" padded "));
    check_value(2, 1, Value::Num(1.0));
    check_value(2, 2, Value::Bool(true));
    check_value(2, 3, Value::Err(ErrorKind::NA));
}

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    match args.as_slice() {
        [command, directory] if command == "make" => make(Path::new(directory)),
        [command, file, state, format, encoding] if command == "check" => {
            check(Path::new(file), state, format, encoding);
        }
        _ => panic!("usage: sheets_delimited_fixture make DIR | check FILE STATE FORMAT ENCODING"),
    }
}
