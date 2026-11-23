//! The window without a display: a store filled through the engine with
//! real parse jails, the view rendered into Pergola's tree, a press on a
//! row and j/k, and what the reader then shows.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use atrium_mail::MailView;
use mail_engine::{Engine, Parser};
use mail_store::Store;
use mail_sync::{Fetched, RemoteFolder, Role, Sink};
use pergola::geom::Point;
use pergola::node::Node;
use pergola::{App, Event, Key, KeyEventKind, Modifiers};

const NOW: i64 = 1_795_425_660;

fn jail() -> PathBuf {
    if let Some(parser) = std::env::var_os("MAIL_PARSE_JAIL") {
        return PathBuf::from(parser);
    }
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-parse/Cargo.toml");
    let st = Command::new(env!("CARGO")).args(["build", "-q", "--bin", "mail-parse-jail", "--manifest-path"]).arg(&manifest).status().unwrap();
    assert!(st.success());
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-parse/target/debug/mail-parse-jail")
}

fn message(i: u32, from: &str, subject: &str, body: &str, html: bool, t: i64) -> Vec<u8> {
    // Every test date is on 23 Nov 2026 (UTC).
    let s = t.rem_euclid(86_400);
    let date = format!("Mon, 23 Nov 2026 {:02}:{:02}:{:02} +0000", s / 3600, s % 3600 / 60, s % 60);
    let ct = if html { "text/html; charset=utf-8" } else { "text/plain; charset=utf-8" };
    format!("From: {from}\r\nTo: Priya <priya@example.org>\r\nSubject: {subject}\r\nDate: {date}\r\nMessage-ID: <m{i}@example.org>\r\nMIME-Version: 1.0\r\nContent-Type: {ct}\r\n\r\n{body}\r\n").into_bytes()
}

fn texts(app: &App<MailView>) -> Vec<String> {
    app.tree().iter().filter_map(|(_, n)| match n { Node::Text { content, .. } => Some(content.clone()), _ => None }).collect()
}

fn note(r: &atrium_mail::view::Shown) -> String {
    r.msgs[0].body.as_ref().and_then(|b| b.note.clone()).unwrap_or_default()
}

fn key(chars: &str) -> Event {
    Event::Key { kind: KeyEventKind::Down, key: Key::Char, modifiers: Modifiers::default(), chars: chars.into() }
}

#[test]
fn list_and_read() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Rc::new(Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() }));
    let account = engine.store.add_account("priya@example.org").unwrap();
    {
        let mut sink = engine.sink(account);
        sink.folders(&[
            RemoteFolder { name: "INBOX".into(), role: Role::Inbox, selectable: true },
            RemoteFolder { name: "Sent".into(), role: Role::Sent, selectable: true },
        ]).unwrap();
        let long = "Here are the Q4 numbers we talked about on Monday, with the regional split broken out for each team. ".repeat(12);
        let msgs = [
            (1, "Arun <arun@acme.example>", "Q4 numbers", long.as_str(), false, true, NOW - 3600),
            (2, "Lena <lena@partner.example>", "Lunch on Friday?", "Shall we try the new place?", false, true, NOW - 1800),
            (3, "News <news@list.example>", "This week", "<p>Only <b>HTML</b> here</p>", true, true, NOW - 900),
            (4, "Sam <sam@acme.example>", "Headers only so far", "", false, false, NOW - 60),
        ];
        let page = msgs.iter().map(|&(uid, from, subj, body, html, full, date)| {
            let mut bytes = message(uid, from, subj, body, html, date);
            if !full {
                let end = bytes.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                bytes.truncate(end);
            }
            Fetched { uid, flags: if uid == 1 { vec!["\\Seen".into()] } else { vec![] }, size: 1000, internal_date: Some(date), server_id: None, bytes, full }
        });
        // Headers-only and whole messages come in separate pages.
        let (whole, heads): (Vec<_>, Vec<_>) = page.partition(|f| f.full);
        sink.messages("INBOX", whole).unwrap();
        sink.messages("INBOX", heads).unwrap();
    }

    let dirty = Arc::new(AtomicBool::new(true));
    let view = MailView::new(engine, dirty.clone(), (1280.0, 800.0), || NOW);
    let mail = view.mail.clone();
    let mut app = App::new_with_flag(view, dirty);
    app.tick();

    // Places, the inbox chosen, and its rows newest first.
    let ui = mail.ui.get_cloned();
    assert_eq!(ui.places.iter().map(|p| (p.label.as_str(), p.count)).collect::<Vec<_>>(), [("Inbox", 4), ("Sent", 0)]);
    assert_eq!(ui.count, 4);
    let t = texts(&app);
    for s in ["Inbox", "Sent", "Sam", "Headers only so far", "Lena", "Lunch on Friday?", "Arun", "Q4 numbers", "Choose a message to read"] {
        assert!(t.iter().any(|x| x == s), "{s:?} not drawn: {t:?}");
    }
    let order: Vec<String> = (0..4).map(|i| mail.row(i).unwrap().subject).collect();
    assert_eq!(order, ["Headers only so far", "This week", "Lunch on Friday?", "Q4 numbers"]);

    // A press on the fourth row (the list starts under its heading and
    // the filter chips, rows are 72 pt) opens Arun's message, wrapped to
    // the column.
    let at = Point::new(224.0 + 200.0, mail.list_top() + 72.0 * 3.0 + 36.0);
    app.handle_event(Event::PointerDown { at });
    app.handle_event(Event::PointerUp { at });
    app.tick();
    let ui = mail.ui.get_cloned();
    assert_eq!(ui.cursor, Some(3));
    let r = ui.reading.clone().unwrap();
    let m = &r.msgs[0];
    assert_eq!((r.subject.as_str(), m.sender.name.as_str(), m.to.as_str()), ("Q4 numbers", "Arun", "Priya"));
    assert_eq!(m.sender.detail, "arun@acme.example", "unsigned: the whole address, not a domain");
    assert!(m.body.as_ref().unwrap().note.is_none());
    let t = texts(&app);
    let body: Vec<&String> = t.iter().filter(|x| x.starts_with("Here are the Q4") || x.contains("regional split")).collect();
    assert!(body.len() > 2, "the text should wrap over several lines: {t:?}");
    assert!(body.iter().all(|l| l.chars().count() < 110), "lines stay inside the reading column");

    // k goes up to Lena's; j back down; k twice more reaches the HTML-only
    // one, which says so, then the one with only its headers.
    assert!(mail.key(&key("k"), None));
    app.tick();
    assert_eq!(mail.ui.get_cloned().reading.unwrap().subject, "Lunch on Friday?");
    assert!(texts(&app).iter().any(|x| x == "Shall we try the new place?"));
    // ⌃P prints the message as it reads: its subject, who and when, its text.
    assert!(mail.take_print().is_none());
    let ctrl_p = Event::Key { kind: KeyEventKind::Down, key: Key::Char, modifiers: Modifiers { ctrl: true, ..Modifiers::default() }, chars: "p".into() };
    assert!(mail.key(&ctrl_p, None));
    let p = mail.take_print().unwrap();
    assert_eq!((p.subject.as_str(), p.text.as_str()), ("Lunch on Friday?", "Shall we try the new place?"));
    assert!(p.head.starts_with("From: Lena <lena@partner.example>\nTo: Priya"), "{:?}", p.head);
    assert!(texts(&app).iter().any(|x| x == "Print"), "the reader has a Print button");
    assert!(mail.key(&key("k"), None));
    let r = mail.ui.get_cloned().reading.unwrap();
    assert_eq!(r.subject, "This week");
    assert_eq!(r.framed, Some(0), "an HTML-only message goes to the doc-viewer");
    assert!(mail.key(&ctrl_p, None));
    assert_eq!(mail.take_print().unwrap().text.trim(), "Only HTML here", "an HTML body prints its text");
    assert!(mail.key(&key("k"), None));
    let r = mail.ui.get_cloned().reading.unwrap();
    assert_eq!(r.subject, "Headers only so far");
    assert!(note(&r).contains("headers"));
    assert!(mail.key(&ctrl_p, None));
    assert!(mail.take_print().is_none(), "an incomplete message must not print a preview");
    assert!(mail.key(&key("k"), None), "at the top k stays");
    assert_eq!(mail.ui.get_cloned().cursor, Some(0));
    assert!(!mail.key(&key("x"), None), "other keys are not Mail's");

    // Sent is empty and says so.
    let sent = mail.ui.get_cloned().places[1].folder;
    mail.choose_folder(sent);
    app.tick();
    assert!(texts(&app).iter().any(|x| x == "Nothing here"));
    assert!(mail.ui.get_cloned().reading.is_none());
}

fn threaded(uid: u32, from: &str, subject: &str, body: &str, t: i64, mid: &str, refs: &str) -> Fetched {
    let s = t.rem_euclid(86_400);
    let date = format!("Mon, 23 Nov 2026 {:02}:{:02}:{:02} +0000", s / 3600, s % 3600 / 60, s % 60);
    let refs = if refs.is_empty() { String::new() } else { format!("References: {refs}\r\nIn-Reply-To: {refs}\r\n") };
    let bytes = format!("From: {from}\r\nTo: Priya <priya@example.org>\r\nSubject: {subject}\r\nDate: {date}\r\nMessage-ID: {mid}\r\n{refs}MIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8; format=flowed\r\n\r\n{body}\r\n").into_bytes();
    Fetched { uid, flags: if uid == 1 { vec!["\\Seen".into()] } else { vec![] }, size: 1000, internal_date: Some(t), server_id: None, bytes, full: true }
}

#[test]
fn a_conversation() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Rc::new(Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() }));
    let account = engine.store.add_account("priya@example.org").unwrap();
    {
        let mut sink = engine.sink(account);
        sink.folders(&[RemoteFolder { name: "INBOX".into(), role: Role::Inbox, selectable: true }]).unwrap();
        sink.messages("INBOX", vec![
            threaded(1, "Lena <lena@partner.example>", "Venue", "Shall we book the hall for \r\nthe 4th?", NOW - 7200, "<a@x>", ""),
            threaded(2, "Arun <arun@acme.example>", "Re: Venue", "Yes, see https://hall.example/book.\r\n\r\nOn Mon, Lena wrote:\r\n> Shall we book the hall for the 4th?\r\n\r\n-- \r\nArun\r\nAcme", NOW - 3600, "<b@x>", "<a@x>"),
            threaded(3, "Sam <sam@other.example>", "Parking", "Where do we park?", NOW - 600, "<c@x>", ""),
        ]).unwrap();
    }
    let dirty = Arc::new(AtomicBool::new(true));
    let view = MailView::new(engine, dirty.clone(), (1280.0, 800.0), || NOW);
    let mail = view.mail.clone();
    let mut app = App::new_with_flag(view, dirty);
    app.tick();

    // Two conversations, the venue one with both people and a count.
    let ui = mail.ui.get_cloned();
    assert!(ui.conversations);
    assert_eq!(ui.count, 2);
    let r = mail.row(1).unwrap();
    assert_eq!((r.people.as_str(), r.count, r.subject.as_str()), ("Lena, Arun", 2, "Venue"));
    let t = texts(&app);
    assert!(t.iter().any(|x| x == "Lena, Arun") && t.iter().any(|x| x == " 2"), "{t:?}");
    assert!(t.iter().any(|x| x == "Conversations") && t.iter().any(|x| x == "Messages"));

    // Opening it: Lena's (read) folded, the new line, Arun's open with its
    // quote and signature folded and the link a link.
    mail.open(1);
    app.tick();
    let s = mail.ui.get_cloned().reading.unwrap();
    assert_eq!(s.msgs.iter().map(|m| m.open).collect::<Vec<_>>(), [false, true]);
    assert_eq!(s.people, "Lena, Arun · 2 messages");
    let items = atrium_mail::view::lay_out(&s, 560.0);
    use atrium_mail::view::Item;
    assert!(matches!(items[0], Item::Folded(0)));
    assert!(matches!(items[1], Item::New));
    assert!(items.iter().any(|i| matches!(i, Item::Fold { quoted: true, shown: false, .. })));
    assert!(items.iter().any(|i| matches!(i, Item::Fold { quoted: false, shown: false, .. })));
    // Arun has never written before (§7.5).
    assert_eq!(s.msgs[1].who, Some(("First message from this address".into(), false)));
    assert!(items.contains(&Item::Who(1)));
    app.tick();
    assert!(texts(&app).iter().any(|x| x == "First message from this address"));
    let link = items.iter().find_map(|i| match i { Item::Text { spans, .. } => spans.iter().find_map(|(_, l)| l.clone()), _ => None });
    assert_eq!(link.as_deref(), Some("https://hall.example/book"));
    let t = texts(&app);
    assert!(t.iter().any(|x| x == "https://hall.example/book"), "{t:?}");
    assert_eq!(t.iter().filter(|x| x.contains("Shall we book the hall for the 4th")).count(), 1, "only Lena's folded preview: the quote is folded");
    assert!(t.iter().any(|x| x == "Show quoted text"));

    // Show the quote; open Lena's (format=flowed joined into one line).
    mail.unfold(1, true);
    mail.toggle(0);
    app.tick();
    let t = texts(&app);
    assert!(t.iter().any(|x| x == "Shall we book the hall for the 4th?"), "{t:?}");
    assert_eq!(t.iter().filter(|x| x.contains("Shall we book the hall")).count(), 2, "Lena's text and Arun's quote of it");
    assert!(t.iter().any(|x| x == "Hide quoted text"));

    // Messages: three rows, one per message, and the choice is kept.
    mail.set_conversations(false);
    app.tick();
    assert_eq!(mail.ui.get_cloned().count, 3);
    let inbox = mail.ui.get_cloned().folder.unwrap();
    assert!(!mail.engine.store.view(inbox).unwrap());
}

