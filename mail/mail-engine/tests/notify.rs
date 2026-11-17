//! Notifications (mail.md §12, §12.1): quiet by default, fresh mail only,
//! bursts grouped, catch-up as one summary, withdrawn when read, nothing
//! for a first sync.

use std::path::{Path, PathBuf};
use std::process::Command;

use mail_engine::notify::{Notifier, Post};
use mail_engine::{Engine, Parser};
use mail_store::{flag, Role, Store};
use mail_sync::{Fetched, RemoteFolder, Sink};

fn jail() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-parse/Cargo.toml");
    let st = Command::new(env!("CARGO")).args(["build", "-q", "--bin", "mail-parse-jail", "--manifest-path"]).arg(&manifest).status().unwrap();
    assert!(st.success());
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-parse/target/debug/mail-parse-jail")
}

// Mon 23 Nov 2026 09:21 UTC.
const NOW: i64 = 1_795_425_660;

fn msg(uid: u32, from: &str, subject: &str, t: i64, extra: &str, body: &str) -> Fetched {
    let s = t.rem_euclid(86_400);
    let day = 23 - (NOW.div_euclid(86_400) - t.div_euclid(86_400));
    let date = format!("Mon, {day} Nov 2026 {:02}:{:02}:{:02} +0000", s / 3600, s % 3600 / 60, s % 60);
    let bytes = format!("From: {from}\r\nTo: Arun <arun@example.org>\r\nSubject: {subject}\r\nDate: {date}\r\n{extra}MIME-Version: 1.0\r\nContent-Type: text/plain\r\n\r\n{body}\r\n").into_bytes();
    Fetched { uid, flags: vec![], size: bytes.len() as u64, internal_date: None, server_id: None, bytes, full: true }
}

#[test]
fn quiet_fresh_grouped_and_withdrawn() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("arun@example.org").unwrap();
    let mut sink = engine.sink(account);
    sink.folders(&[RemoteFolder { name: "INBOX".into(), role: mail_sync::Role::Inbox, selectable: true }]).unwrap();
    sink.messages("INBOX", vec![msg(1, "Old <old@x.example>", "Before", NOW - 86_400, "", "Hi.")]).unwrap();
    let mut n = Notifier::new();
    // What was there when Mail started: nothing (09:21 is after the review hour, nothing due).
    assert!(n.check(&engine, NOW).is_empty());

    // One fresh message from a person: its subject, sender and new text.
    sink.messages("INBOX", vec![msg(2, "Priya Raman <priya@example.com>", "Q4 numbers for the regional review", NOW - 60, "", "Attached.\n\nOn Mon, Arun wrote:\n> Numbers?")]).unwrap();
    let p = n.check(&engine, NOW);
    assert_eq!(p.len(), 1);
    let Post::One { title, body, .. } = &p[0] else { panic!("{p:?}") };
    assert_eq!(title, "Q4 numbers for the regional review");
    assert_eq!(body, "From Priya Raman\nAttached.");

    // A newsletter: counted, not announced.
    sink.messages("INBOX", vec![msg(3, "News <news@list.example>", "This week", NOW - 30, "List-Id: <news.list.example>\r\n", "Hi.")]).unwrap();
    assert!(n.check(&engine, NOW + 1).is_empty());

    // Three people at once (a burst): one grouped post.
    sink.messages("INBOX", vec![
        msg(4, "Lena <lena@partner.example>", "Lunch", NOW - 20, "", "Friday?"),
        msg(5, "Sam <sam@acme.example>", "Contract", NOW - 10, "", "Signed."),
        msg(6, "Lena B <lenab@x.example>", "Hi", NOW - 5, "", "Hey."),
    ]).unwrap();
    let p = n.check(&engine, NOW + 2);
    assert_eq!(p, [Post::Group { title: "4 new messages".into(), body: "From Lena, Sam".into() }]);

    // After a gap: older mail is one silent summary.
    sink.messages("INBOX", vec![
        msg(7, "Ravi <ravi@x.example>", "Yesterday's", NOW - 7200, "", "Hi."),
        msg(8, "Bot <bot@x.example>", "Report", NOW - 7000, "Auto-Submitted: auto-generated\r\n", "Hi."),
    ]).unwrap();
    let p = n.check(&engine, NOW + 600);
    assert_eq!(p, [Post::Summary { title: "While you were away".into(), body: "2 new, 1 from people (Ravi)".into() }]);

    // Priya's read on the phone: her post is taken back.
    let inbox = engine.store.folder_id(account, "INBOX", Role::Inbox).unwrap();
    engine.store.write(|w| w.set_flags(inbox, 2, flag::SEEN)).unwrap();
    let priya = engine.store.at(inbox, 2).unwrap().unwrap().0;
    assert_eq!(n.check(&engine, NOW + 601), [Post::Withdraw { msg: priya }]);

    // A second account's first sync: nothing, however fresh.
    let other = engine.store.add_account("arun@home.example").unwrap();
    let mut s2 = engine.sink(other);
    s2.folders(&[RemoteFolder { name: "INBOX".into(), role: mail_sync::Role::Inbox, selectable: true }]).unwrap();
    s2.messages("INBOX", vec![msg(1, "Mum <mum@home.example>", "Dinner", NOW + 590, "", "Sunday?")]).unwrap();
    assert!(n.check(&engine, NOW + 602).is_empty());
}

