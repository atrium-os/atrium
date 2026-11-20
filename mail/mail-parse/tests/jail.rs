//! The parse jail end to end: the real binary, driven the way the engine
//! drives it (mail.md §4, §7.1), with every frame it sends checked as the
//! engine checks them.
#![cfg(feature = "jail")]

use mail_schema::{self as wire, Caps, FromJail, ToJail};
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair};
use std::io::{BufReader, BufWriter, Write};
use std::process::{Command, Stdio};

fn b64(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 { if i <= c.len() { s.push(T[(n >> (18 - 6 * i) & 63) as usize] as char) } else { s.push('=') } }
    }
    s
}

/// An Ed25519-signed message (simple/simple, so the signed bytes are literal).
fn signed(kp: &Ed25519KeyPair, headers: &str, body: &str) -> Vec<u8> {
    let bh = b64(ring::digest::digest(&ring::digest::SHA256, body.as_bytes()).as_ref());
    let field = format!("DKIM-Signature: v=1; a=ed25519-sha256; c=simple/simple; d=example.com; s=sel; h=from:subject:content-type; bh={bh}; b=");
    let mut data = headers.as_bytes().to_vec();
    data.extend_from_slice(field.as_bytes());
    let sig = kp.sign(ring::digest::digest(&ring::digest::SHA256, &data).as_ref());
    format!("{field}{}\r\n{headers}\r\n{body}", b64(sig.as_ref())).into_bytes()
}

const HEADERS: &str = "From: Priya Nair <priya@example.com>\r\nSubject: Autumn sale\r\nContent-Type: text/html; charset=utf-8\r\n";
const BODY: &str = "<p>Hello <a href=\"https://shop.example/x\">sale</a></p>\r\n<img src=\"https://cdn.example/coat.jpg\" width=\"300\" height=\"200\">\r\n<img src=\"https://track.example/o.gif\" width=\"1\" height=\"1\">\r\n";

struct Engine { child: std::process::Child, r: BufReader<std::process::ChildStdout>, w: BufWriter<std::process::ChildStdin>, queries: Vec<String> }

impl Engine {
    fn start() -> Engine {
        let mut child = Command::new(env!("CARGO_BIN_EXE_mail-parse-jail")).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        let r = BufReader::new(child.stdout.take().unwrap());
        let w = BufWriter::new(child.stdin.take().unwrap());
        Engine { child, r, w, queries: Vec::new() }
    }
    fn send(&mut self, m: &ToJail) { wire::write_frame(&mut self.w, m).unwrap() }
    /// Read frames until `Done`, answering queries from `dns`.
    fn run(&mut self, raw_len: u64, dns: &dyn Fn(&str) -> Option<String>) -> Vec<FromJail> {
        let mut out = Vec::new();
        loop {
            let f: FromJail = wire::read_frame(&mut self.r).unwrap().expect("frame");
            f.check(&Caps::default(), raw_len).expect("the engine accepts it");
            match f {
                FromJail::Query { query, name } => {
                    self.queries.push(name.clone());
                    let txt = dns(&name);
                    self.send(&ToJail::Answer { query, txt });
                }
                FromJail::Done => return out,
                other => out.push(other),
            }
        }
    }
    fn finish(mut self) {
        drop(self.w);
        assert!(self.child.wait().unwrap().success());
        let _ = self.r.get_mut();
    }
}

