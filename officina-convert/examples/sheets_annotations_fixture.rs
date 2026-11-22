//! Comment, link and validation workbooks for the installed Sheets gate.
use std::{env, fs, path::Path, sync::Arc};

use officina_convert::zip::{Archive, ZipWriter};
use officina_convert::{ods, xlsx};
use officina_grid::comments::{
    Comment, CommentRun, CommentStyle, ThreadMention, ThreadMessage, ThreadPerson,
};
use officina_grid::condfmt::CmpOp;
use officina_grid::links::Link;
use officina_grid::validation::{Allow, ErrorStyle, Message, Validation};
use officina_grid::{CellAddr, Rect, Value, Workbook};

const EXTENSION_NOTE: &str = "data validation rules from Excel 2010's extensions";

fn at(col: u32) -> CellAddr {
    CellAddr::new(0, 0, col)
}

fn person(id: u128, name: &str, email: &str) -> Arc<ThreadPerson> {
    Arc::new(ThreadPerson {
        id,
        name: name.into(),
        user_id: email.into(),
        provider_id: "Fixture".into(),
    })
}

fn thread() -> Comment {
    let ana = person(11, "Ana", "ana@example.invalid");
    let bea = person(12, "Bea", "bea@example.invalid");
    Comment::threaded(vec![
        ThreadMessage {
            id: 13,
            parent: None,
            author: ana,
            timestamp: "2026-11-22T10:00:00Z".into(),
            done: false,
            text: "@Bea review".into(),
            mentions: vec![ThreadMention {
                id: 15,
                person: bea.clone(),
                start: 0,
                end: 4,
            }]
            .into(),
        },
        ThreadMessage {
            id: 14,
            parent: Some(13),
            author: bea,
            timestamp: "2026-11-22T10:01:00Z".into(),
            done: false,
            text: "Approved".into(),
            mentions: Arc::default(),
        },
    ])
    .unwrap()
}

fn fixture() -> Workbook {
    let mut book = Workbook::new();
    book.add_sheet("Notes").unwrap();
    for (col, text) in [
        "Note",
        "Place",
        "3",
        "Thread",
        "Old place",
        "New place",
        "Web",
    ]
    .into_iter()
    .enumerate()
    {
        book.set_input(at(col as u32), text).unwrap();
    }
    for col in 0..7 {
        book.set_value(CellAddr::new(0, 1, col), Value::Num(f64::from(col)));
    }
    let rich = Comment::with_runs(
        "Ana",
        "Check",
        [CommentRun {
            start: 0,
            end: 5,
            style: CommentStyle {
                bold: true,
                ..CommentStyle::default()
            },
        }],
    );
    book.set_comments(0, vec![(0, 0, Some(rich))]);
    book.set_links(
        0,
        vec![
            (0, 1, Link::new("#E1")),
            (0, 6, Link::new("https://example.invalid/guide")),
        ],
    );
    let area = Rect {
        sheet: 0,
        r0: 0,
        c0: 2,
        r1: 0,
        c1: 2,
    };
    let number = |text| Some(book.cond_formula(text, 0, (0, 2)).unwrap());
    let mut rule = Validation::new(
        0,
        vec![area],
        Allow::Whole,
        CmpOp::Between,
        number("1"),
        number("5"),
    );
    rule.allow_blank = false;
    rule.dropdown = false;
    rule.prompt = Message::new("Amount", "Enter a whole number.");
    rule.error = Message::new("Outside limit", "Use the allowed range.");
    book.add_validation(rule).unwrap();
    book
}

fn xlsx_with_extension(book: &Workbook) -> Vec<u8> {
    let bytes = xlsx::write(book, Vec::new()).unwrap();
    let archive = Archive::open(&bytes).unwrap();
    let mut result = ZipWriter::new(Vec::new());
    for part in &archive.entries {
        let mut bytes = archive.read(part).unwrap();
        if part.name == "xl/worksheets/sheet1.xml" {
            let xml = String::from_utf8(bytes).unwrap();
            assert!(xml.contains("</worksheet>"));
            bytes = xml.replace("</worksheet>", concat!(
                r#"<extLst><ext uri="{CCE6A557-97BC-4B89-ADB6-D9C93CAAB3DF}""#,
                r#" xmlns:x14="http://schemas.microsoft.com/office/spreadsheetml/2009/9/main""#,
                r#" xmlns:xm="http://schemas.microsoft.com/office/excel/2006/main">"#,
                r#"<x14:dataValidations count="1"><x14:dataValidation type="whole" operator="greaterThan">"#,
                r#"<x14:formula1><xm:f>0</xm:f></x14:formula1><xm:sqref>H1</xm:sqref>"#,
                "</x14:dataValidation></x14:dataValidations></ext></extLst></worksheet>"
            )).into_bytes();
        }
        result.add(&part.name, &bytes).unwrap();
    }
    result.finish().unwrap()
}

