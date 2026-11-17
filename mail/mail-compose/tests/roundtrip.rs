//! What Mail writes, read back by Mail's own parser.

use mail_compose::{write, write_draft, Attachment, Draft, Mailbox, Stamp};
use mail_parse::{parse, Limits, PartKind};

fn stamp() -> Stamp {
    Stamp { date: 1_794_936_353, random: [0xab; 16] }
}

#[test]
fn a_plain_message() {
    let d = Draft {
        from: Mailbox::new("Priya Nair", "priya@example.org"),
        to: vec![Mailbox::new("Arun \"the boss\" K.", "arun@acme.example"), Mailbox::new("Łukasz Żółw", "lukasz@bücher.example")],
        cc: vec![Mailbox::new("", "lena@acme.example")],
        bcc: vec![Mailbox::new("Me", "priya@example.org"), Mailbox::new("", "ARUN@acme.example")],
        subject: "Q4 numbers — ready 🎉 and a rather long subject line that has to be folded somewhere".into(),
        text: "Hi Arun,\n\nThe numbers are in. Total = 42.\nSee you   \n".into(),
        in_reply_to: Some("<q3@acme.example>".into()),
        references: vec!["<q1@acme.example>".into(), "<q3@acme.example>".into()],
        ..Draft::default()
    };
    let w = write(&d, &stamp()).unwrap();
    let raw = String::from_utf8(w.bytes.clone()).unwrap();
    assert!(raw.split("\r\n").all(|l| l.len() <= 78 || !l.is_ascii()), "{raw}");
    assert!(!raw.contains("Bcc") && !raw.to_ascii_lowercase().contains("user-agent"));
    assert!(raw.contains("Date: Tue, 17 Nov 2026 17:25:53 +0000\r\n"));
    assert_eq!(w.rcpts, ["arun@acme.example", "lukasz@xn--bcher-kva.example", "lena@acme.example", "priya@example.org"]);
    assert_eq!(w.message_id, "<abababababababababababababababab@example.org>");

    let s = parse(&w.bytes, &Limits::default()).unwrap();
    assert_eq!(s.subject, d.subject);
    assert_eq!(s.from[0].name, "Priya Nair");
    assert_eq!(s.to[0].name, "Arun \"the boss\" K.");
    assert_eq!(s.to[1].name, "Łukasz Żółw");
    assert_eq!(s.cc[0].addr, "lena@acme.example");
    // The parser keeps ids without their brackets; the writer takes either.
    assert_eq!(s.message_id.as_deref(), Some(w.message_id.trim_matches(['<', '>'])));
    assert_eq!(s.in_reply_to, ["q3@acme.example"]);
    assert_eq!(s.references, ["q1@acme.example", "q3@acme.example"]);
    assert_eq!(s.text.map(|t| t.replace("\r\n", "\n").trim_end().to_string()).as_deref(), Some("Hi Arun,\n\nThe numbers are in. Total = 42.\nSee you"));
}

#[test]
fn html_and_attachments() {
    let d = Draft {
        from: Mailbox::new("", "priya@example.org"),
        to: vec![Mailbox::new("", "arun@acme.example")],
        subject: "Deck".into(),
        text: "See the deck. Café ☕\n".into(),
        html: Some("<p>See the <b>deck</b>. Café ☕</p>".into()),
        attachments: vec![Attachment { name: "Q4 résumé.pdf".into(), mime: "application/pdf".into(), bytes: b"%PDF-1.7 fake".to_vec() }],
        ..Draft::default()
    };
    let w = write(&d, &stamp()).unwrap();
    let s = parse(&w.bytes, &Limits::default()).unwrap();
    assert_eq!(s.text.as_deref().map(str::trim_end), Some("See the deck. Café ☕"));
    assert!(s.parts.iter().any(|p| p.kind == PartKind::Html));
    let a = s.parts.iter().find(|p| p.kind == PartKind::Attachment).expect("an attachment");
    assert_eq!(a.filename.as_deref(), Some("Q4 résumé.pdf"));
    assert_eq!(a.content_type, "application/pdf");
}

#[test]
fn nothing_can_add_a_header() {
    let d = Draft {
        from: Mailbox::new("Priya\r\nBcc: spy@x.example", "priya@example.org"),
        to: vec![Mailbox::new("", "arun@acme.example")],
        subject: "Hi\r\nBcc: spy@x.example".into(),
        text: "x".into(),
        ..Draft::default()
    };
    let raw = String::from_utf8(write(&d, &stamp()).unwrap().bytes).unwrap();
    assert!(!raw.contains("\r\nBcc:"), "{raw}");
    assert!(write(&Draft { to: vec![], ..d.clone() }, &stamp()).is_err());
    assert!(write(&Draft { in_reply_to: Some("<x@y> \r\nBcc: z".into()), ..d }, &stamp()).is_err());
}

#[test]
fn a_draft_copy() {
    let d = Draft {
        from: Mailbox::new("Priya", "priya@example.org"),
        to: vec![Mailbox::new("", "arun@acme.exa")],
        bcc: vec![Mailbox::new("", "sam@acme.example")],
        subject: "Half done".into(),
        text: "Not yet…".into(),
        ..Draft::default()
    };
    assert!(write(&Draft { to: vec![], bcc: vec![], ..d.clone() }, &stamp()).is_err(), "no recipients: not sendable");
    let w = write_draft(&Draft { to: vec![Mailbox::new("", "arun@")], ..d.clone() }, &stamp()).unwrap();
    let raw = String::from_utf8(w.bytes.clone()).unwrap();
    assert!(!raw.contains("arun@"), "a half-typed address is left out: {raw}");
    assert!(raw.contains("Bcc: <sam@acme.example>\r\n"), "{raw}");
    let s = parse(&w.bytes, &Limits::default()).unwrap();
    assert_eq!(s.subject, "Half done");
    assert!(s.to.is_empty());
}