#[test]
fn a_batch_is_summarized_with_keys_from_the_engine() {
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
    let kp = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
    let rec = format!("v=DKIM1; k=ed25519; p={}", b64(kp.public_key().as_ref()));
    let good = signed(&kp, HEADERS, BODY);
    let forged = String::from_utf8(good.clone()).unwrap().replace("Autumn sale", "Wire the money").into_bytes();
    let plain = b"From: a@b.example\r\nSubject: hi\r\n\r\nhello\r\n".to_vec();
    let not_mail = b"\x00\x01garbage".to_vec();

    let mut e = Engine::start();
    let batch: Vec<wire::Raw> = [&good, &forged, &plain, &not_mail].iter().enumerate()
        .map(|(i, b)| wire::Raw { id: i as u64, bytes: b.to_vec() }).collect();
    let max_len = batch.iter().map(|m| m.bytes.len()).max().unwrap() as u64;
    e.send(&ToJail::Summarize { authserv_id: None, me: vec![], now: 0, headers_only: false, roots: vec![], batch });
    let dns = |n: &str| match n { "sel._domainkey.example.com" => Some(rec.clone()), _ => None };
    let out = e.run(max_len, &dns);
    assert_eq!(out.len(), 4);
    let check = |i: usize| match &out[i] { FromJail::Summary { id, result: Ok(s) } if *id == i as u64 => s.sender_check.clone(), o => panic!("{o:?}") };
    assert_eq!(check(0), wire::SenderCheck::Verified { domain: "example.com".into(), by_provider: false });
    assert_eq!(check(1), wire::SenderCheck::NotVerified, "a changed subject breaks the signature");
    assert_eq!(check(2), wire::SenderCheck::NotVerified);
    assert!(matches!(&out[3], FromJail::Summary { id: 3, result: Err(_) }), "{:?}", out[3]);
    // Only key and DMARC lookups, and the unsigned message asked for none.
    assert!(e.queries.iter().all(|q| q == "sel._domainkey.example.com" || q == "_dmarc.example.com"), "{:?}", e.queries);
    e.finish();
}

#[test]
fn local_sender_verification_requires_one_from_mailbox() {
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
    let kp = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
    let key = format!("v=DKIM1; k=ed25519; p={}", b64(kp.public_key().as_ref()));
    let normal = signed(&kp, HEADERS, BODY);
    let with_extra_from = |extra: &[u8]| {
        let mut message = extra.to_vec();
        message.extend_from_slice(&normal);
        message
    };
    let duplicate = with_extra_from(b"fRoM: second@example.com\r\n");
    let obsolete_duplicate = with_extra_from(b"From : second@example.com\r\n");
    let multiple = signed(
        &kp,
        &HEADERS.replace("priya@example.com>", "priya@example.com>, second@example.com"),
        BODY,
    );
    let mut engine = Engine::start();
    for headers_only in [false, true] {
        let batch: Vec<_> = [&normal, &duplicate, &obsolete_duplicate, &multiple]
            .into_iter()
            .enumerate()
            .map(|(id, bytes)| wire::Raw { id: id as u64, bytes: bytes.clone() })
            .collect();
        let max_len = batch.iter().map(|m| m.bytes.len()).max().unwrap() as u64;
        engine.queries.clear();
        engine.send(&ToJail::Summarize {
            authserv_id: None,
            me: vec![],
            now: 0,
            headers_only,
            roots: vec![],
            batch,
        });
        let output = engine.run(max_len, &|name| match name {
            "sel._domainkey.example.com" => Some(key.clone()),
            "_dmarc.example.com" => Some(String::new()),
            _ => panic!("unexpected query: {name}"),
        });
        let normal_check = if headers_only {
            assert!(engine.queries.is_empty());
            wire::SenderCheck::Pending
        } else {
            assert_eq!(engine.queries, ["sel._domainkey.example.com", "_dmarc.example.com"]);
            wire::SenderCheck::Verified { domain: "example.com".into(), by_provider: false }
        };
        let mut checks = output.into_iter().enumerate().map(|(expected_id, frame)| match frame {
            FromJail::Summary { id, result: Ok(summary) } if id == expected_id as u64 => summary.sender_check,
            other => panic!("unexpected frame: {other:?}"),
        });
        assert_eq!(checks.next(), Some(normal_check));
        assert_eq!(checks.collect::<Vec<_>>(), vec![wire::SenderCheck::NotVerified; 3]);
    }
    engine.finish();
}