#[test]
fn settings_off_quiet_hours_and_vips() {
    use mail_engine::prefs::Prefs;
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("arun@example.org").unwrap();
    let mut sink = engine.sink(account);
    sink.folders(&[RemoteFolder { name: "INBOX".into(), role: mail_sync::Role::Inbox, selectable: true }]).unwrap();
    sink.messages("INBOX", vec![msg(1, "Old <old@x.example>", "Before", NOW - 86_400, "", "Hi.")]).unwrap();
    assert_eq!(engine.prefs(), Prefs::default());
    // Quiet 09:00–10:00 (NOW is 09:21 UTC; local = UTC here); the boss is
    // a VIP, even though their mail is sent by a system.
    let p = Prefs { quiet: Some((9 * 60, 10 * 60)), vips: vec!["Boss@Acme.example".into(), "not an address".into()], ..Prefs::default() };
    engine.set_prefs(&p).unwrap();
    let got = engine.prefs();
    assert_eq!(got.vips, ["boss@acme.example"]);
    assert_eq!(got.quiet, Some((540, 600)));
    let mut n = Notifier::new();
    assert!(n.check_local(&engine, NOW, 0).is_empty());
    sink.messages("INBOX", vec![
        msg(2, "Priya <priya@example.com>", "Inside", NOW - 30, "", "Hi."),
        msg(3, "Boss <boss@acme.example>", "Urgent", NOW - 20, "Auto-Submitted: auto-generated\r\n", "Now."),
    ]).unwrap();
    assert!(n.check_local(&engine, NOW, 0).is_empty(), "held back, VIPs too by default");
    // An hour later (from 10:00 UTC, or with the local clock 1 h ahead):
    // one silent summary.
    let p = n.check_local(&engine, NOW + 60, 3600);
    assert_eq!(p, [Post::Summary { title: "During quiet hours".into(), body: "2 new, 2 from people (Priya, Boss)".into() }]);
    // VIPs let through: the boss rings inside quiet hours, Priya waits.
    engine.set_prefs(&Prefs { vips_through: true, ..engine.prefs() }).unwrap();
    sink.messages("INBOX", vec![
        msg(4, "Priya <priya@example.com>", "Again", NOW + 100, "", "Hi."),
        msg(5, "Boss <boss@acme.example>", "Again", NOW + 110, "", "Now."),
    ]).unwrap();
    let p = n.check_local(&engine, NOW + 120, 0);
    assert!(matches!(&p[..], [Post::One { title, .. }] if title == "Again"), "{p:?}");
    // Off: nothing at all, and nothing saved up.
    engine.set_prefs(&Prefs { notify: false, ..engine.prefs() }).unwrap();
    sink.messages("INBOX", vec![msg(6, "Lena <lena@x.example>", "Lunch", NOW + 130, "", "Hi.")]).unwrap();
    assert!(n.check_local(&engine, NOW + 140, 0).is_empty());
    engine.set_prefs(&Prefs { notify: true, quiet: None, ..engine.prefs() }).unwrap();
    assert!(n.check_local(&engine, NOW + 150, 0).is_empty());
}
