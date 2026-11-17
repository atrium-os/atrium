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
    let c = mail_engine::compose::Compose { account, draft: back, identity: 1, notes: vec![] };
    let k2 = Composer::new(engine.clone(), c, None, Arc::new(AtomicBool::new(true)), move || NOW);
    assert!(k2.is_rich());
    assert!(k2.draft().html.unwrap().contains("<b>world</b>"));
    // To plain text: the words stay.
    k2.set_rich(false);
    assert_eq!(k2.draft().text, "Hello world\n\n-- \nBilling\n");
}
