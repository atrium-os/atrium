//! Validation fixtures and persisted checkpoints for the Sheets desktop gate.
use std::{env, fs, path::Path};

use officina_convert::zip::{Archive, ZipWriter};
use officina_convert::{ods, xls, xlsx};
use officina_grid::condfmt::CmpOp;
use officina_grid::validation::{Allow, ErrorStyle, Message, Validation};
use officina_grid::{CellAddr, Rect, Value, Workbook};

fn at(row: u32, col: u32) -> CellAddr {
    CellAddr::new(0, row, col)
}

fn fixture() -> Workbook {
    let mut book = Workbook::new();
    book.add_sheet("Inputs").unwrap();
    for (col, text) in [
        "Control",
        "Paste here",
        "Fill left",
        "Fill right",
        "Mixed",
        "Valid",
        "Check",
    ]
    .into_iter()
    .enumerate()
    {
        book.set_value(at(0, col as u32), Value::str(text));
    }
    for (row, col, value) in [
        (1, 1, 2.0),
        (2, 1, 3.0),
        (1, 2, 6.0),
        (1, 3, 9.0),
        (2, 2, 4.0),
        (2, 3, 5.0),
        (1, 4, 7.0),
        (2, 4, 9.0),
        (1, 5, 6.0),
        (2, 5, 8.0),
    ] {
        book.set_value(at(row, col), Value::Num(value));
    }
    book.set_formula(at(1, 6), "=SUM(B2:B3,C3:D3)").unwrap();
    for range in [
        Rect {
            sheet: 0,
            r0: 1,
            r1: 2,
            c0: 1,
            c1: 1,
        },
        Rect {
            sheet: 0,
            r0: 2,
            r1: 2,
            c0: 2,
            c1: 3,
        },
    ] {
        let mut rule = Validation::new(
            0,
            vec![range],
            Allow::Whole,
            CmpOp::Between,
            Some(
                book.validation_formula("1", 0, (range.r0, range.c0))
                    .unwrap(),
            ),
            Some(
                book.validation_formula("8", 0, (range.r0, range.c0))
                    .unwrap(),
            ),
        );
        rule.dropdown = false;
        rule.error = Message::new("Outside limit", "Use a whole number from 1 to 8.");
        book.add_validation(rule).unwrap();
    }
    book.recalc();
    book
}

