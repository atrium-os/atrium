//! The compose window without a display: fields filled, typing in the
//! text, saving a draft, Send only with a press, Undo bringing it back, and
//! a `mailto:` link opening a filled window.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use atrium_mail::compose::{ComposeView, Composer, Done};
use atrium_mail::MailView;
use mail_engine::{Engine, Parser};
use mail_store::{Identity, Store};
use pergola::geom::Point;
use pergola::node::Node;
use pergola::text_edit::TextEdit;
use pergola::{App, Event, Key, KeyEventKind, Modifiers};

const NOW: i64 = 1_795_425_660;

fn texts<V: pergola::view::View>(app: &App<V>) -> Vec<String> {
    app.tree().iter().filter_map(|(_, n)| match n { Node::Text { content, .. } => Some(content.clone()), _ => None }).collect()
}

fn typed(chars: &str) -> Event {
    Event::Key { kind: KeyEventKind::Down, key: Key::Char, modifiers: Modifiers::default(), chars: chars.into() }
}

#[test]
fn compose_send_undo() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Rc::new(Engine::new(Store::open(dir.path()).unwrap(), Parser { program: "/nonexistent".into() }));
    let account = engine.store.add_account("priya@example.org").unwrap();
    engine.store.set_identities(account, &[
        Identity { name: "Priya Raman".into(), addr: "priya@example.org".into(), signature: "Priya".into() },
        Identity { name: "".into(), addr: "billing@example.org".into(), signature: String::new() },
    ]).unwrap();
    let clock = Rc::new(Cell::new(NOW));
    let ck = clock.clone();

    // The main window asks for a compose window (c).
    let dirty = Arc::new(AtomicBool::new(true));
    let view = MailView::new(engine.clone(), dirty.clone(), (1280.0, 800.0), move || ck.get());
    let mail = view.mail.clone();
    let mut main = App::new_with_flag(view, dirty);
    main.tick();
    assert!(texts(&main).iter().any(|t| t == "Write"));
    assert!(mail.key(&typed("c"), None));
    let mut asked = mail.take_composes();
    assert_eq!(asked.len(), 1);
    let (c, draft_id) = asked.remove(0);
    assert_eq!(c.draft.text, "\n\n-- \nPriya\n");

    let dirty = Arc::new(AtomicBool::new(true));
    let ck = clock.clone();
    let k = Composer::new(engine.clone(), c, draft_id, dirty.clone(), move || ck.get());
    assert!(k.is_rich(), "rich text by default");
    k.set_rich(false);
    assert_eq!(k.draft().text, "\n\n-- \nPriya\n");
    assert!(k.draft().html.is_none());
    let mut app = App::new_with_flag(ComposeView::new(k.clone(), (760.0, 640.0)), dirty);
    app.tick();
    let t = texts(&app);
    for s in ["New message", "Send", "Send later", "Discard", "From", "Priya Raman <priya@example.org>  ⌄", "To", "Cc", "Subject", "Priya"] {
        assert!(t.iter().any(|x| x == s), "{s:?} not drawn: {t:?}");
    }
    // Fill the fields; type in the text (a press puts the caret there).
    k.to.set(TextEdit::new("Arun <arun@acme.example>, lena@acme.example"));
    k.subject.set(TextEdit::new("Q4 numbers"));
    app.tick();
    let body_at = Point::new(30.0, 48.0 + 40.0 * 4.0 + 36.0 + 1.0 + 8.0 + 4.0);
    app.handle_event(Event::PointerDown { at: body_at });
    app.handle_event(Event::PointerUp { at: body_at });
    app.tick();
    for ch in ["H", "i", " ", "A", "r", "u", "n"] {
        app.handle_event(typed(ch));
    }
    app.tick();
    assert_eq!(k.draft().text, "Hi Arun\n\n-- \nPriya\n");
    assert!(texts(&app).iter().any(|x| x == "Hi Arun"));
    // Down twice reaches the separator line; the caret stays on screen.
    app.handle_event(Event::Key { kind: KeyEventKind::Down, key: Key::Down, modifiers: Modifiers::default(), chars: String::new() });
    app.handle_event(Event::Key { kind: KeyEventKind::Down, key: Key::Down, modifiers: Modifiers::default(), chars: String::new() });
    assert!(k.body.get_cloned().caret() > "Hi Arun\n".len());

    // Saved every few seconds, not on every key.
    k.tick();
    assert!(k.ui.get_cloned().draft_id.is_none(), "not yet");
    clock.set(NOW + 5);
    k.tick();
    let id = k.ui.get_cloned().draft_id.expect("saved");
    assert_eq!(engine.drafts(account).unwrap()[0].1.subject, "Q4 numbers");

    // From: the other identity, whose signature is none.
    k.set_from(1);
    assert_eq!(k.draft().from.addr, "billing@example.org");
    assert_eq!(k.draft().text, "Hi Arun");
    k.set_from(0);
    assert_eq!(k.draft().text, "Hi Arun\n\n-- \nPriya\n");

    // Send without a press: refused. A mention of an attachment asks once.
    assert!(!k.send(None, None));
    assert_eq!(k.ui.get_cloned().error.as_deref(), Some("Press Send to send"));
    let mut b = k.body.get_cloned();
    b.set_caret(0, false);
    b.insert("The file is attached. ");
    k.body.set(b);
    assert!(!k.send(Some([7; 16]), None));
    assert!(k.ui.get_cloned().error.unwrap().contains("attachment"));
    assert!(k.send(Some([7; 16]), None));
    let Some(Done::Sent { send, label }) = k.done() else { panic!() };
    assert_eq!(label, "Sending…");
    assert!(engine.drafts(account).unwrap().is_empty(), "the draft became the message");
    assert_eq!(engine.store.outbox().unwrap().len(), 1);
    let _ = id;

    // The main window offers Undo; z takes it back into a compose window.
    mail.sent(send, label);
    main.tick();
    assert!(texts(&main).iter().any(|t| t == "Sending…"));
    assert!(mail.key(&typed("z"), None));
    assert!(engine.store.outbox().unwrap().is_empty());
    let back = mail.take_composes();
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].0.draft.subject, "Q4 numbers");
    assert!(back[0].0.draft.text.starts_with("The file is attached. Hi Arun"));
    assert_eq!(back[0].0.draft.to.len(), 2);

    // A mailto link: a filled window, with what the link did said.
    mail.mailto("mailto:arun@acme.example?subject=Hello&body=Hi%20there&bcc=spy@x.example");
    let (c, _) = mail.take_composes().remove(0);
    let dirty = Arc::new(AtomicBool::new(true));
    let k = Composer::new(engine.clone(), c, None, dirty.clone(), move || NOW);
    let mut app = App::new_with_flag(ComposeView::new(k.clone(), (760.0, 640.0)), dirty);
    app.tick();
    let t = texts(&app);
    assert!(t.iter().any(|x| x.contains("hidden copy to spy@x.example")), "{t:?}");
    assert!(t.iter().any(|x| x == "Bcc"), "Bcc shown when the link fills it");
    assert_eq!(k.draft().bcc[0].addr, "spy@x.example");
    assert_eq!(k.title(), "Hello");
    // Remind me if nobody replies: pressed once, three days.
    assert!(texts(&app).iter().any(|x| x == "Remind me"));
    k.ui.set(atrium_mail::compose::ComposeUi { follow_up: Some(3), ..k.ui.get_cloned() });
    app.tick();
    assert!(texts(&app).iter().any(|x| x == "Remind if no reply: 3 days"));
    // Send later: the choices, then a scheduled send.
    k.ui.set(atrium_mail::compose::ComposeUi { later: true, ..k.ui.get_cloned() });
    app.tick();
    assert!(texts(&app).iter().any(|x| x == "Tomorrow at 08:00"));
    let at = atrium_mail::compose::later_choices(NOW)[1].at;
    assert!(k.send(Some([1; 16]), Some(at)));
    assert_eq!(engine.store.outbox().unwrap()[0].1.later, Some(at));
    // Discard throws a saved draft away.
    let (c, _) = { mail.write_new(); mail.take_composes().remove(0) };
    let k = Composer::new(engine.clone(), c, None, Arc::new(AtomicBool::new(true)), move || NOW);
    k.subject.set(TextEdit::new("Never mind"));
    k.save(true);
    assert_eq!(engine.drafts(account).unwrap().len(), 1);
    k.discard();
    assert!(engine.drafts(account).unwrap().is_empty());
    assert_eq!(k.done(), Some(Done::Discarded));
}

