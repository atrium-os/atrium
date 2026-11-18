//! Later (mail.md §11.4): set aside, back at its time, closed by a reply,
//! brought back by mail to the person, pushed back counted.

use std::path::{Path, PathBuf};
use std::process::Command;

use mail_engine::{Engine, Parser};
use mail_store::later::kind;
use mail_store::{Role, Store};
use mail_sync::{Fetched, RemoteFolder, Sink};

fn jail() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-parse/Cargo.toml");
    let st = Command::new(env!("CARGO")).args(["build", "-q", "--bin", "mail-parse-jail", "--manifest-path"]).arg(&manifest).status().unwrap();
    assert!(st.success());
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-parse/target/debug/mail-parse-jail")
}

fn msg(from: &str, to: &str, subject: &str, day: u32, id: &str, reply_to: &str) -> Vec<u8> {
    let irt = if reply_to.is_empty() { String::new() } else { format!("In-Reply-To: <{reply_to}>\r\nReferences: <{reply_to}>\r\n") };
    format!("From: {from}\r\nTo: {to}\r\nSubject: {subject}\r\nDate: {day:02} Nov 2026 10:00:00 +0000\r\nMessage-ID: <{id}>\r\n{irt}MIME-Version: 1.0\r\nContent-Type: text/plain\r\n\r\nText.\r\n").into_bytes()
}

fn f(uid: u32, bytes: Vec<u8>) -> Fetched {
    Fetched { uid, flags: vec![], size: bytes.len() as u64, internal_date: None, server_id: None, bytes, full: true }
}

#[test]
fn set_aside_comes_back_and_closes() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("arun@example.org").unwrap();
    let me = "Arun <arun@example.org>";
    let mut sink = engine.sink(account);
    sink.folders(&[
        RemoteFolder { name: "INBOX".into(), role: mail_sync::Role::Inbox, selectable: true },
        RemoteFolder { name: "Sent".into(), role: mail_sync::Role::Sent, selectable: true },
    ]).unwrap();
    sink.messages("INBOX", vec![
        f(1, msg("Priya <priya@example.com>", me, "Numbers?", 10, "p1@x", "")),
        f(2, msg("Lena <lena@partner.example>", me, "Lunch", 11, "l1@x", "")),
        f(3, msg("Sam <sam@acme.example>", me, "Contract", 12, "s1@x", "")),
    ]).unwrap();
    let inbox = engine.store.folder_id(account, "INBOX", Role::Inbox).unwrap();
    let subjects = || engine.store.list(inbox, 0, 50).unwrap().into_iter().map(|r| r.subject).collect::<Vec<_>>();
    let id_of = |uid| engine.store.at(inbox, uid).unwrap().unwrap().0;

    // Reply to Priya tomorrow: out of the inbox, flagged for the server.
    let p = engine.set_later(id_of(1), kind::REPLY, 1_000, "send the numbers").unwrap();
    assert_eq!(subjects(), ["Contract", "Lunch"]);
    assert_eq!(engine.store.ops_pending().unwrap(), 1);
    // Waiting on Sam; Lena's to read, due far away.
    let s = engine.set_later(id_of(3), kind::WAITING, 4_000_000_000, "").unwrap();
    let l = engine.set_later(id_of(2), kind::READ, 4_000_000_000, "").unwrap();
    assert!(subjects().is_empty());
    assert_eq!(engine.later_items().unwrap().iter().map(|x| x.0).collect::<Vec<_>>(), [p, s, l]);

    // Its time: back in the inbox, still open.
    assert_eq!(engine.later_due(2_000).unwrap(), 1);
    assert_eq!(subjects(), ["Numbers?"]);
    assert!(engine.later_of(id_of(1)).unwrap().unwrap().1.back);
    // Arun's reply (seen in Sent) closes the Reply item.
    sink.messages("Sent", vec![f(1, msg(me, "priya@example.com", "Re: Numbers?", 13, "a1@x", "p1@x"))]).unwrap();
    assert!(engine.later_of(id_of(1)).unwrap().is_none());

    // Sam answers: the Waiting item closes and the thread is back.
    sink.messages("INBOX", vec![f(4, msg("Sam <sam@acme.example>", me, "Re: Contract", 14, "s2@x", "s1@x"))]).unwrap();
    assert!(engine.later_of(id_of(3)).unwrap().is_none());
    assert!(subjects().contains(&"Contract".to_string()));

    // A list reply in Lena's thread waits with it; mail to Arun brings it back.
    sink.messages("INBOX", vec![f(5, msg("Bot <bot@partner.example>", "team@partner.example", "Re: Lunch", 14, "b1@x", "l1@x"))]).unwrap();
    assert!(!subjects().contains(&"Re: Lunch".to_string()));
    sink.messages("INBOX", vec![f(6, msg("Lena <lena@partner.example>", me, "Re: Lunch", 15, "l2@x", "l1@x"))]).unwrap();
    assert!(subjects().contains(&"Lunch".to_string()));
    let (_, item) = engine.later_of(id_of(2)).unwrap().unwrap();
    assert!(item.back);

    // Later again counts as pushed back; Done closes and unflags.
    engine.set_later(id_of(2), kind::READ, 4_000_000_000, "").unwrap();
    let (lid, item) = engine.later_of(id_of(2)).unwrap().unwrap();
    assert_eq!((item.pushed, item.back), (1, false));
    assert!(!subjects().contains(&"Lunch".to_string()));
    engine.later_done(lid).unwrap();
    assert!(subjects().contains(&"Lunch".to_string()));
    assert!(engine.later_items().unwrap().is_empty());
}

