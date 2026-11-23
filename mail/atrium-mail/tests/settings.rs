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

    // A setup with no mail accounts can still open the trusted S/MIME sheet.
    assert!(texts(&app).iter().any(|t| t == "Add S/MIME identity"));
    assert!(!s.take_smime_ask());

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
    // The keys Mail has (§10.2): listed with where they came from; Forget.
    engine.give_pgp_key(include_bytes!("../../mail-pgp/tests/keys/expiring.asc"), 100).unwrap();
    s.reload();
    drawn(&mut app, &["later@example.com", "Forget"]);
    assert!(s.ui.get_cloned().keys[0].1.contains("Pasted by you · expires 20"), "{:?}", s.ui.get_cloned().keys);
    s.forget_key("later@example.com");
    assert!(s.ui.get_cloned().keys.is_empty() && engine.known_pgp_keys().is_empty());
    // S/MIME (§10.1): a CA pasted is listed and can be forgotten.
    drawn(&mut app, &["S/MIME", "Paste a CA"]);
    let ca = include_bytes!("../../mail-smime/tests/data/ca.der");
    let b64 = {
        const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut o = String::new();
        for c in ca.chunks(3) {
            let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
            for k in 0..4 { o.push(if k <= c.len() { T[(n >> (18 - 6 * k) & 63) as usize] as char } else { '=' }); }
        }
        o
    };
    s.give_ca(&format!("-----BEGIN CERTIFICATE-----\n{b64}\n-----END CERTIFICATE-----\n"), 0);
    let cas = s.ui.get_cloned().cas;
    assert_eq!(cas.len(), 1, "{:?}", s.ui.get_cloned().note);
    drawn(&mut app, &[cas[0].0.as_str()]);
    s.forget_ca(cas[0].2);
    assert!(s.ui.get_cloned().cas.is_empty() && engine.added_cas().is_empty());
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
    assert!(!texts(&app).iter().any(|x| x == "COMPANY"));
    *s.persona.borrow_mut() = Some("Work".into());
    s.report_to.set(TextEdit::new("Phish@"));
    s.tick();
    assert_eq!(engine.prefs().report_to, None);
    s.report_to.set(TextEdit::new("Phish@Acme.example"));
    s.tick();
    assert_eq!(engine.prefs().report_to.as_deref(), Some("phish@acme.example"));
    drawn(&mut app, &["COMPANY", "Also send reports to", "Its other mail domains"]);
    s.company.set(TextEdit::new("acme-corp.example, acme"));
    s.tick();
    assert!(engine.prefs().company.is_empty(), "half-typed");
    s.company.set(TextEdit::new("acme-corp.example, @acme.example"));
    s.tick();
    assert_eq!(engine.prefs().company, ["acme-corp.example", "acme.example"]);
    s.scanners.set(TextEdit::new("scan.acme.example"));
    s.tick();
    assert_eq!(engine.prefs().scanners, ["scan.acme.example"]);
    s.report_to.set(TextEdit::new(""));
    s.tick();
    assert_eq!(engine.prefs().report_to, None);
    // An import whose uploads gave up: said under the account, tried
    // again or dropped.
    let nowhere = engine.store.folder_id(id, "Nowhere", mail_store::Role::Other).unwrap();
    let blob = engine.store.put_blob(b"Subject: lost\r\n\r\nx\r\n").unwrap();
    let give_up = || {
        let qid = engine.store.imports().unwrap().first().map(|(q, _)| *q);
        engine.store.write(|w| {
            let q = match qid { Some(q) => q, None => w.queue_import(nowhere, blob)? };
            (0..mail_store::imports::TRIES_MAX).try_for_each(|_| w.import_failed(q, "the server has no folder Nowhere"))
        }).unwrap();
    };
    give_up();
    s.reload();
    drawn(&mut app, &["1 imported message couldn't upload: the server has no", "folder Nowhere", "Try again", "Clear"]);
    s.stuck_imports(id, true);
    assert_eq!(s.ui.get_cloned().note.as_deref(), Some("Trying 1 message again at the next sync"));
    assert_eq!(engine.imports_waiting(id).unwrap(), 1);
    give_up();
    s.stuck_imports(id, false);
    assert!(engine.store.imports().unwrap().is_empty());
    assert!(s.ui.get_cloned().rows[0].stuck.is_none());
    // Mail's filters changed on the server: Show reads what is there.
    engine.store.write(|w| {
        w.set_sync_state(id, "jmap_sieve_why", Some(mail_engine::sieve::CHANGED_THERE))?;
        w.set_sync_state(id, "sieve_there", Some("# mine now\nkeep;"))
    }).unwrap();
    s.reload();
    drawn(&mut app, &["Show", "Replace"]);
    s.show_filters(id);
    drawn(&mut app, &["Hide", "On the server now:", "# mine now", "keep;"]);
    s.show_filters(id);
    assert!(s.ui.get_cloned().filters_shown.is_none());

    s.close();
    assert!(s.closed());
}