#[test]
fn rich_text() {
    use atrium_mail::compose::Format;
    let dir = tempfile::tempdir().unwrap();
    let engine = Rc::new(Engine::new(Store::open(dir.path()).unwrap(), Parser { program: "/nonexistent".into() }));
    let account = engine.store.add_account("priya@example.org").unwrap();
    engine.store.set_identities(account, &[
        Identity { name: "Priya Raman".into(), addr: "priya@example.org".into(), signature: "Priya".into() },
        Identity { name: "".into(), addr: "billing@example.org".into(), signature: "Billing".into() },
    ]).unwrap();
    let c = engine.compose_new(account, 0).unwrap();
    let dirty = Arc::new(AtomicBool::new(true));
    let k = Composer::new(engine.clone(), c, None, dirty.clone(), move || NOW);
    let mut app = App::new_with_flag(ComposeView::new(k.clone(), (760.0, 640.0)), dirty);
    app.tick();
    let t = texts(&app);
    for s in ["B", "I", "U", "• List", "Plain text"] {
        assert!(t.iter().any(|x| x == s), "{s:?} not drawn: {t:?}");
    }
    assert!(app.tree().iter().any(|(_, n)| matches!(n, Node::Glyphs { .. })), "the text is drawn by the rich editor");
    // Type, then bold the next words, then a list.
    {
        let r = k.rich.borrow();
        let mut e = r.as_ref().unwrap().editor.borrow_mut();
        e.set_caret(pergola_richtext::Pos::new(0, 0), false);
        e.insert("Hello ");
    }
    k.format(Format::Bold);
    {
        let r = k.rich.borrow();
        let mut e = r.as_ref().unwrap().editor.borrow_mut();
        e.insert("world");
    }
    k.format(Format::Bold);
    let d = k.draft();
    let html = d.html.clone().unwrap();
    assert!(html.contains("<p>Hello <b>world</b></p>"), "{html}");
    assert_eq!(d.text, "Hello world\n\n-- \nPriya\n");
    app.tick();
    // Another From: its signature, the formatting kept.
    k.set_from(1);
    let d = k.draft();
    assert!(d.html.as_deref().unwrap().contains("<b>world</b>"));
    assert!(d.text.ends_with("-- \nBilling\n"), "{:?}", d.text);
    // Sent as text and HTML alternatives; Undo opens it as rich text again.
    k.to.set(TextEdit::new("arun@acme.example"));
    assert!(k.send(Some([2; 16]), None));
    let (_, o) = engine.store.outbox().unwrap().remove(0);
    let raw = String::from_utf8(engine.store.blob(&o.message).unwrap()).unwrap();
    assert!(raw.contains("multipart/alternative") && raw.contains("text/html") && raw.contains("text/plain"), "{raw}");
    let back = engine.undo_send(engine.store.outbox().unwrap()[0].0).unwrap().unwrap();
    let c = mail_engine::compose::Compose { account, draft: back, identity: 1, notes: vec![], original: None };
    let k2 = Composer::new(engine.clone(), c, None, Arc::new(AtomicBool::new(true)), move || NOW);
    assert!(k2.is_rich());
    assert!(k2.draft().html.unwrap().contains("<b>world</b>"));
    // To plain text: the words stay.
    k2.set_rich(false);
    assert_eq!(k2.draft().text, "Hello world\n\n-- \nBilling\n");
}