#[test]
fn remind_me_if_nobody_replies() {
    use mail_compose::{Draft, Mailbox};
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("arun@example.org").unwrap();
    let mut sink = engine.sink(account);
    sink.folders(&[
        RemoteFolder { name: "INBOX".into(), role: mail_sync::Role::Inbox, selectable: true },
        RemoteFolder { name: "Sent".into(), role: mail_sync::Role::Sent, selectable: true },
    ]).unwrap();
    let draft = Draft { from: Mailbox::new("Arun", "arun@example.org"), to: vec![Mailbox::new("Sam", "sam@acme.example")], subject: "Contract".into(), text: "Signed?\n".into(), ..Default::default() };
    let when = mail_engine::send::When { follow_up: Some(3 * 86_400), ..Default::default() };
    let id = engine.send(account, &draft, when).unwrap();
    let o = engine.store.outbox().unwrap().into_iter().find(|(k, _)| *k == id).unwrap().1;
    // The sent copy arrives in Sent: a Waiting item three days out.
    let bytes = engine.store.blob(&o.message).unwrap();
    sink.messages("Sent", vec![f(1, bytes)]).unwrap();
    let items = engine.later_items().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].1.kind, kind::WAITING);
    assert!(items[0].1.due > o.due + 3 * 86_400 - 60);
    // Sam answers: closed.
    let mid = o.message_id.trim_matches(['<', '>']).to_string();
    sink.messages("INBOX", vec![f(1, msg("Sam <sam@acme.example>", "arun@example.org", "Re: Contract", 20, "s9@x", &mid))]).unwrap();
    assert!(engine.later_items().unwrap().is_empty());
}

/// The next Friday 16:00 UTC at least 8 days from now, so items set aside
/// now have waited more than a week by then.
fn friday_afternoon() -> i64 {
    let real = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let day = (real + 8 * 86_400).div_euclid(86_400);
    let fri = day + (4 - (day + 3).rem_euclid(7)).rem_euclid(7);
    fri * 86_400 + 16 * 3600
}

