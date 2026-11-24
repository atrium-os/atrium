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

#[test]
fn digest_weekends_replies_and_the_review_time() {
    use mail_engine::prefs::Prefs;
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("arun@example.org").unwrap();
    let mut sink = engine.sink(account);
    sink.folders(&[RemoteFolder { name: "INBOX".into(), role: mail_sync::Role::Inbox, selectable: true }]).unwrap();
    sink.messages("INBOX", vec![msg(1, "Arun <arun@example.org>", "Plan", NOW - 86_400, "Message-ID: <a1@x>\r\n", "Thoughts?")]).unwrap();
    let inbox = engine.store.folder_id(account, "INBOX", Role::Inbox).unwrap();
    let day = NOW.div_euclid(86_400) * 86_400;

    // Lists and automated mail wait for the digest (9:00 and 17:00 by
    // default); one read before then isn't in it.
    assert_eq!(engine.prefs().digest, [540, 1020]);
    let mut n = Notifier::new();
    assert!(n.check_local(&engine, NOW, 0).is_empty());
    sink.messages("INBOX", vec![
        msg(2, "News <news@list.example>", "This week", NOW - 10, "List-Id: <news.list.example>\r\n", "Hi."),
        msg(3, "Shop <orders@shop.example>", "Shipped", NOW - 5, "Auto-Submitted: auto-generated\r\n", "Hi."),
        msg(4, "Forum <forum@lists.example>", "Digest", NOW - 3, "List-Id: <forum.lists.example>\r\n", "Hi."),
    ]).unwrap();
    assert!(n.check_local(&engine, NOW + 60, 0).is_empty());
    engine.store.write(|w| w.set_flags(inbox, 4, flag::SEEN)).unwrap();
    assert!(n.check_local(&engine, day + 16 * 3600, 0).is_empty());
    let p = n.check_local(&engine, day + 17 * 3600 + 30, 0);
    assert_eq!(p, [Post::Summary { title: "Other mail".into(), body: "2 messages, from News, Shop".into() }]);
    assert!(n.check_local(&engine, day + 17 * 3600 + 90, 0).is_empty(), "once");
    // No times: counted only.
    engine.set_prefs(&Prefs { digest: vec![], ..engine.prefs() }).unwrap();
    assert!(engine.prefs().digest.is_empty());
    sink.messages("INBOX", vec![msg(5, "News <news@list.example>", "Next week", day + 17 * 3600 + 100, "List-Id: <news.list.example>\r\n", "Hi.")]).unwrap();
    assert!(n.check_local(&engine, day + 17 * 3600 + 120, 0).is_empty());
    assert!(n.check_local(&engine, day + 86_400 + 9 * 3600 + 30, 0).is_empty());

    // Quiet hours 19:00–09:00 and weekends: a working week. Replies in
    // Arun's own conversation get through when he lets them.
    let p = Prefs { quiet: Some((19 * 60, 9 * 60)), quiet_weekends: true, replies_through: true, ..engine.prefs() };
    engine.set_prefs(&p).unwrap();
    let got = engine.prefs();
    assert!(got.quiet_weekends && got.replies_through);
    assert!(got.is_quiet_on(5, 12 * 60) && got.is_quiet_on(6, 12 * 60) && !got.is_quiet_on(4, 12 * 60) && got.is_quiet_on(4, 20 * 60));
    let sat = day + 5 * 86_400 + 12 * 3600;
    let mut n = Notifier::new();
    assert!(n.check_local(&engine, sat, 0).is_empty());
    sink.messages("INBOX", vec![
        msg(6, "Priya <priya@example.com>", "Weekend", sat - 30, "", "Hi."),
        msg(7, "Lena <lena@partner.example>", "Re: Plan", sat - 20, "In-Reply-To: <a1@x>\r\nReferences: <a1@x>\r\n", "Yes."),
    ]).unwrap();
    let p = n.check_local(&engine, sat + 10, 0);
    assert!(matches!(&p[..], [Post::One { title, .. }] if title == "Re: Plan"), "{p:?}");
    // Monday 9:00: what waited is one summary.
    let p = n.check_local(&engine, day + 7 * 86_400 + 9 * 3600 + 30, 0);
    assert!(p.contains(&Post::Summary { title: "During quiet hours".into(), body: "1 new, 1 from people (Priya)".into() }), "{p:?}");

    // The review at the person's time: 10:30, not 9:00.
    engine.set_prefs(&Prefs { quiet: None, quiet_weekends: false, review: 10 * 60 + 30, ..engine.prefs() }).unwrap();
    assert_eq!(engine.prefs().review, 630);
    let plan = engine.store.at(inbox, 1).unwrap().unwrap().0;
    engine.set_later(plan, mail_store::later::kind::REPLY, day + 8 * 86_400 + 8 * 3600, "").unwrap();
    let mut n = Notifier::new();
    let tue = day + 8 * 86_400;
    assert!(n.check_local(&engine, tue + 10 * 3600, 0).iter().all(|p| !matches!(p, Post::Review { .. })));
    let p = n.check_local(&engine, tue + 10 * 3600 + 31 * 60, 0);
    assert!(p.contains(&Post::Review { title: "Later".into(), body: "1 overdue".into() }), "{p:?}");
}