fn jail() -> std::path::PathBuf {
    use std::path::Path;
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-parse/Cargo.toml");
    let st = std::process::Command::new(env!("CARGO")).args(["build", "-q", "--bin", "mail-parse-jail", "--manifest-path"]).arg(&manifest).status().unwrap();
    assert!(st.success());
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-parse/target/debug/mail-parse-jail")
}

/// A reply to an HTML message quotes it with its formatting: shown in a
/// doc-viewer slot, sent in a cited blockquote, "> " lines in the plain part.
#[test]
fn reply_quotes_the_html_original() {
    use mail_sync::{Fetched, RemoteFolder, Role, Sink};
    let dir = tempfile::tempdir().unwrap();
    let engine = Rc::new(Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() }));
    let account = engine.store.add_account("priya@example.org").unwrap();
    {
        let mut sink = engine.sink(account);
        sink.folders(&[RemoteFolder { name: "INBOX".into(), role: Role::Inbox, selectable: true }]).unwrap();
        let bytes = b"From: Arun <arun@acme.example>\r\nTo: Priya <priya@example.org>\r\nSubject: Q4 numbers\r\nDate: Mon, 23 Nov 2026 08:00:00 +0000\r\nMessage-ID: <q4@acme.example>\r\nMIME-Version: 1.0\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<html><body><p>The <b>numbers</b> are in.</p><ul><li>North up</li></ul><script>x()</script></body></html>\r\n".to_vec();
        sink.messages("INBOX", vec![Fetched { uid: 1, flags: vec![], size: 300, internal_date: Some(NOW), server_id: None, bytes, full: true }]).unwrap();
    }
    let dirty = Arc::new(AtomicBool::new(true));
    let view = MailView::new(engine.clone(), dirty.clone(), (1280.0, 800.0), || NOW);
    let mail = view.mail.clone();
    let mut main = App::new_with_flag(view, dirty);
    main.tick();
    mail.open(0);
    main.tick();
    assert!(mail.key(&typed("r"), None));
    let (c, _) = mail.take_composes().remove(0);
    assert!(c.original.is_some(), "the original's document comes along");
    let q = c.draft.quote.clone().unwrap();
    assert!(q.html.contains("<blockquote type=\"cite\"") && q.html.contains("<b>numbers</b>") && !q.html.contains("script"), "{}", q.html);
    assert!(q.text.contains("> The numbers are in."), "{:?}", q.text);

    let dirty = Arc::new(AtomicBool::new(true));
    let k = Composer::new(engine.clone(), c, None, dirty.clone(), move || NOW);
    let mut app = App::new_with_flag(ComposeView::new(k.clone(), (760.0, 640.0)), dirty);
    app.tick();
    let t = texts(&app);
    assert!(t.iter().any(|x| x.starts_with("On ") && x.ends_with("Arun wrote:")), "{t:?}");
    assert!(t.iter().any(|x| x == "Remove"));
    assert!(app.take_slot_changes().iter().any(|(k, r)| *k == atrium_mail::compose::QUOTE_SLOT && r.is_some()), "a slot for the doc-viewer");
    // Sent: the person's text, then the original.
    k.to.set(TextEdit::new("arun@acme.example"));
    {
        let r = k.rich.borrow();
        let mut e = r.as_ref().unwrap().editor.borrow_mut();
        e.set_caret(pergola_richtext::Pos::new(0, 0), false);
        e.insert("Great, thanks");
    }
    let d = k.draft();
    assert_eq!(d.text, "Great, thanks", "only the person's text; the quote is apart");
    assert!(k.send(Some([3; 16]), None));
    let (_, o) = engine.store.outbox().unwrap().remove(0);
    let raw = String::from_utf8(engine.store.blob(&o.message).unwrap()).unwrap().replace("=\r\n", "");
    let plain = raw.split("text/plain").nth(1).unwrap();
    assert!(plain.contains("Great, thanks") && plain.contains("> The numbers are in.") && plain.contains("> - North up"), "{raw}");
    let html = raw.split("text/html").nth(1).unwrap();
    assert!(html.contains("<p>Great, thanks</p>") && html.contains("<blockquote type=\"cite\""), "{raw}");
    assert!(html.contains("<b>numbers</b>") && html.contains("<li>North up</li>") && !html.contains("x()"), "{raw}");

    // Remove takes the original away; plain text folds the quote into the text.
    mail.reply(mail_compose::reply::Kind::Reply);
    let (c, _) = mail.take_composes().remove(0);
    let k = Composer::new(engine.clone(), c.clone(), None, Arc::new(AtomicBool::new(true)), move || NOW);
    k.remove_quote();
    assert!(k.original.borrow().is_none() && k.draft().quote.is_none());
    let k = Composer::new(engine.clone(), c, None, Arc::new(AtomicBool::new(true)), move || NOW);
    k.set_rich(false);
    assert!(k.original.borrow().is_none() && k.draft().quote.is_none());
    assert!(k.draft().text.contains("wrote:\n> The numbers are in."), "{:?}", k.draft().text);
}

