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
    let mut app = App::new_with_flag(SettingsView::new(s.clone(), (1040.0, 720.0)), dirty);
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
    // Its password in Arca goes too; Arca isn't running here, so: said.
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        s.tick();
        let n = s.ui.get_cloned().note.unwrap_or_default();
        if n.starts_with("Removed the account and its mail, but not its password: Arca isn't reachable") {
            break;
        }
        assert!(Instant::now() < until, "{n}");
        std::thread::sleep(Duration::from_millis(10));
    }

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
    // Mail rules (§11.6): shown in words; one that doesn't read says why.
    drawn(&mut app, &["RULES"]);
    s.mail_rule.set(TextEdit::new("from:@GitHub.com move GitHub"));
    s.add_mail_rule();
    assert_eq!(engine.rules().len(), 1);
    drawn(&mut app, &["Mail from anyone at github.com goes to GitHub"]);
    s.mail_rule.set(TextEdit::new("subject:invoice delete"));
    s.add_mail_rule();
    assert!(s.ui.get_cloned().note.unwrap().starts_with("That rule doesn't read: say what to do"));
    assert_eq!(engine.rules().len(), 1);
    drawn(&mut app, &["Run on the inbox"]);
    s.run_mail_rule(0);
    assert_eq!(s.ui.get_cloned().note.as_deref(), Some("Nothing in the inbox fits that rule"));
    s.remove_mail_rule(0);
    assert!(engine.rules().is_empty());
    // Lane rules: one that reads is kept first; one that doesn't says why.
    drawn(&mut app, &["LANES"]);
    s.rule.set(TextEdit::new("from:@GitHub.com Updates"));
    s.add_rule();
    assert_eq!(engine.lane_rules().len(), 1);
    drawn(&mut app, &["from:@github.com updates"]);
    s.rule.set(TextEdit::new("github updates"));
    s.add_rule();
    assert!(s.ui.get_cloned().note.unwrap().starts_with("That rule doesn't read"));
    s.remove_rule(0);
    assert!(engine.lane_rules().is_empty());
    // Private addresses: a domain of one's own, then one per site.
    let id = engine.store.add_configured(&AccountConfig {
        address: "ada@me.example".into(),
        name: "Ada".into(),
        protocol: Protocol::Jmap { session: "https://api.me.example/jmap".into() },
        incoming_item: Some([1; 16]),
        outgoing_item: Some([1; 16]),
        oauth: None,
        reached: Reached::Ok { at: 1 },
    }).unwrap();
    s.reload();
    drawn(&mut app, &["PRIVATE ADDRESSES", "Domain", "Save", "OPENPGP", "No key of yours yet", "Paste a key"]);
    // Away (§11.6): the form, kept for the next sync; a bad day says so.
    drawn(&mut app, &["AWAY", "Reply automatically", "First day", "Last day", "Reply", "ada@me.example: not read yet"]);
    s.toggle_away();
    s.save_away();
    assert_eq!(s.ui.get_cloned().note.as_deref(), Some("Write the reply first"));
    s.away_text.set(TextEdit::new("Back Monday."));
    s.away_until.set(TextEdit::new("next week"));
    s.save_away();
    assert!(s.ui.get_cloned().note.unwrap().starts_with("The last day reads like"));
    s.away_until.set(TextEdit::new("2026-11-30"));
    s.away_from.set(TextEdit::new("someday"));
    s.save_away();
    assert!(s.ui.get_cloned().note.unwrap().starts_with("The first day reads like"));
    s.away_from.set(TextEdit::new("2026-12-01"));
    s.save_away();
    assert_eq!(s.ui.get_cloned().note.as_deref(), Some("The first day comes after the last"));
    s.away_from.set(TextEdit::new("2026-11-23"));
    let mut text = TextEdit::new("").multiline(true);
    text.set_text("Back Monday.\nSam can help.");
    s.away_text.set(text);
    s.save_away();
    let w = engine.away(id).wanted.unwrap();
    assert_eq!((w.on, w.text.as_str(), w.until.map(mail_engine::clock::day_iso).as_deref()), (true, "Back Monday.\nSam can help.", Some("2026-11-30")));
    assert_eq!(w.from.map(mail_engine::clock::day_iso).as_deref(), Some("2026-11-23"));
    drawn(&mut app, &["ada@me.example: set at the next sync", "First day", "Sam can help."]);
    s.give_key("hello", 5);
    assert!(s.ui.get_cloned().note.unwrap().starts_with("Couldn't use it"));
    s.domain.set(TextEdit::new("me.example"));
    s.save_domain();
    assert_eq!(engine.private_domain(id).as_deref(), Some("me.example"));
    drawn(&mut app, &["New, for", "Make"]);
    s.made_for.set(TextEdit::new("shop.example"));
    s.make_private(5);
    let p = engine.private_addresses(id);
    assert_eq!(p.len(), 1);
    assert!(s.ui.get_cloned().note.unwrap().starts_with(&p[0].addr));
    drawn(&mut app, &[p[0].addr.as_str(), "for shop.example", "Turn off"]);
    s.toggle_private(&p[0].addr);
    assert!(engine.private_addresses(id)[0].off);
    drawn(&mut app, &["Turn on"]);

    // The address book: none, then the people written to, found by typing,
    // forgotten with Forget (and kept forgotten).
    assert!(!texts(&app).iter().any(|x| x == "PEOPLE"));
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    engine.store.write(|w| {
        w.wrote_to(Some("<w1@x>"), &[("Grace Hopper", "grace@navy.example"), ("", "ada@x.example")], now - 86_400)?;
        w.wrote_to(Some("<w2@x>"), &[("Grace Hopper", "grace@navy.example")], now)
    }).unwrap();
    s.reload();
    drawn(&mut app, &["PEOPLE", "2 people you've written to, completed as you type.", "Grace Hopper", "grace@navy.example · 2 messages", "ada@x.example", "1 message"]);
    s.person.set(TextEdit::new("hopp"));
    s.tick();
    assert_eq!(s.ui.get_cloned().people.iter().map(|p| p.addr.as_str()).collect::<Vec<_>>(), ["grace@navy.example"]);
    s.person.set(TextEdit::new("zzz"));
    s.tick();
    drawn(&mut app, &["No one by that"]);
    s.person.set(TextEdit::new(""));
    s.tick();
    s.forget_person("grace@navy.example");
    assert_eq!(s.ui.get_cloned().note.as_deref(), Some("Forgot grace@navy.example"));
    assert!(engine.store.complete("grace", now, 5).unwrap().is_empty());
    drawn(&mut app, &["1 person you've written to, completed as you type."]);
    // Nothing is cut off: the window scrolls.
    for i in 0..12 {
        s.made_for.set(TextEdit::new(&format!("shop{i}.example")));
        s.make_private(6 + i);
    }
    drawn(&mut app, &["for shop11.example", "for shop.example · off: its mail goes to Trash", "KEYBOARD", "Single-key shortcuts (j, k, e, r, …)"]);
    assert!(!texts(&app).iter().any(|x| x == "…"));
    // Single keys off and on.
    s.toggle_single_keys();
    assert!(!engine.prefs().single_keys);
    s.toggle_single_keys();
    assert!(engine.prefs().single_keys);
    // Phishing reports: asked in a named persona (Work); kept once it reads
    // as an address, off again when emptied.
    app.tick();
    assert!(!texts(&app).iter().any(|x| x == "PHISHING"));
    *s.persona.borrow_mut() = Some("Work".into());
    s.report_to.set(TextEdit::new("Phish@"));
    s.tick();
    assert_eq!(engine.prefs().report_to, None);
    s.report_to.set(TextEdit::new("Phish@Acme.example"));
    s.tick();
    assert_eq!(engine.prefs().report_to.as_deref(), Some("phish@acme.example"));
    drawn(&mut app, &["PHISHING", "Also send reports to"]);
    s.report_to.set(TextEdit::new(""));
    s.tick();
    assert_eq!(engine.prefs().report_to, None);

    s.close();
    assert!(s.closed());
}
