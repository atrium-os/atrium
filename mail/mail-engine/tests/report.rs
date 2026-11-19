//! Report phishing to the company (mail.md §7.7): the message goes to the
//! report address attached as it arrived, through the outbox, so Undo
//! takes it back.

use std::path::{Path, PathBuf};
use std::process::Command;

use mail_engine::{Engine, Parser};
use mail_store::{Role, Store};
use mail_sync::{Fetched, RemoteFolder, Sink};

fn jail() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-parse/Cargo.toml");
    let st = Command::new(env!("CARGO")).args(["build", "-q", "--bin", "mail-parse-jail", "--manifest-path"]).arg(&manifest).status().unwrap();
    assert!(st.success());
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-parse/target/debug/mail-parse-jail")
}

#[test]
fn a_report_goes_to_the_company() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("arun@acme.example").unwrap();
    let phish = b"From: IT Desk <it@acme-support.example>\r\nTo: arun@acme.example\r\nSubject: Your password expires today\r\nDate: 18 Nov 2026 10:00:00 +0000\r\nMessage-ID: <p1@acme-support.example>\r\nReceived: from mx.acme-support.example\r\n\r\nSign in here.\r\n".to_vec();
    let head = phish[..phish.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4].to_vec();
    let mut sink = engine.sink(account);
    sink.folders(&[RemoteFolder { name: "INBOX".into(), role: mail_sync::Role::Inbox, selectable: true }]).unwrap();
    sink.messages("INBOX", vec![
        Fetched { uid: 1, flags: vec![], size: 300, internal_date: None, server_id: None, bytes: phish.clone(), full: true },
        Fetched { uid: 2, flags: vec![], size: 300, internal_date: None, server_id: None, bytes: [&head[..20], b"\r\nSubject: Second\r\nMessage-ID: <p2@x>\r\n\r\n"].concat(), full: false },
    ]).unwrap();
    let inbox = engine.store.folder_id(account, "INBOX", Role::Inbox).unwrap();
    let (one, two) = (engine.store.at(inbox, 1).unwrap().unwrap().0, engine.store.at(inbox, 2).unwrap().unwrap().0);

    // Only headers here: nothing to attach, nothing sent.
    assert_eq!(engine.report_phishing(&[two], "phish@acme.example").unwrap(), None);
    // Not an address: refused.
    assert!(engine.report_phishing(&[one], "phish").is_err());

    let id = engine.report_phishing(&[one, two], "Phish@Acme.example").unwrap().unwrap();
    let out = engine.store.outbox().unwrap();
    let (_, o) = out.iter().find(|(k, _)| *k == id).unwrap();
    assert_eq!((o.from.as_str(), o.rcpts.as_slice()), ("arun@acme.example", &["phish@acme.example".to_string()][..]));
    let raw = String::from_utf8(engine.store.blob(&o.message).unwrap()).unwrap();
    assert!(raw.contains("Subject: Phishing report: Your password expires today\r\n"), "{raw}");
    // The message as it arrived, headers and all.
    assert!(raw.contains("Content-Type: message/rfc822; name=\"message.eml\""), "{raw}");
    assert!(raw.contains("Received: from mx.acme-support.example\r\n\r\nSign in here.\r\n"), "{raw}");
    assert!(!raw.contains("message-2.eml"), "{raw}");
    // Undo takes it back.
    assert!(engine.undo_send(id).unwrap().is_some());
    assert!(engine.store.outbox().unwrap().iter().all(|(k, _)| *k != id));
}
