//! DKIM and DMARC lookups through the engine (mail.md §7.5): the real jail
//! asks, the engine answers through its `Dns` and keeps the answers, and a
//! check that waited for a key is done again when the message is opened.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;

use mail_engine::dns::Dns;
use mail_engine::{Engine, Parser};
use mail_schema::SenderCheck;
use mail_store::{Role, Store};
use mail_sync::{Fetched, RemoteFolder, Sink};
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair};

fn jail() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-parse/Cargo.toml");
    let st = Command::new(env!("CARGO")).args(["build", "-q", "--bin", "mail-parse-jail", "--manifest-path"]).arg(&manifest).status().unwrap();
    assert!(st.success());
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-parse/target/debug/mail-parse-jail")
}

fn b64(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 { if i <= c.len() { s.push(T[(n >> (18 - 6 * i) & 63) as usize] as char) } else { s.push('=') } }
    }
    s
}

/// An Ed25519-signed message from `from` (simple/simple), selector `sel`.
fn signed(kp: &Ed25519KeyPair, from: &str, subject: &str) -> Vec<u8> {
    let domain = from.rsplit('@').next().unwrap();
    signed_as(kp, from, domain, subject)
}

fn signed_as(kp: &Ed25519KeyPair, from: &str, domain: &str, subject: &str) -> Vec<u8> {
    let headers = format!("From: Priya <{from}>\r\nSubject: {subject}\r\nContent-Type: text/plain\r\n");
    let body = "Hello.\r\n";
    let bh = b64(ring::digest::digest(&ring::digest::SHA256, body.as_bytes()).as_ref());
    let field = format!("DKIM-Signature: v=1; a=ed25519-sha256; c=simple/simple; d={domain}; s=sel; h=from:subject:content-type; bh={bh}; b=");
    let mut data = headers.as_bytes().to_vec();
    data.extend_from_slice(field.as_bytes());
    let sig = kp.sign(ring::digest::digest(&ring::digest::SHA256, &data).as_ref());
    format!("{field}{}\r\n{headers}\r\n{body}", b64(sig.as_ref())).into_bytes()
}

/// DNS as a test says: records per name, or a failed lookup; every name asked.
#[derive(Default)]
struct Answers {
    records: HashMap<String, Vec<String>>,
    failed: HashSet<String>,
    queried: Vec<String>,
}

#[derive(Clone, Default)]
struct Fake(Rc<RefCell<Answers>>);

impl Dns for Fake {
    fn txt(&self, name: &str) -> Result<(Vec<String>, u32), String> {
        let mut s = self.0.borrow_mut();
        s.queried.push(name.to_string());
        if s.failed.contains(name) {
            return Err("timed out".into());
        }
        Ok((s.records.get(name).cloned().unwrap_or_default(), 120))
    }
}

fn fetched(uid: u32, bytes: Vec<u8>) -> Fetched {
    Fetched { uid, flags: vec![], size: bytes.len() as u64, internal_date: None, server_id: None, bytes, full: true }
}

#[test]
fn senders_are_checked_with_lookups_and_answers_kept() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let dns = Fake::default();
    engine.set_dns(Box::new(dns.clone()));
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
    let kp = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
    let rec = format!("v=DKIM1; k=ed25519; p={}", b64(kp.public_key().as_ref()));
    dns.0.borrow_mut().records.insert("sel._domainkey.example.com".into(), vec!["unrelated".into(), rec.clone()]);
    dns.0.borrow_mut().records.insert("_dmarc.example.com".into(), vec!["v=DMARC1; p=reject".into()]);

    let account = engine.store.add_account("ada@example.org").unwrap();
    let mut sink = engine.sink(account);
    sink.folders(&[RemoteFolder { name: "INBOX".into(), role: mail_sync::Role::Inbox, selectable: true }]).unwrap();
    let inbox = engine.store.folder_id(account, "INBOX", Role::Inbox).unwrap();
    let check = |uid: u32| engine.store.message(engine.store.at(inbox, uid).unwrap().unwrap().0).unwrap().summary.sender_check;

    // Two messages from one sender: one lookup of each name, for both.
    sink.messages("INBOX", vec![fetched(1, signed(&kp, "priya@example.com", "One")), fetched(2, signed(&kp, "priya@example.com", "Two"))]).unwrap();
    assert_eq!(check(1), SenderCheck::Verified { domain: "example.com".into(), by_provider: false });
    assert_eq!(check(2), check(1));
    assert_eq!(engine.dns_lookups(), 2, "{:?}", dns.0.borrow().queried);

    // A domain with no key published: not verified (not pending forever).
    sink.messages("INBOX", vec![fetched(3, signed(&kp, "x@nokey.example", "Three"))]).unwrap();
    assert_eq!(check(3), SenderCheck::NotVerified);

    // The resolver doesn't answer: pending, and checked again on opening.
    dns.0.borrow_mut().failed.insert("sel._domainkey.later.example".into());
    let other = Ed25519KeyPair::from_pkcs8(Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap().as_ref()).unwrap();
    sink.messages("INBOX", vec![fetched(4, signed(&other, "bob@later.example", "Four"))]).unwrap();
    assert_eq!(check(4), SenderCheck::Pending);
    dns.0.borrow_mut().failed.clear();
    dns.0.borrow_mut().records.insert("sel._domainkey.later.example".into(), vec![format!("v=DKIM1; k=ed25519; p={}", b64(other.public_key().as_ref()))]);
    let id4 = engine.store.at(inbox, 4).unwrap().unwrap().0;
    let r = engine.open(id4).unwrap();
    assert!(matches!(r.summary.sender_check, SenderCheck::Verified { .. }));
    assert!(matches!(check(4), SenderCheck::Verified { .. }), "kept");
    // Opened again: from what was kept, with no lookup.
    let n = engine.dns_lookups();
    engine.open(id4).unwrap();
    assert_eq!(engine.dns_lookups(), n);
}

