//! The settings window without a display: asked for with `,` (and opened
//! with no accounts), an account found from its address, Add only once
//! Arca's field holds a password, Arca unreachable said plainly, removal
//! on a second press, and notifications, quiet hours and VIPs kept.

use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use atrium_mail::settings::{Adding, Settings, SettingsView};
use atrium_mail::MailView;
use mail_engine::{Engine, Parser};
use mail_store::{AccountConfig, Protocol, Reached, Server, Store};
use pergola::node::Node;
use pergola::text_edit::TextEdit;
use pergola::{App, Event, Key, KeyEventKind, Modifiers};

fn texts<V: pergola::view::View>(app: &App<V>) -> Vec<String> {
    app.tree().iter().filter_map(|(_, n)| match n { Node::Text { content, .. } => Some(content.clone()), _ => None }).collect()
}

fn drawn<V: pergola::view::View>(app: &mut App<V>, want: &[&str]) {
    app.tick();
    let t = texts(app);
    for s in want {
        assert!(t.iter().any(|x| x == s), "{s:?} not drawn: {t:?}");
    }
}

#[test]
fn settings_window() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Rc::new(Engine::new(Store::open(dir.path()).unwrap(), Parser { program: "/nonexistent".into() }));

    // `,` in the main window asks for it.
    let dirty = Arc::new(AtomicBool::new(true));
    let view = MailView::new(engine.clone(), dirty.clone(), (1280.0, 800.0), || 0);
    let mail = view.mail.clone();
    let mut main = App::new_with_flag(view, dirty);
    drawn(&mut main, &["No accounts yet", "Add an account…", "Settings"]);
    let comma = Event::Key { kind: KeyEventKind::Down, key: Key::Char, modifiers: Modifiers::default(), chars: ",".into() };
    assert!(mail.key(&comma, None));
    assert!(mail.take_settings_ask());
    assert!(!mail.take_settings_ask());

    let dirty = Arc::new(AtomicBool::new(true));
    let s = Settings::new(engine.clone(), dirty.clone(), dir.path().join("no-arcad.sock"));
    let mut app = App::new_with_flag(SettingsView::new(s.clone(), (640.0, 720.0)), dirty);
    drawn(&mut app, &["Mail settings", "Done", "No accounts yet.", "Add an account…", "Notify about new mail", "Quiet hours"]);

    // Found from the address: a known provider needs no lookups.
    s.start_adding();
    drawn(&mut app, &["Address", "Your name", "Find settings", "Cancel"]);
    s.address.set(TextEdit::new("ada@gmail.com"));
    s.name.set(TextEdit::new("Ada"));
    s.find();
    let until = Instant::now() + Duration::from_secs(10);
    while !matches!(s.ui.get_cloned().adding, Adding::Found(_)) {
        assert!(Instant::now() < until, "not found: {:?}", s.ui.get_cloned());
        std::thread::sleep(Duration::from_millis(10));
        s.tick();
    }
    drawn(&mut app, &["Incoming (IMAP): imap.gmail.com:993", "Outgoing (SMTP): smtp.gmail.com:465", "Password", "Add account"]);
    assert!(s.wants_field());
    // Add waits for a password in Arca's field.
    assert!(!s.can_add());
    s.cred.apply(limen::credential::ToParent::Filled { username: None });
    s.cred.apply(limen::credential::ToParent::Handle { handle: [7; 16] });
    assert!(s.can_add());
    // Arca isn't running here: said, and the password is to be typed again.
    s.add();
    let until = Instant::now() + Duration::from_secs(10);
    while !matches!(s.ui.get_cloned().adding, Adding::Found(_)) {
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(10));
        s.tick();
    }
    assert!(s.ui.get_cloned().note.unwrap().starts_with("Arca isn't reachable"));
    assert!(!s.can_add());
    assert!(engine.configured().is_empty());
    s.cancel_adding();
    assert!(!s.wants_field());

    // An account kept earlier: its state, the screener, removal on a second press.
    let server = |h: &str, p| Server { scheme: "imaps".into(), host: h.into(), port: p };
    let id = engine
        .store
        .add_configured(&AccountConfig {
            address: "ada@example.org".into(),
            name: "Ada".into(),
            protocol: Protocol::Imap { imap: server("imap.example.org", 993), smtp: server("smtp.example.org", 465) },
            incoming_item: Some([1; 16]),
            outgoing_item: Some([2; 16]),
            oauth: None,
            reached: Reached::Failed { at: 5, why: "no route".into() },
        })
        .unwrap();
    s.reload();
    drawn(&mut app, &["ada@example.org", "Couldn't reach it: no route", "Screener: off", "Remove"]);
    s.toggle_screener(id);
    assert!(engine.screener(id).unwrap());
    drawn(&mut app, &["Screener: on"]);
    s.remove(id);
    drawn(&mut app, &["Remove it and its mail?"]);
    assert_eq!(engine.configured().len(), 1);
    s.remove(id);
    assert!(engine.configured().is_empty());

    // Notifications, quiet hours (kept as typed once they read as times), VIPs.
    s.toggle_notify();
    assert!(!engine.prefs().notify);
    s.toggle_quiet();
    assert_eq!(engine.prefs().quiet, Some((22 * 60, 7 * 60)));
    drawn(&mut app, &["from", "to", "VIPs notify in quiet hours too"]);
    s.quiet_from.set(TextEdit::new("23:3"));
    s.tick();
    assert_eq!(engine.prefs().quiet, Some((22 * 60, 7 * 60)));
    s.quiet_from.set(TextEdit::new("23:30"));
    s.tick();
    assert_eq!(engine.prefs().quiet, Some((23 * 60 + 30, 7 * 60)));
    s.toggle_vips_through();
    assert!(engine.prefs().vips_through);
    s.vip.set(TextEdit::new("Boss@Example.org"));
    s.add_vip();
    assert_eq!(engine.prefs().vips, vec!["boss@example.org".to_string()]);
    drawn(&mut app, &["boss@example.org"]);
    s.remove_vip("boss@example.org");
    assert!(engine.prefs().vips.is_empty());
    s.close();
    assert!(s.closed());
}