#[test]
fn files_are_attached_and_forwarded() {
    use mail_sync::{Fetched, RemoteFolder, Role, Sink};
    let dir = tempfile::tempdir().unwrap();
    let engine = Rc::new(Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() }));
    let account = engine.store.add_account("priya@example.org").unwrap();
    {
        let mut sink = engine.sink(account);
        sink.folders(&[RemoteFolder { name: "INBOX".into(), role: Role::Inbox, selectable: true }]).unwrap();
        let bytes = b"From: Arun <arun@acme.example>\r\nTo: priya@example.org\r\nSubject: Q4\r\nDate: Mon, 23 Nov 2026 08:00:00 +0000\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=B\r\n\r\n--B\r\nContent-Type: text/plain\r\n\r\nNumbers attached.\r\n--B\r\nContent-Type: text/csv\r\nContent-Disposition: attachment; filename=q4.csv\r\n\r\nregion,total\r\n--B--\r\n".to_vec();
        sink.messages("INBOX", vec![Fetched { uid: 1, flags: vec![], size: 300, internal_date: Some(NOW), server_id: None, bytes, full: true }]).unwrap();
    }
    let dirty = Arc::new(AtomicBool::new(true));
    let view = MailView::new(engine.clone(), dirty.clone(), (1280.0, 800.0), || NOW);
    let mail = view.mail.clone();
    let mut main = App::new_with_flag(view, dirty);
    main.tick();
    mail.open(0);
    main.tick();
    // A forward carries the file.
    assert!(mail.key(&typed("f"), None));
    let (c, _) = mail.take_composes().remove(0);
    assert_eq!(c.draft.attachments.len(), 1);
    assert_eq!((c.draft.attachments[0].name.as_str(), c.draft.attachments[0].bytes.as_slice()), ("q4.csv", b"region,total".as_slice()));

    // Attach… asks the app for the picker once; picked files are listed and sent.
    let dirty = Arc::new(AtomicBool::new(true));
    let k = Composer::new(engine.clone(), c, None, dirty.clone(), move || NOW);
    let mut app = App::new_with_flag(ComposeView::new(k.clone(), (760.0, 640.0)), dirty);
    k.ask_attach();
    assert!(k.take_attach_ask() && !k.take_attach_ask());
    k.set_attaching(true);
    app.tick();
    assert!(app.take_slot_changes().iter().any(|(s, r)| *s == atrium_mail::compose::ATTACH_SLOT && r.is_some()), "a slot for the picker");
    k.set_attaching(false);
    k.attach("report.pdf", b"%PDF-1.7\n".to_vec());
    app.tick();
    let t = texts(&app);
    assert!(t.iter().any(|x| x.starts_with("q4.csv")) && t.iter().any(|x| x.starts_with("report.pdf")), "{t:?}");
    assert_eq!(k.draft().attachments[1].mime, "application/pdf", "the type from the bytes");
    k.remove_attachment(0);
    assert_eq!(k.attachments(), [("report.pdf".to_string(), 9)]);
    k.to.set(TextEdit::new("lena@partner.example"));
    assert!(k.send(Some([5; 16]), None));
    let (_, o) = engine.store.outbox().unwrap().remove(0);
    let raw = String::from_utf8(engine.store.blob(&o.message).unwrap()).unwrap();
    assert!(raw.contains("multipart/mixed") && raw.contains("filename=\"report.pdf\"") && !raw.contains("q4.csv"), "{raw}");
}

