//! The person's rules (mail.md §11.6) on new inbox mail.

use std::path::{Path, PathBuf};
use std::process::Command;

use mail_engine::rules::parse_rule;
use mail_engine::{Engine, Parser};
use mail_store::{flag, OpKind, Role, Store};
use mail_sync::{Fetched, RemoteFolder, Sink};

fn jail() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-parse/Cargo.toml");
    let st = Command::new(env!("CARGO")).args(["build", "-q", "--bin", "mail-parse-jail", "--manifest-path"]).arg(&manifest).status().unwrap();
    assert!(st.success());
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-parse/target/debug/mail-parse-jail")
}

fn f(uid: u32, from: &str, to: &str, subject: &str) -> Fetched {
    let bytes = format!("From: {from}\r\nTo: {to}\r\nSubject: {subject}\r\nDate: 1{uid} Nov 2026 10:00:00 +0000\r\nMessage-ID: <r{uid}@x>\r\nMIME-Version: 1.0\r\nContent-Type: text/plain\r\n\r\nText.\r\n").into_bytes();
    Fetched { uid, flags: vec![], size: bytes.len() as u64, internal_date: None, server_id: None, bytes, full: true }
}

#[test]
fn rules_move_flag_read_and_quiet() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("arun@example.org").unwrap();
    let mut sink = engine.sink(account);
    sink.folders(&[
        RemoteFolder { name: "INBOX".into(), role: mail_sync::Role::Inbox, selectable: true },
        RemoteFolder { name: "GitHub".into(), role: mail_sync::Role::Other, selectable: true },
        RemoteFolder { name: "Archive".into(), role: mail_sync::Role::Archive, selectable: true },
    ]).unwrap();
    for t in ["subject:invoice flag", "from:@github.com move github", "to:ops@example.org read", "from:alerts@bank.example quiet", "from:@shop.example move Nowhere"] {
        engine.add_rule(parse_rule(t).unwrap()).unwrap();
    }
    assert_eq!(engine.rules().len(), 5);
    assert_eq!(engine.missing_folder(account, &parse_rule("from:@shop.example move Nowhere").unwrap()), Some("Nowhere".into()));
    assert_eq!(engine.missing_folder(account, &parse_rule("from:a@b.c move GITHUB").unwrap()), None);

    sink.messages("INBOX", vec![
        f(1, "GitHub <noreply@github.com>", "arun@example.org", "[repo] PR merged"),
        f(2, "Billing <billing@acme.example>", "arun@example.org", "Your invoice"),
        f(3, "Pager <pager@acme.example>", "ops@example.org", "Disk full"),
        f(4, "Bank <alerts@bank.example>", "arun@example.org", "Login"),
        f(5, "Shop <hi@shop.example>", "arun@example.org", "Sale"),
        f(6, "Lena <lena@partner.example>", "arun@example.org", "Lunch"),
    ]).unwrap();
    let inbox = engine.store.folder_id(account, "INBOX", Role::Inbox).unwrap();
    let rows = engine.store.list(inbox, 0, 10).unwrap();
    let subjects: Vec<&str> = rows.iter().map(|r| r.subject.as_str()).collect();
    // Moved out at once (the move reaches the server next pass); a folder
    // that isn't there moves nothing.
    assert!(!subjects.contains(&"[repo] PR merged"), "{subjects:?}");
    assert!(subjects.contains(&"Sale") && subjects.contains(&"Lunch"));
    let flags_of = |s: &str| rows.iter().find(|r| r.subject == s).unwrap().flags;
    assert!(flags_of("Your invoice") & flag::FLAGGED != 0);
    assert!(flags_of("Disk full") & flag::SEEN != 0);
    assert_eq!(flags_of("Lunch") & (flag::SEEN | flag::FLAGGED), 0);
    let ops = engine.store.ops_due(account, i64::MAX).unwrap();
    let github = engine.store.folder_id(account, "GitHub", Role::Other).unwrap();
    assert!(ops.iter().any(|(_, o)| o.uid == 1 && o.kind == OpKind::Move { to: github }), "{ops:?}");
    assert_eq!(ops.len(), 3, "{ops:?}");
    // Quiet: no notification for the bank.
    let bank = engine.store.message(rows.iter().find(|r| r.subject == "Login").unwrap().msg).unwrap();
    assert!(engine.rule_quiet(&bank.summary));

    // Mail already here is left alone when a rule is added; new mail isn't.
    engine.add_rule(parse_rule("from:lena@partner.example move Archive").unwrap()).unwrap();
    assert!(engine.store.list(inbox, 0, 10).unwrap().iter().any(|r| r.subject == "Lunch"));
    engine.sink(account).messages("INBOX", vec![f(7, "Lena <lena@partner.example>", "arun@example.org", "Dinner")]).unwrap();
    assert!(!engine.store.list(inbox, 0, 10).unwrap().iter().any(|r| r.subject == "Dinner"));
    // Other folders aren't touched.
    engine.sink(account).messages("Archive", vec![f(8, "GitHub <noreply@github.com>", "arun@example.org", "[repo] old")]).unwrap();
    assert_eq!(engine.store.ops_due(account, i64::MAX).unwrap().len(), 4);
}
