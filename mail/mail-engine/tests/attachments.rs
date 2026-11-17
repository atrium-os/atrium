//! Attachments (mail.md §8): listed with the bytes' first part, decoded in
//! a jail on request, their text kept for search, a saved copy's origin.

use std::path::{Path, PathBuf};
use std::process::Command;

use mail_engine::{Engine, Parser};
use mail_schema::Which;
use mail_store::{Role, Store};
use mail_sync::{Fetched, RemoteFolder, Sink};

fn jail() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-parse/Cargo.toml");
    let st = Command::new(env!("CARGO")).args(["build", "-q", "--bin", "mail-parse-jail", "--manifest-path"]).arg(&manifest).status().unwrap();
    assert!(st.success());
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-parse/target/debug/mail-parse-jail")
}

const MSG: &str = "From: Priya <priya@example.com>\r\nTo: arun@example.org\r\nSubject: numbers\r\nDate: Sun, 15 Nov 2026 10:00:00 +0000\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=B\r\n\r\n--B\r\nContent-Type: text/plain\r\n\r\nAttached.\r\n--B\r\nContent-Type: text/csv; name=q4.csv\r\nContent-Disposition: attachment; filename=q4.csv\r\nContent-Transfer-Encoding: base64\r\n\r\ncmVnaW9uLHRvdGFsCk5vcnRoLDQyCg==\r\n--B\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=setup.exe\r\nContent-Transfer-Encoding: base64\r\n\r\nTVqQAAMAAAAEAAAA//8AALgAAAA=\r\n--B--\r\n";

#[test]
fn attachments_are_listed_decoded_and_indexed() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("arun@example.org").unwrap();
    {
        let mut sink = engine.sink(account);
        sink.folders(&[RemoteFolder { name: "INBOX".into(), role: mail_sync::Role::Inbox, selectable: true }]).unwrap();
        let f = Fetched { uid: 1, flags: vec![], size: MSG.len() as u64, internal_date: None, server_id: None, bytes: MSG.as_bytes().to_vec(), full: true };
        sink.messages("INBOX", vec![f]).unwrap();
    }
    let inbox = engine.store.folder_id(account, "INBOX", Role::Inbox).unwrap();
    let msg = engine.store.list(inbox, 0, 10).unwrap()[0].msg;

    let r = engine.open(msg).unwrap();
    let names: Vec<&str> = r.attachments.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["q4.csv", "setup.exe"]);
    assert_eq!(&r.attachments[1].head[..2], b"MZ", "the type comes from the bytes");
    // Kept: a second opening lists them without a jail.
    assert_eq!(engine.open(msg).unwrap().attachments, r.attachments);

    let a = engine.attachment(msg, &r.attachments[0].which).unwrap();
    assert_eq!(a.bytes, b"region,total\nNorth,42\n");
    assert!(engine.attachment(msg, &Which::Part("9".into())).is_err());

    let text = engine.index_files(msg).unwrap().unwrap();
    assert!(text.contains("q4.csv\nregion,total\nNorth,42") && text.contains("setup.exe"), "{text:?}");
    assert_eq!(engine.files_text(msg).unwrap().as_deref(), Some(text.as_str()));
    assert_eq!(engine.origin(msg).unwrap(), "mail: from priya@example.com, 2026-11-15");
}