#[test]
fn opening_converts_the_html_and_drops_the_pixel() {
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
    let kp = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
    let raw = signed(&kp, HEADERS, BODY);
    let mut e = Engine::start();
    e.send(&ToJail::Open { authserv_id: None, me: vec![], now: 0, roots: vec![], key: None, message: wire::Raw { id: 7, bytes: raw.clone() }, loaded: vec![], pgp_keys: vec![] });
    let out = e.run(raw.len() as u64, &|_| None);
    let FromJail::Opened { id: 7, result: Ok(o) } = &out[0] else { panic!("{out:?}") };
    assert_eq!(o.summary.sender_check, wire::SenderCheck::Pending, "no key could be fetched");
    let d = o.document.as_ref().expect("an HTML document");
    assert!(d.html.contains("Hello") && !d.html.contains("cdn.example") && !d.html.contains("track.example"));
    let loadable: Vec<_> = d.resources.iter().filter(|r| r.loadable()).collect();
    assert_eq!(loadable.len(), 1, "{:?}", d.resources);
    assert!(d.resources.iter().any(|r| r.hidden && r.source == wire::Source::Remote("https://track.example/o.gif".into())));
    e.finish();
}

#[test]
fn the_engine_refuses_a_lying_frame() {
    let lie = FromJail::Query { query: 0, name: "evil.example".into() };
    assert!(lie.check(&Caps::default(), 0).is_err());
    let mut s = wire::Summary { from: vec![wire::Address { addr: "x@bank.example".into(), ..Default::default() }], ..Default::default() };
    s.sender_check = wire::SenderCheck::Verified { domain: "evil.example".into(), by_provider: false };
    assert!(s.check(&Caps::default(), 10).is_err(), "verified domain must be From's");
    s.sender_check = wire::SenderCheck::NotVerified;
    s.parts.push(wire::Part { path: "1".into(), kind: wire::PartKind::Text, content_type: "text/plain".into(), filename: None, content_id: None, size: 1, body: (0, 99) });
    assert!(s.check(&Caps::default(), 10).is_err(), "part outside the message");
    assert!(wire::query_allowed("sel._domainkey.example.com") && wire::query_allowed("_dmarc.example.com"));
    assert!(!wire::query_allowed("_dmarc.com") && !wire::query_allowed("a.example.com") && !wire::query_allowed("x._domainkey.a b.com"));
    // A frame over the limit is refused before it is read.
    let mut big = Vec::new();
    big.extend_from_slice(&((wire::MAX_FRAME as u32) + 1).to_be_bytes());
    assert!(wire::read_frame::<FromJail>(&mut big.as_slice()).is_err());
    let _ = std::io::sink().flush();
}

fn png(w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let mut e = png::Encoder::new(&mut out, w, h);
    e.set_color(png::ColorType::Rgb);
    e.write_header().unwrap().write_image_data(&vec![120u8; (w * h * 3) as usize]).unwrap();
    out
}