#[test]
fn lost_microsoft_sheet_releases_ticket_and_retries_existing_account() {
    use arcad::proto::{op, read_frame, status, write_reply};
    use std::os::unix::net::UnixListener;

    let dir = tempfile::tempdir().unwrap();
    let engine = Rc::new(Engine::new(Store::open(dir.path()).unwrap(), Parser { program: "/nonexistent".into() }));
    let address = "ada@outlook.com";
    let cfg = AccountConfig {
        address: address.into(), name: "Ada".into(), oauth: Some("microsoft".into()),
        incoming_item: Some([1; 16]), outgoing_item: Some([2; 16]),
        protocol: Protocol::Imap {
            imap: Server { scheme: "imaps".into(), host: "outlook.office365.com".into(), port: 993 },
            smtp: Server { scheme: "submission".into(), host: "smtp.office365.com".into(), port: 587 },
        },
        reached: Reached::SignInAgain { at: 1 },
    };
    let id = engine.store.add_configured(&cfg).unwrap();
    let path = dir.path().join("arca.sock");
    let socket = UnixListener::bind(&path).unwrap();
    let server = std::thread::spawn(move || {
        let refs = [[1; 16], [2; 16]].concat();
        let renewal = [refs.as_slice(), address.as_bytes()].concat();
        let mut result = arca_store::codec::Writer::new();
        result.raw(&[4; 16]);
        result.str(address);
        for (opcode, request, response) in [
            (op::MAIL_REAUTH, renewal.clone(), vec![3; 16]),
            (op::MAIL_SIGN_IN_CANCEL, vec![3; 16], vec![]),
            (op::MAIL_REAUTH, renewal, vec![4; 16]),
            (op::MAIL_SIGN_IN_RESULT, result.buf, refs),
        ] {
            let (mut stream, _) = socket.accept().unwrap();
            let frame = read_frame(&mut stream).unwrap().unwrap();
            assert_eq!(frame.as_ref(), [opcode.to_le_bytes().as_slice(), &request].concat());
            write_reply(&mut stream, status::OK, &response).unwrap();
        }
    });
    let settings = Settings::new(engine.clone(), Arc::new(AtomicBool::new(true)), path);
    let retry = || {
        settings.renew_microsoft(id);
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(ticket) = settings.take_microsoft_ticket() { break ticket; }
            assert!(Instant::now() < until, "sign-in ticket timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    let first = retry();
    settings.microsoft_sheet_open.set(true);
    settings.dismiss_microsoft(first, "The Microsoft sign-in sheet closed.");
    assert!(!settings.microsoft_sheet_open.get());
    assert_eq!(settings.ui.get_cloned().adding, Adding::Closed);
    assert_eq!(settings.ui.get_cloned().note.as_deref(), Some("The Microsoft sign-in sheet closed."));
    assert!(!settings.take_added());
    let second = retry();
    assert_ne!(first, second);
    settings.complete_microsoft(second);
    let until = Instant::now() + Duration::from_secs(5);
    while !settings.take_added() {
        assert!(Instant::now() < until, "retry did not wake sync");
        settings.tick();
        std::thread::sleep(Duration::from_millis(10));
    }
    server.join().unwrap();
    let accounts = engine.configured();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].0, id);
    assert_eq!(accounts[0].1.incoming_item, cfg.incoming_item);
    assert_eq!(accounts[0].1.outgoing_item, cfg.outgoing_item);
}
