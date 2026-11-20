//! DKIM end to end. The signed data is written out by hand here (simple
//! canonicalization is the literal bytes), so these tests don't lean on
//! the verifier's own canonicalization code.

use mail_parse::dkim::{self, Outcome, Verdict};
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair, RsaKeyPair, RSA_PKCS1_SHA256};

fn b64(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                s.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

const HEADERS: &str = "From: Priya Nair <priya@mail.example.com>\r\nSubject: Q4 numbers\r\n";
const BODY: &str = "Here are the numbers.\r\n";

/// Builds a signed message: `extra` goes into the tag list before `b=`.
fn signed(alg: &str, d: &str, extra: &str, sign: &dyn Fn(&[u8]) -> Vec<u8>, headers: &str, body: &str) -> Vec<u8> {
    let bh = b64(ring::digest::digest(&ring::digest::SHA256, b"Here are the numbers.\r\n").as_ref());
    let sig_field = format!("DKIM-Signature: v=1; a={alg}; c=simple/simple; d={d}; s=sel; h=from:subject; bh={bh};{extra} b=");
    let mut data = HEADERS.as_bytes().to_vec();
    data.extend_from_slice(sig_field.as_bytes());
    let b = b64(&sign(&data));
    format!("{sig_field}{b}\r\n{headers}\r\n{body}").into_bytes()
}

fn ed() -> (Ed25519KeyPair, String) {
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
    let kp = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
    let rec = format!("v=DKIM1; k=ed25519; p={}", b64(kp.public_key().as_ref()));
    (kp, rec)
}

fn ed_sign(kp: &Ed25519KeyPair) -> impl Fn(&[u8]) -> Vec<u8> + '_ {
    // RFC 8463: Ed25519 signs the SHA-256 of the data
    move |d| kp.sign(ring::digest::digest(&ring::digest::SHA256, d).as_ref()).as_ref().to_vec()
}

#[test]
fn ed25519_pass_and_tamper() {
    let (kp, rec) = ed();
    let key = |s: &str, d: &str| (s == "sel" && d == "example.com").then(|| rec.clone());
    let m = signed("ed25519-sha256", "example.com", "", &ed_sign(&kp), HEADERS, BODY);
    let r = dkim::verify(&m, &key, 0);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].outcome, Outcome::Pass);
    assert_eq!(dkim::verdict("mail.example.com", &r, None, None), Verdict::Pending);
    assert_eq!(dkim::verdict("mail.example.com", &r, Some(""), None), Verdict::Verified { domain: "mail.example.com".into(), by_provider: false });
    // strict alignment refuses a subdomain From
    assert_eq!(dkim::verdict("mail.example.com", &r, Some("v=DMARC1; p=reject; adkim=s"), None), Verdict::NotVerified);
    // a different organisation can't borrow it
    assert_eq!(dkim::verdict("bank.example", &r, None, None), Verdict::NotVerified);

    let changed = signed("ed25519-sha256", "example.com", "", &ed_sign(&kp), "From: Priya Nair <priya@mail.example.com>\r\nSubject: Q4 numbers (paid)\r\n", BODY);
    assert_eq!(dkim::verify(&changed, &key, 0)[0].outcome, Outcome::Fail("signature"));
    let body = signed("ed25519-sha256", "example.com", "", &ed_sign(&kp), HEADERS, "Here are the numbers. Wire to IBAN X.\r\n");
    assert_eq!(dkim::verify(&body, &key, 0)[0].outcome, Outcome::Fail("body hash"));
    // a second From added above the signed one is what gets checked (bottom-up rule),
    // so a forged From on top changes nothing signed — but Mail reads the first From:
    // the verdict is on the displayed From domain, so a forged one isn't verified
    let mut two_from = b"From: Boss <ceo@bank.example>\r\n".to_vec();
    two_from.extend_from_slice(&m);
    assert_eq!(dkim::verify(&two_from, &key, 0)[0].outcome, Outcome::Pass);
    assert_eq!(dkim::verdict("bank.example", &dkim::verify(&two_from, &key, 0), None, None), Verdict::NotVerified);
}

