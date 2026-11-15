use mail_parse::{parse, part_bytes, Limits, PartKind, Refused};

fn p(s: &str) -> mail_parse::Summary {
    parse(s.as_bytes(), &Limits::default()).expect("parses")
}

#[test]
fn simple_plain() {
    let m = p("From: Priya Nair <Priya@Example.COM>\r\nTo: you@example.org\r\nSubject: Hello\r\nDate: Sun, 15 Nov 2026 22:07:54 +0530\r\nMessage-ID: <abc@example.com>\r\n\r\nHi there,\r\nsee you.\r\n");
    assert_eq!(m.subject, "Hello");
    assert_eq!(m.from[0].name, "Priya Nair");
    assert_eq!(m.from[0].addr, "Priya@example.com");
    assert_eq!(m.from[0].domain(), "example.com");
    assert_eq!(m.to[0].addr, "you@example.org");
    assert_eq!(m.message_id.as_deref(), Some("abc@example.com"));
    assert_eq!(m.date, Some(1_794_760_674));
    assert_eq!(m.text.as_deref(), Some("Hi there,\r\nsee you.\r\n"));
    assert_eq!(m.preview, "Hi there, see you.");
}

#[test]
fn bare_lf_and_folding() {
    let m = p("Subject: a long\n subject\n\tfolded\nFrom: a@b.example\n\nbody");
    assert_eq!(m.subject, "a long subject folded");
}

#[test]
fn rfc2047_words() {
    let m = p("Subject: =?UTF-8?B?w4lsw6h2ZQ==?= =?ISO-8859-1?Q?caf=E9_cr=E8me?= done\r\nFrom: =?utf-8?q?J=C3=BCrgen?= <j@x.example>\r\n\r\n.");
    assert_eq!(m.subject, "Élèvecafé crème done");
    assert_eq!(m.from[0].name, "Jürgen");
}

#[test]
fn raw_utf8_headers() {
    let m = p("Subject: Grüße aus Köln\r\nFrom: \"Zoë\" <z@x.example>\r\n\r\n.");
    assert_eq!(m.subject, "Grüße aus Köln");
    assert_eq!(m.from[0].name, "Zoë");
}

#[test]
fn bidi_and_controls_dropped() {
    let m = p("Subject: invoice\u{202E}fdp.exe\u{200B}\x07x\r\nFrom: a@b.example\r\n\r\n.");
    assert_eq!(m.subject, "invoicefdp.exex");
    let m = p("Subject: =?utf-8?b?4oCuZXhl?=\r\nFrom: a@b.example\r\n\r\n.");
    assert_eq!(m.subject, "exe");
}

#[test]
fn address_forms() {
    let m = p("To: \"Doe, Jane\" <jane@x.example>, bob@y.example (Bob B), Team: c@z.example, d@z.example;, <e@w.example>\r\nFrom: a@b.example\r\n\r\n.");
    let a: Vec<(&str, &str)> = m.to.iter().map(|a| (a.name.as_str(), a.addr.as_str())).collect();
    assert_eq!(a, vec![("Doe, Jane", "jane@x.example"), ("Bob B", "bob@y.example"), ("", "c@z.example"), ("", "d@z.example"), ("", "e@w.example")]);
}

#[test]
fn display_name_spoofs_flagged() {
    let m = p("From: \"service@paypal.com\" <x@evil.example>\r\nReply-To: PayPal.com <y@evil.example>\r\nTo: Dr. Who <w@tardis.example>\r\n\r\n.");
    assert!(m.from[0].name_looks_like_address);
    assert!(m.reply_to[0].name_looks_like_address);
    assert!(!m.to[0].name_looks_like_address);
}