#[test]
fn a_message_s_own_pictures_come_with_its_document() {
    let logo = png(120, 40);
    let dot = png(2, 2);
    let raw = format!(
        "From: Shop <news@shop.example>\r\nTo: p@example.org\r\nSubject: New\r\nMIME-Version: 1.0\r\nContent-Type: multipart/related; boundary=\"b\"\r\n\r\n\
--b\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>Hi</p><img src=\"cid:logo@shop\"><img src=\"data:image/png;base64,{}\" width=\"2\" height=\"2\"><img src=\"cid:missing@shop\" width=\"10\" height=\"10\"><img src=\"https://cdn.example/a.png\" width=\"50\" height=\"50\">\r\n\
--b\r\nContent-Type: image/png\r\nContent-ID: <logo@shop>\r\nContent-Transfer-Encoding: base64\r\n\r\n{}\r\n--b--\r\n",
        b64(&dot), b64(&logo)).into_bytes();
    let mut e = Engine::start();
    e.send(&ToJail::Open { authserv_id: None, me: vec![], now: 0, roots: vec![], key: None, message: wire::Raw { id: 9, bytes: raw.clone() }, loaded: vec![], pgp_keys: vec![] });
    let out = e.run(raw.len() as u64, &|_| None);
    let FromJail::Opened { id: 9, result: Ok(o) } = &out[0] else { panic!("{out:?}") };
    let d = o.document.as_ref().unwrap();
    let id_of = |src: &wire::Source| d.resources.iter().find(|r| r.source == *src).map(|r| r.id.clone()).unwrap();
    let logo_id = id_of(&wire::Source::Cid("logo@shop".into()));
    let got: Vec<&str> = d.pictures.iter().map(|(i, _)| i.as_str()).collect();
    assert_eq!(got.len(), 2, "the cid part and the data URL, not the missing part or the remote one: {got:?}");
    assert_eq!(d.pictures.iter().find(|(i, _)| *i == logo_id).unwrap().1, logo);
    // The logo declared no size: its real one is in the document.
    assert!(d.html.contains(&format!("src=\"{logo_id}\" width=\"120\" height=\"40\"")) || d.html.contains(&format!("src=\"{logo_id}\" class")), "{}", d.html);
    assert!(d.html.contains("width=\"120\""), "{}", d.html);
    e.finish();
}

#[test]
fn attachments_are_listed_and_handed_over() {
    let raw = b"From: a@example.com\r\nTo: b@example.com\r\nSubject: files\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=B\r\n\r\n--B\r\nContent-Type: text/plain\r\n\r\nSee attached.\r\n--B\r\nContent-Type: application/pdf; name=\"invoice.pdf\"\r\nContent-Disposition: attachment; filename=\"invoice.pdf\"\r\nContent-Transfer-Encoding: base64\r\n\r\nUEsDBHNjcmlwdA==\r\n--B\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Disposition: attachment; filename=\"../notes.txt\"\r\n\r\nquarterly numbers\r\n--B--\r\n".to_vec();
    let mut e = Engine::start();
    e.send(&ToJail::Open { authserv_id: None, me: vec![], now: 0, roots: vec![], key: None, message: wire::Raw { id: 3, bytes: raw.clone() }, loaded: vec![], pgp_keys: vec![] });
    let out = e.run(raw.len() as u64, &|_| None);
    let FromJail::Opened { id: 3, result: Ok(o) } = &out[0] else { panic!("{out:?}") };
    let names: Vec<&str> = o.attachments.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["invoice.pdf", "notes.txt"]);
    // Called a PDF, but the bytes are a zip: the head shows it.
    assert_eq!(&o.attachments[0].head[..4], b"PK\x03\x04");
    let which = o.attachments[1].which.clone();
    e.send(&ToJail::Attachment { message: wire::Raw { id: 4, bytes: raw.clone() }, which, key: None });
    let out = e.run(raw.len() as u64, &|_| None);
    let FromJail::Attachment { id: 4, result: Ok(a) } = &out[0] else { panic!("{out:?}") };
    assert_eq!(a.bytes, b"quarterly numbers");
    assert_eq!(a.text.as_deref(), Some("quarterly numbers"));
    e.send(&ToJail::Attachment { message: wire::Raw { id: 5, bytes: raw.clone() }, which: o.attachments[0].which.clone(), key: None });
    let out = e.run(raw.len() as u64, &|_| None);
    let FromJail::Attachment { id: 5, result: Ok(a) } = &out[0] else { panic!("{out:?}") };
    assert_eq!(a.bytes, b"PK\x03\x04script");
    assert_eq!(a.text, None, "a zip has no text");
    e.finish();
}

