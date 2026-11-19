//! Export (mail.md §6.5): a folder, some messages or everything as one
//! mbox file, oldest first, each message once, read back byte for byte.

use std::path::{Path, PathBuf};
use std::process::Command;

use mail_engine::export::{self, What};
use mail_engine::{Engine, Parser};
use mail_store::{Role, Store};
use mail_sync::{Fetched, RemoteFolder, Sink};

fn jail() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-parse/Cargo.toml");
    let st = Command::new(env!("CARGO")).args(["build", "-q", "--bin", "mail-parse-jail", "--manifest-path"]).arg(&manifest).status().unwrap();
    assert!(st.success());
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-parse/target/debug/mail-parse-jail")
}

fn msg(subject: &str, day: u32, id: &str, body: &str) -> Vec<u8> {
    format!("From: Priya <priya@example.com>\r\nTo: arun@example.org\r\nSubject: {subject}\r\nDate: {day:02} Nov 2026 10:00:00 +0000\r\nMessage-ID: <{id}>\r\nMIME-Version: 1.0\r\nContent-Type: text/plain\r\n\r\n{body}\r\n").into_bytes()
}

fn f(uid: u32, bytes: Vec<u8>, full: bool, sid: &str) -> Fetched {
    let bytes = if full { bytes } else { bytes[..bytes.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4].to_vec() };
    Fetched { uid, flags: vec![], size: 500, internal_date: None, server_id: Some(sid.into()), bytes, full }
}

#[test]
fn folders_messages_and_everything() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("arun@example.org").unwrap();
    let mut sink = engine.sink(account);
    sink.folders(&[
        RemoteFolder { name: "INBOX".into(), role: mail_sync::Role::Inbox, selectable: true },
        RemoteFolder { name: "Archive".into(), role: mail_sync::Role::Archive, selectable: true },
        RemoteFolder { name: "All Mail".into(), role: mail_sync::Role::All, selectable: true },
    ]).unwrap();
    let (a, b, c) = (msg("Numbers", 12, "a@x", "From the top:\r\n>From me"), msg("Lunch", 10, "b@x", "Noon?"), msg("Old", 2, "c@x", "Hi"));
    sink.messages("INBOX", vec![f(1, a.clone(), true, "A"), f(2, b.clone(), true, "B")]).unwrap();
    sink.messages("Archive", vec![f(1, c.clone(), false, "C")]).unwrap();
    // The same messages under Gmail's All Mail: once each in "everything".
    sink.messages("All Mail", vec![f(7, a.clone(), true, "A"), f(8, b.clone(), true, "B")]).unwrap();
    let inbox = engine.store.folder_id(account, "INBOX", Role::Inbox).unwrap();
    // Lunch is held by the screener (out of the inbox's list): exported all the same.
    assert!(engine.store.write(|w| w.hold(inbox, 2, mail_store::senders::why::SCREENER)).unwrap());
    assert_eq!(engine.store.count(inbox).unwrap(), 1);

    // A folder: oldest first, read back as it was.
    let ids = export::messages(&engine.store, &What::Folder(inbox)).unwrap();
    let mut out = Vec::new();
    let r = export::mbox(&engine.store, &ids, &mut out).unwrap();
    assert_eq!(r, export::Exported { written: 2, missing: 0 });
    assert_eq!(export::split(&out), vec![b.clone(), a.clone()]);
    assert!(String::from_utf8_lossy(&out).contains("\n>From the top:\n>>From me\n"));

    // Some messages (a search's results), as given.
    let one = vec![engine.store.at(inbox, 1).unwrap().unwrap().0];
    let mut out = Vec::new();
    export::mbox(&engine.store, &export::messages(&engine.store, &What::Messages(one)).unwrap(), &mut out).unwrap();
    assert_eq!(export::split(&out), vec![a.clone()]);

    // Everything: each message once; the headers-only one counted, not written.
    let ids = export::messages(&engine.store, &What::Everything).unwrap();
    assert_eq!(ids.len(), 3);
    let mut out = Vec::new();
    let r = export::mbox(&engine.store, &ids, &mut out).unwrap();
    assert_eq!(r, export::Exported { written: 2, missing: 1 });
    assert_eq!(export::split(&out), vec![b, a]);
}
