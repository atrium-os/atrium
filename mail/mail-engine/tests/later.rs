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