#[test]
fn the_notification_s_buttons() {
    use mail_engine::notify::{action_of, actions_for, Acted};
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("arun@example.org").unwrap();
    let mut sink = engine.sink(account);
    sink.folders(&[
        RemoteFolder { name: "INBOX".into(), role: mail_sync::Role::Inbox, selectable: true },
        RemoteFolder { name: "Archive".into(), role: mail_sync::Role::Archive, selectable: true },
    ]).unwrap();
    sink.messages("INBOX", vec![
        msg(1, "Priya <priya@example.com>", "Numbers", NOW - 60, "Message-ID: <p1@x>\r\n", "Here."),
        msg(2, "Lena <lena@partner.example>", "Lunch", NOW - 50, "Message-ID: <l1@x>\r\n", "Noon?"),
        msg(3, "Sam <sam@acme.example>", "Contract", NOW - 40, "Message-ID: <s1@x>\r\n", "Signed."),
    ]).unwrap();
    let inbox = engine.store.folder_id(account, "INBOX", Role::Inbox).unwrap();
    let id = |uid| engine.store.at(inbox, uid).unwrap().unwrap().0;

    // The buttons name their message, within Praeco's rules.
    let a = actions_for(id(1));
    assert_eq!(a.iter().map(|(_, l)| *l).collect::<Vec<_>>(), ["Archive", "Mark read", "Reply"]);
    for (i, _) in &a {
        assert!(i.len() <= 32 && i.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.'), "{i}");
        assert_eq!(action_of(i).map(|(m, _)| m), Some(id(1)));
    }
    assert_eq!(action_of(&format!("archive.{}", u64::MAX)), Some((u64::MAX, "archive")));
    assert_eq!(action_of("delete.1"), None);
    assert_eq!(action_of("archive"), None);

    // Archive: out of the inbox, with ops for Undo.
    let Acted::Done { ops, label } = engine.notification_action(id(1), "archive").unwrap() else { panic!() };
    assert!(!ops.is_empty());
    assert_eq!(label, "Message archived");
    assert!(engine.store.list(inbox, 0, 10).unwrap().iter().all(|r| r.subject != "Numbers"));
    // An action Mail never offered: nothing.
    assert!(matches!(engine.notification_action(id(2), "nonsense").unwrap(), Acted::Nothing));

    // Mark read.
    let Acted::Done { label, .. } = engine.notification_action(id(2), "read").unwrap() else { panic!() };
    assert_eq!(label, "Marked read");
    assert!(engine.store.at(inbox, 2).unwrap().unwrap().1 & flag::SEEN != 0);

    // Reply: a reply to Sam, to open in a window.
    let Acted::Reply(c) = engine.notification_action(id(3), "reply").unwrap() else { panic!() };
    assert_eq!(c.draft.subject, "Re: Contract");
    assert_eq!(c.draft.to[0].addr, "sam@acme.example");
    // A message that's gone: nothing.
    assert!(matches!(engine.notification_action(999_999, "archive").unwrap(), Acted::Nothing));
}

fn meeting_mail(uid: u32, sequence: u32, t: i64, method: &str) -> Fetched {
    let calendar = format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:{method}\r\nBEGIN:VEVENT\r\nUID:design@fixture.invalid\r\nSEQUENCE:{sequence}\r\nDTSTART:20261201T150000Z\r\nDTEND:20261201T153000Z\r\nORGANIZER:mailto:mina@fixture.invalid\r\nATTENDEE:mailto:arun@example.org\r\nATTENDEE:mailto:guest{uid}@fixture.invalid\r\nEND:VEVENT\r\nEND:VCALENDAR");
    let mut mail = msg(uid, "Mina <mina@fixture.invalid>", &format!("Design meeting {uid}"), t, "", &calendar);
    mail.bytes = String::from_utf8(mail.bytes).unwrap().replace("Content-Type: text/plain", &format!("Content-Type: text/calendar; method={method}")).into_bytes();
    mail.size = mail.bytes.len() as u64;
    mail
}

#[test]
fn organizer_changes_stay_quiet_and_one_wake_summary_covers_late_pages() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("arun@example.org").unwrap();
    let mut sink = engine.sink(account);
    sink.folders(&[RemoteFolder { name: "INBOX".into(), role: mail_sync::Role::Inbox, selectable: true }]).unwrap();
    sink.messages("INBOX", vec![msg(1, "Mina <mina@fixture.invalid>", "Old mail", NOW, "", "Hello")]).unwrap();
    let mut n = Notifier::new();
    assert!(n.check(&engine, NOW).is_empty());
    for (id, revision, method) in [(2, 3, "REQUEST"), (3, 2, "REQUEST"), (4, 4, "CANCEL"), (5, 0, "REPLY")] {
        sink.messages("INBOX", vec![meeting_mail(id, revision, NOW, method)]).unwrap();
        assert!(n.check(&engine, NOW + i64::from(id)).is_empty());
    }
    let wake = NOW + 3600;
    // Sync may finish after the first post-wake poll. The newest message is
    // just one second old and would ordinarily qualify for a fresh alert.
    assert!(n.check(&engine, wake).is_empty());
    let batch = || (6..26).map(|id| meeting_mail(id, id, wake - i64::from(26-id), "REQUEST")).collect();
    sink.messages("INBOX", batch()).unwrap();
    assert!(matches!(&n.check(&engine, wake + 1)[..], [Post::Summary { title, .. }] if title == "Meeting invitations"));
    sink.messages("INBOX", vec![meeting_mail(26, 26, wake - 1, "CANCEL")]).unwrap();
    assert!(n.check(&engine, wake + 2).is_empty());
    sink.messages("INBOX", batch()).unwrap();
    assert!(n.check(&engine, wake + 3).is_empty());
    let inbox = engine.store.folder_id(account, "INBOX", Role::Inbox).unwrap();
    assert_eq!(engine.store.count(inbox).unwrap(), 26);
    // A second gap while syncing cannot replay a late part of the first backlog.
    sink.messages("INBOX", vec![meeting_mail(28, 27, wake - 1, "REQUEST")]).unwrap();
    assert!(n.check(&engine, wake + 120).is_empty());
    let mut restarted = Notifier::new();
    assert!(restarted.check(&engine, wake + 240).is_empty());
    // A real reply-all conversation still reaches the normal notification sink.
    sink.messages("INBOX", vec![msg(27, "Mina <mina@fixture.invalid>", "Re: Design meeting", wake + 241, "", "Should we invite Lee?")]).unwrap();
    assert!(matches!(&restarted.check(&engine, wake + 241)[..], [Post::One { title, .. }] if title == "Re: Design meeting"));
}
