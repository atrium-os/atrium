//! Cell appearance inputs and persisted checkpoints for the desktop gate.
use std::{env, fs, path::Path};

use officina_convert::zip::{Archive, ZipWriter};
use officina_convert::{cfb, ods, xls, xlsx};
use officina_grid::style::{Edge, GradientFill, Line, PatternFill, Style};
use officina_grid::{CellAddr, Rect, Value, Workbook};

const SHRINK: &str = "Complete label";

fn style(book: &mut Workbook, row: u32, col: u32, style: Style) {
    book.set_style(
        &Rect {
            sheet: 0,
            r0: row,
            r1: row,
            c0: col,
            c1: col,
        },
        style,
    );
}

fn fixture(excel: bool) -> Workbook {
    let mut book = Workbook::new();
    book.add_sheet("Appearance").unwrap();
    book.sheet_mut(0).row_sizes.set(1, 88.0);
    for (col, label) in [(1, "Angle"), (2, SHRINK), (6, "Stack")] {
        book.set_value(CellAddr::new(0, 1, col), Value::str(label));
    }
    book.set_value(CellAddr::new(0, 0, 0), Value::Num(3.0));
    style(
        &mut book,
        1,
        1,
        Style {
            rotation: 45,
            ..Style::default()
        },
    );
    style(
        &mut book,
        1,
        2,
        Style {
            shrink_to_fit: true,
            ..Style::default()
        },
    );
    style(
        &mut book,
        1,
        6,
        Style {
            rotation: 255,
            ..Style::default()
        },
    );
    let red = Edge {
        line: Line::Thin,
        color: Some(0xD62728),
    };
    style(
        &mut book,
        1,
        5,
        Style {
            diagonals: [red; 2],
            ..Style::default()
        },
    );
    if excel {
        style(
            &mut book,
            1,
            3,
            Style {
                fill: Some(0x246AC4),
                pattern: PatternFill::new("darkGrid", Some(0x246AC4), Some(0xF8DB46)),
                ..Style::default()
            },
        );
        style(
            &mut book,
            1,
            4,
            Style {
                fill: Some(0xDE3260),
                gradient: GradientFill::new(
                    0.0,
                    [(0.0, 0xDE3260), (0.5, 0x143926), (1.0, 0x32B45B)],
                ),
                ..Style::default()
            },
        );
    }
    style(
        &mut book,
        4,
        0,
        Style {
            fill: Some(0x96DB9C),
            ..Style::default()
        },
    );
    book
}