#[test]
fn an_html_body_is_framed_for_the_doc_viewer() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Rc::new(Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() }));
    let account = engine.store.add_account("priya@example.org").unwrap();
    let html = |uid: u32, mid: &str, refs: &str, t: i64, body: &str| {
        let refs = if refs.is_empty() { String::new() } else { format!("References: {refs}\r\n") };
        let bytes = format!("From: Shop <news@shop.example>\r\nTo: p@example.org\r\nSubject: Sale\r\nMessage-ID: {mid}\r\n{refs}Date: Mon, 23 Nov 2026 0{uid}:00:00 +0000\r\nMIME-Version: 1.0\r\nContent-Type: text/html; charset=utf-8\r\n\r\n{body}\r\n").into_bytes();
        Fetched { uid, flags: vec![], size: 1000, internal_date: Some(t), server_id: None, bytes, full: true }
    };
    {
        let mut sink = engine.sink(account);
        sink.folders(&[RemoteFolder { name: "INBOX".into(), role: Role::Inbox, selectable: true }]).unwrap();
        sink.messages("INBOX", vec![
            html(1, "<s1@x>", "", NOW - 7200, "<h1>First sale</h1><img src=\"https://cdn.example/a.png\" width=\"50\" height=\"50\">"),
            html(2, "<s2@x>", "<s1@x>", NOW - 3600, "<h1>Second sale</h1><img src=\"data:image/gif;base64,R0lGODlhAQABAAAAACw=\" width=\"1\" height=\"1\">"),
        ]).unwrap();
    }
    let dirty = Arc::new(AtomicBool::new(true));
    let view = MailView::new(engine.clone(), dirty.clone(), (1280.0, 800.0), || NOW);
    let mail = view.mail.clone();
    let mut app = App::new_with_flag(view, dirty);
    app.tick();
    mail.open(0);
    app.tick();
    // Both unread, so both open; the newest is framed, the other offers itself.
    let s = mail.ui.get_cloned().reading.unwrap();
    assert_eq!(s.framed, Some(1));
    let (msg, doc, dark) = mail.framed().unwrap();
    assert_eq!(msg, s.msgs[1].msg);
    assert!(doc.0.html.contains("Second sale") && !dark);
    assert_eq!(doc.0.pictures.len(), 1, "the data: picture is the message's own");
    let t = texts(&app);
    assert!(t.iter().any(|x| x == "Show this message"), "{t:?}");
    // Choosing the first moves the frame.
    let items = atrium_mail::view::lay_out(&s, 560.0);
    assert!(!items.iter().any(|i| matches!(i, atrium_mail::view::Item::Text { .. })), "the bodies are the doc-viewer's, not drawn by Mail");
    assert!(items.iter().any(|i| *i == atrium_mail::view::Item::ShowHtml(0)));
    mail.frame(0);
    assert!(mail.framed().unwrap().1 .0.html.contains("First sale"));
    // The document was kept: opening again parses nothing (the stored one is the same).
    let m = engine.store.message(s.msgs[1].msg).unwrap();
    assert!(m.document.is_some());
    let again = engine.open(s.msgs[1].msg).unwrap();
    assert_eq!(again.document.as_ref().map(|d| &d.html), Some(&doc.0.html));

    // Remote pictures (§7.4): a bar, and nothing fetched until it is pressed.
    let m0 = s.msgs[0].msg;
    assert_eq!(s.msgs[0].body.as_ref().unwrap().remote, 1);
    assert_eq!(s.msgs[1].body.as_ref().unwrap().remote, 0, "a data: picture is not remote");
    app.tick();
    let t = texts(&app);
    assert!(t.iter().any(|x| x == "1 picture not loaded") && t.iter().any(|x| x == "Load pictures"), "{t:?}");
    assert!(!t.iter().any(|x| x == "Always for this sender"), "not offered for an unverified sender");
    assert!(mail.take_loads().is_empty());
    // "Always" is refused for an unverified sender even if asked.
    mail.load_pictures(0, true);
    assert!(!engine.store.always_pictures(account, "news@shop.example").unwrap());
    assert_eq!(mail.take_loads(), [m0]);
    app.tick();
    assert!(texts(&app).iter().any(|x| x == "Loading pictures…"));
    let want = engine.pictures_to_load(m0).unwrap();
    assert_eq!(want.iter().map(|w| w.1.as_str()).collect::<Vec<_>>(), ["https://cdn.example/a.png"]);
    mail.pictures_loaded(m0, vec![(want[0].0.clone(), b"\x89PNG fetched".to_vec())]);
    let b = mail.ui.get_cloned().reading.unwrap().msgs[0].body.clone().unwrap();
    assert_eq!((b.remote, b.loading), (0, false));
    assert_eq!(engine.pictures(m0).unwrap().len(), 1);
    app.tick();
    let t = texts(&app);
    assert!(!t.iter().any(|x| x.contains("not loaded") || x.contains("Loading")), "{t:?}");
}

#[test]
fn the_link_bar_shows_where_a_link_goes_and_opens_it_through_monstrator() {
    use std::io::{BufRead, BufReader, Write};
    // A Monstrator that answers Opened and keeps what it was asked.
    let sockdir = tempfile::tempdir().unwrap();
    let sock = sockdir.path().join("monstrator.sock");
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let asked = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen = asked.clone();
    std::thread::spawn(move || {
        for s in listener.incoming() {
            let Ok(mut s) = s else { break };
            let mut line = String::new();
            BufReader::new(s.try_clone().unwrap()).read_line(&mut line).unwrap();
            seen.lock().unwrap().push(line);
            s.write_all(b"\"Opened\"\n").unwrap();
        }
    });
    std::env::set_var("MONSTRATOR_SOCKET", &sock);

    let dir = tempfile::tempdir().unwrap();
    let engine = Rc::new(Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() }));
    let account = engine.store.add_account("priya@example.org").unwrap();
    let wrapped = "https://eur01.safelinks.protection.outlook.com/?url=https%3A%2F%2Flogin.evil.example%2Fx%3Futm_source%3Dmail&data=05";
    let body = format!("<p>Your account: <a href=\"{wrapped}\">https://www.bank.com/login</a></p>");
    {
        let mut sink = engine.sink(account);
        sink.folders(&[RemoteFolder { name: "INBOX".into(), role: Role::Inbox, selectable: true }]).unwrap();
        let fetched = |uid, body: &str, html, date| {
            let bytes = message(uid, "Bank <alerts@bank.example>", if html { "Verify now" } else { "Plain text link" }, body, html, date);
            Fetched { uid, flags: vec![], size: bytes.len() as u64, internal_date: Some(date), server_id: None, bytes, full: true }
        };
        sink.messages("INBOX", vec![fetched(1, &body, true, NOW - 60), fetched(2, wrapped, false, NOW - 120)]).unwrap();
    }
    let dirty = Arc::new(AtomicBool::new(true));
    let view = MailView::new(engine.clone(), dirty.clone(), (1280.0, 800.0), || NOW);
    let mail = view.mail.clone();
    let mut app = App::new_with_flag(view, dirty);
    app.tick();
    mail.open(0);
    // The doc-viewer reports the press as LinkActivated with the href.
    let doc = mail.framed().unwrap().1;
    let (href, text) = doc.0.links.first().cloned().unwrap();
    assert_eq!(text, "https://www.bank.com/login");
    mail.link(&href);
    app.tick();
    let l = mail.ui.get_cloned().link.unwrap();
    let b = l.bar.clone().unwrap();
    assert_eq!(b.open, "https://login.evil.example/x", "peeled, without the tracker");
    assert_eq!((b.sub.as_str(), b.domain.as_str()), ("login.", "evil.example"));
    assert_eq!(b.says.as_deref(), Some("https://www.bank.com/login"));
    let t = texts(&app);
    assert!(t.iter().any(|x| x == "evil.example") && t.iter().any(|x| x == "Open") && t.iter().any(|x| x == "Copy"), "{t:?}");
    assert!(t.iter().any(|x| x.starts_with("The link's text says https://www.bank.com/login")), "{t:?}");

    // Enter with no press token doesn't open; with one, Monstrator is asked.
    let enter = |a| mail.key(&Event::Key { kind: KeyEventKind::Down, key: Key::Enter, modifiers: Modifiers::default(), chars: String::new() }, a);
    assert!(enter(None));
    assert!(asked.lock().unwrap().is_empty());
    assert!(enter(Some([7; 16])));
    let t0 = std::time::Instant::now();
    while mail.ui.get_cloned().link.is_some() && t0.elapsed() < std::time::Duration::from_secs(5) {
        std::thread::sleep(std::time::Duration::from_millis(20));
        mail.poll_link();
    }
    assert!(mail.ui.get_cloned().link.is_none(), "opened: the bar closes");
    let line = asked.lock().unwrap().pop().unwrap();
    assert!(line.contains("https://login.evil.example/x") && !line.contains("safelinks") && line.contains("[7,7,7"), "{line}");

    // A plain-text link, then Escape.
    mail.link("https://acme.example/report?utm_campaign=q4");
    assert_eq!(mail.ui.get_cloned().link.unwrap().bar.unwrap().open, "https://acme.example/report");
    assert!(mail.key(&Event::Key { kind: KeyEventKind::Down, key: Key::Escape, modifiers: Modifiers::default(), chars: String::new() }, None));
    assert!(mail.ui.get_cloned().link.is_none());

    // In Work, company link scanning is kept (§7.6): the bar shows where it
    // goes, and what opens is the Safe Links link as it came.
    *mail.persona.borrow_mut() = Some("work".into());
    mail.link(&href);
    app.tick();
    let b = mail.ui.get_cloned().link.unwrap().bar.unwrap();
    assert_eq!((b.domain.as_str(), b.scanned), ("evil.example", Some("Microsoft Safe Links")));
    assert_eq!(b.open, wrapped);
    // The mismatch warning still comes first; without it, the scanner is named.
    assert!(texts(&app).iter().any(|x| x.starts_with("The link's text says")));
    let other = "https://eur01.safelinks.protection.outlook.com/?url=https%3A%2F%2Fdash.acme.example%2Fq4&data=05";
    mail.link(other);
    app.tick();
    assert!(texts(&app).iter().any(|x| x == "Opens through Microsoft Safe Links, your company's link scanning"), "{:?}", texts(&app));
    assert_eq!(mail.ui.get_cloned().link.unwrap().bar.unwrap().open, other);
    assert!(enter(Some([7; 16])));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while mail.ui.get_cloned().link.is_some() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
        mail.poll_link();
    }
    assert!(mail.ui.get_cloned().link.is_none(), "Monstrator opened the Work link");
    let request = asked.lock().unwrap().pop().unwrap();
    assert!(request.contains(other), "{request}");
    // Turned off: peeled as in Personal.
    engine.set_prefs(&mail_engine::prefs::Prefs { link_scanning: Some(false), ..engine.prefs() }).unwrap();
    mail.link(wrapped);
    assert_eq!(mail.ui.get_cloned().link.unwrap().bar.unwrap().open, "https://login.evil.example/x");
    mail.link(other);
    assert_eq!(mail.ui.get_cloned().link.unwrap().bar.unwrap().open, "https://dash.acme.example/q4");
    mail.close_link();
    mail.open(1);
    app.tick();
    let at = app.tree().iter().find_map(|(_, node)| match node {
        Node::Text { rect, content, .. } if rect.origin.x > 624.0 && content.starts_with("https://eur01.safelinks") => {
            Some(Point::new(rect.origin.x + 4.0, rect.origin.y + rect.size.h / 2.0))
        }
        _ => None,
    }).expect("plain-text link");
    app.handle_event(Event::PointerDown { at });
    app.handle_event(Event::PointerUp { at });
    assert_eq!(mail.ui.get_cloned().link.unwrap().bar.unwrap().open, "https://login.evil.example/x");
}

