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

    // A press on the fourth row (the list starts under its 48 pt heading,
    // rows are 72 pt) opens Arun's message, wrapped to the column.
    let at = Point::new(224.0 + 200.0, 48.0 + 72.0 * 3.0 + 36.0);
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
    assert!(mail.key(&key("k"), None));
    let r = mail.ui.get_cloned().reading.unwrap();
    assert_eq!(r.subject, "This week");
    assert_eq!(r.framed, Some(0), "an HTML-only message goes to the doc-viewer");
    assert!(mail.key(&key("k"), None));
    let r = mail.ui.get_cloned().reading.unwrap();
    assert_eq!(r.subject, "Headers only so far");
    assert!(note(&r).contains("headers"));
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
        let bytes = message(1, "Bank <alerts@bank.example>", "Verify now", &body, true, NOW - 60);
        sink.messages("INBOX", vec![Fetched { uid: 1, flags: vec![], size: bytes.len() as u64, internal_date: Some(NOW - 60), server_id: None, bytes, full: true }]).unwrap();
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
}