#[test]
fn multipart_alternative_and_attachment() {
    let raw = "From: a@b.example\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"XX\"\r\n\r\npreamble\r\n--XX\r\nContent-Type: multipart/alternative; boundary=YY\r\n\r\n--YY\r\nContent-Type: text/plain; charset=iso-8859-1\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\nCaf=E9 au lait=\r\n, please\r\n--YY\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>Caf&eacute;</p>\r\n--YY--\r\n--XX\r\nContent-Type: application/pdf; name=\"r.pdf\"\r\nContent-Disposition: attachment; filename=\"../../etc/report.pdf\"\r\nContent-Transfer-Encoding: base64\r\n\r\nJVBERi0x\r\nLjQK\r\n--XX--\r\nepilogue\r\n";
    let m = p(raw);
    assert_eq!(m.text.as_deref(), Some("Café au lait, please"));
    assert_eq!(m.html.as_deref(), Some("<p>Caf&eacute;</p>"));
    let att = m.parts.iter().find(|p| p.kind == PartKind::Attachment).unwrap();
    assert_eq!(att.path, "2");
    assert_eq!(att.filename.as_deref(), Some("report.pdf"));
    assert_eq!(part_bytes(raw.as_bytes(), att), b"%PDF-1.4\n");
    let paths: Vec<&str> = m.parts.iter().map(|p| p.path.as_str()).collect();
    assert_eq!(paths, vec!["", "1", "1.1", "1.2", "2"]);
}

#[test]
fn rfc2231_filename() {
    let raw = "From: a@b.example\r\nContent-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\n\r\nbody\r\n--b\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment;\r\n filename*0*=utf-8''R%C3%A9sum;\r\n filename*1*=%C3%A9.pdf\r\n\r\nxx\r\n--b--\r\n";
    let m = p(raw);
    assert_eq!(m.text.as_deref(), Some("body"));
    let att = m.parts.iter().find(|p| p.kind == PartKind::Attachment).unwrap();
    assert_eq!(att.filename.as_deref(), Some("Résumé.pdf"));
}

#[test]
fn inline_image_and_calendar() {
    let raw = "From: a@b.example\r\nContent-Type: multipart/related; boundary=r\r\n\r\n--r\r\nContent-Type: text/html\r\n\r\n<img src=cid:logo>\r\n--r\r\nContent-Type: image/png\r\nContent-ID: <logo>\r\nContent-Transfer-Encoding: base64\r\n\r\niVBORw0KGgo=\r\n--r\r\nContent-Type: text/calendar; method=REQUEST\r\n\r\nBEGIN:VCALENDAR\r\n--r--\r\n";
    let m = p(raw);
    let kinds: Vec<PartKind> = m.parts.iter().map(|p| p.kind).collect();
    assert_eq!(kinds, vec![PartKind::Multipart, PartKind::Html, PartKind::Inline, PartKind::Calendar]);
    assert_eq!(m.parts[2].content_id.as_deref(), Some("logo"));
}

#[test]
fn lists_and_receipts() {
    let m = p("From: news@shop.example\r\nList-Id: Shop news <news.shop.example>\r\nList-Unsubscribe: <mailto:unsub@shop.example?subject=x>, <https://shop.example/u/123>, <javascript:alert(1)>, <http://plain.example/u>\r\nList-Unsubscribe-Post: List-Unsubscribe=One-Click\r\nPrecedence: bulk\r\nDisposition-Notification-To: news@shop.example\r\n\r\n.");
    assert_eq!(m.list_unsubscribe, vec!["mailto:unsub@shop.example?subject=x", "https://shop.example/u/123"]);
    assert!(m.one_click_unsubscribe && m.automated && m.wants_receipt);
    assert_eq!(m.list_id.as_deref(), Some("Shop news <news.shop.example>"));
}

#[test]
fn threading_ids() {
    let m = p("From: a@b.example\r\nIn-Reply-To: <p1@x>\r\nReferences: <r1@x>\r\n <r2@x> junk <p1@x>\r\n\r\n.");
    assert_eq!(m.in_reply_to, vec!["p1@x"]);
    assert_eq!(m.references, vec!["r1@x", "r2@x", "p1@x"]);
}

#[test]
fn html_only_preview() {
    let m = p("From: a@b.example\r\nContent-Type: text/html\r\n\r\n<html><head><title>T</title><style>p{}</style></head><body><p>Hello&nbsp;&amp; welcome &#8212; friend</p><script>x()</script></body></html>");
    assert_eq!(m.preview, "Hello & welcome — friend");
}

#[test]
fn preview_skips_quotes_and_signature() {
    let m = p("From: a@b.example\r\n\r\nSounds good.\r\n\r\n> earlier\r\n> text\r\n-- \r\nPriya\r\n");
    assert_eq!(m.preview, "Sounds good.");
}