#[test]
fn relaxed_alignment_waits_for_dmarc_policy() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let dns = Fake::default();
    engine.set_dns(Box::new(dns.clone()));
    let kp = Ed25519KeyPair::from_pkcs8(
        Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap().as_ref(),
    ).unwrap();
    let key = format!("v=DKIM1; k=ed25519; p={}", b64(kp.public_key().as_ref()));
    let child = "_dmarc.mail.example.com";
    let parent = "_dmarc.example.com";
    {
        let mut answers = dns.0.borrow_mut();
        answers.records.insert("sel._domainkey.example.com".into(), vec![key.clone()]);
        answers.records.insert("sel._domainkey.mail.example.com".into(), vec![key]);
        answers.failed.insert(child.into());
    }

    let account = engine.store.add_account("ada@example.org").unwrap();
    let mut sink = engine.sink(account);
    sink.folders(&[RemoteFolder {
        name: "INBOX".into(),
        role: mail_sync::Role::Inbox,
        selectable: true,
    }]).unwrap();
    let inbox = engine.store.folder_id(account, "INBOX", Role::Inbox).unwrap();
    let from = "priya@mail.example.com";
    sink.messages("INBOX", vec![
        fetched(1, signed_as(&kp, from, "example.com", "Needs relaxed alignment")),
        fetched(2, signed(&kp, from, "Exact alignment")),
    ]).unwrap();
    let first = engine.store.at(inbox, 1).unwrap().unwrap().0;
    let second = engine.store.at(inbox, 2).unwrap().unwrap().0;
    assert_eq!(engine.store.message(first).unwrap().summary.sender_check, SenderCheck::Pending);
    assert_eq!(engine.store.message(second).unwrap().summary.sender_check, SenderCheck::Verified {
        domain: "mail.example.com".into(),
        by_provider: false,
    });
    assert!(!dns.0.borrow().queried.iter().any(|name| name == parent));

    // Once absence is confirmed at the From domain, a failed organizational
    // lookup still leaves the alignment policy unknown.
    {
        let mut answers = dns.0.borrow_mut();
        answers.failed.remove(child);
        answers.failed.insert(parent.into());
    }
    assert_eq!(engine.open(first).unwrap().summary.sender_check, SenderCheck::Pending);
    assert!(dns.0.borrow().queried.iter().any(|name| name == parent));

    // Retry on the next open, enforcing the now-known strict policy.
    {
        let mut answers = dns.0.borrow_mut();
        answers.failed.remove(parent);
        answers.records.insert(parent.into(), vec!["v=DMARC1; p=reject; adkim=s".into()]);
    }
    assert_eq!(engine.open(first).unwrap().summary.sender_check, SenderCheck::NotVerified);
    assert_eq!(engine.store.message(first).unwrap().summary.sender_check, SenderCheck::NotVerified);
}

#[test]
fn inbox_import_round_trip_does_not_establish_provider_provenance() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("reader@example.org").unwrap();
    let mut sink = engine.sink(account);
    sink.folders(&[RemoteFolder {
        name: "INBOX".into(),
        role: mail_sync::Role::Inbox,
        selectable: true,
    }]).unwrap();
    let inbox = engine.store.folder_id(account, "INBOX", Role::Inbox).unwrap();
    let mbox = b"From sender@example.com Thu Jan  1 00:00:00 1970\r\n\
        Authentication-Results: edge.example; dmarc=pass header.from=example.com\r\n\
        From: sender@example.com\r\nSubject: Archive import\r\n\r\nHello\r\n";
    assert_eq!(engine.import_mbox(inbox, mbox.as_slice()).unwrap().queued, 1);
    let mut uploaded = Vec::new();
    assert_eq!(engine.upload_imports(account, &mut |folder, raw| {
        assert_eq!(folder, "INBOX");
        uploaded.extend_from_slice(raw);
        Ok(())
    }).unwrap(), 1);
    assert!(uploaded.starts_with(b"Authentication-Results: edge.example;"));
    sink.messages("INBOX", vec![fetched(1, uploaded)]).unwrap();

    let id = engine.store.at(inbox, 1).unwrap().unwrap().0;
    assert_eq!(engine.store.message(id).unwrap().summary.sender_check, SenderCheck::NotVerified);
    assert_eq!(engine.open(id).unwrap().summary.sender_check, SenderCheck::NotVerified);
}

#[test]
fn a_pending_verdict_is_rechecked_even_with_a_cached_document() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    engine.set_dns(Box::new(Fake::default()));
    let account = engine.store.add_account("reader@example.org").unwrap();
    let mut sink = engine.sink(account);
    sink.folders(&[RemoteFolder {
        name: "INBOX".into(),
        role: mail_sync::Role::Inbox,
        selectable: true,
    }]).unwrap();
    let inbox = engine.store.folder_id(account, "INBOX", Role::Inbox).unwrap();
    let raw = b"From: sender@example.com\r\nSubject: Example\r\n\r\nHello\r\n";
    sink.messages("INBOX", vec![fetched(1, raw.to_vec())]).unwrap();

    let id = engine.store.at(inbox, 1).unwrap().unwrap().0;
    engine.open(id).unwrap();
    assert!(engine.store.message(id).unwrap().document.is_some());
    engine.store.write(|w| w.set_sender_check(id, SenderCheck::Pending)).unwrap();

    assert_eq!(engine.open(id).unwrap().summary.sender_check, SenderCheck::NotVerified);
    assert_eq!(engine.store.message(id).unwrap().summary.sender_check, SenderCheck::NotVerified);
}