/// Encrypt turns on only when every recipient's certificate is known, and
/// then the message leaves encrypted.
#[test]
fn encrypt_needs_every_recipient_s_certificate() {
    use mail_sync::{Fetched, RemoteFolder, Role, Sink};
    let data = |f: &str| std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-smime/tests/data").join(f)).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    engine.set_roots(vec![data("ca.der")]);
    let engine = Rc::new(engine);
    let account = engine.store.add_account("grace@example.net").unwrap();
    {
        let mut sink = engine.sink(account);
        sink.folders(&[RemoteFolder { name: "INBOX".into(), role: Role::Inbox, selectable: true }]).unwrap();
        let bytes = data("signed.eml");
        sink.messages("INBOX", vec![Fetched { uid: 1, flags: vec![], size: bytes.len() as u64, internal_date: Some(NOW), server_id: None, bytes, full: true }]).unwrap();
    }
    let dirty = Arc::new(AtomicBool::new(true));
    let view = MailView::new(engine.clone(), dirty.clone(), (1280.0, 800.0), || NOW);
    let mail = view.mail.clone();
    let (c, _) = { mail.write_new(); mail.take_composes().remove(0) };
    let dirty = Arc::new(AtomicBool::new(true));
    let k = Composer::new(engine.clone(), c, None, dirty.clone(), move || NOW);
    k.set_rich(false);
    let mut app = App::new_with_flag(ComposeView::new(k.clone(), (900.0, 640.0)), dirty);
    k.to.set(TextEdit::new("bob@example.net, grace@example.net"));
    k.subject.set(TextEdit::new("Secret"));
    k.toggle_encrypt();
    app.tick();
    assert!(!k.ui.get_cloned().encrypt);
    assert!(texts(&app).iter().any(|x| x.contains("Can't encrypt to bob@example.net yet")), "{:?}", texts(&app));
    k.to.set(TextEdit::new("grace@example.net"));
    k.toggle_encrypt();
    app.tick();
    assert!(k.ui.get_cloned().encrypt);
    assert!(texts(&app).iter().any(|x| x == "Encrypted"));
    assert!(k.send(Some([1; 16]), None));
    let (_, o) = engine.store.outbox().unwrap().remove(0);
    let sent = String::from_utf8(engine.store.blob(&o.message).unwrap()).unwrap();
    assert!(sent.contains("smime-type=enveloped-data") && sent.contains("Subject: Secret"), "{sent}");
}

