//! Senders and lists (mail.md §11.3): unsubscribe targets, blocking a
//! list, the newsletter lane and the screener, over mail stored through
//! the engine's own jails.

use std::path::{Path, PathBuf};
use std::process::Command;

use mail_engine::senders::Unsubscribe;
use mail_engine::{Engine, Parser};
use mail_store::senders::list;
use mail_store::{Role, Store};
use mail_sync::{Fetched, RemoteFolder, Sink};

fn jail() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-parse/Cargo.toml");
    let st = Command::new(env!("CARGO")).args(["build", "-q", "--bin", "mail-parse-jail", "--manifest-path"]).arg(&manifest).status().unwrap();
    assert!(st.success());
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-parse/target/debug/mail-parse-jail")
}

fn msg(from: &str, to: &str, subject: &str, date: &str, extra: &str) -> Vec<u8> {
    format!("From: {from}\r\nTo: {to}\r\nSubject: {subject}\r\nDate: {date}\r\n{extra}MIME-Version: 1.0\r\nContent-Type: text/plain\r\n\r\nText.\r\n").into_bytes()
}

fn fetched(uid: u32, bytes: Vec<u8>) -> Fetched {
    Fetched { uid, flags: vec![], size: bytes.len() as u64, internal_date: None, server_id: None, bytes, full: true }
}

const WEEKLY: &str = "List-Id: The Weekly <weekly.list.example>\r\nList-Unsubscribe: <https://list.example/u/1>, <mailto:u@list.example?subject=unsubscribe>\r\nList-Unsubscribe-Post: List-Unsubscribe=One-Click\r\n";

#[test]
fn lists_newsletters_and_the_screener() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("arun@example.org").unwrap();
    let me = "Arun <arun@example.org>";
    let mut sink = engine.sink(account);
    sink.folders(&[
        RemoteFolder { name: "INBOX".into(), role: mail_sync::Role::Inbox, selectable: true },
        RemoteFolder { name: "Sent".into(), role: mail_sync::Role::Sent, selectable: true },
        RemoteFolder { name: "Trash".into(), role: mail_sync::Role::Trash, selectable: true },
    ]).unwrap();
    // Arun has written to Lena: her list mail is not a newsletter.
    sink.messages("Sent", vec![fetched(1, msg(me, "Lena <lena@partner.example>", "Hi", "10 Nov 2026 09:00:00 +0000", ""))]).unwrap();
    sink.messages("INBOX", vec![
        fetched(1, msg("The Weekly <news@list.example>", me, "Issue 1", "10 Nov 2026 10:00:00 +0000", WEEKLY)),
        fetched(2, msg("Lena <lena@partner.example>", me, "Team list", "10 Nov 2026 11:00:00 +0000", "List-Id: <team.partner.example>\r\n")),
        fetched(3, msg("Sam <sam@acme.example>", me, "Hello", "10 Nov 2026 12:00:00 +0000", "")),
        fetched(4, msg("Shop <offers@shop.example>", me, "Sale", "10 Nov 2026 13:00:00 +0000", "List-Unsubscribe: <https://shop.example/unsub>\r\n")),
    ]).unwrap();
    let inbox = engine.store.folder_id(account, "INBOX", Role::Inbox).unwrap();
    let subjects = |ids: &[u64]| ids.iter().map(|m| engine.store.message(*m).unwrap().summary.subject).collect::<Vec<_>>();
    let inbox_subjects = || engine.store.list(inbox, 0, 50).unwrap().into_iter().map(|r| r.subject).collect::<Vec<_>>();

    // The lane: list mail from people Arun hasn't written to, newest first.
    assert_eq!(subjects(&engine.newsletters(account, 0, 50).unwrap()), ["Sale", "Issue 1"]);

    // How to unsubscribe: one press where RFC 8058 allows, else mail, else web.
    let weekly = engine.store.at(inbox, 1).unwrap().unwrap().0;
    let l = engine.list_of(weekly).unwrap().unwrap();
    assert_eq!((l.key.as_str(), l.name.as_str()), ("weekly.list.example", "The Weekly"));
    assert_eq!(l.how, Some(Unsubscribe::OneClick("https://list.example/u/1".into())));
    let shop = engine.store.at(inbox, 4).unwrap().unwrap().0;
    let s = engine.list_of(shop).unwrap().unwrap();
    assert_eq!((s.key.as_str(), s.how.clone()), ("offers@shop.example", Some(Unsubscribe::Web("https://shop.example/unsub".into()))));
    let sam = engine.store.at(inbox, 3).unwrap().unwrap().0;
    assert!(engine.list_of(sam).unwrap().is_none());

    // Blocked: the next issue goes to Trash on arrival; the old one stays.
    engine.set_list(account, &l.key, list::BLOCKED).unwrap();
    sink.messages("INBOX", vec![fetched(5, msg("The Weekly <news@list.example>", me, "Issue 2", "11 Nov 2026 10:00:00 +0000", WEEKLY))]).unwrap();
    assert_eq!(inbox_subjects(), ["Sale", "Hello", "Team list", "Issue 1"]);
    assert_eq!(engine.store.ops_pending().unwrap(), 1);

    // The screener: someone new waits in New senders; Sam is known.
    engine.set_screener(account, true).unwrap();
    let later = "02 Jan 2031 10:00:00 +0000";
    sink.messages("INBOX", vec![
        fetched(6, msg("Stranger <who@new.example>", me, "Hi there", later, "")),
        fetched(7, msg("Sam <sam@acme.example>", me, "Again", later, "")),
        fetched(8, msg("Spam <x@spam.example>", me, "Win", later, "")),
    ]).unwrap();
    assert_eq!(inbox_subjects()[..1], ["Again"]);
    let (new, out) = engine.screened(account).unwrap();
    assert_eq!(subjects(&new).len(), 2);
    assert!(out.is_empty());
    // Accept once: in the inbox, and later mail from them too.
    engine.accept_sender(account, "who@new.example").unwrap();
    assert!(inbox_subjects().contains(&"Hi there".to_string()));
    sink.messages("INBOX", vec![fetched(9, msg("Stranger <who@new.example>", me, "Follow-up", later, ""))]).unwrap();
    assert!(inbox_subjects().contains(&"Follow-up".to_string()));
    // Refuse: under Screened out, still out of the inbox.
    engine.refuse_sender(account, "x@spam.example").unwrap();
    let (new, out) = engine.screened(account).unwrap();
    assert!(new.is_empty());
    assert_eq!(subjects(&out), ["Win"]);
    assert!(!inbox_subjects().contains(&"Win".to_string()));
    // Off: whatever it held goes back.
    engine.set_screener(account, false).unwrap();
    assert!(inbox_subjects().contains(&"Win".to_string()));
    assert!(engine.store.held(account).unwrap().is_empty());
}