fn excel(book: &Workbook) -> Vec<u8> {
    let data = if book.macros().is_some() {
        xlsx::write_xlsm(book, Vec::new())
    } else {
        xlsx::write(book, Vec::new())
    }
    .unwrap();
    let zip = Archive::open(&data).unwrap();
    let mut out = ZipWriter::new(Vec::new());
    for part in &zip.entries {
        let bytes = zip.read(part).unwrap();
        if part.name == "xl/worksheets/sheet1.xml" {
            let xml = String::from_utf8(bytes).unwrap();
            let id = book.style_id(CellAddr::new(0, 4, 0));
            assert!(xml.contains(r#"<row r="5""#));
            let styled = xml
                .replace(
                    r#"<row r="5""#,
                    &format!(r#"<row r="5" s="{id}" customFormat="1""#),
                )
                .replace(
                    "</sheetData>",
                    r#"<row r="7"><c r="A7" s="0"/></row></sheetData>"#,
                );
            out.add(&part.name, styled.as_bytes()).unwrap();
        } else {
            out.add(&part.name, &bytes).unwrap();
        }
    }
    out.finish().unwrap()
}

fn open_document() -> Vec<u8> {
    let book = fixture(false);
    let data = ods::write(&book, Vec::new()).unwrap();
    let zip = Archive::open(&data).unwrap();
    let mut out = ZipWriter::new(Vec::new());
    for part in &zip.entries {
        let bytes = zip.read(part).unwrap();
        if part.name == "content.xml" {
            let mut xml = String::from_utf8(bytes).unwrap();
            let id = book.style_id(CellAddr::new(0, 4, 0));
            let cell = xml
                .find(&format!(r#"<table:table-cell table:style-name="ce{id}""#))
                .unwrap();
            let row = xml[..cell].rfind("<table:table-row").unwrap();
            let end =
                xml[cell..].find("</table:table-row>").unwrap() + cell + "</table:table-row>".len();
            xml.replace_range(
                row..end,
                &format!(r#"<table:table-row table:default-cell-style-name="ce{id}"/>"#),
            );
            out.add(&part.name, xml.as_bytes()).unwrap();
        } else if part.name == "mimetype" {
            out.add_stored(&part.name, &bytes).unwrap();
        } else {
            out.add(&part.name, &bytes).unwrap();
        }
    }
    out.finish().unwrap()
}

fn legacy() -> Vec<u8> {
    fn record(out: &mut Vec<u8>, kind: u16, data: &[u8]) {
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(&(data.len() as u16).to_le_bytes());
        out.extend_from_slice(data);
    }
    fn xf(out: &mut Vec<u8>, rotation: u8, shrink: bool, diagonal: bool, fill: u8) {
        let mut data = [0u8; 20];
        data[7] = rotation;
        data[8] = u8::from(shrink) << 4;
        if diagonal {
            data[13] = 0xC0;
        }
        let bits = (u32::from(fill) << 26) | if diagonal { (1 << 21) | (10 << 14) } else { 0 };
        data[14..18].copy_from_slice(&bits.to_le_bytes());
        data[18..].copy_from_slice(&(12u16 | (13 << 7)).to_le_bytes());
        record(out, 0x00E0, &data);
    }
    let mut stream = Vec::new();
    let mut bof = [0u8; 16];
    bof[..4].copy_from_slice(&[0, 6, 5, 0]);
    record(&mut stream, 0x0809, &bof);
    let bound = stream.len() + 4;
    record(&mut stream, 0x0085, b"\0\0\0\0\0\0\x0a\0Appearance");
    for (rotation, shrink, diagonal, fill) in [
        (0, false, false, 0),
        (45, false, false, 0),
        (0, true, false, 0),
        (0, false, false, 9),
        (0, false, true, 0),
        (255, false, false, 0),
        (0, false, false, 1),
    ] {
        xf(&mut stream, rotation, shrink, diagonal, fill);
    }
    record(&mut stream, 0x000A, &[]);
    let offset = stream.len() as u32;
    stream[bound..bound + 4].copy_from_slice(&offset.to_le_bytes());
    bof[2] = 16;
    record(&mut stream, 0x0809, &bof);
    let mut num = vec![0; 6];
    num.extend_from_slice(&3.0f64.to_le_bytes());
    record(&mut stream, 0x0203, &num);
    for (col, id, text) in [
        (1, 1, "Angle"),
        (2, 2, SHRINK),
        (3, 3, ""),
        (5, 4, ""),
        (6, 5, "Stack"),
    ] {
        let mut cell = vec![1, 0, col, 0, id, 0, text.len() as u8, 0, 0];
        cell.extend_from_slice(text.as_bytes());
        record(&mut stream, 0x0204, &cell);
    }
    let row = |n: u8, height: u16, flags: u32| {
        let mut bytes = [0u8; 16];
        bytes[0] = n;
        bytes[6..8].copy_from_slice(&height.to_le_bytes());
        bytes[12..].copy_from_slice(&flags.to_le_bytes());
        bytes
    };
    record(&mut stream, 0x0208, &row(1, 1320, 0x40));
    record(&mut stream, 0x0208, &row(4, 300, (6 << 16) | 0x80));
    record(&mut stream, 0x000A, &[]);
    cfb::write(&[("Workbook", &stream)])
}

fn verify(book: &Workbook, format: &str, wanted: f64) {
    let style = |r, c| book.style(CellAddr::new(0, r, c));
    assert_eq!(book.get(CellAddr::new(0, 0, 0)), Value::Num(wanted));
    assert_eq!(book.get(CellAddr::new(0, 1, 2)), Value::str(SHRINK));
    assert_eq!(style(1, 1).rotation, 45);
    assert_eq!(style(1, 6).rotation, 255);
    assert!(style(1, 2).shrink_to_fit);
    assert!(style(1, 5).diagonals.iter().all(|e| e.line == Line::Thin));
    assert_eq!(
        style(1, 3).pattern.map(|p| p.name()),
        (format != "ods").then_some("darkGrid")
    );
    assert_eq!(
        style(1, 4).gradient.as_ref().map(|g| g.stops.len()),
        matches!(format, "xlsx" | "xlsm").then_some(3)
    );
    assert!(style(4, 7).fill.is_some());
    assert!(style(5, 0).fill.is_none(), "style leaked down the sheet");
}

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    match args.as_slice() {
        [op, dir, vba] if op == "make" => {
            let dir = Path::new(dir);
            fs::write(dir.join("legacy.xls"), legacy()).unwrap();
            fs::write(dir.join("sales.ods"), open_document()).unwrap();
            fs::write(dir.join("sales.xlsx"), excel(&fixture(true))).unwrap();
            let mut macros = fixture(true);
            let source = xlsx::read(&fs::read(vba).unwrap()).unwrap().0;
            for sheet in 1..source.sheet_count() {
                macros.add_sheet(&source.sheet_name(sheet as u16)).unwrap();
            }
            macros.set_macros(source.macros().unwrap().clone()).unwrap();
            fs::write(dir.join("macro.xlsm"), excel(&macros)).unwrap();
        }
        [op, file, state, format] if op == "check" => {
            let bytes = fs::read(file).unwrap();
            let book = match Path::new(file).extension().unwrap().to_str().unwrap() {
                "xls" => xls::read(&bytes, None).unwrap().0,
                "xlsx" | "xlsm" => xlsx::read(&bytes).unwrap().0,
                "ods" => ods::read(&bytes).unwrap().0,
                "osheet" => officina_grid::native::load(&bytes).unwrap(),
                _ => panic!("unexpected format"),
            };
            if format == "xlsm" {
                let source = env::var("SHEETS_APPEARANCE_VBA").unwrap();
                let reference = xlsx::read(&fs::read(source).unwrap()).unwrap().0;
                assert_eq!(book.macros(), reference.macros());
            }
            verify(&book, format, if state == "initial" { 3.0 } else { 8.0 });
        }
        _ => panic!("usage: sheets_appearance_fixture make DIR VBA | check FILE STATE FORMAT"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_and_native_copies_retain_appearance() {
        let (excel, notice) = xlsx::read(&excel(&fixture(true))).unwrap();
        assert!(notice
            .report
            .human()
            .iter()
            .any(|s| s.contains("distant blank cells")));
        let paired = [
            (excel, "xlsx"),
            (ods::read(&open_document()).unwrap().0, "ods"),
            (xls::read(&legacy(), None).unwrap().0, "xls"),
        ];
        for (book, format) in paired {
            verify(&book, format, 3.0);
            let native =
                officina_grid::native::load(&officina_grid::native::save_to_vec(&book)).unwrap();
            verify(&native, format, 3.0);
            match format {
                "xlsx" => verify(
                    &xlsx::read(&xlsx::write(&book, Vec::new()).unwrap())
                        .unwrap()
                        .0,
                    format,
                    3.0,
                ),
                "ods" => verify(
                    &ods::read(&ods::write(&book, Vec::new()).unwrap())
                        .unwrap()
                        .0,
                    format,
                    3.0,
                ),
                _ => {}
            }
        }
    }
}