/// Sign: Mail asks Sigillum (here a stand-in on a socket, the token's key
/// in software) for the certificate and then the signature; what leaves is
/// signed so the jail finds it valid.
#[test]
fn sign_through_sigillum() {
    use rsa::pkcs8::DecodePrivateKey;
    use sigillum_proto::client::{Reply, Request, CertPurpose, MAX_MESSAGE};
    let data = |f: &str| std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-smime/tests/data").join(f)).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("sigillum.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let (cert, key) = (data("leaf.der"), rsa::RsaPrivateKey::from_pkcs8_der(&data("leaf.p8")).unwrap());
    let server = std::thread::spawn(move || {
        let mut asked = Vec::new();
        for _ in 0..2 {
            let (mut s, _) = listener.accept().unwrap();
            let req: Request = sigillum_proto::read_msg(&mut s, MAX_MESSAGE).unwrap().unwrap();
            let reply = match &req {
                Request::ChooseCertificate(c) => Reply::CertificateChosen { ceremony: c.ceremony, cert: cert.clone() },
                Request::SignWithCertificate(w) => Reply::CertificateSigned { ceremony: w.ceremony, signature: key.sign(rsa::Pkcs1v15Sign::new::<sha2::Sha256>(), &w.digest).unwrap() },
                _ => panic!("{req:?}"),
            };
            asked.push(req);
            sigillum_proto::write_msg(&mut s, &reply, MAX_MESSAGE).unwrap();
        }
        asked
    });

    let mut engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    engine.set_roots(vec![data("ca.der")]);
    let engine = Rc::new(engine);
    engine.store.add_account("grace@example.net").unwrap();
    let view = MailView::new(engine.clone(), Arc::new(AtomicBool::new(true)), (1280.0, 800.0), || NOW);
    let mail = view.mail.clone();
    let (c, _) = { mail.write_new(); mail.take_composes().remove(0) };
    let dirty = Arc::new(AtomicBool::new(true));
    let k = Composer::new(engine.clone(), c, None, dirty.clone(), move || NOW);
    k.set_rich(false);
    *k.signer.borrow_mut() = Some(atrium_mail::signing::through_sigillum(&socket));
    let mut app = App::new_with_flag(ComposeView::new(k.clone(), (900.0, 640.0)), dirty);
    app.tick();
    assert!(texts(&app).iter().any(|x| x == "Sign"));
    k.ui.set(atrium_mail::compose::ComposeUi { sign: true, ..k.ui.get_cloned() });
    k.to.set(TextEdit::new("ada@example.org"));
    k.subject.set(TextEdit::new("Signed"));
    k.body.set(TextEdit::new("Hello Ada."));
    assert!(k.send(Some([1; 16]), None), "{:?}", k.ui.get_cloned().error);
    let asked = server.join().unwrap();
    assert!(matches!(&asked[0], Request::ChooseCertificate(c) if c.purpose == CertPurpose::MailSign { from: "grace@example.net".into() }));
    let (_, o) = engine.store.outbox().unwrap().remove(0);
    let sent = engine.store.blob(&o.message).unwrap();
    let opened = engine.parser.open(&sent, vec![], &[data("ca.der")], None, vec![]).unwrap().unwrap();
    assert_eq!(opened.summary.smime.unwrap().state, mail_schema::SmimeState::Valid);
    assert_eq!(opened.summary.text.as_deref().map(str::trim), Some("Hello Ada."));
}

