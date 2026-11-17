//! Search (mail.md §11.2): words, operators, prefixes as you type, and
//! attachment text, over mail stored through the engine's own jails.

use std::path::{Path, PathBuf};
use std::process::Command;

use mail_engine::search::parse;
use mail_engine::{Engine, Parser};
use mail_store::{Role, Store};
use mail_sync::{Fetched, RemoteFolder, Sink};

fn jail() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-parse/Cargo.toml");
    let st = Command::new(env!("CARGO")).args(["build", "-q", "--bin", "mail-parse-jail", "--manifest-path"]).arg(&manifest).status().unwrap();
    assert!(st.success());
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-parse/target/debug/mail-parse-jail")
}

fn msg(from: &str, subject: &str, day: u32, body: &str) -> Vec<u8> {
    format!("From: {from}\r\nTo: arun@example.org\r\nSubject: {subject}\r\nDate: {day:02} Nov 2026 10:00:00 +0000\r\nMIME-Version: 1.0\r\nContent-Type: text/plain\r\n\r\n{body}\r\n").into_bytes()
}

#[test]
fn search_finds_by_words_operators_and_files() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("arun@example.org").unwrap();
    let with_file = b"From: Priya <priya@example.com>\r\nTo: arun@example.org\r\nSubject: Numbers\r\nDate: 15 Nov 2026 10:00:00 +0000\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=B\r\n\r\n--B\r\nContent-Type: text/plain\r\n\r\nSee the file.\r\n--B\r\nContent-Type: text/csv\r\nContent-Disposition: attachment; filename=q4.csv\r\n\r\nregion,total\r\nNorthwest,42\r\n--B--\r\n".to_vec();
    let msgs = vec![
        msg("Lena <lena@partner.example>", "Lunch on Friday?", 10, "Shall we try the new place by the river?"),
        msg("Priya <priya@example.com>", "Q4 plan", 12, "The plan for the north region is attached later."),
        with_file,
    ];
    {
        let mut sink = engine.sink(account);
        sink.folders(&[RemoteFolder { name: "INBOX".into(), role: mail_sync::Role::Inbox, selectable: true }]).unwrap();
        let fs = msgs.into_iter().enumerate().map(|(i, bytes)| Fetched { uid: i as u32 + 1, flags: if i == 0 { vec!["\\Seen".into()] } else { vec![] }, size: bytes.len() as u64, internal_date: None, server_id: None, bytes, full: true }).collect();
        sink.messages("INBOX", fs).unwrap();
    }
    let inbox = engine.store.folder_id(account, "INBOX", Role::Inbox).unwrap();
    let ids: Vec<u64> = engine.store.list(inbox, 0, 10).unwrap().into_iter().map(|r| r.msg).collect();
    let subjects = |q: &str, typing: bool| -> Vec<String> { engine.search(&parse(q, typing)).unwrap().into_iter().map(|h| h.subject).collect() };

    assert_eq!(subjects("river", false), ["Lunch on Friday?"]);
    assert_eq!(subjects("from:priya", false), ["Numbers", "Q4 plan"], "newest first");
    assert_eq!(subjects("from:priya@example.com subject:plan", false), ["Q4 plan"]);
    assert_eq!(subjects("is:unread", false), ["Numbers", "Q4 plan"]);
    assert_eq!(subjects("has:attachment", false), ["Numbers"]);
    assert_eq!(subjects("filename:q4.csv", false), ["Numbers"]);
    assert_eq!(subjects("before:2026-11-11", false), ["Lunch on Friday?"]);
    assert_eq!(subjects("after:2026-11-12 in:inbox", false), ["Numbers"]);
    // As you type: "fri" finds Friday; typed out with a space, it doesn't.
    assert_eq!(subjects("fri", true), ["Lunch on Friday?"]);
    assert!(subjects("fri ", true).is_empty());
    // Attachment text is found once taken out.
    assert!(subjects("northwest", false).is_empty());
    for id in ids {
        engine.index_files(id).unwrap();
    }
    assert_eq!(subjects("northwest", false), ["Numbers"]);
    assert_eq!(subjects("nothing-like-this", false), Vec::<String>::new());
}
