//! Who a sender is (mail.md §7.5): a first message from an address, and a
//! domain that looks like one the person writes to; the same note when
//! replying (§9.1).

use std::path::{Path, PathBuf};
use std::process::Command;

use mail_compose::reply::Kind;
use mail_engine::contact::SenderNote;
use mail_engine::{Engine, Parser};
use mail_store::{Role, Store};
use mail_sync::{Fetched, RemoteFolder, Sink};

fn jail() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-parse/Cargo.toml");
    let st = Command::new(env!("CARGO")).args(["build", "-q", "--bin", "mail-parse-jail", "--manifest-path"]).arg(&manifest).status().unwrap();
    assert!(st.success());
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-parse/target/debug/mail-parse-jail")
}

fn msg(from: &str, to: &str, subject: &str, id: &str) -> Vec<u8> {
    format!("From: {from}\r\nTo: {to}\r\nSubject: {subject}\r\nDate: 10 Nov 2026 10:00:00 +0000\r\nMessage-ID: <{id}>\r\nMIME-Version: 1.0\r\nContent-Type: text/plain\r\n\r\nText.\r\n").into_bytes()
}

fn f(uid: u32, bytes: Vec<u8>) -> Fetched {
    Fetched { uid, flags: vec![], size: bytes.len() as u64, internal_date: None, server_id: None, bytes, full: true }
}

#[test]
fn first_contact_and_look_alikes() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("arun@example.org").unwrap();
    let me = "Arun <arun@example.org>";
    let mut sink = engine.sink(account);
    sink.folders(&[
        RemoteFolder { name: "INBOX".into(), role: mail_sync::Role::Inbox, selectable: true },
        RemoteFolder { name: "Sent".into(), role: mail_sync::Role::Sent, selectable: true },
    ]).unwrap();
    // Arun writes to Priya at example.com.
    sink.messages("Sent", vec![f(1, msg(me, "Priya <priya@example.com>", "Numbers", "a1@x"))]).unwrap();
    sink.messages("INBOX", vec![
        // Priya answers: written to, nothing to say.
        f(1, msg("Priya <priya@example.com>", me, "Re: Numbers", "p1@x")),
        // Someone new at a domain nobody knows: a first contact.
        f(2, msg("Sam <sam@acme.example>", me, "Hello", "s1@x")),
        // Look-alikes of example.com: a digit for a letter, two letters
        // swapped, Cyrillic а.
        f(3, msg("Priya <priya@examp1e.com>", me, "Invoice", "x1@x")),
        f(4, msg("Priya <priya@exmaple.com>", me, "Invoice", "x2@x")),
        f(5, msg("Priya <priya@xn--xample-2of.com>", me, "Invoice", "x3@x")),
        // Arun's own domain looked like.
        f(6, msg("IT <it@examp1e.org>", me, "Password", "x4@x")),
    ]).unwrap();
    let inbox = engine.store.folder_id(account, "INBOX", Role::Inbox).unwrap();
    let id = |uid| engine.store.at(inbox, uid).unwrap().unwrap().0;
    let note = |uid| engine.sender_note(id(uid)).unwrap();

    assert_eq!(note(1), None);
    assert_eq!(note(2), Some(SenderNote::FirstContact));
    assert_eq!(note(3), Some(SenderNote::LookAlike { like: "example.com".into(), is: "examp1e.com".into() }));
    assert_eq!(note(3).unwrap().line(), ("Looks like example.com, but is examp1e.com".into(), true));
    assert_eq!(note(4), Some(SenderNote::LookAlike { like: "example.com".into(), is: "exmaple.com".into() }));
    let n = note(5).unwrap();
    assert!(matches!(&n, SenderNote::LookAlike { like, .. } if like == "example.com"), "{n:?}");
    assert!(n.line().0.ends_with("but is xn--xample-2of.com"), "{n:?}");
    assert_eq!(note(6), Some(SenderNote::LookAlike { like: "example.org".into(), is: "examp1e.org".into() }));

    // Replying shows the same note; forwarding doesn't.
    let c = engine.compose_reply(id(3), Kind::Reply).unwrap();
    assert!(c.notes.contains(&"Looks like example.com, but is examp1e.com".to_string()), "{:?}", c.notes);
    let c = engine.compose_reply(id(2), Kind::Reply).unwrap();
    assert!(c.notes.contains(&"First message from this address".to_string()), "{:?}", c.notes);
    assert!(engine.compose_reply(id(3), Kind::Forward).unwrap().notes.is_empty());
    assert!(engine.compose_reply(id(1), Kind::Reply).unwrap().notes.is_empty());

    // Sam writes again: no longer a first contact. Once Arun writes to
    // acme.example, neither is anything.
    sink.messages("INBOX", vec![f(7, msg("Sam <sam@acme.example>", me, "Again", "s2@x"))]).unwrap();
    assert_eq!(note(2), None);
    assert_eq!(note(7), None);
    // Arun's own mail says nothing about him.
    sink.messages("INBOX", vec![f(8, msg(me, me, "Note to self", "n1@x"))]).unwrap();
    assert_eq!(note(8), None);
}
