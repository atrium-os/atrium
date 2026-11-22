//! Pictures, print area, and explicit print titles for the Sheets desktop gate.
use std::{env, fs, path::Path};

use officina_convert::zip::{crc32, Archive, ZipWriter};
use officina_convert::{ods, xlsx};
use officina_grid::charts::{Anchor, Picture};
use officina_grid::{CellAddr, Value, Workbook};

const AREA: &str = "_xlnm.Print_Area";
const TITLES: &str = "_xlnm.Print_Titles";
const IMAGE_PATH: &str = "Pictures/proof.png";

fn at(row: u32, col: u32) -> CellAddr {
    CellAddr::new(0, row, col)
}

fn png() -> Vec<u8> {
    // Two colored halves make the picture recognizable in a rendered PDF.
    let mut rows = Vec::new();
    for _ in 0..30 {
        rows.push(0);
        rows.extend([219, 32, 90].repeat(24));
        rows.extend([26, 155, 196].repeat(24));
    }
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut chunk = |name: &[u8; 4], data: &[u8]| {
        out.extend((data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend(name);
        out.extend(data);
        out.extend(crc32(&out[start..]).to_be_bytes());
    };
    chunk(b"IHDR", &[0, 0, 0, 48, 0, 0, 0, 30, 8, 2, 0, 0, 0]);
    chunk(
        b"IDAT",
        &miniz_oxide::deflate::compress_to_vec_zlib(&rows, 6),
    );
    chunk(b"IEND", &[]);
    out
}

fn picture(row: u32) -> Picture {
    Picture {
        anchor: Anchor {
            sheet: 0,
            from: (row, 2, 0.0, 0.0),
            to: (row, 2, 72.0, 45.0),
        },
        bytes: png().into(),
        link: None,
        crop: [0; 4],
        rotation: 0,
    }
}

fn workbook() -> Workbook {
    let mut book = Workbook::new();
    book.add_sheet("Report").unwrap();
    for (row, col, text) in [
        (0, 2, "FrozenRow"),
        (1, 0, "FrozenCol"),
        (1, 2, "TitleRow"),
        (2, 1, "TitleCol"),
        (0, 14, "OutRight"),
        (75, 0, "OutBottom"),
        (70, 13, "FarBody"),
    ] {
        book.set_value(at(row, col), Value::str(text));
    }
    book.set_value(at(2, 2), Value::Num(3.0));
    book.set_formula(at(2, 3), "=C3*21").unwrap();
    let sheet = book.sheet_mut(0);
    sheet.freeze = (1, 1);
    sheet.col_sizes.set(0, 88.0);
    sheet.col_sizes.set(1, 92.0);
    sheet.col_sizes.set(2, 96.0);
    book.recalc();
    book
}

fn wrap_element(xml: &mut String, tag: &str, index: usize, wrapper: &str, end_tag: &str) {
    let start = xml.match_indices(&format!("<{tag} ")).nth(index).unwrap().0;
    let end = start + xml[start..].find(end_tag).unwrap() + end_tag.len();
    xml.insert_str(end, &format!("</{wrapper}>"));
    xml.insert_str(start, &format!("<{wrapper}>"));
}

fn ods_fixture(book: &Workbook) -> Vec<u8> {
    let bytes = ods::write(book, Vec::new()).unwrap();
    let archive = Archive::open(&bytes).unwrap();
    let mut out = ZipWriter::new(Vec::new());
    for part in &archive.entries {
        let bytes = archive.read(part).unwrap();
        match part.name.as_str() {
            "mimetype" => out.add_stored(&part.name, &bytes).unwrap(),
            "content.xml" => {
                let mut xml = String::from_utf8(bytes).unwrap().replace(
                    r#"<table:table table:name="Report">"#,
                    concat!(
                        r#"<table:table table:name="Report" table:print-ranges="$Report.$A$1:$N$75">"#,
                        r#"<table:shapes><draw:frame draw:name="Proof" svg:x="1.875in" svg:y="0.833333in" svg:width="1in" svg:height="0.625in">"#,
                        r#"<draw:image xlink:href="Pictures/proof.png" xlink:type="simple" xlink:show="embed" xlink:actuate="onLoad"/>"#,
                        "</draw:frame></table:shapes>",
                    ),
                );
                assert!(xml.contains("table:print-ranges"));
                wrap_element(
                    &mut xml,
                    "table:table-column",
                    1,
                    "table:table-header-columns",
                    "/>",
                );
                wrap_element(
                    &mut xml,
                    "table:table-row",
                    1,
                    "table:table-header-rows",
                    "</table:table-row>",
                );
                out.add(&part.name, xml.as_bytes()).unwrap();
            }
            "META-INF/manifest.xml" => {
                let xml = String::from_utf8(bytes).unwrap().replace(
                    "</manifest:manifest>",
                    r#"<manifest:file-entry manifest:full-path="Pictures/proof.png" manifest:media-type="image/png"/></manifest:manifest>"#,
                );
                out.add(&part.name, xml.as_bytes()).unwrap();
            }
            _ => out.add(&part.name, &bytes).unwrap(),
        }
    }
    out.add(IMAGE_PATH, &png()).unwrap();
    out.finish().unwrap()
}

fn print_names(book: &mut Workbook) {
    book.define_name(AREA, Some(0), "=Report!$A$1:$N$75")
        .unwrap();
    book.define_name(TITLES, Some(0), "=Report!$2:$2").unwrap();
}

fn make(dir: &Path) {
    let mut book = workbook();
    fs::write(dir.join("sales.ods"), ods_fixture(&book)).unwrap();
    print_names(&mut book);
    // The second copy is entirely outside the print area.
    book.add_picture(picture(4)).unwrap();
    book.add_picture(picture(79)).unwrap();
    fs::write(
        dir.join("sales.xlsx"),
        xlsx::write(&book, Vec::new()).unwrap(),
    )
    .unwrap();
}

fn check(file: &Path, format: &str, state: &str) {
    let bytes = fs::read(file).unwrap();
    let (book, report) = match file.extension().unwrap().to_str().unwrap() {
        "ods" => {
            let (book, loaded) = ods::read(&bytes).unwrap();
            let archive = Archive::open(&bytes).unwrap();
            let xml =
                String::from_utf8(archive.read(archive.find("content.xml").unwrap()).unwrap())
                    .unwrap();
            let initial = state == "initial";
            for feature in [
                "table:print-ranges",
                "table:table-header-rows",
                "table:table-header-columns",
            ] {
                assert_eq!(xml.contains(feature), initial, "{feature}");
            }
            assert_eq!(archive.find(IMAGE_PATH).is_some(), initial);
            (book, Some(loaded.report.human().join("\n")))
        }
        "xlsx" => {
            let (book, loaded) = xlsx::read(&bytes).unwrap();
            (book, Some(loaded.report.human().join("\n")))
        }
        "osheet" => (officina_grid::native::load(&bytes).unwrap(), None),
        _ => panic!("Unexpected format"),
    };
    assert_eq!(book.sheet_name(0), "Report");
    assert_eq!(book.sheet(0).freeze, (1, 1));
    let value = match state {
        "initial" => 3.0,
        "edited" => 8.0,
        _ => panic!("Unexpected checkpoint"),
    };
    assert_eq!(book.get(at(2, 2)), Value::Num(value));
    assert_eq!(book.get(at(2, 3)), Value::Num(value * 21.0));
    for (row, col, label) in [
        (1, 2, "TitleRow"),
        (2, 1, "TitleCol"),
        (0, 14, "OutRight"),
        (75, 0, "OutBottom"),
        (70, 13, "FarBody"),
    ] {
        assert_eq!(book.get(at(row, col)), Value::str(label));
    }
    if format == "xlsx" {
        assert_eq!(book.pictures(), &[picture(4), picture(79)]);
        let mut expected = workbook();
        print_names(&mut expected);
        for name in [AREA, TITLES] {
            assert_eq!(book.names().get(0, name), expected.names().get(0, name));
        }
    } else {
        assert!(book.pictures().is_empty());
        assert!(book.names().get(0, AREA).is_none());
        assert!(book.names().get(0, TITLES).is_none());
        if let Some(report) = report {
            assert_eq!(
                report.contains("images and drawings"),
                state == "initial",
                "{report}"
            );
        }
    }
}

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    match args.as_slice() {
        [action, dir] if action == "make" => make(Path::new(dir)),
        [action, file, state, format] if action == "check" => check(Path::new(file), format, state),
        _ => panic!("usage: sheets_print_fixture make DIR | check FILE STATE FORMAT"),
    }
}