/// An S/MIME signed message (OpenSSL's, kept with bare LFs as some stores
/// do): checked against the roots the engine sends; the signature part
/// isn't listed as a file.
#[test]
fn smime_signature_is_checked_against_the_engine_s_roots() {
    let d = |f: &str| std::fs::read(format!("{}/../mail-smime/tests/data/{f}", env!("CARGO_MANIFEST_DIR"))).unwrap();
    let raw = d("signed.eml");
    let now = 1_800_000_000;
    let run = |roots: Vec<Vec<u8>>, raw: &[u8]| {
        let mut e = Engine::start();
        e.send(&ToJail::Open { authserv_id: None, me: vec![], now, roots, key: None, message: wire::Raw { id: 1, bytes: raw.to_vec() }, loaded: vec![], pgp_keys: vec![] });
        let f = e.run(raw.len() as u64, &|_| None);
        e.finish();
        match f.into_iter().next() { Some(FromJail::Opened { result: Ok(o), .. }) => o, other => panic!("{other:?}") }
    };
    let o = run(vec![d("ca.der")], &raw);
    let m = o.summary.smime.clone().expect("signed");
    assert_eq!(m.state, wire::SmimeState::Valid);
    let s = m.signer.unwrap();
    assert_eq!(s.name, "Grace Hopper");
    assert_eq!(s.issuer, "Fixture");
    assert_eq!(s.der, d("leaf.der"));
    assert_eq!(m.issuers, vec!["OpenSSL Test Root".to_string()]);
    assert!(o.attachments.is_empty(), "{:?}", o.attachments);
    assert!(o.summary.parts.iter().any(|p| p.kind == wire::PartKind::Signature));
    assert_eq!(o.summary.text.as_deref().map(str::trim), Some("From OpenSSL."));

    // Not a root here.
    assert!(matches!(run(vec![], &raw).summary.smime.unwrap().state, wire::SmimeState::NotTrusted(_)));
    // Changed in transit.
    let changed = String::from_utf8(raw.clone()).unwrap().replace("From OpenSSL.", "From 0penSSL.").into_bytes();
    assert!(matches!(run(vec![d("ca.der")], &changed).summary.smime.unwrap().state, wire::SmimeState::Invalid(_)));
    // Someone else in From.
    let other = String::from_utf8(raw.clone()).unwrap().replace("<grace@example.net>", "<ceo@example.net>").into_bytes();
    assert_eq!(run(vec![d("ca.der")], &other).summary.smime.unwrap().state, wire::SmimeState::WrongAddress(vec!["grace@example.net".into()]));
}

/// A lying jail can't claim a signature for From with someone else's
/// certificate, or a certificate that isn't the one it hashed.
#[test]
fn the_engine_checks_smime_claims() {
    let cert = wire::SmimeCert { name: "Bank".into(), emails: vec!["help@bank.example".into()], issuer: "CA".into(), not_before: 0, not_after: 1, sha256: [0; 32], der: vec![1, 2, 3] };
    let mut s = wire::Summary { from: vec![wire::Address { addr: "help@bank.example".into(), ..Default::default() }], ..Default::default() };
    s.smime = Some(wire::Smime { state: wire::SmimeState::Valid, signer: Some(cert.clone()), issuers: vec![], signed_at: None });
    assert!(s.check(&Caps::default(), 10).is_err(), "hash of another certificate");
    let mut good = cert.clone();
    good.sha256 = ring::digest::digest(&ring::digest::SHA256, &good.der).as_ref().try_into().unwrap();
    s.smime.as_mut().unwrap().signer = Some(good.clone());
    assert_eq!(s.check(&Caps::default(), 10), Ok(()));
    s.from[0].addr = "help@evil.example".into();
    assert!(s.check(&Caps::default(), 10).is_err(), "valid for another address");
}