#[test]
fn recipients_complete_from_the_address_book() {
    use atrium_mail::compose::typing;
    let dir = tempfile::tempdir().unwrap();
    let engine = Rc::new(Engine::new(Store::open(dir.path()).unwrap(), Parser { program: "/nonexistent".into() }));
    let account = engine.store.add_account("priya@example.org").unwrap();
    engine.store.set_identities(account, &[Identity { name: "Priya Raman".into(), addr: "priya@example.org".into(), signature: String::new() }]).unwrap();
    // Each of these written to once a day ago; Grace twice.
    engine.store.write(|w| {
        w.wrote_to(Some("<1@x>"), &[("Grace Hopper", "grace@navy.example"), ("", "greg@acme.example")], NOW - 86_400)?;
        w.wrote_to(Some("<2@x>"), &[("Grace Hopper", "grace@navy.example"), ("Arun", "arun@acme.example")], NOW - 86_400)
    }).unwrap();
    assert_eq!(typing("Arun <arun@acme.example>, gr"), Some((25, "gr")));
    assert_eq!(typing("Arun <arun@acme.example>, "), None);
    assert_eq!(typing("\"Raman, Priya\" <p@x>"), None);

    let c = engine.compose_new(account, 0).unwrap();
    let dirty = Arc::new(AtomicBool::new(true));
    let k = Composer::new(engine.clone(), c, None, dirty.clone(), move || NOW);
    let mut app = App::new_with_flag(ComposeView::new(k.clone(), (760.0, 640.0)), dirty);
    app.tick();
    assert!(app.focused().is_some(), "To has the caret");
    app.handle_event(typed("gr"));
    app.tick();
    let t = texts(&app);
    assert!(t.iter().any(|x| x == "Grace Hopper") && t.iter().any(|x| x == "greg@acme.example"), "{t:?}");
    assert_eq!(k.completing.borrow().people[0].addr, "grace@navy.example", "the most written to first");
    let key = |key: Key| Event::Key { kind: KeyEventKind::Down, key, modifiers: Modifiers::default(), chars: String::new() };
    // Down picks the second; Enter takes it, with a comma for the next.
    assert!(k.key(&key(Key::Down), None));
    app.tick();
    assert!(k.key(&key(Key::Enter), None));
    assert_eq!(k.to.get_cloned().text(), "greg@acme.example, ");
    app.tick();
    assert!(k.completing.borrow().people.is_empty());
    // By a word of the name; Tab takes the first.
    app.handle_event(typed("hop"));
    app.tick();
    assert!(k.key(&key(Key::Tab), None));
    assert_eq!(k.to.get_cloned().text(), "greg@acme.example, Grace Hopper <grace@navy.example>, ");
    assert_eq!(k.draft().to.len(), 2);
    // Escape puts them away for what was typed; Enter is the field's again.
    app.handle_event(typed("ar"));
    app.tick();
    assert!(k.key(&key(Key::Escape), None));
    app.tick();
    assert!(!texts(&app).iter().any(|x| x == "Arun"));
    assert!(!k.key(&key(Key::Enter), None));
    // Nothing is suggested in a field without the caret.
    app.handle_event(typed("u"));
    app.tick();
    assert!(texts(&app).iter().any(|x| x == "Arun"));
    k.subject.set(TextEdit::new("x"));
    assert!(k.suggest(None).people.is_empty());
}