#[test]
fn no_key_pending_and_revoked() {
    let (kp, _) = ed();
    let m = signed("ed25519-sha256", "example.com", "", &ed_sign(&kp), HEADERS, BODY);
    let r = dkim::verify(&m, &|_, _| None, 0);
    assert_eq!(r[0].outcome, Outcome::NoKey);
    assert_eq!(dkim::verdict("example.com", &r, None, None), Verdict::Pending);
    let r = dkim::verify(&m, &|_, _| Some("v=DKIM1; k=ed25519; p=".into()), 0);
    assert_eq!(r[0].outcome, Outcome::Fail("key revoked"));
    // a key for another algorithm
    let r = dkim::verify(&m, &|_, _| Some("v=DKIM1; k=rsa; p=AAAA".into()), 0);
    assert_eq!(r[0].outcome, Outcome::PermError("key type"));
}

#[test]
fn length_limit_and_expiry() {
    let (kp, rec) = ed();
    let key = |_: &str, _: &str| Some(rec.clone());
    let m = signed("ed25519-sha256", "example.com", " l=23;", &ed_sign(&kp), HEADERS, "Here are the numbers.\r\nP.S. new bank details\r\n");
    let r = dkim::verify(&m, &key, 0);
    assert_eq!(r[0].outcome, Outcome::Pass);
    assert!(r[0].partial_body);
    assert_eq!(dkim::verdict("example.com", &r, None, None), Verdict::NotVerified);
    let m = signed("ed25519-sha256", "example.com", " x=100;", &ed_sign(&kp), HEADERS, BODY);
    assert_eq!(dkim::verify(&m, &key, 1000)[0].outcome, Outcome::PermError("expired"));
    let m = signed("rsa-sha1", "example.com", "", &ed_sign(&kp), HEADERS, BODY);
    assert_eq!(dkim::verify(&m, &key, 0)[0].outcome, Outcome::PermError("algorithm"));
}

#[test]
fn rsa_with_openssl_key() {
    // RSA keys can't be made with ring; use the system openssl if present.
    let dir = std::env::temp_dir().join(format!("mail-dkim-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let pem = dir.join("k.pem");
    let der = dir.join("k.der");
    let ok = std::process::Command::new("openssl").args(["genpkey", "-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:2048", "-out"]).arg(&pem).status().map(|s| s.success()).unwrap_or(false)
        && std::process::Command::new("openssl").args(["pkcs8", "-topk8", "-nocrypt", "-outform", "DER", "-in"]).arg(&pem).arg("-out").arg(&der).status().map(|s| s.success()).unwrap_or(false);
    if !ok {
        eprintln!("openssl not available; skipping");
        return;
    }
    let pub_der = std::process::Command::new("openssl").args(["pkey", "-pubout", "-outform", "DER", "-in"]).arg(&pem).output().unwrap().stdout;
    let kp = RsaKeyPair::from_pkcs8(&std::fs::read(&der).unwrap()).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let sign = |d: &[u8]| {
        let mut sig = vec![0; kp.public().modulus_len()];
        kp.sign(&RSA_PKCS1_SHA256, &SystemRandom::new(), d, &mut sig).unwrap();
        sig
    };
    let rec = format!("v=DKIM1; k=rsa; p={}", b64(&pub_der));
    let m = signed("rsa-sha256", "example.com", "", &sign, HEADERS, BODY);
    assert_eq!(dkim::verify(&m, &|_, _| Some(rec.clone()), 0)[0].outcome, Outcome::Pass);
    let bad = signed("rsa-sha256", "example.com", "", &sign, "From: Priya Nair <priya@mail.example.com>\r\nSubject: Q5\r\n", BODY);
    assert_eq!(dkim::verify(&bad, &|_, _| Some(rec.clone()), 0)[0].outcome, Outcome::Fail("signature"));
}