/// An encrypted message (OpenSSL's): the jail says which of the person's
/// certificates it is for and hands out the wrapped key; with the key
/// unwrapped outside, it reads the message inside and its attachment.
#[test]
fn encrypted_mail_is_read_with_a_key_unwrapped_outside() {
    use rsa::pkcs8::DecodePrivateKey;
    let d = |f: &str| std::fs::read(format!("{}/../mail-smime/tests/data/{f}", env!("CARGO_MANIFEST_DIR"))).unwrap();
    let raw = d("encrypted.eml");
    let len = raw.len() as u64;
    let msg = || wire::Raw { id: 1, bytes: raw.clone() };

    let mut e = Engine::start();
    e.send(&ToJail::Summarize { authserv_id: None, me: vec![], now: 0, headers_only: false, roots: vec![], batch: vec![msg()] });
    let f = e.run(len, &|_| None);
    let FromJail::Summary { result: Ok(s), .. } = &f[0] else { panic!("{f:?}") };
    assert!(s.encrypted && s.text.is_none() && s.subject == "Encrypted plan");

    e.send(&ToJail::KeyFor { message: msg(), certs: vec![d("ca.der"), d("leaf.der")] });
    let f = e.run(len, &|_| None);
    let Some(FromJail::KeyFor { which: Some((1, wrapped)), .. }) = f.into_iter().next() else { panic!() };
    let key = rsa::RsaPrivateKey::from_pkcs8_der(&d("leaf.p8")).unwrap().decrypt(rsa::Pkcs1v15Encrypt, &wrapped).unwrap();

    e.send(&ToJail::Open { authserv_id: None, me: vec![], now: 0, roots: vec![], key: Some(key.clone()), message: msg(), loaded: vec![], pgp_keys: vec![] });
    let f = e.run(len, &|_| None);
    let Some(FromJail::Opened { result: Ok(o), .. }) = f.into_iter().next() else { panic!() };
    assert!(o.summary.encrypted);
    assert_eq!(o.summary.subject, "Encrypted plan");
    assert_eq!(o.summary.text.as_deref().map(str::trim), Some("The secret plan."));
    assert_eq!(o.attachments.len(), 1);
    e.send(&ToJail::Attachment { message: msg(), which: o.attachments[0].which.clone(), key: Some(key) });
    let f = e.run(len, &|_| None);
    let Some(FromJail::Attachment { result: Ok(a), .. }) = f.into_iter().next() else { panic!() };
    assert_eq!(a.bytes, b"step,when\r\none,now");
    // A wrong key reads nothing.
    e.send(&ToJail::Open { authserv_id: None, me: vec![], now: 0, roots: vec![], key: Some(vec![7; 32]), message: msg(), loaded: vec![], pgp_keys: vec![] });
    let f = e.run(len, &|_| None);
    assert!(matches!(f.into_iter().next(), Some(FromJail::Opened { result: Err(_), .. })));
    e.finish();
}

#[test]
fn an_order_in_json_ld_becomes_a_card() {
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
    let kp = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
    let rec = format!("v=DKIM1; k=ed25519; p={}", b64(kp.public_key().as_ref()));
    let body = "<script type=\"application/ld+json\">{\"@type\":\"Order\",\"orderNumber\":\"A-77\",\"merchant\":{\"name\":\"Shop\"},\"orderStatus\":\"http://schema.org/OrderDelivered\",\"url\":\"https://shop.example/o/77\"}</script>\r\n<p>Delivered.</p>\r\n";
    let raw = signed(&kp, HEADERS, body);
    let mut e = Engine::start();
    e.send(&ToJail::Open { authserv_id: None, me: vec![], now: 0, roots: vec![], key: None, message: wire::Raw { id: 9, bytes: raw.clone() }, loaded: vec![], pgp_keys: vec![] });
    let dns = |n: &str| (n == "sel._domainkey.example.com").then(|| rec.clone());
    let out = e.run(raw.len() as u64, &dns);
    let FromJail::Opened { id: 9, result: Ok(o) } = &out[0] else { panic!("{out:?}") };
    assert!(matches!(o.summary.sender_check, wire::SenderCheck::Verified { .. }));
    let c = o.card.as_ref().expect("a card");
    assert_eq!((c.kind, c.title.as_str()), (wire::CardKind::Order, "Order A-77"));
    assert_eq!(c.fields, vec![(wire::CardField::Status, "Delivered".into()), (wire::CardField::Merchant, "Shop".into()), (wire::CardField::Number, "A-77".into())]);
    // The script is gone from the document the viewer gets.
    assert!(!o.document.as_ref().unwrap().html.contains("ld+json"));
    e.finish();
}