#[test]
fn archive_delete_and_undo_from_the_keyboard() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Rc::new(Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() }));
    let account = engine.store.add_account("priya@example.org").unwrap();
    {
        let mut sink = engine.sink(account);
        sink.folders(&[
            RemoteFolder { name: "INBOX".into(), role: Role::Inbox, selectable: true },
            RemoteFolder { name: "Archive".into(), role: Role::Archive, selectable: true },
            RemoteFolder { name: "Trash".into(), role: Role::Trash, selectable: true },
            RemoteFolder { name: "Junk".into(), role: Role::Junk, selectable: true },
        ]).unwrap();
        let f = |uid: u32, subject: &str, t: i64| {
            let bytes = message(uid, "Arun <arun@example.org>", subject, "Text.", false, t);
            Fetched { uid, flags: vec![], size: bytes.len() as u64, internal_date: Some(t), server_id: None, bytes, full: true }
        };
        sink.messages("INBOX", vec![f(1, "One", NOW - 300), f(2, "Two", NOW - 200), f(3, "Three", NOW - 100)]).unwrap();
        sink.messages("Junk", vec![f(5, "Real order", NOW - 50)]).unwrap();
    }
    let dirty = Arc::new(AtomicBool::new(true));
    let view = MailView::new(engine.clone(), dirty.clone(), (1280.0, 800.0), || NOW);
    let mail = view.mail.clone();
    let mut app = App::new_with_flag(view, dirty);
    app.tick();
    let subjects = |m: &atrium_mail::view::Mail| (0..m.ui.get_cloned().count).map(|i| m.row(i).unwrap().subject).collect::<Vec<_>>();
    assert_eq!(subjects(&mail), ["Three", "Two", "One"]);

    // Opening reads it: in the list at once, on the server at the next sync.
    assert!(mail.key(&key("j"), None));
    assert!(!mail.row(0).unwrap().unread);
    assert_eq!(engine.store.ops_pending().unwrap(), 1);
    app.tick();
    let t = texts(&app);
    assert!(t.iter().any(|x| x == "Archive") && t.iter().any(|x| x == "e") && t.iter().any(|x| x == "#"), "{t:?}");

    // e: archived, the next one opens in its place, Undo offered.
    assert!(mail.key(&key("e"), None));
    assert_eq!(subjects(&mail), ["Two", "One"]);
    let ui = mail.ui.get_cloned();
    assert_eq!(ui.reading.as_ref().unwrap().subject, "Two");
    assert_eq!(ui.undo.as_ref().unwrap().label, "Message archived");
    app.tick();
    let t = texts(&app);
    assert!(t.iter().any(|x| x == "Message archived") && t.iter().any(|x| x == "Undo"), "{t:?}");
    // z: back.
    assert!(mail.key(&key("z"), None));
    assert_eq!(subjects(&mail), ["Three", "Two", "One"]);
    assert!(mail.ui.get_cloned().undo.is_none());
    assert!(!mail.key(&key("z"), None), "nothing more to undo");

    // #: to Trash.
    mail.open(2);
    assert!(mail.key(&key("#"), None));
    assert_eq!(subjects(&mail), ["Three", "Two"]);
    assert_eq!(mail.ui.get_cloned().undo.unwrap().label, "Message deleted");
    // Due after the undo window: archive and delete wait, the reads don't.
    let real_now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let due_now = engine.store.ops_due(account, real_now).unwrap();
    assert!(due_now.iter().all(|(_, o)| matches!(o.kind, mail_store::OpKind::Flags { .. })), "{due_now:?}");

    // !: to Junk (§7.7), marked $Junk for the server's filter; z undoes it.
    mail.open(0);
    assert!(mail.key(&key("!"), None));
    assert_eq!(subjects(&mail), ["Two"]);
    assert_eq!(mail.ui.get_cloned().undo.unwrap().label, "Message moved to Junk");
    let pending = engine.store.ops_due(account, i64::MAX).unwrap();
    assert!(pending.iter().any(|(_, o)| o.kind == mail_store::OpKind::Flags { add: mail_store::flag::JUNK, remove: mail_store::flag::NOT_JUNK }), "{pending:?}");
    assert!(mail.key(&key("z"), None));
    assert_eq!(subjects(&mail), ["Three", "Two"]);
    // Report phishing (§7.7): to Junk, marked $Junk and $Phishing; undone.
    mail.open(0);
    let report = mail.palette_entries().into_iter().find(|e| e.label == "Report phishing").expect("offered");
    mail.run_cmd(report.cmd);
    assert_eq!(mail.ui.get_cloned().undo.unwrap().label, "Message reported as phishing");
    let pending = engine.store.ops_due(account, i64::MAX).unwrap();
    assert!(pending.iter().any(|(_, o)| o.kind == mail_store::OpKind::Flags { add: mail_store::flag::JUNK | mail_store::flag::PHISHING, remove: mail_store::flag::NOT_JUNK }), "{pending:?}");
    assert!(mail.key(&key("z"), None));
    assert_eq!(subjects(&mail), ["Three", "Two"]);
    // With the company's report address (Work): sent there too; Undo takes
    // the report back unsent.
    engine.set_prefs(&mail_engine::prefs::Prefs { report_to: Some("phish@acme.example".into()), ..engine.prefs() }).unwrap();
    mail.open(0);
    mail.run_cmd(report.cmd);
    let undo = mail.ui.get_cloned().undo.unwrap();
    assert_eq!(undo.label, "Message reported as phishing and sent to phish@acme.example");
    let sent = engine.store.outbox().unwrap();
    assert!(sent.iter().any(|(k, o)| Some(*k) == undo.report && o.rcpts == ["phish@acme.example"]), "{sent:?}");
    assert!(mail.key(&key("z"), None));
    assert!(engine.store.outbox().unwrap().iter().all(|(k, _)| Some(*k) != undo.report));
    assert!(mail.take_composes().is_empty(), "nothing to compose");
    assert_eq!(subjects(&mail), ["Three", "Two"]);
    engine.set_prefs(&mail_engine::prefs::Prefs { report_to: None, ..engine.prefs() }).unwrap();
    // In Junk: the palette offers Not junk, and ! sends it to the inbox.
    let junk = engine.store.folder_id(account, "Junk", mail_store::Role::Junk).unwrap();
    mail.choose_folder(junk);
    mail.open(0);
    assert!(mail.key(&Event::Key { kind: KeyEventKind::Down, key: Key::Char, modifiers: Modifiers { ctrl: true, ..Modifiers::default() }, chars: "k".into() }, None));
    assert!(mail.palette_entries().iter().any(|e| e.label == "Not junk" && e.key == "!"));
    assert!(!mail.palette_entries().iter().any(|e| e.label == "Report phishing"));
    assert!(mail.key(&Event::Key { kind: KeyEventKind::Down, key: Key::Escape, modifiers: Modifiers::default(), chars: String::new() }, None));
    assert!(mail.key(&key("!"), None));
    assert_eq!(mail.ui.get_cloned().count, 0);
    assert_eq!(mail.ui.get_cloned().undo.unwrap().label, "Message moved to the inbox");
    let pending = engine.store.ops_due(account, i64::MAX).unwrap();
    assert!(pending.iter().any(|(_, o)| o.folder == junk && matches!(o.kind, mail_store::OpKind::Move { .. })), "{pending:?}");

    // "Always…" (§11.6): a rule for the open message's sender, kept and
    // run over the inbox at once, with Undo for what it did.
    let inbox = engine.store.folder_id(account, "INBOX", mail_store::Role::Inbox).unwrap();
    mail.choose_folder(inbox);
    mail.open(0);
    let flag_it = mail.palette_entries().into_iter().find(|e| e.label.starts_with("Always: Mail from ") && e.label.ends_with(" is flagged")).expect("an Always entry");
    let mail_engine_rules = engine.rules().len();
    match flag_it.cmd {
        atrium_mail::palette::Cmd::Always(i) => mail.always(i as usize),
        other => panic!("{other:?}"),
    }
    assert_eq!(engine.rules().len(), mail_engine_rules + 1);
    assert!(mail.ui.get_cloned().undo.unwrap().label.ends_with(" flagged"), "{:?}", mail.ui.get_cloned().undo);
}

#[test]
fn attachments_show_what_they_are_and_quick_look_text() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Rc::new(Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() }));
    let account = engine.store.add_account("priya@example.org").unwrap();
    let raw = "From: Arun <arun@acme.example>\r\nTo: priya@example.org\r\nSubject: Files\r\nDate: Mon, 23 Nov 2026 10:00:00 +0000\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=B\r\n\r\n--B\r\nContent-Type: text/plain\r\n\r\nThree files.\r\n--B\r\nContent-Type: text/csv\r\nContent-Disposition: attachment; filename=q4.csv\r\n\r\nregion,total\r\nNorth,42\r\n--B\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=invoice.pdf\r\nContent-Transfer-Encoding: base64\r\n\r\nUEsDBHNjcmlwdA==\r\n--B\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=report.pdf\r\n\r\n%PDF-1.7\r\n--B--\r\n";
    {
        let mut sink = engine.sink(account);
        sink.folders(&[RemoteFolder { name: "INBOX".into(), role: Role::Inbox, selectable: true }]).unwrap();
        sink.messages("INBOX", vec![Fetched { uid: 1, flags: vec![], size: raw.len() as u64, internal_date: Some(NOW), server_id: None, bytes: raw.as_bytes().to_vec(), full: true }]).unwrap();
    }
    let dirty = Arc::new(AtomicBool::new(true));
    let view = MailView::new(engine, dirty.clone(), (1280.0, 800.0), || NOW);
    let mail = view.mail.clone();
    let mut app = App::new_with_flag(view, dirty);
    app.tick();
    mail.open(0);
    mail.sanitize_file(0, 0, Some([1; 16]));
    assert!(mail.take_file_asks().is_empty(), "CSV is ineligible for a sanitized PDF");
    app.tick();
    let t = texts(&app);
    for s in ["q4.csv", "invoice.pdf", "report.pdf", "Quick look", "Save"] {
        assert!(t.iter().any(|x| x == s), "{s:?} not drawn: {t:?}");
    }
    // The zip called a PDF says so, and has no Open.
    assert!(t.iter().any(|x| x.contains("says it is a PDF document but is really")), "{t:?}");
    let files = mail.ui.get_cloned().reading.unwrap().msgs[0].body.clone().unwrap().files;
    assert_eq!(files.iter().map(|f| f.can_open).collect::<Vec<_>>(), [true, false, true]);
    // Open without a press does nothing but say so.
    mail.open_file(0, 2, None);
    assert!(mail.take_file_asks().is_empty());
    mail.open_file(0, 2, Some([7; 16]));
    let asks = mail.take_file_asks();
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].act, atrium_mail::view::FileAct::Open { activation: [7; 16], app: None });
    // Open with… (§8.2): the apps asked for, shown with the default marked;
    // a press on one opens it there; Escape closes the choices.
    assert!(t.iter().any(|x| x == "Open with…"));
    mail.ask_open_with(0, 2);
    assert_eq!(mail.take_file_asks()[0].act, atrium_mail::view::FileAct::Apps);
    app.tick();
    assert!(texts(&app).iter().any(|x| x == "Finding apps…"));
    mail.open_with_apps("report.pdf", Ok(vec![("org.atrium.pdf".into(), "Atrium PDF".into(), true), ("org.example.reader".into(), "Reader".into(), false)]));
    app.tick();
    let t2 = texts(&app);
    assert!(t2.iter().any(|x| x == "Open report.pdf with") && t2.iter().any(|x| x == "Reader") && t2.iter().any(|x| x == "default"), "{t2:?}");
    mail.open_with("org.example.reader", Some([8; 16]));
    assert_eq!(mail.take_file_asks()[0].act, atrium_mail::view::FileAct::Open { activation: [8; 16], app: Some("org.example.reader".into()) });
    assert!(mail.ui.get_cloned().open_with.is_none());
    mail.ask_open_with(0, 1);
    assert!(mail.ui.get_cloned().open_with.is_none(), "not for the file whose name lies");
    mail.ask_open_with(0, 2);
    assert!(mail.close_open_with() && mail.ui.get_cloned().open_with.is_none());
    mail.take_file_asks();
    // Quick look of the CSV: Mail draws the text itself.
    mail.look_file(0, 0);
    app.tick();
    assert!(texts(&app).iter().any(|x| x == "North,42"), "{:?}", texts(&app));
    assert!(mail.looking().is_none(), "no viewer for text");
    // The PDF goes to a viewer the app embeds.
    mail.look_file(0, 2);
    let asks = mail.take_file_asks();
    assert_eq!(asks[0].act, atrium_mail::view::FileAct::Look);
    assert_eq!(mail.looking().map(|(_, f)| f.name), Some("report.pdf".into()));
    mail.close_look();
    assert!(mail.looking().is_none());
}

