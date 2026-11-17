//! S/MIME through the engine (mail.md §10.1): signatures checked by the
//! real jail against the roots the engine holds, and the signer's
//! certificate kept for encrypting back.

use std::path::{Path, PathBuf};
use std::process::Command;

use mail_engine::{smime, Engine, Parser};
use mail_schema::SmimeState;
use mail_store::{Role, Store};
use mail_sync::{Fetched, RemoteFolder, Sink};

fn jail() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-parse/Cargo.toml");
    let st = Command::new(env!("CARGO")).args(["build", "-q", "--bin", "mail-parse-jail", "--manifest-path"]).arg(&manifest).status().unwrap();
    assert!(st.success());
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-parse/target/debug/mail-parse-jail")
}

fn data(f: &str) -> Vec<u8> {
    std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-smime/tests/data").join(f)).unwrap()
}

fn fetched(uid: u32, bytes: Vec<u8>, full: bool) -> Fetched {
    let bytes = if full { bytes } else { let s = String::from_utf8(bytes).unwrap(); s[..s.find("\n\n").unwrap() + 2].as_bytes().to_vec() };
    Fetched { uid, flags: vec![], size: bytes.len() as u64, internal_date: None, server_id: None, bytes, full }
}

#[test]
fn signed_mail_is_checked_and_its_certificate_kept() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    // The fixture's root, as a PEM file would give it.
    let ca = data("ca.der");
    let pem = format!("junk\n-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n", base64(&ca));
    assert_eq!(smime::pem_certs(&pem), vec![ca.clone()]);
    engine.set_roots(smime::pem_certs(&pem));

    let account = engine.store.add_account("ada@example.org").unwrap();
    let mut sink = engine.sink(account);
    sink.folders(&[RemoteFolder { name: "INBOX".into(), role: mail_sync::Role::Inbox, selectable: true }]).unwrap();
    // First the headers only (the staged first sync): nothing checked yet.
    sink.messages("INBOX", vec![fetched(1, data("signed.eml"), false)]).unwrap();
    let inbox = engine.store.folder_id(account, "INBOX", Role::Inbox).unwrap();
    let id = engine.store.at(inbox, 1).unwrap().unwrap().0;
    assert!(engine.store.message(id).unwrap().summary.smime.is_none());
    assert!(engine.cert_for("grace@example.net").is_none());
    // Then the whole message.
    sink.messages("INBOX", vec![fetched(1, data("signed.eml"), true)]).unwrap();
    let m = engine.store.message(id).unwrap();
    assert_eq!(m.summary.smime.as_ref().unwrap().state, SmimeState::Valid);
    let c = engine.cert_for("Grace@Example.net").expect("kept");
    assert_eq!(c.der, data("leaf.der"));
    assert_eq!(c.name, "Grace Hopper");
    assert!(smime::can_encrypt_to(&engine.store, &["grace@example.net".into()]));
    assert!(!smime::can_encrypt_to(&engine.store, &["grace@example.net".into(), "bob@example.net".into()]));

    // Reading it shows who signed.
    let r = engine.open(id).unwrap();
    assert_eq!(r.summary.smime.unwrap().signer.unwrap().emails, vec!["grace@example.net".to_string()]);
    assert!(r.attachments.is_empty());

    // From someone else, the same signature keeps nothing.
    let forged = String::from_utf8(data("signed.eml")).unwrap().replace("<grace@example.net>", "<ceo@example.net>").into_bytes();
    let mut sink = engine.sink(account);
    sink.messages("INBOX", vec![fetched(2, forged, true)]).unwrap();
    let id2 = engine.store.at(inbox, 2).unwrap().unwrap().0;
    assert!(matches!(engine.store.message(id2).unwrap().summary.smime.unwrap().state, SmimeState::WrongAddress(_)));
    assert!(engine.cert_for("ceo@example.net").is_none());
}

fn base64(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for (i, c) in data.chunks(3).enumerate() {
        if i > 0 && i % 16 == 0 { s.push('\n') }
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for k in 0..4 { if k <= c.len() { s.push(T[(n >> (18 - 6 * k) & 63) as usize] as char) } else { s.push('=') } }
    }
    s
}

