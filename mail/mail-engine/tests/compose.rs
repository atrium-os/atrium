//! Compose starts: identities, reply from the address it was sent to,
//! signatures, `mailto:` links.

use mail_compose::reply::Kind;
use mail_engine::{Engine, Parser};
use mail_schema::{Address, Summary};
use mail_store::{Identity, Incoming, Role, Store};

fn a(name: &str, addr: &str) -> Address {
    Address { name: name.into(), addr: addr.into(), name_looks_like_address: false }
}

#[test]
fn starts() {
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: "/nonexistent".into() });
    let acc = e.store.add_account("priya@example.org").unwrap();
    // No identities set: the account's address, no signature.
    let c = e.compose_new(acc, 0).unwrap();
    assert_eq!(c.draft.from.addr, "priya@example.org");
    assert_eq!(c.draft.text, "");
    e.store
        .set_identities(acc, &[
            Identity { name: "Priya Raman".into(), addr: "priya@example.org".into(), signature: "Priya".into() },
            Identity { name: "Priya (billing)".into(), addr: "billing@example.org".into(), signature: "Billing team\nexample.org".into() },
        ])
        .unwrap();
    let c = e.compose_new(acc, 0).unwrap();
    assert_eq!(c.draft.text, "\n\n-- \nPriya\n");
    // A message sent to the alias: the reply comes from it.
    let inbox = e.store.folder_id(acc, "INBOX", Role::Inbox).unwrap();
    let blob = e.store.put_blob(b"Subject: Invoice\r\n\r\n").unwrap();
    let summary = Summary {
        subject: "Invoice 42".into(),
        date: Some(1_794_936_353),
        message_id: Some("inv42@acme.example".into()),
        from: vec![a("Arun", "arun@acme.example")],
        to: vec![a("", "Billing@example.org")],
        text: Some("Please pay.\r\nThanks".into()),
        ..Default::default()
    };
    let msg = e.store.write(|w| w.put(inbox, acc, 1, Incoming { server_id: None, size: 10, internal_date: None, flags: 0, blob, full: false, summary })).unwrap();
    let mut r = e.compose_reply(msg, Kind::Reply).unwrap();
    assert_eq!(r.identity, 1);
    assert_eq!(r.draft.from.addr, "billing@example.org");
    assert_eq!(r.draft.to[0].addr, "arun@acme.example");
    assert_eq!(r.draft.subject, "Re: Invoice 42");
    assert_eq!(r.draft.text, "\n\n-- \nBilling team\nexample.org\n\n\nOn Tue, 17 Nov 2026 17:25:53 +0000, Arun wrote:\n> Please pay.\n> Thanks\n");
    // It writes: the parser's bare id is taken.
    let w = mail_compose::write(&r.draft, &mail_compose::Stamp { date: 0, random: [1; 16] }).unwrap();
    assert!(String::from_utf8_lossy(&w.bytes).contains("In-Reply-To: <inv42@acme.example>"));
    // Switching From swaps the signature.
    e.switch_identity(&mut r, 0).unwrap();
    assert_eq!(r.draft.from.name, "Priya Raman");
    assert!(r.draft.text.starts_with("\n\n-- \nPriya\n\n\nOn "), "{:?}", r.draft.text);
    // A mailto link: fields filled, notes kept, signature under the body.
    let m = e.compose_mailto(acc, "mailto:arun@acme.example?subject=Hi&body=Hello&bcc=spy@x.example").unwrap();
    assert_eq!(m.draft.to[0].addr, "arun@acme.example");
    assert_eq!(m.draft.bcc[0].addr, "spy@x.example");
    assert_eq!(m.draft.text, "Hello\n\n-- \nPriya\n");
    assert!(m.notes.iter().any(|n| n.contains("hidden copy")));
    assert!(e.compose_mailto(acc, "https://x.example").is_err());
}