#[test]
fn search_as_you_type_and_a_saved_search() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Rc::new(Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() }));
    let account = engine.store.add_account("priya@example.org").unwrap();
    {
        let mut sink = engine.sink(account);
        sink.folders(&[
            RemoteFolder { name: "INBOX".into(), role: Role::Inbox, selectable: true },
            RemoteFolder { name: "Archive".into(), role: Role::Archive, selectable: true },
        ]).unwrap();
        let f = |uid: u32, from: &str, subject: &str, body: &str, t: i64| {
            let bytes = message(uid, from, subject, body, false, t);
            Fetched { uid, flags: vec![], size: bytes.len() as u64, internal_date: Some(t), server_id: None, bytes, full: true }
        };
        sink.messages("INBOX", vec![
            f(1, "Arun <arun@acme.example>", "Q4 numbers", "The northern region grew.", NOW - 300),
            f(2, "Lena <lena@partner.example>", "Lunch", "North Beach on Friday?", NOW - 200),
            f(3, "Sam <sam@acme.example>", "Hello", "Nothing to see.", NOW - 100),
        ]).unwrap();
        sink.messages("Archive", vec![f(9, "Arun <arun@acme.example>", "Old north plan", "From last year.", NOW - 400)]).unwrap();
    }
    let dirty = Arc::new(AtomicBool::new(true));
    let view = MailView::new(engine.clone(), dirty.clone(), (1280.0, 800.0), || NOW);
    let mail = view.mail.clone();
    let mut app = App::new_with_flag(view, dirty);
    app.tick();
    let subjects = |m: &atrium_mail::view::Mail| (0..m.ui.get_cloned().count).map(|i| m.row(i).unwrap().subject).collect::<Vec<_>>();
    assert!(texts(&app).iter().any(|x| x == "Search"));

    // `/` puts the caret in the search; keys are the query's now, even e and j.
    assert!(mail.key(&key("/"), None));
    app.tick();
    assert!(texts(&app).iter().any(|x| x.starts_with("Words anywhere")), "{:?}", texts(&app));
    for c in ["n", "o", "r"] {
        assert!(mail.key(&key(c), None));
    }
    // "nor" matches the start of words, in every folder, newest first.
    assert_eq!(subjects(&mail), ["Lunch", "Q4 numbers", "Old north plan"]);
    app.tick();
    assert!(texts(&app).iter().any(|x| x == "3 found"));
    assert!(mail.key(&key("t"), None));
    assert!(mail.key(&key("h"), None));
    assert_eq!(mail.ui.get_cloned().search.unwrap().query, "north");
    // Enter: whole words only ("northern" drops out) and the first opens.
    assert!(mail.key(&Event::Key { kind: KeyEventKind::Down, key: Key::Enter, modifiers: Modifiers::default(), chars: String::new() }, None));
    assert_eq!(subjects(&mail), ["Lunch", "Old north plan"]);
    assert_eq!(mail.ui.get_cloned().reading.unwrap().subject, "Lunch");
    // Now j moves through the results; one in Archive reads from there.
    assert!(mail.key(&key("j"), None));
    let r = mail.ui.get_cloned().reading.unwrap();
    assert_eq!(r.subject, "Old north plan");
    let archive = mail.ui.get_cloned().places.iter().find(|p| p.label == "Archive").unwrap().folder;
    assert_eq!(r.folder, Some(archive));
    assert!(r.msgs[0].here.is_some());

    // Save: a place in the sidebar; choosing it searches again.
    mail.save_search();
    app.tick();
    let ui = mail.ui.get_cloned();
    assert_eq!(ui.saved.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["north"]);
    assert!(texts(&app).iter().any(|x| x == "SAVED SEARCHES"));
    assert_eq!(engine.store.saved_searches().unwrap().len(), 1);

    // Escape: back to the inbox.
    assert!(mail.key(&Event::Key { kind: KeyEventKind::Down, key: Key::Escape, modifiers: Modifiers::default(), chars: String::new() }, None));
    assert!(mail.ui.get_cloned().search.is_none());
    assert_eq!(subjects(&mail), ["Hello", "Lunch", "Q4 numbers"]);

    // Operators: from Arun with "plan", via the saved place too.
    mail.search_for("from:arun@acme.example plan", None);
    assert_eq!(subjects(&mail), ["Old north plan"]);
    // The chips over a search's results are operators in its words (§11.7).
    app.tick();
    assert!(texts(&app).iter().any(|x| x == "Flagged"), "chips shown");
    mail.toggle_quick(mail_engine::quick::FLAGGED);
    assert_eq!(mail.ui.get_cloned().search.unwrap().query, "from:arun@acme.example plan is:flagged");
    assert_eq!(mail.ui.get_cloned().count, 0);
    assert_eq!(mail.chips_on(&mail.ui.get_cloned()), mail_engine::quick::FLAGGED);
    mail.toggle_quick(mail_engine::quick::FLAGGED);
    assert_eq!(subjects(&mail), ["Old north plan"]);
    let id = mail.ui.get_cloned().saved[0].id;
    mail.search_for("north", Some(id));
    app.tick();
    assert!(texts(&app).iter().any(|x| x == "Remove"));
    // Archive from the results: out of them, Undo puts it back.
    mail.open(0);
    assert!(mail.key(&key("e"), None));
    assert_eq!(subjects(&mail), ["Old north plan"]);
    mail.forget_search(id);
    assert!(mail.ui.get_cloned().saved.is_empty());
    assert!(engine.store.saved_searches().unwrap().is_empty());

    // Search server (§11.2): the words go to the sync thread; what it
    // stored joins the results, with a note.
    mail.search_for("zanzibar contract", None);
    app.tick();
    assert!(texts(&app).iter().any(|x| x == "Search server"), "{:?}", texts(&app));
    assert_eq!(mail.ui.get_cloned().count, 0);
    mail.search_server();
    assert_eq!(mail.take_server_search(), Some(mail_sync::ServerQuery { text: vec!["zanzibar".into(), "contract".into()], ..Default::default() }));
    app.tick();
    assert!(texts(&app).iter().any(|x| x == "Searching server…"));
    {
        let bytes = message(10, "Arun <arun@acme.example>", "Old deal", "The zanzibar contract.", false, NOW - 900 * 86_400);
        engine.sink(account).messages("Archive", vec![Fetched { uid: 10, flags: vec![], size: bytes.len() as u64, internal_date: Some(NOW - 900 * 86_400), server_id: None, bytes, full: true }]).unwrap();
    }
    mail.server_searched(Ok(1));
    let ui = mail.ui.get_cloned();
    assert_eq!(subjects(&mail), ["Old deal"]);
    assert_eq!(ui.file_note.map(|n| n.0).as_deref(), Some("1 more found on the server"));
    assert_eq!(ui.search.unwrap().server, Some(false));
    mail.search_server();
    assert_eq!(mail.take_server_search(), None, "once per query");
}

#[test]
fn unsubscribe_block_and_the_screener() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Rc::new(Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() }));
    let account = engine.store.add_account("priya@example.org").unwrap();
    engine.set_screener(account, true).unwrap();
    {
        let mut sink = engine.sink(account);
        sink.folders(&[
            RemoteFolder { name: "INBOX".into(), role: Role::Inbox, selectable: true },
            RemoteFolder { name: "Trash".into(), role: Role::Trash, selectable: true },
        ]).unwrap();
        // Before the screener was on (dated earlier): let through.
        let weekly = message(1, "The Weekly <news@list.example>", "Issue 1", "Text.", false, NOW - 300);
        let weekly = String::from_utf8(weekly).unwrap().replacen("23 Nov 2026", "23 Nov 2020", 1).replacen("MIME-Version", "List-Id: The Weekly <weekly.list.example>\r\nList-Unsubscribe: <https://list.example/u/1>\r\nList-Unsubscribe-Post: List-Unsubscribe=One-Click\r\nMIME-Version", 1).into_bytes();
        let stranger = b"From: Stranger <who@new.example>\r\nTo: priya@example.org\r\nSubject: Hi there\r\nDate: 02 Jan 2031 10:00:00 +0000\r\nMIME-Version: 1.0\r\nContent-Type: text/plain\r\n\r\nHello.\r\n".to_vec();
        let f = |uid: u32, bytes: Vec<u8>| Fetched { uid, flags: vec![], size: bytes.len() as u64, internal_date: None, server_id: None, bytes, full: true };
        sink.messages("INBOX", vec![f(1, weekly), f(2, stranger)]).unwrap();
    }
    let dirty = Arc::new(AtomicBool::new(true));
    let view = MailView::new(engine.clone(), dirty.clone(), (1280.0, 800.0), || NOW);
    let mail = view.mail.clone();
    let mut app = App::new_with_flag(view, dirty);
    app.tick();
    use atrium_mail::view::Lane;
    let ui = mail.ui.get_cloned();
    assert_eq!(ui.count, 1, "the stranger waits");
    assert_eq!(ui.views, [(Lane::Later, 0), (Lane::People, 0), (Lane::Updates, 0), (Lane::Newsletters, 1), (Lane::Files, 0), (Lane::NewSenders, 1)]);
    let t = texts(&app);
    assert!(t.iter().any(|x| x == "Newsletters") && t.iter().any(|x| x == "New senders"), "{t:?}");

    // The newsletter: Unsubscribe is one press, made by the app.
    mail.show_view(Lane::Newsletters);
    mail.open(0);
    app.tick();
    let t = texts(&app);
    assert!(t.iter().any(|x| x == "From the list The Weekly") && t.iter().any(|x| x == "Unsubscribe"), "{t:?}");
    // Its lane, why, and the other lanes to send its list to (E10).
    assert!(t.iter().any(|x| x == "In Newsletters: it's from a mailing list") && t.iter().any(|x| x == "Always People") && t.iter().any(|x| x == "Always Updates"), "{t:?}");
    mail.always_lane(0, mail_engine::lanes::LaneKind::People);
    app.tick();
    let t = texts(&app);
    assert!(t.iter().any(|x| x == "In People: your rule “list:weekly.list.example people”"), "{t:?}");
    assert_eq!(engine.lane_rules().len(), 1);
    assert!(engine.lane(account, mail_engine::lanes::LaneKind::Newsletters, 10).unwrap().is_empty());
    assert_eq!(engine.lane(account, mail_engine::lanes::LaneKind::People, 10).unwrap().len(), 1);
    engine.set_lane_rules(&[]).unwrap();
    assert_eq!(engine.lane(account, mail_engine::lanes::LaneKind::Newsletters, 10).unwrap().len(), 1);
    mail.unsubscribe(0);
    let asked = mail.take_unsubscribes();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].1, "https://list.example/u/1");
    app.tick();
    assert!(texts(&app).iter().any(|x| x == "Unsubscribing…"));
    mail.unsubscribed(asked[0].0, Ok(()));
    app.tick();
    let t = texts(&app);
    assert!(t.iter().any(|x| x == "Unsubscribed from The Weekly") && t.iter().any(|x| x == "Block future mail from this list"), "{t:?}");
    mail.block_list(0, true);
    assert_eq!(engine.store.list_state(account, "weekly.list.example").unwrap(), mail_store::senders::list::BLOCKED);
    app.tick();
    assert!(texts(&app).iter().any(|x| x == "The Weekly is blocked: its mail goes to Trash"));

    // New senders: Accept puts the stranger in the inbox.
    mail.show_view(Lane::NewSenders);
    assert_eq!(mail.ui.get_cloned().count, 1);
    mail.open(0);
    app.tick();
    assert!(texts(&app).iter().any(|x| x == "Accept"));
    mail.screen(0, true);
    let ui = mail.ui.get_cloned();
    assert_eq!(ui.count, 0);
    // The stranger accepted is unread in the inbox: counted under People.
    assert_eq!(ui.views, [(Lane::Later, 0), (Lane::People, 1), (Lane::Updates, 0), (Lane::Newsletters, 0), (Lane::Files, 0), (Lane::NewSenders, 0)]);
    mail.end_search();
    assert_eq!(mail.ui.get_cloned().count, 2);
}

#[test]
fn later_sets_aside_and_done_closes() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Rc::new(Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() }));
    let account = engine.store.add_account("priya@example.org").unwrap();
    {
        let mut sink = engine.sink(account);
        sink.folders(&[RemoteFolder { name: "INBOX".into(), role: Role::Inbox, selectable: true }]).unwrap();
        let f = |uid: u32, subject: &str, t: i64| {
            let bytes = message(uid, "Arun <arun@example.org>", subject, "Text.", false, t);
            Fetched { uid, flags: vec![], size: bytes.len() as u64, internal_date: Some(t), server_id: None, bytes, full: true }
        };
        sink.messages("INBOX", vec![f(1, "One", NOW - 300), f(2, "Two", NOW - 200), f(3, "Three", NOW - 100)]).unwrap();
    }
    let dirty = Arc::new(AtomicBool::new(true));
    let view = MailView::new(engine.clone(), dirty.clone(), (1280.0, 800.0), || NOW);
    let mail = view.mail.clone();
    let mut app = App::new_with_flag(view, dirty);
    app.tick();
    use atrium_mail::view::Lane;
    let subjects = |m: &atrium_mail::view::Mail| (0..m.ui.get_cloned().count).map(|i| m.row(i).unwrap().subject).collect::<Vec<_>>();
    mail.open(0);
    // l: the sheet, Reply chosen (the last message is to Priya).
    assert!(mail.key(&key("l"), None));
    assert_eq!(mail.ui.get_cloned().later_sheet, Some(mail_store::later::kind::REPLY));
    app.tick();
    let t = texts(&app);
    for s in ["Reply", "Waiting", "This evening", "Tomorrow morning", "Someday", "2"] {
        assert!(t.iter().any(|x| x == s), "{s:?} not drawn: {t:?}");
    }
    // 2: tomorrow morning. Out of the inbox; the next opens.
    assert!(mail.key(&key("2"), None));
    assert_eq!(subjects(&mail), ["Two", "One"]);
    let ui = mail.ui.get_cloned();
    assert_eq!(ui.reading.as_ref().unwrap().subject, "Two");
    assert!(ui.later_sheet.is_none());
    assert_eq!(ui.views[0], (Lane::Later, 1));

    // The Later list: what for and when.
    mail.show_view(Lane::Later);
    let r = mail.row(0).unwrap();
    assert_eq!((r.subject.as_str(), r.preview.as_str()), ("Three", "Reply · 24 Nov"));
    mail.open(0);
    app.tick();
    let t = texts(&app);
    assert!(t.iter().any(|x| x == "Reply · comes back 24 Nov") && t.iter().any(|x| x == "Done"), "{t:?}");
    // d: done; the list empties and Three is back in the inbox.
    assert!(mail.key(&key("d"), None));
    assert_eq!(mail.ui.get_cloned().count, 0);
    mail.end_search();
    assert_eq!(subjects(&mail), ["Three", "Two", "One"]);

    // Its time comes: back in the inbox, marked.
    mail.open(0);
    assert!(mail.key(&key("l"), None));
    assert!(mail.key(&key("1"), None));
    assert_eq!(subjects(&mail), ["Two", "One"]);
    let item = engine.later_items().unwrap()[0].1.clone();
    engine.later_due(item.due).unwrap();
    mail.refresh();
    assert_eq!(subjects(&mail), ["Three", "Two", "One"]);
    mail.open(0);
    app.tick();
    assert!(texts(&app).iter().any(|x| x.starts_with("Back · Reply")), "{:?}", texts(&app));
}

