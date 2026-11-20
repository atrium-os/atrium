use mail_engine::{Engine, Parser};
use mail_store::Store;
use mail_sync::{Fetched, RemoteFolder, Role, Sink};
use std::{os::unix::process::CommandExt, path::PathBuf};

fn main() {
    let root = PathBuf::from(std::env::args_os().nth(1).expect("fixture directory"));
    let store = std::env::temp_dir().join(format!("prelum-mail-store-{}", std::process::id()));
    let parser = root.join("mail-parse-jail");
    let engine = Engine::new(
        Store::open(&store).unwrap(),
        Parser {
            program: parser.clone(),
        },
    );
    let account = engine.store.add_account("reader@example.test").unwrap();
    let mut sink = engine.sink(account);
    sink.folders(&[RemoteFolder {
        name: "INBOX".into(),
        role: Role::Inbox,
        selectable: true,
    }])
    .unwrap();
    let lines = (1..=75)
        .map(|n| format!("Message body line {n:02}: synthetic print fixture.\r\n"))
        .collect::<String>();
    let bytes = format!("From: Printer Test <sender@example.test>\r\nTo: reader@example.test\r\nSubject: Mail print fixture\r\nDate: Fri, 20 Nov 2026 10:00:00 +0000\r\nMessage-ID: <print-fixture@example.test>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=fixture\r\n\r\n--fixture\r\nContent-Type: text/plain; charset=UTF-8\r\n\r\n{lines}\r\n--fixture\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=synthetic-receipt.bin\r\n\r\nSECRET_ATTACHMENT_PAYLOAD\r\n--fixture--\r\n").into_bytes();
    sink.messages(
        "INBOX",
        vec![Fetched {
            uid: 1,
            flags: vec![],
            size: bytes.len() as u64,
            internal_date: None,
            server_id: None,
            bytes,
            full: true,
        }],
    )
    .unwrap();
    drop(sink);
    drop(engine);
    let err = std::process::Command::new(root.join("atrium-mail"))
        .env("MAIL_STORE", store)
        .env("MAIL_PARSE_JAIL", parser)
        .env("MAIL_TRACE", "1")
        .exec();
    panic!("{err}");
}