#[test]
fn overdue_is_asked_then_dropped_and_the_weekly_look() {
    use mail_engine::later::{Ask, When};
    use mail_engine::notify::{Notifier, Post};
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("arun@example.org").unwrap();
    let me = "Arun <arun@example.org>";
    let mut sink = engine.sink(account);
    sink.folders(&[RemoteFolder { name: "INBOX".into(), role: mail_sync::Role::Inbox, selectable: true }]).unwrap();
    sink.messages("INBOX", vec![
        f(1, msg("Priya <priya@example.com>", me, "Numbers?", 10, "p1@x", "")),
        f(2, msg("Lena <lena@partner.example>", me, "Lunch", 11, "l1@x", "")),
        f(3, msg("Sam <sam@acme.example>", me, "Contract", 12, "s1@x", "")),
        f(4, msg("Dev <dev@acme.example>", me, "Deploy", 13, "d1@x", "")),
    ]).unwrap();
    let inbox = engine.store.folder_id(account, "INBOX", Role::Inbox).unwrap();
    let subjects = || engine.store.list(inbox, 0, 50).unwrap().into_iter().map(|r| r.subject).collect::<Vec<_>>();
    let id_of = |uid| engine.store.at(inbox, uid).unwrap().unwrap().0;
    let now = friday_afternoon();

    // A note is one line.
    engine.set_later(id_of(1), kind::REPLY, now - 10 * 86_400, "  send the\nnumbers ").unwrap();
    assert_eq!(engine.later_of(id_of(1)).unwrap().unwrap().1.note, "send the numbers");
    let lena = engine.set_later(id_of(2), kind::READ, now - 31 * 86_400, "").unwrap();
    engine.set_later(id_of(3), kind::WAITING, now + 30 * 86_400, "").unwrap();
    // Deploy: next Tuesday, pushed back three times.
    let tuesday = When::NextWeek.at_local(now) + 86_400;
    for _ in 0..4 {
        engine.set_later(id_of(4), kind::DO, tuesday, "").unwrap();
    }
    assert!(subjects().is_empty());
    let asking: Vec<_> = engine.later_asking(now).unwrap().into_iter().map(|(_, l, a)| (l.msg, a)).collect();
    assert_eq!(asking, [(id_of(2), Ask::Stale), (id_of(1), Ask::Stale), (id_of(4), Ask::Pushed)]);

    // 30 days overdue: dropped, back where it was, unflagged; undo brings
    // it back open; dropping by hand keeps it in Dropped too.
    assert_eq!(engine.later_drop_stale(now).unwrap(), [lena]);
    assert_eq!(subjects(), ["Lunch"]);
    assert!(engine.later_of(id_of(2)).unwrap().is_none());
    assert_eq!(engine.later_dropped_of(id_of(2)).unwrap().unwrap().0, lena);
    assert!(engine.later_undrop(lena).unwrap());
    let (_, back) = engine.later_of(id_of(2)).unwrap().unwrap();
    assert!(back.back);
    assert!(engine.later_dropped().unwrap().is_empty());
    engine.later_drop(lena, now).unwrap();
    assert_eq!(engine.later_dropped().unwrap().len(), 1);
    assert!(engine.later_drop_stale(now + 91 * 86_400).unwrap().len() >= 1, "Priya's is 30 days overdue by then");
    assert!(engine.later_dropped().unwrap().iter().all(|d| d.0 != lena), "dropped 90 days ago: forgotten");

    // The review names what has waited a week and what keeps being pushed;
    // on Friday afternoon the weekly look says what's next week and who is
    // waited on.
    let dir2 = tempfile::tempdir().unwrap();
    let e2 = Engine::new(Store::open(dir2.path()).unwrap(), Parser { program: jail() });
    let a2 = e2.store.add_account("arun@example.org").unwrap();
    let mut s2 = e2.sink(a2);
    s2.folders(&[RemoteFolder { name: "INBOX".into(), role: mail_sync::Role::Inbox, selectable: true }]).unwrap();
    s2.messages("INBOX", vec![
        f(1, msg("Priya <priya@example.com>", me, "Numbers?", 10, "p1@x", "")),
        f(3, msg("Sam <sam@acme.example>", me, "Contract", 12, "s1@x", "")),
        f(4, msg("Dev <dev@acme.example>", me, "Deploy", 13, "d1@x", "")),
    ]).unwrap();
    let in2 = e2.store.folder_id(a2, "INBOX", Role::Inbox).unwrap();
    let id2 = |uid| e2.store.at(in2, uid).unwrap().unwrap().0;
    e2.set_later(id2(1), kind::REPLY, now - 10 * 86_400, "send the numbers").unwrap();
    // Waiting on Sam, set aside over a week before `now` (set_at is real time).
    e2.set_later(id2(3), kind::WAITING, now + 30 * 86_400, "").unwrap();
    for _ in 0..4 {
        e2.set_later(id2(4), kind::DO, tuesday, "").unwrap();
    }
    let mut n = Notifier::new();
    let posts = n.check_local(&e2, now, 0);
    let review = posts.iter().find_map(|p| if let Post::Review { body, .. } = p { Some(body.clone()) } else { None }).unwrap();
    assert_eq!(review, "1 overdue\nOver a week overdue: send the numbers\nPushed back 3 times or more: Deploy");
    let week = posts.iter().find_map(|p| if let Post::Week { body, .. } = p { Some(body.clone()) } else { None }).unwrap();
    assert_eq!(week, "1 due next week: Deploy\nWaiting over a week: Contract");
    assert!(n.check_local(&e2, now + 3600, 0).is_empty(), "once a day, once a week");
    // Turned off: no weekly look.
    let mut p = e2.prefs();
    p.weekly = false;
    e2.set_prefs(&p).unwrap();
    let mut n = Notifier::new();
    assert!(n.check_local(&e2, now, 0).iter().all(|p| !matches!(p, Post::Week { .. })));
}

