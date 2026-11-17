//! Private reply addresses (mail.md §13.3): made per site at the person's
//! own domain, remembered when mail first comes to one, replied from, and
//! turned off (its mail goes to Trash).

use std::path::{Path, PathBuf};
use std::process::Command;

use mail_compose::reply::Kind;
use mail_engine::{Engine, Parser};
use mail_store::Store;
use mail_sync::{Fetched, RemoteFolder, Sink};

fn jail() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-parse/Cargo.toml");
    let st = Command::new(env!("CARGO")).args(["build", "-q", "--bin", "mail-parse-jail", "--manifest-path"]).arg(&manifest).status().unwrap();
    assert!(st.success());
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-parse/target/debug/mail-parse-jail")
}

fn msg(uid: u32, from: &str, to: &str, subject: &str) -> Fetched {
    let bytes = format!("From: {from}\r\nTo: {to}\r\nSubject: {subject}\r\nMessage-ID: <{uid}@x.example>\r\nDate: Mon, 23 Nov 2026 09:00:00 +0000\r\nMIME-Version: 1.0\r\nContent-Type: text/plain\r\n\r\nHi.\r\n").into_bytes();
    Fetched { uid, flags: vec![], size: bytes.len() as u64, internal_date: None, server_id: None, bytes, full: true }
}

#[test]
fn made_remembered_replied_from_and_turned_off() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("arun@example.org").unwrap();
    engine.store.set_identities(account, &[mail_store::Identity { name: "Arun".into(), addr: "arun@example.org".into(), signature: String::new() }]).unwrap();
    let mut sink = engine.sink(account);
    sink.folders(&[
        RemoteFolder { name: "INBOX".into(), role: mail_sync::Role::Inbox, selectable: true },
        RemoteFolder { name: "Trash".into(), role: mail_sync::Role::Trash, selectable: true },
    ]).unwrap();

    // None without a domain; a bad domain is refused.
    assert!(engine.make_private(account, "shop.example", 1).is_err());
    assert!(engine.set_private_domain(account, Some("not a domain")).is_err());
    engine.set_private_domain(account, Some("@Me.Example")).unwrap();
    assert_eq!(engine.private_domain(account).as_deref(), Some("me.example"));
    let shop = engine.make_private(account, "Shop.example", 1).unwrap();
    assert!(shop.starts_with("shop-") && shop.ends_with("@me.example"), "{shop}");
    assert_eq!(engine.make_private(account, "shop.example", 2).unwrap(), shop, "the same one again");

    // Mail to it from the shop; mail to an address never made, remembered for its sender.
    sink.messages("INBOX", vec![
        msg(1, "Shop <orders@mail.shop.example>", &shop, "Your order"),
        msg(2, "Club <hi@club.example>", "club@me.example", "Welcome"),
        msg(3, "Spam <win@spam.example>", &shop, "You won"),
    ]).unwrap();
    let inbox = engine.store.folders(account).unwrap().into_iter().find(|(_, f)| f.role == mail_store::Role::Inbox).unwrap().0;
    let at = |uid| engine.store.at(inbox, uid).unwrap().unwrap().0;
    let c = engine.came_to(at(1)).unwrap();
    assert_eq!((c.addr.as_str(), c.made_for.as_str(), c.leaked, c.off), (shop.as_str(), "shop.example", false, false));
    let club = engine.came_to(at(2)).unwrap();
    assert_eq!((club.addr.as_str(), club.made_for.as_str()), ("club@me.example", "club.example"));
    assert!(engine.came_to(at(3)).unwrap().leaked, "the shop's address, from someone else");
    assert_eq!(engine.private_addresses(account).len(), 2);

    // A reply comes from the address it was sent to.
    let r = engine.compose_reply(at(1), Kind::Reply).unwrap();
    assert_eq!(r.draft.from.addr, shop);
    assert_eq!(r.draft.from.name, "Arun");
    assert_eq!(engine.compose_reply(at(2), Kind::Reply).unwrap().draft.from.addr, "club@me.example");

    // Turned off: the next mail to it goes to Trash on arrival.
    engine.set_private_off(account, &shop, true).unwrap();
    assert_eq!(engine.store.ops_pending().unwrap(), 0);
    sink.messages("INBOX", vec![msg(4, "Spam <win@spam.example>", &shop, "You won again")]).unwrap();
    assert_eq!(engine.store.ops_pending().unwrap(), 1);
    assert!(engine.came_to(at(1)).unwrap().off);
}