#[test]
fn autocrypt_keys_come_out_and_pgp_signatures_are_checked() {
    use pgp::composed::{ArmorOptions, DetachedSignature, EncryptionCaps, KeyType, SecretKeyParamsBuilder, SubkeyParamsBuilder};
    use pgp::crypto::{ecc_curve::ECCCurve, hash::HashAlgorithm};
    use pgp::ser::Serialize;
    use pgp::types::Password;
    let mut p = SecretKeyParamsBuilder::default();
    p.key_type(KeyType::Ed25519Legacy).can_certify(true).can_sign(true).primary_user_id("Priya Nair <priya@example.com>".into())
        .subkeys(vec![SubkeyParamsBuilder::default().key_type(KeyType::ECDH(ECCCurve::Curve25519Legacy)).can_encrypt(EncryptionCaps::All).build().unwrap()]);
    let sk = p.build().unwrap().generate(rand::thread_rng()).unwrap();
    let key = sk.to_public_key().to_bytes().unwrap();

    // The signed part ends before the line end that starts the boundary.
    let content = "Content-Type: text/plain; charset=utf-8\r\n\r\nThe numbers are in.";
    let sig = DetachedSignature::sign_binary_data(rand::thread_rng(), &sk.primary_key, &Password::empty(), HashAlgorithm::Sha256, content.as_bytes()).unwrap();
    let sig = sig.to_armored_string(ArmorOptions::default()).unwrap().replace("\r\n", "\n").replace('\n', "\r\n");
    let keydata: String = b64(&key).as_bytes().chunks(76).map(|c| format!(" {}\r\n", std::str::from_utf8(c).unwrap())).collect();
    let raw = format!(
        "From: Priya Nair <priya@example.com>\r\nTo: me@example.org\r\nSubject: Numbers\r\nAutocrypt: addr=priya@example.com; keydata=\r\n{keydata}\
MIME-Version: 1.0\r\nContent-Type: multipart/signed; micalg=pgp-sha256; protocol=\"application/pgp-signature\"; boundary=\"b1\"\r\n\r\n\
--b1\r\n{content}\r\n--b1\r\nContent-Type: application/pgp-signature; name=\"signature.asc\"\r\n\r\n{sig}\r\n--b1--\r\n"
    ).into_bytes();
    let len = raw.len() as u64;

    let mut e = Engine::start();
    e.send(&ToJail::Summarize { authserv_id: None, me: vec![], now: 0, headers_only: false, roots: vec![], batch: vec![wire::Raw { id: 4, bytes: raw.clone() }] });
    let f = e.run(len, &|_| None);
    let learned = f.iter().find_map(|x| match x { FromJail::Autocrypt { id: 4, addr, key } => Some((addr.clone(), key.clone())), _ => None });
    assert_eq!(learned, Some(("priya@example.com".into(), key.clone())), "{f:?}");
    assert!(f.iter().any(|x| matches!(x, FromJail::Summary { id: 4, result: Ok(_) })));

    let open = |e: &mut Engine, keys: Vec<Vec<u8>>, bytes: Vec<u8>| {
        let len = bytes.len() as u64;
        e.send(&ToJail::Open { authserv_id: None, me: vec![], now: 0, roots: vec![], key: None, message: wire::Raw { id: 5, bytes }, loaded: vec![], pgp_keys: keys });
        match e.run(len, &|_| None).into_iter().next() { Some(FromJail::Opened { result: Ok(o), .. }) => o.pgp.expect("a pgp check"), o => panic!("{o:?}") }
    };
    let ok = open(&mut e, vec![key.clone()], raw.clone());
    assert_eq!(ok.state, wire::PgpState::Valid);
    assert_eq!(ok.fingerprint.as_ref().unwrap().len(), 40);
    assert_eq!(open(&mut e, vec![], raw.clone()).state, wire::PgpState::NoKey);
    let changed = String::from_utf8(raw.clone()).unwrap().replace("numbers are in", "numbers are out").into_bytes();
    assert!(matches!(open(&mut e, vec![key.clone()], changed).state, wire::PgpState::Invalid(_)));
    // Kept with bare line ends, it still checks as sent.
    let lf = String::from_utf8(raw).unwrap().replace("\r\n", "\n").into_bytes();
    assert_eq!(open(&mut e, vec![key], lf).state, wire::PgpState::Valid);
    e.finish();
}