#[test]
fn a_flag_from_elsewhere_gets_a_date() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("arun@example.org").unwrap();
    let me = "Arun <arun@example.org>";
    let mut sink = engine.sink(account);
    sink.folders(&[
        RemoteFolder { name: "INBOX".into(), role: mail_sync::Role::Inbox, selectable: true },
        RemoteFolder { name: "Trash".into(), role: mail_sync::Role::Trash, selectable: true },
    ]).unwrap();
    // Arrives flagged (a phone flagged it): a Reply item, out of the inbox.
    let mut one = f(1, msg("Priya <priya@example.com>", me, "Numbers?", 10, "p1@x", ""));
    one.flags = vec!["\\Flagged".into()];
    sink.messages("INBOX", vec![one, f(2, msg("Lena <lena@partner.example>", me, "Lunch", 11, "l1@x", ""))]).unwrap();
    let inbox = engine.store.folder_id(account, "INBOX", Role::Inbox).unwrap();
    let id_of = |uid| engine.store.at(inbox, uid).unwrap().unwrap().0;
    let items = engine.later_items().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!((items[0].1.msg, items[0].1.kind), (id_of(1), kind::REPLY));
    let soon = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    assert!(items[0].1.due > soon && items[0].1.due <= soon + 2 * 86_400, "tomorrow morning");
    assert_eq!(engine.store.ops_pending().unwrap(), 0, "already flagged on the server");

    // Flagged later elsewhere: the next flags report makes one too.
    sink.flags("INBOX", &[(2, vec!["\\Flagged".into()])]).unwrap();
    assert!(engine.later_of(id_of(2)).unwrap().is_some_and(|(_, l)| l.kind == kind::REPLY));

    // Done in Mail: the unflag is still to be sent, so a report that still
    // says flagged doesn't bring the item back.
    let (lid, _) = engine.later_of(id_of(2)).unwrap().unwrap();
    engine.later_done(lid).unwrap();
    sink.flags("INBOX", &[(2, vec!["\\Flagged".into()])]).unwrap();
    assert!(engine.later_of(id_of(2)).unwrap().is_none());

    // Flagged in Trash: nothing.
    let mut t = f(1, msg("Sam <sam@acme.example>", me, "Old", 9, "s1@x", ""));
    t.flags = vec!["\\Flagged".into()];
    sink.messages("Trash", vec![t]).unwrap();
    assert_eq!(engine.later_items().unwrap().len(), 1);
}