#[test]
fn work_points_out_recipients_outside_the_company() {
    use atrium_mail::compose::ComposeUi;
    let dir = tempfile::tempdir().unwrap();
    let engine = Rc::new(Engine::new(Store::open(dir.path()).unwrap(), Parser { program: "/nonexistent".into() }));
    let account = engine.store.add_account("arun@example.com").unwrap();
    engine.store.set_identities(account, &[Identity { name: "Arun".into(), addr: "arun@example.com".into(), signature: String::new() }]).unwrap();
    let c = engine.compose_new(account, 0).unwrap();
    let dirty = Arc::new(AtomicBool::new(true));
    let k = Composer::new(engine.clone(), c, None, dirty.clone(), move || NOW);
    let mut app = App::new_with_flag(ComposeView::new(k.clone(), (760.0, 640.0)), dirty);
    k.to.set(TextEdit::new("Priya <priya@example.com>, lena@eu.example.com, ada@other.org"));
    k.cc.set(TextEdit::new("sam@notexample.com"));
    // Personal: nothing said.
    app.tick();
    assert!(k.outside().is_none());
    assert!(!texts(&app).iter().any(|x| x.starts_with("Outside")));
    // Work: who is outside example.com (a subdomain is inside).
    k.ui.set(ComposeUi { persona: Some("work".into()), ..k.ui.get_cloned() });
    app.tick();
    assert_eq!(k.outside(), Some(("example.com".into(), vec!["ada@other.org".into(), "sam@notexample.com".into()])));
    assert!(texts(&app).iter().any(|x| x == "Outside example.com: ada@other.org, sam@notexample.com"), "{:?}", texts(&app));
    // All inside: the line goes.
    k.to.set(TextEdit::new("priya@example.com"));
    k.cc.set(TextEdit::new(""));
    app.tick();
    assert!(k.outside().is_none());
    assert!(!texts(&app).iter().any(|x| x.starts_with("Outside")));
}