#[test]
fn sanitized_copy_rejects_oversize_html_and_supplies_only_local_resources() {
    let mut engine = Engine::start();
    let raw = |html: &str| format!("From: a@example.com\r\nContent-Type: multipart/mixed; boundary=B\r\n\r\n--B\r\nContent-Type: text/html\r\nContent-Disposition: attachment; filename=invoice.html\r\n\r\n{html}\r\n--B--\r\n").into_bytes();
    let html = "<html><body>Invoice <img src='https://remote.example/pixel'><script>doSomething()</script></body></html>";
    let bytes = raw(html);
    engine.send(&ToJail::Open { authserv_id: None, me: vec![], now: 0, roots: vec![], key: None, message: wire::Raw { id: 1, bytes: bytes.clone() }, loaded: vec![], pgp_keys: vec![] });
    let out = engine.run(bytes.len() as u64, &|_| None);
    let FromJail::Opened { result: Ok(o), .. } = &out[0] else { panic!("missing attachment") };
    let which = o.attachments[0].which.clone();
    let mut export = |bytes: Vec<u8>| {
        let len = bytes.len();
        engine.send(&ToJail::HtmlAttachment { message: wire::Raw { id: 2, bytes }, which: which.clone(), key: None });
        engine.run(len as u64, &|_| panic!("unexpected query"))
    };
    let result = export(bytes);
    let FromJail::Opened { result: Ok(o), .. } = &result[0] else { panic!("conversion failed") };
    let doc = o.document.as_ref().unwrap();
    assert!(doc.html.contains("Invoice"));
    assert!(doc.pictures.is_empty());
    assert!(!doc.html.contains("remote.example") && !doc.html.contains("doSomething"));
    let result = export(raw(&"a".repeat((4 << 20) + 1)));
    assert!(matches!(&result[..], [FromJail::Opened { result: Err(wire::Refusal::TooLarge), .. }]));
    engine.finish();
}

#[test]
fn provider_verdict_requires_parent_supplied_identity_and_one_from_mailbox() {
    for (trust, from, verified) in [
        (None, "sender@example.com", false),
        (Some("edge.example"), "sender@example.com", true),
        (Some("edge.example"), "sender@other.example", false),
        (Some("edge.example"), "sender@example.com, other@example.com", false),
    ] {
        let message = format!(
            "Authentication-Results: edge.example; dmarc=pass header.from=example.com\r\nFrom: {from}\r\n\r\nHello\r\n"
        ).into_bytes();
        let mut engine = Engine::start();
        engine.send(&ToJail::Summarize {
            authserv_id: trust.map(str::to_owned),
            me: vec![],
            now: 0,
            headers_only: false,
            roots: vec![],
            batch: vec![wire::Raw { id: 1, bytes: message.clone() }],
        });
        let output = engine.run(message.len() as u64, &|_| None);
        let [FromJail::Summary { result: Ok(summary), .. }] = output.as_slice() else {
            panic!("{output:?}");
        };
        let expected = if verified {
            wire::SenderCheck::Verified { domain: "example.com".into(), by_provider: true }
        } else {
            wire::SenderCheck::NotVerified
        };
        assert_eq!(summary.sender_check, expected);
        engine.finish();
    }
}