/// Sent signed and encrypted to a person whose certificate was seen: the
/// copy decrypts with their key, and the jail then finds Grace's signature.
#[test]
fn sent_signed_and_encrypted() {
    use mail_compose::{Draft, Mailbox};
    use mail_engine::smime::{Protect, Signer};
    use rsa::pkcs8::DecodePrivateKey;
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    engine.set_roots(vec![data("ca.der")]);
    let account = engine.store.add_account("grace@example.net").unwrap();
    let mut sink = engine.sink(account);
    sink.folders(&[RemoteFolder { name: "INBOX".into(), role: mail_sync::Role::Inbox, selectable: true }]).unwrap();
    sink.messages("INBOX", vec![fetched(1, data("signed.eml"), true)]).unwrap();

    let draft = Draft { from: Mailbox::new("Grace Hopper", "grace@example.net"), to: vec![Mailbox::new("", "grace@example.net")], subject: "Secret".into(), text: "The plan.".into(), ..Draft::default() };
    let key = rsa::RsaPrivateKey::from_pkcs8_der(&data("leaf.p8")).unwrap();
    let signer = |key: rsa::RsaPrivateKey| Signer { cert: data("leaf.der"), chain: vec![], sign: Box::new(move |d: &[u8; 32]| key.sign(rsa::Pkcs1v15Sign::new::<sha2::Sha256>(), d).map_err(|e| e.to_string())) };

    // Someone with no certificate can't be encrypted to.
    let mut other = draft.clone();
    other.cc = vec![Mailbox::new("", "bob@example.net")];
    assert_eq!(engine.missing_certs(&other), vec!["bob@example.net".to_string()]);
    let e = engine.send_protected(account, &other, Default::default(), Protect { sign: None, encrypt: true }).unwrap_err();
    assert!(e.to_string().contains("bob@example.net"), "{e}");

    engine.send_protected(account, &draft, Default::default(), Protect { sign: Some(signer(key.clone())), encrypt: true }).unwrap();
    let (_, o) = engine.store.sends_due(i64::MAX).unwrap().into_iter().next().unwrap();
    let sent = engine.store.blob(&o.message).unwrap();
    let text = String::from_utf8_lossy(&sent).to_string();
    assert!(text.contains("Subject: Secret\r\n") && text.contains("smime-type=enveloped-data"), "{text}");
    assert!(!text.contains("The plan."));

    // Grace reads it: unwrap with her key, decrypt, and the inside is signed.
    let (outer, body) = text.split_once("\r\n\r\n").unwrap();
    let der: Vec<u8> = b64d(body);
    let ed = mail_smime::envelope::enveloped_data(&der).unwrap();
    let cek = key.decrypt(rsa::Pkcs1v15Encrypt, &mail_smime::envelope::wrapped_key(&ed, &data("leaf.der")).unwrap()).unwrap();
    let inner = mail_smime::envelope::decrypt(&ed, &cek).unwrap();
    let outer: String = outer.lines().filter(|l| !l.starts_with("Content-") && !l.starts_with(' ')).map(|l| format!("{l}\r\n")).collect();
    let mut opened = outer.into_bytes();
    opened.extend_from_slice(&inner);
    let o = engine.parser.open(&opened, vec![], &[data("ca.der")], None).unwrap().unwrap();
    assert_eq!(o.summary.smime.unwrap().state, SmimeState::Valid);
    assert_eq!(o.summary.text.as_deref().map(str::trim), Some("The plan."));
}

fn b64d(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes() {
        let v = match c { b'A'..=b'Z' => c - b'A', b'a'..=b'z' => c - b'a' + 26, b'0'..=b'9' => c - b'0' + 52, b'+' => 62, b'/' => 63, _ => continue } as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 { bits -= 8; out.push((acc >> bits) as u8) }
    }
    out
}

/// An encrypted message is unlocked with a key unwrapped outside (here in
/// software, as the token would), read each time, and nothing of its
/// inside is kept or indexed.
#[test]
fn encrypted_mail_is_unlocked_and_read_but_not_kept() {
    use rsa::pkcs8::DecodePrivateKey;
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    engine.set_roots(vec![data("ca.der")]);
    let account = engine.store.add_account("grace@example.net").unwrap();
    let mut sink = engine.sink(account);
    sink.folders(&[RemoteFolder { name: "INBOX".into(), role: mail_sync::Role::Inbox, selectable: true }]).unwrap();
    // Grace's own signed mail: her certificate is known.
    sink.messages("INBOX", vec![fetched(1, data("signed.eml"), true), fetched(2, data("encrypted.eml"), true)]).unwrap();
    let inbox = engine.store.folder_id(account, "INBOX", Role::Inbox).unwrap();
    let id = engine.store.at(inbox, 2).unwrap().unwrap().0;
    let m = engine.store.message(id).unwrap();
    assert!(m.summary.encrypted && m.summary.text.is_none());
    let r = engine.open(id).unwrap();
    assert!(r.attachments.is_empty() && r.text.as_deref() == Some(""));

    let certs = engine.my_certs(account, &[]);
    assert_eq!(certs, vec![data("leaf.der")]);
    let key = rsa::RsaPrivateKey::from_pkcs8_der(&data("leaf.p8")).unwrap();
    let unwrap = |cert: &[u8], wrapped: &[u8]| {
        assert_eq!(cert, data("leaf.der"));
        key.decrypt(rsa::Pkcs1v15Encrypt, wrapped).map_err(|e| e.to_string())
    };
    assert!(!engine.unlock(id, &[data("ca.der")], &unwrap).unwrap(), "not for that certificate");
    assert!(engine.unlock(id, &certs, &unwrap).unwrap());
    let r = engine.open(id).unwrap();
    assert_eq!(r.text.as_deref().map(str::trim), Some("The secret plan."));
    assert_eq!(r.attachments.len(), 1);
    assert_eq!(engine.attachment(id, &r.attachments[0].which).unwrap().bytes, b"step,when\r\none,now");
    // Nothing of the inside is kept: no document, no files text.
    let m = engine.store.message(id).unwrap();
    assert!(m.document.is_none() && m.summary.text.is_none());
    assert_eq!(engine.index_files(id).unwrap(), None);
    // Forgotten keys lock it again.
    engine.forget_keys();
    assert_eq!(engine.open(id).unwrap().text.as_deref(), Some(""));
}