#[test]
fn unknown_charset_falls_back() {
    let m = parse(b"From: a@b.example\r\nContent-Type: text/plain; charset=x-made-up\r\nContent-Transfer-Encoding: 8bit\r\n\r\ncaf\xe9", &Limits::default()).unwrap();
    assert!(m.unknown_charset);
    assert_eq!(m.text.as_deref(), Some("caf\u{e9}"));
    let m = p("From: a@b.example\r\nContent-Type: text/plain; charset=utf-7\r\n\r\n+ADw-script+AD4-");
    assert!(m.unknown_charset);
    assert_eq!(m.text.as_deref(), Some("+ADw-script+AD4-"));
}

#[test]
fn dates() {
    use mail_parse::parse_date;
    assert_eq!(parse_date("Thu, 1 Jan 1970 00:00:00 GMT"), Some(0));
    assert_eq!(parse_date("1 Jan 70 01:00:00 +0100"), Some(0));
    assert_eq!(parse_date("Fri, 31 Dec 1999 23:59:60 -0800 (PST)"), Some(946_684_800 + 8 * 3600));
    assert_eq!(parse_date("Thu, 1 Jan 1970 00:00:00 EST"), Some(5 * 3600));
    assert_eq!(parse_date("garbage"), None);
    assert_eq!(parse_date("32 Jan 2020 00:00:00 +0000"), None);
}

#[test]
fn hard_limits() {
    let l = Limits { max_message: 100, ..Limits::default() };
    assert_eq!(parse(&[b'a'; 101], &l).unwrap_err(), Refused::TooLarge);
    let many = "X-A: 1\r\n".repeat(1001) + "\r\n.";
    assert_eq!(parse(many.as_bytes(), &Limits::default()).unwrap_err(), Refused::TooManyHeaders);
    // 40 nested multiparts
    let mut raw = String::from("From: a@b.example\r\n");
    for i in 0..40 {
        raw.push_str(&format!("Content-Type: multipart/mixed; boundary=b{i}\r\n\r\n--b{i}\r\n"));
    }
    raw.push_str("\r\nx\r\n");
    assert_eq!(parse(raw.as_bytes(), &Limits::default()).unwrap_err(), Refused::TooDeep);
    // too many parts
    let mut raw = String::from("From: a@b.example\r\nContent-Type: multipart/mixed; boundary=b\r\n\r\n");
    for _ in 0..1001 {
        raw.push_str("--b\r\n\r\nx\r\n");
    }
    raw.push_str("--b--\r\n");
    assert_eq!(parse(raw.as_bytes(), &Limits::default()).unwrap_err(), Refused::TooManyParts);
    assert_eq!(parse(b"\r\nno headers", &Limits::default()).unwrap_err(), Refused::Empty);
}

#[test]
fn text_cap_truncates() {
    let l = Limits { max_text: 10, ..Limits::default() };
    let m = parse("From: a@b.example\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nééééééééé".as_bytes(), &l).unwrap();
    assert!(m.truncated);
    assert_eq!(m.text.as_deref(), Some("ééééé"));
}

#[test]
fn missing_close_and_no_boundary() {
    let m = p("From: a@b.example\r\nContent-Type: multipart/mixed; boundary=z\r\n\r\n--z\r\n\r\nonly part, never closed");
    assert_eq!(m.text.as_deref(), Some("only part, never closed"));
    let m = p("From: a@b.example\r\nContent-Type: multipart/mixed\r\n\r\nstuff");
    assert_eq!(m.parts[0].kind, PartKind::Attachment);
}

#[test]
fn never_panics_on_junk() {
    // cheap deterministic smoke run; the fuzz target does the real work
    let mut x: u64 = 0x9E3779B97F4A7C15;
    let pieces: [&[u8]; 14] = [b"\r\n", b"\n", b"--b", b"Content-Type: multipart/mixed; boundary=b", b"=?utf-8?B?", b"?=", b"=", b"<", b">", b"\"", b"(", b":", b"\xff\xfe", b"Content-Transfer-Encoding: quoted-printable"];
    for _ in 0..20000 {
        let mut msg = b"From: a@b.example\r\n".to_vec();
        for _ in 0..(x % 40) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            msg.extend_from_slice(pieces[(x % pieces.len() as u64) as usize]);
        }
        let _ = parse(&msg, &Limits::default());
    }
}