#[test]
fn later_note_drop_and_restore() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Rc::new(Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() }));
    let account = engine.store.add_account("priya@example.org").unwrap();
    {
        let mut sink = engine.sink(account);
        sink.folders(&[RemoteFolder { name: "INBOX".into(), role: Role::Inbox, selectable: true }]).unwrap();
        let f = |uid: u32, subject: &str, t: i64| {
            let bytes = message(uid, "Arun <arun@example.org>", subject, "Text.", false, t);
            Fetched { uid, flags: vec![], size: bytes.len() as u64, internal_date: Some(t), server_id: None, bytes, full: true }
        };
        sink.messages("INBOX", vec![f(1, "One", NOW - 300), f(2, "Two", NOW - 200)]).unwrap();
    }
    let dirty = Arc::new(AtomicBool::new(true));
    let view = MailView::new(engine.clone(), dirty.clone(), (1280.0, 800.0), || NOW);
    let mail = view.mail.clone();
    let mut app = App::new_with_flag(view, dirty);
    app.tick();
    use atrium_mail::view::Lane;
    let ev = |k: Key| Event::Key { kind: KeyEventKind::Down, key: k, modifiers: Modifiers::default(), chars: String::new() };
    mail.open(0);
    // l, then n: a note; typing "2" goes into it, not to the times.
    assert!(mail.key(&key("l"), None));
    app.tick();
    assert!(texts(&app).iter().any(|x| x == "Add a note"), "{:?}", texts(&app));
    assert!(mail.key(&key("n"), None));
    for c in ["c", "a", "l", "l", " ", "S", "a", "m", "x"] {
        assert!(mail.key(&key(c), None));
    }
    assert!(mail.key(&ev(Key::Backspace), None));
    assert!(mail.key(&key(" "), None));
    assert!(mail.key(&key("2"), None));
    assert_eq!(mail.ui.get_cloned().later_note, "call Sam 2");
    app.tick();
    assert!(texts(&app).iter().any(|x| x == "call Sam 2|"), "{:?}", texts(&app));
    assert!(mail.key(&ev(Key::Enter), None));
    assert!(mail.ui.get_cloned().later_sheet.is_some(), "Enter ends the note, the sheet stays");
    assert!(mail.key(&key("2"), None));
    let (_, item) = engine.later_items().unwrap()[0].clone();
    assert_eq!(item.note, "call Sam 2");

    // In the Later list, x drops it: it goes to Dropped, which appears.
    mail.show_view(Lane::Later);
    mail.open(0);
    app.tick();
    assert!(texts(&app).iter().any(|x| x == "Drop"), "{:?}", texts(&app));
    assert!(mail.key(&key("x"), None));
    assert_eq!(mail.ui.get_cloned().count, 0);
    assert!(engine.later_items().unwrap().is_empty());
    assert!(mail.ui.get_cloned().views.contains(&(Lane::Dropped, 1)));
    mail.show_view(Lane::Dropped);
    let r = mail.row(0).unwrap();
    assert_eq!((r.subject.as_str(), r.preview.as_str()), ("Two", "Dropped 09:21 · was Reply · call Sam 2"));
    mail.open(0);
    app.tick();
    assert!(texts(&app).iter().any(|x| x == "Restore"), "{:?}", texts(&app));
    // u restores it, open again.
    assert!(mail.key(&key("u"), None));
    assert_eq!(mail.ui.get_cloned().count, 0);
    assert_eq!(engine.later_items().unwrap().len(), 1);
    assert!(!mail.ui.get_cloned().views.iter().any(|v| v.0 == Lane::Dropped));

    // A month overdue: dropped on the next refresh, with an Undo.
    let (id, mut l) = engine.later_items().unwrap()[0].clone();
    l.due = NOW - 31 * 86_400;
    engine.store.write(|w| w.put_later(Some(id), &l)).unwrap();
    mail.end_search();
    mail.refresh();
    let ui = mail.ui.get_cloned();
    assert_eq!(ui.undo.as_ref().map(|u| u.label.as_str()), Some("1 item 30 days overdue dropped"));
    assert!(engine.later_items().unwrap().is_empty());
    assert!(mail.key(&key("z"), None));
    assert_eq!(engine.later_items().unwrap().len(), 1);
}

#[test]
fn a_read_item_closes_when_read_to_the_end() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Rc::new(Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() }));
    let account = engine.store.add_account("priya@example.org").unwrap();
    {
        let mut sink = engine.sink(account);
        sink.folders(&[RemoteFolder { name: "INBOX".into(), role: Role::Inbox, selectable: true }]).unwrap();
        let bytes = message(1, "Arun <arun@example.org>", "Essay", "A short text.", false, NOW - 100);
        sink.messages("INBOX", vec![Fetched { uid: 1, flags: vec![], size: bytes.len() as u64, internal_date: Some(NOW - 100), server_id: None, bytes, full: true }]).unwrap();
    }
    let clock = Rc::new(std::cell::Cell::new(NOW));
    let c = clock.clone();
    let dirty = Arc::new(AtomicBool::new(true));
    let view = MailView::new(engine.clone(), dirty.clone(), (1280.0, 800.0), move || c.get());
    let mail = view.mail.clone();
    let mut app = App::new_with_flag(view, dirty);
    app.tick();
    mail.open(0);
    assert!(mail.key(&key("l"), None));
    mail.later_kind(mail_store::later::kind::READ);
    assert!(mail.key(&key("2"), None));
    mail.show_view(atrium_mail::view::Lane::Later);
    mail.open(0);
    app.tick();
    mail.tick();
    assert_eq!(engine.later_items().unwrap().len(), 1, "not yet: just opened");
    clock.set(NOW + atrium_mail::view::READ_SECS);
    app.tick();
    mail.tick();
    assert!(engine.later_items().unwrap().is_empty(), "open 5 s, all of it on screen");
    assert_eq!(mail.ui.get_cloned().file_note.map(|n| n.0).as_deref(), Some("Read: closed"));
}

#[test]
fn a_signed_message_says_who_signed_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    engine.set_roots(vec![include_bytes!("../../mail-smime/tests/data/ca.der").to_vec()]);
    let engine = Rc::new(engine);
    let account = engine.store.add_account("priya@example.org").unwrap();
    let raw = include_bytes!("../../mail-smime/tests/data/signed.eml").to_vec();
    {
        let mut sink = engine.sink(account);
        sink.folders(&[RemoteFolder { name: "INBOX".into(), role: Role::Inbox, selectable: true }]).unwrap();
        sink.messages("INBOX", vec![Fetched { uid: 1, flags: vec![], size: raw.len() as u64, internal_date: Some(NOW), server_id: None, bytes: raw, full: true }]).unwrap();
    }
    let dirty = Arc::new(AtomicBool::new(true));
    let view = MailView::new(engine, dirty.clone(), (1280.0, 800.0), || NOW);
    let mail = view.mail.clone();
    let mut app = App::new_with_flag(view, dirty);
    app.tick();
    mail.open(0);
    app.tick();
    let t = texts(&app);
    assert!(t.iter().any(|x| x == "Signed by Grace Hopper <grace@example.net> · certificate from Fixture"), "{t:?}");
    // The signature isn't a file.
    assert!(!t.iter().any(|x| x.contains("smime.p7s")), "{t:?}");
}

#[test]
fn an_encrypted_message_is_read_with_the_person_s_key() {
    use rsa::pkcs8::DecodePrivateKey;
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    engine.set_roots(vec![include_bytes!("../../mail-smime/tests/data/ca.der").to_vec()]);
    let engine = Rc::new(engine);
    let account = engine.store.add_account("grace@example.net").unwrap();
    {
        let mut sink = engine.sink(account);
        sink.folders(&[RemoteFolder { name: "INBOX".into(), role: Role::Inbox, selectable: true }]).unwrap();
        let f = |uid, bytes: Vec<u8>| Fetched { uid, flags: vec![], size: bytes.len() as u64, internal_date: Some(NOW), server_id: None, bytes, full: true };
        sink.messages("INBOX", vec![f(1, include_bytes!("../../mail-smime/tests/data/signed.eml").to_vec()), f(2, include_bytes!("../../mail-smime/tests/data/encrypted.eml").to_vec())]).unwrap();
    }
    let dirty = Arc::new(AtomicBool::new(true));
    let view = MailView::new(engine, dirty.clone(), (1280.0, 800.0), || NOW);
    let mail = view.mail.clone();
    let mut app = App::new_with_flag(view, dirty);
    app.tick();
    let row = (0..2).find(|&i| mail.row(i).is_some_and(|r| r.subject == "Encrypted plan")).unwrap();
    mail.open(row);
    app.tick();
    assert!(texts(&app).iter().any(|x| x.contains("This message is encrypted.")), "{:?}", texts(&app));
    assert!(texts(&app).iter().any(|x| x == "Encrypted with S/MIME"));
    // With a key (the token's, here in software): read.
    let key = rsa::RsaPrivateKey::from_pkcs8_der(include_bytes!("../../mail-smime/tests/data/leaf.p8")).unwrap();
    *mail.unwrapper.borrow_mut() = Some(Rc::new(move |_: &[u8], w: &[u8]| key.decrypt(rsa::Pkcs1v15Encrypt, w).map_err(|e| e.to_string())));
    mail.open(row);
    app.tick();
    assert!(texts(&app).iter().any(|x| x.contains("The secret plan.")), "{:?}", texts(&app));
    assert!(texts(&app).iter().any(|x| x == "plan.csv"));
}

#[cfg(target_os = "freebsd")]
#[test]
fn provisioned_work_certificate_opens_oaep_mail_after_unlock() {
    use std::io::ErrorKind;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixListener;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use arca_store::codec::Writer;
    use arcad::label::{Label, Resolver};
    use arcad::proto::{op, status};
    use arcad::server::State;

    let dir = tempfile::tempdir().unwrap();
    let mut state = State::new(dir.path().join("arca")).unwrap();
    state.mail_sign_ins.mail_publisher = Some([7; 32]);
    let mut unlock = Writer::new();
    unlock.str("alice");
    unlock.raw(&[1; 32]);
    assert_eq!(state.handle(&Label::Vestibulum, op::V_CREATE, &unlock.buf).status, status::OK);
    let mut import = Writer::new();
    import.str("alice");
    import.str("work");
    import.str("Synthetic S/MIME key");
    import.bytes(include_bytes!("../../mail-smime/tests/data/leaf.der"));
    import.bytes(include_bytes!("../../mail-smime/tests/data/leaf.p8"));
    let added = state.handle(&Label::TrustedUi, op::U_SMIME_PROVISION, &import.buf);
    assert_eq!(added.status, status::OK);
    let mut lock = Writer::new();
    lock.str("alice");
    assert_eq!(state.handle(&Label::Vestibulum, op::V_LOCK, &lock.buf).status, status::OK);

    let registry = dir.path().join("launch-registry");
    let uid = unsafe { libc::getuid() };
    std::fs::write(&registry, format!("{uid} alice org.atrium.mail {} persona=work\n", "07".repeat(32))).unwrap();
    let resolver = Resolver {
        registry, vestibulum_uid: None, ui_uid: None, allow_unsigned: false,
        courier_registry: None, web_broker: None, share_registry: None,
        file_manager: None, services: vec![],
    };
    let socket = dir.path().join("arca.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let state = Arc::new(Mutex::new(state));
    let broker = state.clone();
    let server = std::thread::spawn(move || {
        // Locked lookup, then the successful certificate lookup and unwrap.
        let until = Instant::now() + Duration::from_secs(15);
        for _ in 0..3 {
            let stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == ErrorKind::WouldBlock && Instant::now() < until =>
                        std::thread::sleep(Duration::from_millis(10)),
                    Err(e) => panic!("Arca did not receive the expected request: {e}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            let (mut uid, mut gid) = (0, 0);
            assert_eq!(unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) }, 0);
            let label = resolver.resolve(uid);
            assert!(matches!(&label, Label::App { persona: Some(name), .. } if name == "work"));
            arcad::serve_conn(stream, &label, &broker).unwrap();
        }
    });

    let mut engine = Engine::new(Store::open(&dir.path().join("mail")).unwrap(), Parser { program: jail() });
    engine.set_roots(vec![include_bytes!("../../mail-smime/tests/data/ca.der").to_vec()]);
    let engine = Rc::new(engine);
    let account = engine.store.add_account("grace@example.net").unwrap();
    {
        let mut sink = engine.sink(account);
        sink.folders(&[RemoteFolder { name: "INBOX".into(), role: Role::Inbox, selectable: true }]).unwrap();
        let fetched = |uid, raw: &[u8]| Fetched {
            uid, flags: vec![], size: raw.len() as u64, internal_date: Some(NOW),
            server_id: None, bytes: raw.to_vec(), full: true,
        };
        sink.messages("INBOX", vec![
            fetched(2, include_bytes!("../../mail-smime/tests/data/encrypted.eml")),
            fetched(3, include_bytes!("../../mail-smime/tests/data/oaep.eml")),
        ]).unwrap();
    }
    assert!(engine.my_certs(account, &[]).is_empty(), "no signed mail has supplied a certificate");
    let other = engine.store.add_account("other@example.net").unwrap();
    let public_cert = include_bytes!("../../mail-smime/tests/data/leaf.der").to_vec();
    assert!(engine.my_oaep_certs(other, &[public_cert]).is_empty(), "certificate must match the account");
    let dirty = Arc::new(AtomicBool::new(true));
    let view = MailView::new(engine.clone(), dirty.clone(), (1280.0, 800.0), || NOW);
    let mail = view.mail.clone();
    let cert_socket = socket.clone();
    *mail.oaep_unwrapper.borrow_mut() = Some(Rc::new(move |fingerprint, wrapped| {
        atrium_mail::accounts::unwrap_smime(&socket, fingerprint, wrapped)
    }));
    let mut app = App::new_with_flag(view, dirty);
    app.tick();
    let row = |subject| (0..2).find(|&i| mail.row(i).is_some_and(|r| r.subject == subject)).unwrap();
    mail.open(row("Encrypted plan"));
    app.tick();
    assert!(texts(&app).iter().any(|t| t.contains("supported S/MIME envelope")));

    *mail.oaep_certificates.borrow_mut() = Some(Rc::new(move || {
        atrium_mail::accounts::smime_certificates(&cert_socket)
    }));
    let oaep = row("OAEP encrypted plan");
    mail.open(oaep);
    app.tick();
    assert!(texts(&app).iter().any(|t| t.contains("Unlock your Atrium session")));
    assert!(texts(&app).iter().all(|t| !t.contains("OAEP content from OpenSSL.")));

    assert_eq!(state.lock().unwrap().handle(&Label::Vestibulum, op::V_UNLOCK, &unlock.buf).status, status::OK);
    mail.open(oaep);
    app.tick();
    assert!(texts(&app).iter().any(|t| t.contains("OAEP content from OpenSSL.")), "{:?}", texts(&app));
    server.join().unwrap();
}