fn check(book: &mut Workbook, format: &str, state: &str) {
    let (note_edited, link_edited, rule_edited, thread_edited, accepted) = match state {
        "initial" => (false, false, false, false, false),
        "note" => (true, false, false, false, false),
        "link" => (true, true, false, false, false),
        "rule" => (true, true, true, false, false),
        "thread" => (true, true, true, true, false),
        "accepted" => (true, true, true, true, true),
        _ => panic!("Unknown checkpoint {state}"),
    };
    let note = book.comment(0, 0, 0).unwrap();
    assert_eq!(
        note.text.as_ref(),
        if note_edited { "Checkx" } else { "Check" }
    );
    assert_eq!(note.author.as_ref(), "Ana");
    assert!(note
        .runs()
        .iter()
        .any(|run| run.start == 0 && run.end >= 5 && run.style.bold));
    let link = book.link(0, 0, 1).unwrap();
    assert_eq!(
        link.place(book, 0),
        Some(at(if link_edited { 5 } else { 4 }))
    );
    assert_eq!(book.get(at(1)), Value::str("Place"));
    assert_eq!(
        book.link(0, 0, 6).unwrap().target.as_ref(),
        "https://example.invalid/guide"
    );

    assert_eq!(book.validations().len(), 1);
    let rule = book.validation_at(at(2)).unwrap();
    assert_eq!(rule.allow, Allow::Whole);
    assert_eq!(rule.op, CmpOp::Between);
    assert_eq!(rule.style, ErrorStyle::Stop);
    assert!(!rule.allow_blank && rule.show_prompt && rule.show_error);
    assert_eq!(rule.prompt, Message::new("Amount", "Enter a whole number."));
    assert_eq!(
        rule.error,
        Message::new("Outside limit", "Use the allowed range.")
    );
    assert_eq!(
        book.get(at(2)),
        Value::Num(if accepted { 8.0 } else { 3.0 })
    );
    for (value, allowed) in [(0.0, false), (5.0, true), (8.0, rule_edited), (9.0, false)] {
        book.set_value(at(2), Value::Num(value));
        assert_eq!(
            book.check_cell(at(2)).is_ok(),
            allowed,
            "validation for {value}"
        );
    }

    match (format, thread_edited) {
        ("xlsx", _) => {
            let [root, reply] = book.comment(0, 0, 3).unwrap().thread() else {
                panic!("expected two messages")
            };
            assert_eq!(
                root.text.as_ref(),
                if thread_edited {
                    "@Bea reviewx"
                } else {
                    "@Bea review"
                }
            );
            assert_eq!(root.id, 13);
            assert_eq!(root.author.name.as_ref(), "Ana");
            assert_eq!(
                root.mentions[0].person.user_id.as_ref(),
                "bea@example.invalid"
            );
            assert_eq!(
                &root.text[root.mentions[0].start..root.mentions[0].end],
                "@Bea"
            );
            assert_eq!(reply.parent, Some(root.id));
            assert_eq!(reply.author.name.as_ref(), "Bea");
            assert_eq!(reply.text.as_ref(), "Approved");
        }
        ("ods", false) => assert!(book.comment(0, 0, 3).is_none()),
        ("ods", true) => {
            let note = book.comment(0, 0, 3).unwrap();
            assert!(note.thread().is_empty());
            assert!(!note.author.is_empty());
            assert_eq!(note.text.as_ref(), format!("q\n\n{}: r", note.author));
        }
        _ => panic!("Unknown format {format}"),
    }
}

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    match args.as_slice() {
        [action, dir] if action == "make" => {
            let mut book = fixture();
            let dir = Path::new(dir);
            fs::write(
                dir.join("sales.ods"),
                ods::write(&book, Vec::new()).unwrap(),
            )
            .unwrap();
            book.set_comments(0, vec![(0, 3, Some(thread()))]);
            fs::write(dir.join("sales.xlsx"), xlsx_with_extension(&book)).unwrap();
        }
        [action, file, state, format] if action == "check" => {
            let bytes = fs::read(file).unwrap();
            let mut book = if file.ends_with(".ods") {
                let (book, report) = ods::read(&bytes).unwrap();
                assert!(report.report.is_empty(), "{:?}", report.report.human());
                book
            } else if file.ends_with(".xlsx") {
                let (book, loaded) = xlsx::read(&bytes).unwrap();
                let notices = loaded.report.lines();
                if state == "initial" {
                    assert_eq!(notices, [format!("dropped\t1\t{EXTENSION_NOTE}\t")]);
                } else {
                    assert!(notices.is_empty(), "{notices:?}");
                }
                book
            } else {
                officina_grid::native::load(&bytes).unwrap()
            };
            check(&mut book, format, state);
        }
        _ => panic!("Expected make DIR or check FILE STATE FORMAT"),
    }
}