const CHOICES: [&str; 5] = ["2,0", "6", "8", r#"say "yes""#, "literal _x000D_"];

fn extension_workbook(with_fallback: bool) -> Vec<u8> {
    let bytes = xlsx::write(&fixture(), Vec::new()).unwrap();
    let zip = Archive::open(&bytes).unwrap();
    let mut out = ZipWriter::new(Vec::new());
    for entry in &zip.entries {
        let mut data = zip.read(entry).unwrap();
        if entry.name == "xl/worksheets/sheet1.xml" {
            let sheet = std::str::from_utf8(&data).unwrap();
            // The root binds aliased prefixes. The first choice requires
            // another namespace and must be skipped, including its list.
            let sheet = sheet.replace("<worksheet ", concat!(
                r#"<worksheet xmlns:compat="http://schemas.openxmlformats.org/markup-compatibility/2006" "#,
                r#"xmlns:extlist="http://schemas.microsoft.com/office/spreadsheetml/2011/1/ac" xmlns:future="urn:future" "#,
            ));
            let rule = concat!(
                r#"<dataValidation type="list" sqref="H2:H3" allowBlank="1" showErrorMessage="1" "#,
                r#"errorTitle="Outside list" error="Choose an item from the list.">"#,
                r#"<compat:AlternateContent><compat:Choice Requires="extlist future">"#,
                r#"<extlist:list>"2","0"</extlist:list></compat:Choice><compat:Choice Requires="extlist">"#,
                r#"<extlist:list>"2,0","6","8","say ""yes""","literal _x005F_x000D_"</extlist:list></compat:Choice>"#,
            );
            let fallback = if with_fallback {
                r#"<compat:Fallback><formula1>"2,0,6,8"</formula1></compat:Fallback>"#
            } else { "" };
            assert!(sheet.contains("</dataValidations>"));
            data = sheet.replace(r#"<dataValidations count="2">"#, r#"<dataValidations count="3">"#)
                .replace("</dataValidations>", &format!(
                    "{rule}{fallback}</compat:AlternateContent></dataValidation></dataValidations>",
                )).into_bytes();
        }
        out.add(&entry.name, &data).unwrap();
    }
    out.finish().unwrap()
}

fn assert_list(book: &Workbook) {
    for cell in [at(1, 7), at(2, 7)] {
        let rule = book.validation_at(cell).expect("exact list rule");
        assert_eq!(rule.allow, Allow::List);
        assert!(rule.dropdown);
        assert!(rule.show_error);
        assert_eq!(rule.error, Message::new("Outside list", "Choose an item from the list."));
        assert_eq!(book.list_items(cell), CHOICES.map(Value::str));
        assert!(book.check_cell(cell).is_ok());
    }
}

fn check(file: &Path, state: &str) {
    let bytes = fs::read(file).unwrap();
    let book = match file.extension().unwrap().to_str().unwrap() {
        "xls" => xls::read(&bytes, None).unwrap().0,
        "xlsx" => xlsx::read(&bytes).unwrap().0,
        "ods" => ods::read(&bytes).unwrap().0,
        "osheet" => officina_grid::native::load(&bytes).unwrap(),
        _ => panic!("unexpected extension"),
    };
    let (state, variant) = state.split_once('-').unwrap_or((state, ""));
    let (state, list_state) = state.split_once('+').unwrap_or((state, ""));
    let (b2, b3, d2, c3, d3) = match state {
        "initial" => (2.0, 3.0, 9.0, 4.0, 5.0),
        "typed" => (8.0, 3.0, 9.0, 4.0, 5.0),
        "pasted" => (6.0, 8.0, 9.0, 4.0, 5.0),
        "ready" => (6.0, 8.0, 8.0, 4.0, 5.0),
        "filled" => (6.0, 8.0, 8.0, 6.0, 8.0),
        "reopened" => (7.0, 8.0, 8.0, 6.0, 8.0),
        _ => panic!("unknown case {state}"),
    };
    for (row, col, value) in [
        (1, 1, b2),
        (2, 1, b3),
        (1, 2, 6.0),
        (1, 3, d2),
        (2, 2, c3),
        (2, 3, d3),
        (1, 4, 7.0),
        (2, 4, 9.0),
        (1, 5, 6.0),
        (2, 5, 8.0),
        (1, 6, b2 + b3 + c3 + d3),
    ] {
        assert_eq!(
            book.get(at(row, col)),
            Value::Num(value),
            "row {row}, column {col}"
        );
    }
    if !variant.is_empty() {
        assert_list(&book);
        let (h2, h3) = match list_state {
            "" => (Value::Empty, Value::Empty),
            "list" => (Value::Empty, Value::Num(8.0)),
            "paste" => (Value::Num(6.0), Value::Num(8.0)),
            "fill" => (Value::Num(6.0), Value::Num(6.0)),
            _ => panic!("unknown list case {list_state}"),
        };
        assert_eq!(book.get(at(1, 7)), h2);
        assert_eq!(book.get(at(2, 7)), h3);
    }
    if variant == "legacy" {
        assert_eq!(book.validation_at(at(1, 1)).unwrap().prompt.title.as_ref(), "Choose");
        assert!(book.validation_at(at(1, 1)).unwrap().show_prompt);
        assert!(book.check_cell(at(1, 8)).is_ok());
        let refusal = book.check_cell(at(2, 8)).unwrap_err();
        assert_eq!(refusal.error.title.as_ref(), "Too low");
        assert_eq!(book.validation_at(at(2, 8)).unwrap().allow, Allow::Custom);
    }
    for cell in [at(1, 1), at(2, 1), at(2, 2), at(2, 3)] {
        let rule = book.validation_at(cell).unwrap();
        assert_eq!(rule.allow, Allow::Whole);
        assert_eq!(rule.style, ErrorStyle::Stop);
        assert_eq!(
            rule.error,
            Message::new("Outside limit", "Use a whole number from 1 to 8.")
        );
        assert!(rule.show_error);
        assert!(book.check_cell(cell).is_ok());
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
            fs::write(
                dir.join("sales.xlsx"),
                xlsx::write(&book, Vec::new()).unwrap(),
            )
            .unwrap();
        }
        [command, dir] if command == "make-legacy" => {
            let dir = Path::new(dir);
            fs::write(dir.join("extension.xlsx"), extension_workbook(false)).unwrap();
            fs::write(dir.join("validation.xls"), xls::sample::validation_workbook()).unwrap();
        }
        [command, file, state] if command == "check" => check(Path::new(file), state),
        _ => panic!("usage: sheets_validation_fixture make DIR | make-legacy DIR | check FILE STATE"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use officina_grid::native;

    #[test]
    fn excel_choice_retains_exact_items_with_or_without_a_fallback() {
        for with_fallback in [false, true] {
            let (mut book, loaded) = xlsx::read(&extension_workbook(with_fallback)).unwrap();
            assert_list(&book);
            assert!(!loaded.report.human().join(" ").contains("data validation rules"));
            book.set_value(at(1, 7), Value::str("2,0"));
            book.set_value(at(2, 7), Value::Num(2.0));
            assert!(book.check_cell(at(1, 7)).is_ok());
            assert!(book.check_cell(at(2, 7)).is_err());
            book.set_value(at(2, 7), Value::str(r#"say "yes""#));
            for reopened in [
                xlsx::read(&xlsx::write(&book, Vec::new()).unwrap()).unwrap().0,
                ods::read(&ods::write(&book, Vec::new()).unwrap()).unwrap().0,
                native::load(&native::save_to_vec(&book)).unwrap(),
            ] {
                assert_list(&reopened);
            }
        }
    }

    #[test]
    fn biff_validation_is_live_after_native_save_as() {
        let (book, loaded) = xls::read(&xls::sample::validation_workbook(), None).unwrap();
        assert!(loaded.report.human().join(" ").contains("malformed BIFF8 data validation rules"));
        assert_eq!(book.validations().len(), 3);
        let mut book = native::load(&native::save_to_vec(&book)).unwrap();
        assert_list(&book);
        book.set_value(at(2, 3), Value::Num(9.0));
        assert_eq!(book.check_cell(at(2, 3)).unwrap_err().error.title.as_ref(), "Outside limit");
        assert!(book.check_cell(at(1, 8)).is_ok());
        assert!(book.check_cell(at(2, 8)).is_err());
        book.set_value(at(2, 5), Value::Num(6.0));
        assert!(book.check_cell(at(2, 8)).is_ok());
    }
}