#[test]
fn quick_filters_narrow_the_list() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Rc::new(Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() }));
    let account = engine.store.add_account("priya@example.org").unwrap();
    let mut prefs = engine.prefs();
    prefs.vips = vec!["arun@acme.example".into()];
    engine.set_prefs(&prefs).unwrap();
    let mail_of = |uid: u32, head: &str, body: &str, t: i64, seen: bool| {
        let s = t.rem_euclid(86_400);
        let date = format!("Mon, 23 Nov 2026 {:02}:{:02}:{:02} +0000", s / 3600, s % 3600 / 60, s % 60);
        let bytes = format!("{head}Date: {date}\r\nMessage-ID: <q{uid}@x>\r\nMIME-Version: 1.0\r\n{body}").into_bytes();
        Fetched { uid, flags: if seen { vec!["\\Seen".into()] } else { vec![] }, size: 1000, internal_date: Some(t), server_id: None, bytes, full: true }
    };
    {
        let mut sink = engine.sink(account);
        sink.folders(&[RemoteFolder { name: "INBOX".into(), role: Role::Inbox, selectable: true }]).unwrap();
        sink.messages("INBOX", vec![
            // A person asks, with a file: unread, to me, needs a reply.
            mail_of(1, "From: Lena <lena@partner.example>\r\nTo: Priya <priya@example.org>\r\nSubject: Contract\r\n",
                "Content-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nCan you sign it?\r\n--b\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=c.pdf\r\n\r\n%PDF-1.4\r\n--b--\r\n", NOW - 600, false),
            // A newsletter: to me, but a list.
            mail_of(2, "From: News <news@shop.example>\r\nTo: priya@example.org\r\nSubject: Sale\r\nList-Id: <deals.shop.example>\r\n",
                "Content-Type: text/plain\r\n\r\nHalf off.\r\n", NOW - 1200, true),
            // A VIP, Cc only.
            mail_of(3, "From: Arun <arun@acme.example>\r\nTo: team@acme.example\r\nCc: priya@example.org\r\nSubject: Plan\r\n",
                "Content-Type: text/plain\r\n\r\nFYI.\r\n", NOW - 1800, true),
        ]).unwrap();
    }
    let dirty = Arc::new(AtomicBool::new(true));
    let view = MailView::new(engine, dirty.clone(), (1280.0, 800.0), || NOW);
    let mail = view.mail.clone();
    let mut app = App::new_with_flag(view, dirty);
    app.tick();
    assert_eq!(mail.ui.get_cloned().count, 3);
    let t = texts(&app);
    for chip in ["Unread", "To me", "From people", "VIPs", "Has attachment", "Needs reply", "Snoozed back"] {
        assert!(t.iter().any(|x| x == chip), "{chip} not drawn");
    }
    let subjects = |m: &atrium_mail::view::Mail| (0..m.ui.get_cloned().count).map(|i| m.row(i).unwrap().subject).collect::<Vec<_>>();
    use mail_engine::quick::*;
    for (bits, want) in [
        (UNREAD, vec!["Contract"]),
        (TO_ME, vec!["Contract"]),
        (FROM_PEOPLE, vec!["Contract", "Plan"]),
        (VIPS, vec!["Plan"]),
        (ATTACHMENT, vec!["Contract"]),
        (NEEDS_REPLY, vec!["Contract"]),
        (FLAGGED, vec![]),
    ] {
        mail.toggle_quick(bits);
        app.tick();
        assert_eq!(subjects(&mail), want, "filter {bits}");
        mail.toggle_quick(bits);
    }
    // Combined: from people and not unread-only → both; plus unread → one.
    mail.toggle_quick(FROM_PEOPLE);
    mail.toggle_quick(UNREAD);
    app.tick();
    assert_eq!(subjects(&mail), ["Contract"]);
    // Nothing matches: said so.
    mail.toggle_quick(VIPS);
    app.tick();
    assert_eq!(mail.ui.get_cloned().count, 0);
    assert!(texts(&app).iter().any(|x| x == "Nothing matches these filters"));
    // A press on a chip turns it off again.
    mail.toggle_quick(VIPS);
    mail.toggle_quick(UNREAD);
    mail.toggle_quick(FROM_PEOPLE);
    app.tick();
    assert_eq!(mail.ui.get_cloned().count, 3);
    // Priya answers Lena (her copy in Sent): it no longer needs a reply.
    {
        let mut sink = mail.engine.sink(account);
        sink.folders(&[
            RemoteFolder { name: "INBOX".into(), role: Role::Inbox, selectable: true },
            RemoteFolder { name: "Sent".into(), role: Role::Sent, selectable: true },
        ]).unwrap();
        sink.messages("Sent", vec![mail_of(9, "From: Priya <priya@example.org>\r\nTo: Lena <lena@partner.example>\r\nSubject: Re: Contract\r\nIn-Reply-To: <q1@x>\r\nReferences: <q1@x>\r\n",
            "Content-Type: text/plain\r\n\r\nSigned.\r\n", NOW - 60, true)]).unwrap();
    }
    mail.refresh();
    mail.toggle_quick(NEEDS_REPLY);
    app.tick();
    assert_eq!(mail.ui.get_cloned().count, 0, "{:?}", subjects(&mail));
}

#[test]
fn grouped_by_date_and_by_sender_and_folded() {
    use atrium_mail::view::{date_section, Group};
    // Sections of local wall-clock times (Mon 23 Nov 2026).
    let now = NOW;
    let d = 86_400;
    assert_eq!(date_section(now - 60, now), "Today");
    assert_eq!(date_section(now - d, now), "Yesterday");
    assert_eq!(date_section(now - 4 * d, now), "This week");
    assert_eq!(date_section(now - 20 * d, now), "Earlier this month");
    assert_eq!(date_section(now - 40 * d, now), "October 2026");

    let dir = tempfile::tempdir().unwrap();
    let engine = Rc::new(Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() }));
    let account = engine.store.add_account("priya@example.org").unwrap();
    let at = |uid: u32, from: &str, subject: &str, t: i64, seen: bool| {
        let mut f = threaded(uid, from, subject, "Hi.", t, &format!("<g{uid}@x>"), "");
        f.flags = if seen { vec!["\\Seen".into()] } else { vec![] };
        // threaded() writes every date on 23 Nov; the stored date is the
        // server's arrival time.
        f.internal_date = Some(t);
        f
    };
    {
        let mut sink = engine.sink(account);
        sink.folders(&[
            RemoteFolder { name: "INBOX".into(), role: Role::Inbox, selectable: true },
            RemoteFolder { name: "Archive".into(), role: Role::Archive, selectable: true },
        ]).unwrap();
        sink.messages("INBOX", vec![
            at(1, "Lena <lena@partner.example>", "Today 1", NOW - 600, false),
            at(2, "Arun <arun@acme.example>", "Today 2", NOW - 1200, true),
        ]).unwrap();
    }
    let dirty = Arc::new(AtomicBool::new(true));
    let view = MailView::new(engine.clone(), dirty.clone(), (1280.0, 800.0), || NOW);
    let mail = view.mail.clone();
    let mut app = App::new_with_flag(view, dirty);
    app.tick();
    assert!(texts(&app).iter().any(|x| x == "No grouping ⌄"));
    let fid = |name: &str| engine.store.folders(account).unwrap().into_iter().find(|(_, f)| f.name == name).unwrap().0;
    let (inbox, archive) = (fid("INBOX"), fid("Archive"));
    // By date: one header, then both.
    mail.set_group(Group::Date);
    app.tick();
    let ui = mail.ui.get_cloned();
    assert_eq!(ui.count, 3);
    let h = mail.header(0).unwrap();
    assert_eq!((h.label.as_str(), h.rows, h.unread), ("Today", 2, 1));
    assert!(mail.row(0).is_none() && mail.row(1).is_some());
    let t = texts(&app);
    assert!(t.iter().any(|x| x == "Today") && t.iter().any(|x| x == "1 unread") && t.iter().any(|x| x == "By date ⌄"), "{t:?}");
    // A press on the header folds it; its unread count stays.
    mail.open(0);
    app.tick();
    assert_eq!(mail.ui.get_cloned().count, 1);
    assert!(mail.header(0).unwrap().folded);
    assert!(texts(&app).iter().any(|x| x == "1 unread"));
    mail.open(0);
    assert_eq!(mail.ui.get_cloned().count, 3);
    // j passes over the header to the first message.
    assert!(mail.key(&key("j"), None));
    assert_eq!(mail.ui.get_cloned().cursor, Some(1));
    assert_eq!(mail.ui.get_cloned().reading.unwrap().subject, "Today 1");
    // By sender: a section each, the newest sender first.
    mail.next_group();
    app.tick();
    let ui = mail.ui.get_cloned();
    assert_eq!(ui.group, Group::Sender);
    assert_eq!(ui.count, 4);
    assert_eq!(mail.header(0).unwrap().label, "Lena");
    assert_eq!(mail.header(2).unwrap().label, "Arun");
    // With a filter on, only what passes is grouped: Lena's was read
    // above, so nothing is unread and no section is left.
    mail.toggle_quick(mail_engine::quick::FROM_PEOPLE);
    assert_eq!(mail.ui.get_cloned().count, 4);
    mail.toggle_quick(mail_engine::quick::FROM_PEOPLE);
    mail.toggle_quick(mail_engine::quick::UNREAD);
    assert_eq!(mail.ui.get_cloned().count, 0);
    mail.toggle_quick(mail_engine::quick::UNREAD);
    // In an inbox, by lane next: both are from people.
    mail.next_group();
    assert_eq!(mail.ui.get_cloned().group, Group::Lane);
    let h = mail.header(0).unwrap();
    assert_eq!((h.label.as_str(), h.rows), ("People", 2));
    assert_eq!(mail.ui.get_cloned().count, 3);
    // Kept per folder: Archive has none, the inbox gets its own back, also
    // after a restart.
    mail.choose_folder(archive);
    assert_eq!(mail.ui.get_cloned().group, Group::None);
    mail.choose_folder(inbox);
    assert_eq!(mail.ui.get_cloned().group, Group::Lane);
    assert_eq!(mail.ui.get_cloned().count, 3);
    let again = MailView::new(engine.clone(), Arc::new(AtomicBool::new(true)), (1280.0, 800.0), || NOW);
    again.mail.refresh();
    assert_eq!((again.mail.ui.get_cloned().group, again.mail.ui.get_cloned().count), (Group::Lane, 3));
    mail.next_group();
    assert_eq!(mail.ui.get_cloned().group, Group::None);
    assert!(engine.store.view(inbox).unwrap(), "conversations stays");

    // Filters on: Save as folder keeps them as a smart folder and shows it.
    mail.toggle_quick(mail_engine::quick::FROM_PEOPLE);
    app.tick();
    assert!(texts(&app).iter().any(|x| x == "Save as folder"));
    mail.save_filters();
    let ui = mail.ui.get_cloned();
    let saved = ui.saved.last().unwrap().clone();
    assert_eq!((saved.name.as_str(), saved.query.as_str()), ("Inbox · From people", "in:\"INBOX\" is:from-people"));
    assert_eq!(ui.search.as_ref().unwrap().saved, Some(saved.id));
    assert_eq!(ui.quick, 0);
    assert_eq!(ui.count, 2);
    assert_eq!(mail.ui.get_cloned().count, 2);
}

