//! OpenPGP through the engine (mail.md §10.2): keys learned from
//! `Autocrypt:` by the real jail, signatures checked when read, and
//! PGP/MIME encryption to people with keys.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;

use mail_compose::{Draft, Mailbox};
use mail_engine::pgp::Scheme;
use mail_engine::smime::Protect;
use mail_engine::{Engine, Parser};
use mail_schema::PgpState;
use mail_store::{Role, Store};
use mail_sync::{Fetched, RemoteFolder, Sink};
use pgp::composed::{ArmorOptions, DetachedSignature, EncryptionCaps, KeyType, Message, SecretKeyParamsBuilder, SignedSecretKey, SubkeyParamsBuilder};
use pgp::crypto::{ecc_curve::ECCCurve, hash::HashAlgorithm};
use pgp::ser::Serialize;
use pgp::types::Password;

fn jail() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-parse/Cargo.toml");
    let st = Command::new(env!("CARGO")).args(["build", "-q", "--bin", "mail-parse-jail", "--manifest-path"]).arg(&manifest).status().unwrap();
    assert!(st.success());
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-parse/target/debug/mail-parse-jail")
}

fn key(uid: &str) -> SignedSecretKey {
    let mut p = SecretKeyParamsBuilder::default();
    p.key_type(KeyType::Ed25519Legacy).can_certify(true).can_sign(true).primary_user_id(uid.into())
        .subkeys(vec![SubkeyParamsBuilder::default().key_type(KeyType::ECDH(ECCCurve::Curve25519Legacy)).can_encrypt(EncryptionCaps::All).build().unwrap()]);
    p.build().unwrap().generate(rand::thread_rng()).unwrap()
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

/// A PGP/MIME signed message from `from`, with `autocrypt` (a public key) if given.
fn signed(sk: &SignedSecretKey, from: &str, autocrypt: Option<&[u8]>, text: &str) -> Vec<u8> {
    let content = format!("Content-Type: text/plain; charset=utf-8\r\n\r\n{text}");
    let sig = DetachedSignature::sign_binary_data(rand::thread_rng(), &sk.primary_key, &Password::empty(), HashAlgorithm::Sha256, content.as_bytes()).unwrap();
    let sig = sig.to_armored_string(ArmorOptions::default()).unwrap().replace("\r\n", "\n").replace('\n', "\r\n");
    let ac = autocrypt.map(|k| {
        let lines: String = b64(k).as_bytes().chunks(76).map(|c| format!(" {}\r\n", std::str::from_utf8(c).unwrap())).collect();
        format!("Autocrypt: addr={from}; keydata=\r\n{lines}")
    }).unwrap_or_default();
    format!(
        "From: <{from}>\r\nTo: ada@example.org\r\nSubject: Numbers\r\nDate: Tue, 17 Nov 2026 10:00:00 +0000\r\n{ac}MIME-Version: 1.0\r\n\
Content-Type: multipart/signed; micalg=pgp-sha256; protocol=\"application/pgp-signature\"; boundary=\"b1\"\r\n\r\n\
--b1\r\n{content}\r\n--b1\r\nContent-Type: application/pgp-signature\r\n\r\n{sig}\r\n--b1--\r\n"
    ).into_bytes()
}

fn fetched(uid: u32, bytes: Vec<u8>) -> Fetched {
    Fetched { uid, flags: vec![], size: bytes.len() as u64, internal_date: None, server_id: None, bytes, full: true }
}

#[test]
fn keys_learned_signatures_checked_and_mail_encrypted() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("ada@example.org").unwrap();
    let mut sink = engine.sink(account);
    sink.folders(&[
        RemoteFolder { name: "INBOX".into(), role: mail_sync::Role::Inbox, selectable: true },
        RemoteFolder { name: "Sent".into(), role: mail_sync::Role::Sent, selectable: true },
    ]).unwrap();
    let inbox = engine.store.folder_id(account, "INBOX", Role::Inbox).unwrap();

    let priya = key("Priya Nair <priya@example.com>");
    let priya_pub = priya.to_public_key().to_bytes().unwrap();
    let ada = key("Ada <ada@example.org>");
    let ada_pub = ada.to_public_key().to_bytes().unwrap();
    let mallory = key("Ada <ada@example.org>");
    let bob = key("Bob <bob@example.net>");

    // Priya's key comes with her signed mail, and her signature checks out.
    sink.messages("INBOX", vec![fetched(1, signed(&priya, "priya@example.com", Some(&priya_pub), "The numbers are in."))]).unwrap();
    assert_eq!(engine.pgp_keys_for("Priya@Example.com"), vec![priya_pub.clone()]);
    let id = engine.store.at(inbox, 1).unwrap().unwrap().0;
    let r = engine.open(id).unwrap();
    assert_eq!(r.pgp.as_ref().unwrap().state, PgpState::Valid);
    assert_eq!(r.text.as_deref().map(str::trim), Some("The numbers are in."));

    // Someone claiming to be Ada, in her inbox, can't give her a key.
    sink.messages("INBOX", vec![fetched(2, signed(&mallory, "ada@example.org", Some(&mallory.to_public_key().to_bytes().unwrap()), "Trust me."))]).unwrap();
    assert!(engine.pgp_keys_for("ada@example.org").is_empty());

    // Bob sends no key: no key to check with, until Ada gives it.
    sink.messages("INBOX", vec![fetched(3, signed(&bob, "bob@example.net", None, "Hello."))]).unwrap();
    let id3 = engine.store.at(inbox, 3).unwrap().unwrap().0;
    assert_eq!(engine.open(id3).unwrap().pgp.unwrap().state, PgpState::NoKey);
    let armored = bob.to_public_key().to_armored_bytes(ArmorOptions::default()).unwrap();
    let info = engine.give_pgp_key(&armored, 100).unwrap();
    assert_eq!(info.emails, vec!["bob@example.net".to_string()]);
    assert_eq!(engine.open(id3).unwrap().pgp.unwrap().state, PgpState::Valid, "parsed again with the key");
    assert!(engine.give_pgp_key(b"not a key", 100).is_err());

    // Encrypting to Priya: OpenPGP, since she has a key and no certificate.
    let draft = Draft { from: Mailbox::new("Ada", "ada@example.org"), to: vec![Mailbox::new("", "priya@example.com")], subject: "Plan".into(), text: "The plan.".into(), ..Draft::default() };
    assert_eq!(engine.encryption_for(&draft), Ok(Scheme::OpenPgp));
    let mut more = draft.clone();
    more.cc = vec![Mailbox::new("", "carol@example.net")];
    assert_eq!(engine.encryption_for(&more), Err(vec!["carol@example.net".to_string()]));
    // Not without a key of Ada's own, for the copy she keeps.
    let e = engine.send_protected(account, &draft, Default::default(), Protect { sign: None, encrypt: true }).unwrap_err();
    assert!(e.to_string().contains("no OpenPGP key of yours"), "{e}");
    // Her key, from her own Sent mail.
    sink.messages("Sent", vec![fetched(1, signed(&ada, "ada@example.org", Some(&ada_pub), "Sent."))]).unwrap();
    assert_eq!(engine.my_pgp_key(account).unwrap().emails, vec!["ada@example.org".to_string()]);

    engine.send_protected(account, &draft, Default::default(), Protect { sign: None, encrypt: true }).unwrap();
    let (_, o) = engine.store.sends_due(i64::MAX).unwrap().into_iter().next().unwrap();
    let sent = String::from_utf8(engine.store.blob(&o.message).unwrap()).unwrap();
    assert!(sent.contains("Subject: Plan\r\n") && sent.contains("Content-Type: multipart/encrypted; protocol=\"application/pgp-encrypted\""), "{sent}");
    assert!(!sent.contains("The plan."));
    let a = sent.find("-----BEGIN PGP MESSAGE-----").unwrap();
    let b = sent.find("-----END PGP MESSAGE-----").unwrap() + "-----END PGP MESSAGE-----".len();
    for sk in [&priya, &ada] {
        let (m, _) = Message::from_armor(Cursor::new(sent[a..b].as_bytes())).unwrap();
        let mut d = m.decrypt(&Password::empty(), sk).unwrap();
        if d.is_compressed() { d = d.decompress().unwrap(); }
        let inner = String::from_utf8(d.as_data_vec().unwrap()).unwrap();
        assert!(inner.starts_with("Content-Type: text/plain") && inner.contains("The plan."), "{inner}");
    }
    // And the parser sees it as PGP/MIME encrypted.
    let o = engine.parser.open(sent.as_bytes(), vec![], &[], None, vec![]).unwrap().unwrap();
    assert!(o.pgp_encrypted);
}
