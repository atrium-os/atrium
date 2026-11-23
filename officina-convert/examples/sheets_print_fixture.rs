//! Pictures, print area, and explicit print titles for the Sheets desktop gate.
use std::{env, fs, path::Path};

use officina_convert::zip::{crc32, Archive, ZipWriter};
use officina_convert::{ods, xls, xlsx};
use officina_grid::charts::{Anchor, Picture};
use officina_grid::comments::{Comment, ThreadMention, ThreadMessage, ThreadPerson};
use officina_grid::{print, CellAddr, Value, Workbook};
use std::sync::Arc;

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
            from: (row, 4, 0.0, 0.0),
            to: (row + 3, 5, 24.0, 0.0),
        },
        bytes: png().into(),
        link: None,
        crop: [0; 4],
        rotation: 0,
    }
}

const LABELS: &[(u16, u16, &str)] = &[
    (0, 0, "Frozen"),
    (1, 1, "Corner"),
    (1, 3, "NearTitle"),
    (1, 14, "FarTitle"),
    (3, 1, "NearKey"),
    (69, 1, "FarKey"),
    (3, 2, "OutLeft"),
    (3, 15, "OutRight"),
    (90, 3, "OutBottom"),
    (79, 14, "FarBody"),
];

fn workbook() -> Workbook {
    let mut book = Workbook::new();
    book.add_sheet("Report").unwrap();
    for &(row, col, text) in LABELS {
        book.set_value(at(row.into(), col.into()), Value::str(text));
    }
    book.set_value(at(3, 4), Value::Num(3.0));
    book.set_formula(at(3, 5), "=E4*21").unwrap();
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
                        r#"<table:table table:name="Report" table:print-ranges="$Report.$D$4:$Report.$O$90">"#,
                        "<table:shapes>",
                        r#"<draw:frame draw:name="Inside" svg:x="3.541667in" svg:y="1.041667in" svg:width="1in" svg:height="0.625in">"#,
                        r#"<draw:image xlink:href="Pictures/proof.png" xlink:type="simple" xlink:show="embed" xlink:actuate="onLoad"/></draw:frame>"#,
                        r#"<draw:frame draw:name="Outside" svg:x="3.541667in" svg:y="19.166667in" svg:width="1in" svg:height="0.625in">"#,
                        r#"<draw:image xlink:href="./Pictures/proof.png" xlink:type="simple" xlink:show="embed" xlink:actuate="onLoad"/></draw:frame>"#,
                        "</table:shapes>",
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

fn make(dir: &Path) {
    let mut book = workbook();
    fs::write(dir.join("sales.ods"), ods_fixture(&book)).unwrap();
    book.define_name(print::AREA, Some(0), "=Report!$D$4:$O$90")
        .unwrap();
    book.define_name(print::ROWS, Some(0), "=$2:$2").unwrap();
    book.define_name(print::COLS, Some(0), "=$B:$B").unwrap();
    // The second copy is entirely outside the print area.
    book.add_picture(picture(5)).unwrap();
    book.add_picture(picture(92)).unwrap();
    fs::write(
        dir.join("sales.xlsx"),
        xlsx::write(&book, Vec::new()).unwrap(),
    )
    .unwrap();
}

fn conversation() -> Comment {
    let ana = Arc::new(ThreadPerson {
        id: 10,
        name: "Ana".into(),
        user_id: "ana@example.test".into(),
        provider_id: "local".into(),
    });
    let jo = Arc::new(ThreadPerson {
        id: 20,
        name: "Jo".into(),
        user_id: "jo@example.test".into(),
        provider_id: "local".into(),
    });
    Comment::threaded(vec![
        ThreadMessage {
            id: 11,
            parent: None,
            author: ana.clone(),
            timestamp: "2026-11-23T12:00:00Z".into(),
            done: true,
            text: "Check  🧭\n@Jo".into(),
            mentions: vec![ThreadMention {
                id: 12,
                person: jo.clone(),
                start: 12,
                end: 15,
            }]
            .into(),
        },
        ThreadMessage {
            id: 21,
            parent: Some(11),
            author: jo,
            timestamp: "2026-11-23T12:30:00Z".into(),
            done: false,
            text: "Checked @Ana".into(),
            mentions: vec![ThreadMention {
                id: 22,
                person: ana,
                start: 8,
                end: 12,
            }]
            .into(),
        },
    ])
    .unwrap()
}

fn make_comments_print(dir: &Path) {
    let mut book = workbook();
    book.set_comments(
        0,
        vec![
            (3, 3, Some(conversation())),
            (4, 3, Some(Comment::new("Ana", "Plain control"))),
        ],
    );
    fs::write(dir.join("sales.ods"), ods_fixture(&book)).unwrap();
    fs::write(dir.join("legacy.xls"), xls::sample::print_workbook(LABELS)).unwrap();
}

const STYLE_LABELS: &[(u32, u32, &str)] = &[
    (3, 3, "JumpLink"),
    (5, 3, "Wrap alpha beta\nTail"),
    (5, 6, "VERT"),
    (5, 7, "STACK"),
    (5, 8, "ShrunkenLabel"),
    (5, 9, "Styled"),
    (6, 6, "DocsLink"),
    (6, 7, "ExcludedLink"),
    (6, 8, "UnsafeLink"),
    (14, 3, "HiddenProof"),
];

fn make_fidelity(dir: &Path) {
    use officina_grid::comments::{CommentRun, CommentStyle};
    use officina_grid::links::Link;
    use officina_grid::style::{Edge, GradientFill, HAlign, Line, PatternFill, Style, VAlign};
    use officina_grid::Rect;
    for format in ["xlsx", "ods"] {
        let mut book = workbook();
        book.define_name(print::AREA, Some(0), "=Report!$D$4:$O$90").unwrap();
        book.define_name(print::ROWS, Some(0), "=$2:$2").unwrap();
        book.define_name(print::COLS, Some(0), "=$B:$B").unwrap();
        book.add_picture(picture(5)).unwrap();
        book.add_picture(picture(92)).unwrap();
        for &(r, c, text) in STYLE_LABELS {
            book.set_value(at(r, c), Value::str(text));
        }
        let sheet = book.sheet_mut(0);
        sheet.row_sizes.set(5, 78.0);
        sheet.hidden = vec![(14, 14)];
        let set = |book: &mut Workbook, c, style| book.set_style(&Rect::cell(at(5, c)), style);
        set(&mut book, 3, Style { wrap: true, v_align: VAlign::Top, fill: Some(0xFEECC1), ..Default::default() });
        set(&mut book, 6, Style { rotation: 90, h_align: HAlign::Center, v_align: VAlign::Center,
            fill: Some(0xE1F3EF), ..Default::default() });
        set(&mut book, 7, Style { rotation: 255, v_align: VAlign::Center, ..Default::default() });
        set(&mut book, 8, Style { shrink_to_fit: true, ..Default::default() });
        let edge = Edge { line: Line::Medium, color: Some(0x26386A) };
        let mut styled = Style { fill: Some(0xDBE7FA), diagonals: [edge, Edge::default()],
            borders: [Edge::default(), edge, Edge::default(), edge], ..Default::default() };
        styled.font.bold = true;
        styled.font.italic = true;
        styled.font.underline = true;
        styled.font.color = Some(0x26386A);
        set(&mut book, 9, styled);
        if format == "xlsx" {
            set(&mut book, 10, Style {
                pattern: PatternFill::new("darkDown", Some(0x174B68), Some(0xE5F6F9)), ..Default::default()
            });
            set(&mut book, 11, Style {
                gradient: GradientFill::new(0.0, [(0.0, 0x9C1F8D), (1.0, 0xFFE081)]), ..Default::default()
            });
        }
        book.set_links(0, vec![
            (3, 3, Link::new("#Report!O80")),
            (6, 6, Link::new("https://example.test/printed-report")),
            (6, 7, Link::new("#Report!D91")),
            (6, 8, Link::new("file:///private/excluded")),
        ]);
        book.set_comments(0, vec![
            (3, 3, Some(conversation())),
            (1, 1, Some(Comment::with_runs("Kim", "Rich title note", [
                CommentRun { start: 0, end: 4, style: CommentStyle { bold: true, italic: true, ..Default::default() } }
            ]))),
            (14, 3, Some(Comment::new("HiddenAuthor", "HiddenComment"))),
            (90, 3, Some(Comment::new("OutsideAuthor", "OutsideComment"))),
        ]);
        book.recalc();
        let bytes = if format == "xlsx" { xlsx::write(&book, Vec::new()).unwrap() } else { ods::write(&book, Vec::new()).unwrap() };
        fs::write(dir.join(format!("sales.{format}")), bytes).unwrap();
    }
}

fn check(file: &Path, state: &str) {
    let bytes = fs::read(file).unwrap();
    let format = file.extension().unwrap().to_str().unwrap();
    let (state, variant) = state.split_once('-').unwrap_or((state, ""));
    let book = match format {
        "xls" => {
            let (book, loaded) = xls::read(&bytes, None).unwrap();
            assert!(loaded.report.is_empty(), "{:?}", loaded.report.human());
            book
        }
        "ods" => {
            let (book, loaded) = ods::read(&bytes).unwrap();
            assert!(loaded.report.is_empty(), "{:?}", loaded.report.human());
            let archive = Archive::open(&bytes).unwrap();
            let content =
                String::from_utf8(archive.read_name("content.xml").unwrap().unwrap()).unwrap();
            for setting in [
                "table:print-ranges",
                "table:table-header-rows",
                "table:table-header-columns",
            ] {
                assert!(content.contains(setting), "{setting}");
            }
            book
        }
        "xlsx" => {
            let (book, loaded) = xlsx::read(&bytes).unwrap();
            assert!(loaded.report.is_empty(), "{:?}", loaded.report.human());
            let archive = Archive::open(&bytes).unwrap();
            let content =
                String::from_utf8(archive.read_name("xl/workbook.xml").unwrap().unwrap()).unwrap();
            assert!(content
                .contains(r#"_xlnm.Print_Titles" localSheetId="0">Report!$2:$2,Report!$B:$B"#));
            book
        }
        "osheet" => officina_grid::native::load(&bytes).unwrap(),
        _ => panic!("Unexpected format"),
    };
    assert_eq!(book.sheet_name(0), "Report");
    assert_eq!(
        book.sheet(0).freeze,
        if variant == "legacy" { (0, 0) } else { (1, 1) }
    );
    let value = match state {
        "initial" => 3.0,
        "edited" => 8.0,
        _ => panic!("Unexpected checkpoint"),
    };
    assert_eq!(book.get(at(3, 4)), Value::Num(value));
    assert_eq!(book.get(at(3, 5)), Value::Num(value * 21.0));
    let area = book.names().get(0, print::AREA).unwrap();
    assert_eq!(print::area(&area.expr, 0), Some([3, 3, 90, 15]));
    assert_eq!(print::titles(&book, 0), (Some((1, 2)), Some((1, 2))));
    if variant == "legacy" {
        let note = book.comment(0, 3, 3).unwrap();
        assert_eq!((&*note.author, &*note.text), ("Ana", "Hi 🧭"));
        let runs: Vec<_> = note
            .segments()
            .map(|(text, style)| (text, style.bold, style.italic))
            .collect();
        assert_eq!(runs, [("Hi ", true, false), ("🧭", false, true)]);
        assert_eq!(book.link(0, 3, 3).unwrap().place(&book, 0), Some(at(3, 5)));
        for col in 3..=4 {
            assert_eq!(
                &*book.link(0, 4, col).unwrap().target,
                "https://example.test/report"
            );
        }
        assert!(book.pictures().is_empty());
        return;
    }
    if variant == "fidelity" {
        assert_eq!(book.comment(0, 3, 3).unwrap(), &conversation());
        assert_eq!(book.comment(0, 1, 1).unwrap().runs().len(), 1);
        assert_eq!(book.link(0, 3, 3).unwrap().place(&book, 0), Some(at(79, 14)));
        assert_eq!(book.link(0, 6, 6).unwrap().target.as_ref(), "https://example.test/printed-report");
        assert!(book.style(at(5, 3)).wrap);
        assert_eq!(book.style(at(5, 6)).rotation, 90);
        assert_eq!(book.style(at(5, 7)).rotation, 255);
        assert!(book.style(at(5, 8)).shrink_to_fit);
        assert!(book.style(at(5, 9)).font.bold && book.style(at(5, 9)).font.italic);
        assert!(book.sheet(0).row_hidden(14));
        if format == "xlsx" {
            assert!(book.style(at(5, 10)).pattern.is_some());
            assert!(book.style(at(5, 11)).gradient.is_some());
        }
    }
    if variant == "threads" {
        assert_eq!(book.comment(0, 3, 3).unwrap(), &conversation());
        assert_eq!(
            book.comment(0, 4, 3).unwrap(),
            &Comment::new("Ana", "Plain control")
        );
    }
    assert_eq!(book.pictures().len(), 2);
    for (actual, expected) in book.pictures().iter().zip([picture(5), picture(92)]) {
        assert_eq!(actual.bytes, expected.bytes);
        assert_eq!(actual.anchor.sheet, expected.anchor.sheet);
        for (actual, expected) in [
            (actual.anchor.from, expected.anchor.from),
            (actual.anchor.to, expected.anchor.to),
        ] {
            assert_eq!((actual.0, actual.1), (expected.0, expected.1));
            assert!(
                (actual.2 - expected.2).abs() < 0.01 && (actual.3 - expected.3).abs() < 0.01,
                "{actual:?} vs {expected:?}"
            );
        }
    }
}

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    match args.as_slice() {
        [action, dir] if action == "make" => make(Path::new(dir)),
        [action, dir] if action == "make-comments-print" => make_comments_print(Path::new(dir)),
        [action, dir] if action == "make-fidelity" => make_fidelity(Path::new(dir)),
        [action, file, state] if action == "check" => check(Path::new(file), state),
        _ => panic!(
            "usage: sheets_print_fixture make DIR | make-comments-print DIR | make-fidelity DIR | check FILE STATE"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ods_threads_round_trip_and_visible_edits_take_precedence() {
        let mut book = workbook();
        let thread = conversation();
        book.set_comments(0, vec![(0, 0, Some(thread.clone()))]);
        let bytes = ods::write(&book, Vec::new()).unwrap();
        let (reopened, loaded) = ods::read(&bytes).unwrap();
        assert_eq!(reopened.comment(0, 0, 0), Some(&thread));
        assert!(loaded.report.is_empty());

        for malformed in [false, true] {
            let archive = Archive::open(&bytes).unwrap();
            let mut out = ZipWriter::new(Vec::new());
            for part in &archive.entries {
                let bytes = archive.read(part).unwrap();
                if part.name == "content.xml" {
                    let xml = String::from_utf8(bytes).unwrap();
                    let edited = if malformed {
                        xml.replace(
                            "atrium:comment-thread-v1=\"",
                            "atrium:comment-thread-v1=\"invalid",
                        )
                    } else {
                        xml.replace("Checked @Ana", "Edited in another editor")
                    };
                    assert_ne!(edited, xml);
                    out.add(&part.name, edited.as_bytes()).unwrap();
                } else if part.name == "mimetype" {
                    out.add_stored(&part.name, &bytes).unwrap();
                } else {
                    out.add(&part.name, &bytes).unwrap();
                }
            }
            let (reopened, loaded) = ods::read(&out.finish().unwrap()).unwrap();
            let comment = reopened.comment(0, 0, 0).unwrap();
            assert!(comment.thread().is_empty());
            assert!(comment.text.contains(if malformed {
                "Checked @Ana"
            } else {
                "Edited in another editor"
            }));
            assert_eq!(loaded.report.human().len(), 1);
            assert!(loaded.report.human()[0].contains("kept the"));
        }
    }

    #[test]
    fn legacy_notes_links_area_and_titles_survive_native_round_trip() {
        let bytes = xls::sample::print_workbook(LABELS);
        let (book, loaded) = xls::read(&bytes, None).unwrap();
        assert!(loaded.report.is_empty(), "{:?}", loaded.report.human());
        let native = officina_grid::native::save_to_vec(&book);
        let reopened = officina_grid::native::load(&native).unwrap();
        assert_eq!(reopened.comments(0), book.comments(0));
        assert_eq!(reopened.links(0), book.links(0));
        let area = reopened.names().get(0, print::AREA).unwrap();
        assert_eq!(print::area(&area.expr, 0), Some([3, 3, 90, 15]));
        assert_eq!(print::titles(&reopened, 0), (Some((1, 2)), Some((1, 2))));
        assert_eq!(reopened.get(at(1, 3)), Value::str("NearTitle"));
        assert_eq!(reopened.get(at(3, 5)), Value::Num(63.0));
    }
}