#[test]
fn a_notification_s_buttons_act_in_the_window() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Rc::new(Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() }));
    let account = engine.store.add_account("priya@example.org").unwrap();
    {
        let mut sink = engine.sink(account);
        sink.folders(&[
            RemoteFolder { name: "INBOX".into(), role: Role::Inbox, selectable: true },
            RemoteFolder { name: "Archive".into(), role: Role::Archive, selectable: true },
        ]).unwrap();
        let f = |uid: u32, subject: &str, t: i64| {
            let bytes = message(uid, "Arun <arun@example.org>", subject, "Text.", false, t);
            Fetched { uid, flags: vec![], size: bytes.len() as u64, internal_date: Some(t), server_id: None, bytes, full: true }
        };
        sink.messages("INBOX", vec![f(1, "One", NOW - 300), f(2, "Two", NOW - 200), f(3, "Three", NOW - 100)]).unwrap();
    }
    let dirty = Arc::new(AtomicBool::new(true));
    let view = MailView::new(engine.clone(), dirty.clone(), (1280.0, 800.0), || NOW);
    let mail = view.mail.clone();
    let mut app = App::new_with_flag(view, dirty);
    app.tick();
    let subjects = |m: &atrium_mail::view::Mail| (0..m.ui.get_cloned().count).map(|i| m.row(i).unwrap().subject).collect::<Vec<_>>();
    let inbox = engine.store.folder_id(account, "INBOX", mail_store::Role::Inbox).unwrap();
    let id = |uid| engine.store.at(inbox, uid).unwrap().unwrap().0;

    // Reading Three when its notification's Archive is pressed: it leaves
    // the list and the reader, with Undo.
    mail.open(0);
    mail.notification_action(id(3), "archive");
    assert_eq!(subjects(&mail), ["Two", "One"]);
    let ui = mail.ui.get_cloned();
    assert!(ui.reading.is_none());
    assert_eq!(ui.undo.as_ref().unwrap().label, "Message archived");
    app.tick();
    assert!(texts(&app).iter().any(|x| x == "Message archived"));
    assert!(mail.key(&key("z"), None));
    assert_eq!(subjects(&mail), ["Three", "Two", "One"]);

    // Mark read: the row is read.
    assert!(mail.row(1).unwrap().unread);
    mail.notification_action(id(2), "read");
    assert!(!mail.row(1).unwrap().unread);
    assert_eq!(mail.ui.get_cloned().undo.unwrap().label, "Marked read");

    // Reply: a compose window to Arun.
    assert!(mail.take_composes().is_empty());
    assert_eq!(mail.composes_waiting(), 0);
    mail.notification_action(id(1), "reply");
    assert_eq!(mail.composes_waiting(), 1, "the app raises it once open");
    let c = mail.take_composes();
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].0.draft.subject, "Re: One");
}

#[test]
fn the_command_palette() {
    use atrium_mail::palette::Cmd;
    let dir = tempfile::tempdir().unwrap();
    let engine = Rc::new(Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() }));
    let account = engine.store.add_account("priya@example.org").unwrap();
    {
        let mut sink = engine.sink(account);
        sink.folders(&[
            RemoteFolder { name: "INBOX".into(), role: Role::Inbox, selectable: true },
            RemoteFolder { name: "Archive".into(), role: Role::Archive, selectable: true },
            RemoteFolder { name: "Receipts".into(), role: Role::Other, selectable: true },
        ]).unwrap();
        let f = |uid: u32, subject: &str, t: i64| {
            let bytes = message(uid, "Arun <arun@example.org>", subject, "Text.", false, t);
            Fetched { uid, flags: vec![], size: bytes.len() as u64, internal_date: Some(t), server_id: None, bytes, full: true }
        };
        sink.messages("INBOX", vec![f(1, "One", NOW - 300), f(2, "Two", NOW - 200)]).unwrap();
        sink.messages("Receipts", vec![f(9, "Order 1", NOW - 900)]).unwrap();
    }
    let dirty = Arc::new(AtomicBool::new(true));
    let view = MailView::new(engine.clone(), dirty.clone(), (1280.0, 800.0), || NOW);
    let mail = view.mail.clone();
    let mut app = App::new_with_flag(view, dirty);
    app.tick();
    let ctrl_k = Event::Key { kind: KeyEventKind::Down, key: Key::Char, modifiers: Modifiers { ctrl: true, ..Modifiers::default() }, chars: "k".into() };
    let press = |k: Key| Event::Key { kind: KeyEventKind::Down, key: k, modifiers: Modifiers::default(), chars: String::new() };
    let typed = |s: &str| for c in s.chars() { assert!(mail.key(&key(&c.to_string()), None)); };
    let subjects = |m: &atrium_mail::view::Mail| (0..m.ui.get_cloned().count).map(|i| m.row(i).unwrap().subject).collect::<Vec<_>>();
    assert!(texts(&app).iter().any(|x| x == "/   Ctrl-K"));

    // Nothing read: no Reply; places to go.
    assert!(mail.key(&ctrl_k, None));
    app.tick();
    assert!(texts(&app).iter().any(|x| x == "Type a command or a place"));
    let cmds: Vec<Cmd> = mail.palette_entries().iter().map(|e| e.cmd).collect();
    assert!(!cmds.contains(&Cmd::Reply) && cmds.contains(&Cmd::Write) && cmds.contains(&Cmd::GetMail));
    // "rec": Receipts; Enter goes there, the palette closes.
    typed("rec");
    assert_eq!(mail.palette_shown()[0].label, "Go to Receipts");
    app.tick();
    assert!(texts(&app).iter().any(|x| x == "Go to Receipts"));
    assert!(mail.key(&press(Key::Enter), None));
    assert!(mail.ui.get_cloned().palette.is_none());
    assert_eq!(subjects(&mail), ["Order 1"]);

    // Back to the inbox, read Two, archive it from the palette.
    assert!(mail.key(&ctrl_k, None));
    typed("go inb");
    assert!(mail.key(&press(Key::Enter), None));
    assert_eq!(subjects(&mail), ["Two", "One"]);
    mail.open(0);
    assert!(mail.key(&ctrl_k, None));
    let shown = mail.palette_shown();
    assert_eq!(shown[0].label, "Reply");
    assert_eq!(shown[0].key, "r");
    typed("arch");
    let shown = mail.palette_shown();
    assert_eq!((shown[0].label.as_str(), shown[0].key), ("Archive", "e"));
    assert!(shown.iter().any(|e| e.label == "Go to Archive"));
    // Down then Up: still the first; letters other keys would take are typed.
    assert!(mail.key(&press(Key::Down), None));
    assert!(mail.key(&press(Key::Up), None));
    assert!(mail.key(&press(Key::Enter), None));
    assert_eq!(subjects(&mail), ["One"]);
    assert_eq!(mail.ui.get_cloned().undo.unwrap().label, "Message archived");
    // Undo is offered, and does it.
    assert!(mail.key(&ctrl_k, None));
    typed("undo");
    assert_eq!(mail.palette_shown()[0].label, "Undo: Message archived");
    assert!(mail.key(&press(Key::Enter), None));
    assert_eq!(subjects(&mail), ["Two", "One"]);

    // A filter; nothing matching; Escape closes without doing anything.
    assert!(mail.key(&ctrl_k, None));
    typed("unread");
    assert_eq!(mail.palette_shown()[0].cmd, Cmd::Quick(mail_engine::quick::UNREAD));
    assert!(mail.key(&press(Key::Backspace), None));
    typed("qqq");
    assert!(mail.palette_shown().is_empty());
    app.tick();
    assert!(texts(&app).iter().any(|x| x == "Nothing matches"));
    assert!(mail.key(&press(Key::Escape), None));
    assert!(mail.ui.get_cloned().palette.is_none());
    assert_eq!(mail.ui.get_cloned().quick, 0);
    // Ctrl-K, Ctrl-K: open and closed.
    assert!(mail.key(&ctrl_k, None) && mail.key(&ctrl_k, None));
    assert!(mail.ui.get_cloned().palette.is_none());
    // A press on an entry does it: Write.
    assert!(mail.key(&ctrl_k, None));
    typed("write");
    assert!(mail.palette_run(0));
    assert_eq!(mail.take_composes().len(), 1);
    // Single keys off (§15.5): j and e do nothing; the arrows, Ctrl-K and
    // the palette still work.
    engine.set_prefs(&mail_engine::prefs::Prefs { single_keys: false, ..engine.prefs() }).unwrap();
    mail.open(0);
    assert!(!mail.key(&key("e"), None));
    assert_eq!(subjects(&mail), ["Two", "One"]);
    assert!(mail.key(&press(Key::Down), None));
    assert_eq!(mail.ui.get_cloned().cursor, Some(1));
    // Their keys aren't shown either; Ctrl-P's still is.
    let entries = mail.palette_entries();
    assert!(entries.iter().any(|e| e.label == "Archive" && e.key.is_empty()));
    assert!(entries.iter().any(|e| e.label == "Print" && e.key == "Ctrl-P"));
    app.tick();
    assert!(!texts(&app).iter().any(|x| x == "e"), "no e on Archive");
    assert!(mail.key(&ctrl_k, None));
    typed("arch");
    assert!(mail.key(&press(Key::Enter), None));
    assert_eq!(subjects(&mail), ["Two"]);
    // Nor the undo's key, nor search's: Ctrl-K alone.
    app.tick();
    let t = texts(&app);
    assert!(t.iter().any(|x| x == "Undo") && !t.iter().any(|x| x == "z"), "{t:?}");
    assert!(t.iter().any(|x| x == "Ctrl-K") && !t.iter().any(|x| x.starts_with("/ ")), "{t:?}");

    // Export (§6.5): this folder, as mbox through the picker's Save.
    use mail_engine::export::What;
    let inbox = engine.store.folder_id(account, "INBOX", mail_store::Role::Inbox).unwrap();
    assert!(mail.key(&ctrl_k, None));
    typed("export inbox");
    assert_eq!(mail.palette_shown()[0].label, "Export Inbox as mbox");
    assert!(mail.key(&press(Key::Enter), None));
    let ask = mail.take_export().unwrap();
    assert_eq!((ask.what, ask.name.as_str()), (What::Folder(inbox), "Inbox.mbox"));
    // A search's results; all mail.
    mail.search_for("Order", None);
    assert!(mail.key(&ctrl_k, None));
    typed("export these");
    assert!(mail.key(&press(Key::Enter), None));
    let ask = mail.take_export().unwrap();
    assert_eq!(ask.name, "Order.mbox");
    assert!(matches!(ask.what, What::Messages(ref v) if v.len() == 1));
    assert!(mail.key(&ctrl_k, None));
    typed("export all");
    assert!(mail.key(&press(Key::Enter), None));
    assert_eq!(mail.take_export().unwrap().what, What::Everything);
    // Import (§6.5): into the folder shown, not from a search.
    assert!(mail.key(&ctrl_k, None));
    typed("import");
    assert!(mail.palette_shown().is_empty(), "not from search results");
    assert!(mail.key(&press(Key::Escape), None));
    mail.end_search();
    assert!(mail.key(&ctrl_k, None));
    typed("import");
    assert_eq!(mail.palette_shown()[0].label, "Import an mbox file into Inbox");
    assert!(mail.key(&press(Key::Enter), None));
    assert_eq!(mail.take_import(), Some((inbox, "Inbox".to_string())));
    // Not while the picker is up for something else.
    mail.set_saving(true);
    mail.export(atrium_mail::palette::Export::Everything);
    assert!(mail.take_export().is_none());
    mail.import();
    assert!(mail.take_import().is_none());
}

#[test]
fn wide_medium_and_narrow_windows() {
    use atrium_mail::view::{layout_for, short_name, Layout};
    use pergola::theme::size::em;
    assert_eq!((layout_for(em(1280.0)), layout_for(em(1000.0)), layout_for(em(600.0))), (Layout::Wide, Layout::Medium, Layout::Narrow));
    assert_eq!((short_name("Inbox").as_str(), short_name("Sent").as_str(), short_name("Saved Mail").as_str(), short_name("").as_str()), ("In", "Se", "SM", ""));
    let dir = tempfile::tempdir().unwrap();
    let engine = Rc::new(Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() }));
    let account = engine.store.add_account("priya@example.org").unwrap();
    {
        let mut sink = engine.sink(account);
        sink.folders(&[
            RemoteFolder { name: "INBOX".into(), role: Role::Inbox, selectable: true },
            RemoteFolder { name: "Receipts".into(), role: Role::Other, selectable: true },
        ]).unwrap();
        let f = |uid: u32, subject: &str, t: i64| {
            let bytes = message(uid, "Arun <arun@example.org>", subject, "Text.", false, t);
            Fetched { uid, flags: vec![], size: bytes.len() as u64, internal_date: Some(t), server_id: None, bytes, full: true }
        };
        sink.messages("INBOX", vec![f(1, "One", NOW - 300), f(2, "Two", NOW - 200)]).unwrap();
    }
    let dirty = Arc::new(AtomicBool::new(true));
    let view = MailView::new(engine.clone(), dirty.clone(), (em(1280.0), 800.0), || NOW);
    let mail = view.mail.clone();
    let mut app = App::new_with_flag(view, dirty);
    let esc = Event::Key { kind: KeyEventKind::Down, key: Key::Escape, modifiers: Modifiers::default(), chars: String::new() };
    app.tick();
    // Wide: the places in full, no ≡.
    let t = texts(&app);
    assert!(t.iter().any(|x| x == "Receipts") && !t.iter().any(|x| x == "≡"), "{t:?}");

    // Medium: the places fold to icons (the person's own folders to short
    // names); ≡ shows them in full, Escape folds them.
    mail.set_size(em(1000.0), 800.0);
    app.tick();
    let t = texts(&app);
    let tray = atrium_mail::view::rail_icon(mail_store::Role::Inbox).unwrap().to_string();
    assert!(t.iter().any(|x| x == "≡") && t.iter().any(|x| x == "Re") && t.contains(&tray) && !t.iter().any(|x| x == "In" || x == "Receipts"), "{t:?}");
    assert_eq!(atrium_mail::view::rail_icon(mail_store::Role::Other), None);
    mail.toggle_side();
    app.tick();
    assert!(texts(&app).iter().any(|x| x == "Receipts"));
    assert!(mail.key(&esc, None));
    app.tick();
    assert!(!texts(&app).iter().any(|x| x == "Receipts"));
    // Searching shows the box over the list while typing.
    assert!(mail.key(&key("/"), None));
    app.tick();
    assert!(texts(&app).iter().any(|x| x == "Receipts"));
    assert!(mail.key(&esc, None));
    assert!(mail.ui.get_cloned().search.is_none());

    // Narrow: the list alone, then the reader alone with Back.
    mail.set_size(em(600.0), 800.0);
    app.tick();
    let t = texts(&app);
    assert!(t.iter().any(|x| x == "Two") && !t.iter().any(|x| x == "Re") && t.iter().any(|x| x == "≡"), "{t:?}");
    assert!(mail.key(&key("j"), None));
    app.tick();
    let t = texts(&app);
    assert!(t.iter().any(|x| x == "‹ Back") && t.iter().any(|x| x == "Archive") && !t.iter().any(|x| x == "Forward"), "{t:?}");
    // Back (Escape): the list again, the row still chosen; j reads the next.
    assert!(mail.key(&esc, None));
    let ui = mail.ui.get_cloned();
    assert!(ui.reading.is_none() && ui.cursor == Some(0));
    app.tick();
    assert!(!texts(&app).iter().any(|x| x == "‹ Back"));
    assert!(mail.key(&key("j"), None));
    assert_eq!(mail.ui.get_cloned().reading.unwrap().subject, "One");
    assert!(mail.back());
    // ≡ in narrow: the places, a choice closes them.
    mail.toggle_side();
    app.tick();
    assert!(texts(&app).iter().any(|x| x == "Receipts"));
}

#[test]
fn files_across_mail() {
    use atrium_mail::view::Lane;
    let dir = tempfile::tempdir().unwrap();
    let engine = Rc::new(Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() }));
    let account = engine.store.add_account("priya@example.org").unwrap();
    let with_file = |id: &str, from: &str, subject: &str, extra: &str, name: &str, t: &str| {
        format!("From: {from}\r\nTo: priya@example.org\r\nSubject: {subject}\r\nDate: Mon, 23 Nov 2026 {t} +0000\r\nMessage-ID: <{id}>\r\n{extra}MIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=B\r\n\r\n--B\r\nContent-Type: text/plain\r\n\r\nSee attached.\r\n--B\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename={name}\r\n\r\n%PDF-1.7 a small one\r\n--B--\r\n").into_bytes()
    };
    let plan = with_file("p1@acme.example", "Arun <arun@acme.example>", "Plan", "", "plan.pdf", "08:00:00");
    let reply = "From: Arun <arun@acme.example>\r\nTo: priya@example.org\r\nSubject: Re: Plan\r\nDate: Mon, 23 Nov 2026 09:00:00 +0000\r\nMessage-ID: <p2@acme.example>\r\nIn-Reply-To: <p1@acme.example>\r\nReferences: <p1@acme.example>\r\n\r\nDid you read it?\r\n".as_bytes().to_vec();
    let photo = with_file("l1@x.example", "Lena <lena@x.example>", "Photos", "", "beach.png", "07:00:00");
    let photo = String::from_utf8(photo).unwrap()
        .replace("Content-Type: application/pdf", "Content-Type: image/png\r\nContent-Transfer-Encoding: base64")
        .replace("%PDF-1.7 a small one", "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aRuoAAAAASUVORK5CYII=")
        .into_bytes();
    {
        let mut sink = engine.sink(account);
        sink.folders(&[RemoteFolder { name: "INBOX".into(), role: Role::Inbox, selectable: true }]).unwrap();
        let f = |uid, bytes: Vec<u8>, seen: bool| Fetched { uid, flags: if seen { vec!["\\Seen".into()] } else { vec![] }, size: bytes.len() as u64, internal_date: Some(NOW), server_id: None, bytes, full: true };
        sink.messages("INBOX", vec![f(1, photo, true), f(2, plan, true), f(3, reply, false)]).unwrap();
    }
    let dirty = Arc::new(AtomicBool::new(true));
    let view = MailView::new(engine, dirty.clone(), (1280.0, 800.0), || NOW);
    let mail = view.mail.clone();
    let mut app = App::new_with_flag(view, dirty);
    app.tick();

    // The conversation's files at its top while their message is folded.
    mail.open(0);
    assert_eq!(mail.ui.get_cloned().reading.unwrap().msgs.len(), 2);
    app.tick();
    let t = texts(&app);
    assert!(t.iter().any(|x| x == "1 file in this conversation: plan.pdf") && t.iter().any(|x| x == "Show them"), "{t:?}");
    assert!(mail.palette_entries().iter().any(|e| e.label == "Show this conversation's files"));
    mail.show_files();
    app.tick();
    let t = texts(&app);
    assert!(t.iter().any(|x| x == "plan.pdf") && !t.iter().any(|x| x == "Show them"), "{t:?}");

    // Files from the sender: a search for their attachments.
    let from = mail.palette_entries().into_iter().find(|e| e.label == "Files from Arun").expect("offered");
    mail.run_cmd(from.cmd);
    let s = mail.ui.get_cloned().search.unwrap();
    assert_eq!(s.query, "from:arun@acme.example has:attachment");
    assert_eq!(s.hits.len(), 1);
    assert_eq!(s.hits[0].subject, "Plan");
    assert_eq!(s.hits[0].files[0].name, "plan.pdf");
    assert_eq!(s.hits[0].files[0].size, 20);
    app.tick();
    assert!(texts(&app).iter().any(|x| x == "plan.pdf"));

    // The Files view: every message with files, newest first.
    assert!(mail.ui.get_cloned().views.iter().any(|(l, _)| *l == Lane::Files));
    mail.show_view(Lane::Files);
    let s = mail.ui.get_cloned().search.unwrap();
    assert_eq!(s.hits.iter().flat_map(|h| h.files.iter().map(|file| file.name.as_str())).collect::<Vec<_>>(), ["plan.pdf", "beach.png"]);
    app.tick();
    let t = texts(&app);
    for label in ["plan.pdf", "beach.png", "20 bytes"] {
        assert!(t.iter().any(|x| x == label), "{t:?}");
    }

    assert!(!app.tree().iter().any(|(_, n)| matches!(n, Node::Image { .. })));
    let part = mail.next_thumbnail().expect("visible picture");
    assert_eq!(Some(&part), s.hits[1].files[0].picture.as_ref());
    mail.thumbnail_ready(part, Some(pergola::node::Image::new(2, 1, vec![128; 8])));
    app.tick();
    assert!(mail.next_thumbnail().is_none());
    assert_eq!(app.tree().iter().filter(|(_, n)| matches!(n, Node::Image { .. })).count(), 1);

    // Clicking the raster inside a tile still opens its source.
    let at = Point::new(224.0 + 199.0, mail.list_top() + 32.0);
    app.handle_event(Event::PointerDown { at });
    app.handle_event(Event::PointerUp { at });
    app.tick();
    assert_eq!(mail.ui.get_cloned().reading.unwrap().subject, "Photos");
    // Counted in its header.
    let total = 20 + s.hits[1].files[0].size;
    assert_eq!(mail.ui.get_cloned().files_total, (2, total));
    assert!(texts(&app).contains(&format!("2 files, {total} bytes")), "{:?}", texts(&app));
}

#[test]
fn jmap_send_status_words_stay_distinct_and_held_has_no_retry() {
    use atrium_mail::view::{place_h, Lane};
    use mail_compose::{Draft, Mailbox};
    use mail_engine::send::When;
    use mail_store::{AccountConfig, Protocol};
    use pergola::theme::Semantic;

    let dir = tempfile::tempdir().unwrap();
    let engine = Rc::new(Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() }));
    let account = engine.store.add_configured(&AccountConfig {
        address: "ada@example.org".into(),
        name: "Ada".into(),
        protocol: Protocol::Jmap { session: "https://jmap.test/.well-known/jmap".into() },
        incoming_item: None,
        outgoing_item: None,
        oauth: None,
        reached: Default::default(),
    }).unwrap();

    let open_status = |engine| {
        let dirty = Arc::new(AtomicBool::new(true));
        let view = MailView::new(engine, dirty.clone(), (1280.0, 800.0), || NOW);
        let mail = view.mail.clone();
        let mut app = App::new_with_flag(view, dirty);
        app.tick();
        assert!(texts(&app).contains(&"Send status".into()));
        let at = Point::new(48.0, 800.0 - 8.0 - 1.5 * place_h());
        app.handle_event(Event::PointerDown { at });
        app.handle_event(Event::PointerUp { at });
        app.tick();
        assert_eq!(mail.ui.get_cloned().search.unwrap().view, Some(Lane::SendStatus));
        (mail, app)
    };
    let (mail, mut app) = open_status(engine.clone());
    assert!(texts(&app).contains(&"No JMAP send activity".into()));

    let queue = |subject: &str| {
        let draft = Draft {
            from: Mailbox::new("Ada", "ada@example.org"),
            to: vec![Mailbox::new("Grace", "grace@example.net")],
            subject: subject.into(),
            text: "One copy, please.".into(),
            ..Default::default()
        };
        engine.send(account, &draft, When::default()).unwrap()
    };
    let (accepted, canceled, held) = (queue("Planning"), queue("Budget"), queue("Meeting"));
    let evidence = b"synthetic submission evidence";
    engine.store.write(|w| {
        for id in [accepted, canceled, held] {
            assert!(w.claim_jmap_send(id, i64::MAX)?);
            assert!(w.save_jmap_submission(id, evidence)?);
            w.send_uncertain(id, "Reply lost")?;
        }
        assert!(w.resolve_jmap_submission(canceled, evidence, true)?.is_none());
        assert!(w.resolve_jmap_submission(accepted, evidence, false)?.is_some());
        assert!(w.record_jmap_acceptance(account, "Planning")?);
        Ok(())
    }).unwrap();

    mail.refresh();
    for theme in [Semantic::LIGHT, Semantic::DARK] {
        app.set_theme(theme);
        app.tick();
        let t = texts(&app);
        let index = |label| t.iter().position(|line| line == label).unwrap_or_else(|| panic!("Missing {label:?}: {t:?}"));
        for (subject, label) in [
            ("Meeting", "Awaiting confirmation"),
            ("Budget", "Canceled by server"),
            ("Planning", "Accepted for sending"),
        ] {
            assert_eq!(index(subject) + 1, index(label));
            assert!(app.tree().iter().any(|(_, node)| matches!(
                node, Node::Text { content, style, .. }
                if content == label && style.color == theme.text_primary()
            )));
        }
        assert_eq!(index("Awaiting confirmation") + 1, index("This message may already have been sent."));
        assert!(t.iter().all(|line| !line.contains("Retry") && !line.contains("Send again")), "{t:?}");
    }

    engine.store.write(|w| {
        assert!(w.resolve_jmap_submission(held, evidence, false)?.is_some());
        assert!(w.record_jmap_acceptance(account, "Meeting")?);
        Ok(())
    }).unwrap();
    mail.refresh();
    app.tick();
    let t = texts(&app);
    assert!(!t.contains(&"Awaiting confirmation".into()));
    assert_eq!(t.iter().filter(|line| *line == "Accepted for sending").count(), 2);
    drop((app, mail, engine));

    let engine = Rc::new(Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() }));
    let (mail, mut app) = open_status(engine.clone());
    let t = texts(&app);
    for subject in ["Meeting", "Planning"] {
        assert!(t.windows(2).any(|pair| pair[0] == subject && pair[1] == "Accepted for sending"), "{t:?}");
    }
    assert_eq!(t.iter().filter(|line| *line == "Accepted for sending").count(), 2);
    assert!(t.windows(2).any(|pair| pair[0] == "Budget" && pair[1] == "Canceled by server"), "{t:?}");
    assert!(!t.contains(&"Awaiting confirmation".into()));

    engine.store.write(|w| w.remove_account(account)).unwrap();
    assert!(engine.jmap_send_rows().unwrap().is_empty());
    mail.refresh();
    app.tick();
    assert!(texts(&app).contains(&"No JMAP send activity".into()));
}
