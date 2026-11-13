//! S5 token ceremonies in the broker (sigillum.md §10.1): choosing a
//! certificate and signing with it, against a scripted token driver that
//! answers as sigillum-p11 does. The P-256 certificate is built here; the
//! RSA one, its PKCS#1 and PSS signatures over `digest.bin` were made once
//! with openssl (tests/data; the private key was not kept).

use p256::ecdsa::signature::hazmat::PrehashSigner;
use p256::ecdsa::{Signature, SigningKey};
use sha2::{Digest, Sha256};
use sigillum_proto::client::{CertPurpose, ChooseCertificate, HashAlg, Refusal, Reply, Request, SignScheme, SignWithCertificate};
use sigillum_proto::driver::{DeviceClass, DriverError, FromDriver, KeyKind, Pin, ToDriver, TokenCert};
use sigillumd::broker::{Action, Broker, Choice, Config, Key, Sheet};
use sigillumd::cert;
use sigillumd::label::{AppId, Label};
use zeroize::Zeroizing;

const TOKEN: u64 = 5;
const RSA_DER: &[u8] = include_bytes!("data/rsa-test.der");
const DIGEST: &[u8] = include_bytes!("data/digest.bin");
const PKCS1_SIG: &[u8] = include_bytes!("data/pkcs1-sha256.sig");
const PSS_SIG: &[u8] = include_bytes!("data/pss-sha256.sig");

fn tlv(tag: u8, c: &[u8]) -> Vec<u8> {
    let mut v = vec![tag];
    match c.len() {
        n if n < 0x80 => v.push(n as u8),
        n if n < 0x100 => v.extend_from_slice(&[0x81, n as u8]),
        n => v.extend_from_slice(&[0x82, (n >> 8) as u8, n as u8]),
    }
    v.extend_from_slice(c);
    v
}

fn cat(parts: &[Vec<u8>]) -> Vec<u8> {
    parts.concat()
}

fn name(cn: &str) -> Vec<u8> {
    tlv(0x30, &tlv(0x31, &tlv(0x30, &cat(&[tlv(0x06, &[0x55, 0x04, 0x03]), tlv(0x0C, cn.as_bytes())]))))
}

fn ec_key() -> SigningKey {
    SigningKey::from_bytes(&[0x24u8; 32].into()).unwrap()
}

/// A P-256 certificate for [`ec_key`]. Its own signature is filler: the
/// broker checks no chain.
fn ec_cert(subject: &str) -> Vec<u8> {
    let point = ec_key().verifying_key().to_encoded_point(false);
    let alg = tlv(0x30, &tlv(0x06, &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x02]));
    let spki = tlv(
        0x30,
        &cat(&[
            tlv(0x30, &cat(&[tlv(0x06, &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01]), tlv(0x06, &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07])])),
            tlv(0x03, &cat(&[vec![0], point.as_bytes().to_vec()])),
        ]),
    );
    let tbs = tlv(
        0x30,
        &cat(&[
            tlv(0xA0, &tlv(0x02, &[2])),
            tlv(0x02, &[0x1F]),
            alg.clone(),
            name("Test CA \u{202E}gnp.exe"),
            tlv(0x30, &cat(&[tlv(0x17, b"260101000000Z"), tlv(0x18, b"20291231235959Z")])),
            name(subject),
            spki,
        ]),
    );
    tlv(0x30, &cat(&[tbs, alg, tlv(0x03, &[0, 0x30, 0x00])]))
}

fn tc(key_id: &[u8], label: &str, der: &[u8]) -> TokenCert {
    TokenCert { key_id: key_id.to_vec(), label: label.into(), cert: der.to_vec(), key: KeyKind::Other }
}

fn broker_app() -> AppId {
    AppId { publisher: [0xAB; 32], app_id: "org.atrium.navigator".into() }
}

fn navigator() -> Label {
    Label::App { user: "alice".into(), app: broker_app(), rp_ids: vec![] }
}

fn purpose() -> CertPurpose {
    CertPurpose::TlsClient { host: "portal.example.in".into(), port: 443 }
}

struct Rig {
    b: Broker,
    out: Vec<Action>,
}

impl Rig {
    fn new(token: bool) -> Rig {
        let mut b = Broker::new(Config { web_broker: Some(broker_app()), ceremony_ms: 120_000 });
        let mut out = Vec::new();
        if token {
            out = b.driver(TOKEN, FromDriver::Hello { class: DeviceClass::Ccid, vendor: 0x338c, product: 0x32, name: "InnaitKey DSC".into(), fido: None });
        }
        Rig { b, out }
    }

    fn run(&mut self, a: Vec<Action>) -> &Vec<Action> {
        self.out = a;
        &self.out
    }

    fn sheet(&self) -> Option<(Key, Sheet)> {
        self.out.iter().rev().find_map(|a| match a {
            Action::Sheet { key, sheet } => Some((*key, sheet.clone())),
            _ => None,
        })
    }

    fn to_driver(&self) -> Vec<(u64, ToDriver)> {
        self.out
            .iter()
            .filter_map(|a| match a {
                Action::ToDriver { device, msg } => Some((*device, msg.clone())),
                _ => None,
            })
            .collect()
    }

    fn reply(&self) -> Option<Reply> {
        self.out.iter().find_map(|a| match a {
            Action::Reply { reply, .. } => Some(reply.clone()),
            _ => None,
        })
    }

    fn audit(&self, s: &str) -> bool {
        self.out.iter().any(|a| matches!(a, Action::Audit(t) if t.contains(s)))
    }
}

fn choose(schemes: Vec<SignScheme>) -> Request {
    Request::ChooseCertificate(ChooseCertificate { ceremony: 9, timeout_ms: 0, purpose: purpose(), schemes })
}

fn sign(der: &[u8], scheme: SignScheme, digest: &[u8]) -> Request {
    Request::SignWithCertificate(SignWithCertificate {
        ceremony: 9,
        timeout_ms: 0,
        purpose: purpose(),
        cert_sha256: Sha256::digest(der).into(),
        scheme,
        digest: digest.to_vec(),
    })
}

const ECDSA: SignScheme = SignScheme::Ecdsa(HashAlg::Sha256);

#[test]
fn certificates_are_read_from_the_certificate() {
    let i = cert::parse(&ec_cert("Asha Rao")).unwrap();
    assert_eq!(i.subject, "Asha Rao");
    assert_eq!(i.issuer, "Test CA gnp.exe", "bidi overrides are dropped");
    assert_eq!(i.not_after, "2029-12-31");
    assert!(matches!(i.key, cert::Key::P256(_)));
    let r = cert::parse(RSA_DER).unwrap();
    assert_eq!(r.subject, "Test Signer RSA");
    assert!(matches!(r.key, cert::Key::Rsa { bits: 2048, .. }));
    // Truncated, trailing bytes, garbage.
    let ec = ec_cert("x");
    assert!(cert::parse(&ec[..ec.len() - 1]).is_err());
    assert!(cert::parse(&[ec.clone(), vec![0]].concat()).is_err());
    assert!(cert::parse(&[0x30, 0x84, 0, 0, 0, 1, 0]).is_err());
}

#[test]
fn rsa_signatures_verify_and_others_do_not() {
    let k = cert::parse(RSA_DER).unwrap().key;
    assert!(cert::verify(&k, SignScheme::RsaPkcs1(HashAlg::Sha256), DIGEST, PKCS1_SIG));
    assert!(cert::verify(&k, SignScheme::RsaPss(HashAlg::Sha256), DIGEST, PSS_SIG));
    assert!(!cert::verify(&k, SignScheme::RsaPss(HashAlg::Sha256), DIGEST, PKCS1_SIG));
    assert!(!cert::verify(&k, SignScheme::RsaPkcs1(HashAlg::Sha384), DIGEST, PKCS1_SIG));
    let mut other = DIGEST.to_vec();
    other[0] ^= 1;
    assert!(!cert::verify(&k, SignScheme::RsaPkcs1(HashAlg::Sha256), &other, PKCS1_SIG));
    assert!(!cert::verify(&k, ECDSA, DIGEST, PKCS1_SIG));
}

#[test]
fn choose_lists_what_fits_and_returns_the_pick() {
    let mut r = Rig::new(true);
    let ec = ec_cert("Asha Rao");
    let a = r.b.request(1, &navigator(), choose(vec![ECDSA]), 0);
    r.run(a);
    assert_eq!(r.to_driver(), vec![(TOKEN, ToDriver::ListCertificates { ceremony: 1 })]);
    assert!(r.sheet().is_none(), "no sheet until the token answered");
    let a = r.b.driver(
        TOKEN,
        FromDriver::Certificates {
            ceremony: 1,
            token_label: "DSC".into(),
            certs: vec![tc(&[1], "Bank of Trust", &ec), tc(&[2], "rsa", RSA_DER), tc(&[3], "junk", &[0x30, 0x01, 0x00])],
        },
    );
    r.run(a);
    assert!(r.audit("a certificate skipped"));
    let (key, sheet) = r.sheet().unwrap();
    let Sheet::Certificates { requester, sign, certs, .. } = sheet else { panic!("{sheet:?}") };
    assert_eq!((requester.as_str(), sign), ("Navigator", false));
    assert_eq!(certs.len(), 1, "the RSA certificate can't sign ECDSA");
    assert_eq!((certs[0].subject.as_str(), certs[0].token.as_str()), ("Asha Rao", "InnaitKey DSC"));
    // Nothing until a valid press; then the certificate, and no device work.
    assert!(r.b.press(key, Choice::Certificate(0), false).iter().all(|a| matches!(a, Action::Audit(_))));
    let a = r.b.press(key, Choice::Certificate(0), true);
    r.run(a);
    assert_eq!(r.reply(), Some(Reply::CertificateChosen { ceremony: 9, cert: ec }));
    assert!(r.to_driver().is_empty());
}

#[test]
fn sign_asks_the_pin_and_verifies_the_signature() {
    let mut r = Rig::new(true);
    let ec = ec_cert("Asha Rao");
    let digest: [u8; 32] = Sha256::digest(b"CertificateVerify").into();
    let a = r.b.request(1, &Label::Arca, sign(&ec, ECDSA, &digest), 0);
    r.run(a);
    let a = r.b.driver(TOKEN, FromDriver::Certificates { ceremony: 1, token_label: "DSC".into(), certs: vec![tc(&[9], "rsa", RSA_DER), tc(&[1, 2], "mine", &ec)] });
    r.run(a);
    let (key, sheet) = r.sheet().unwrap();
    assert!(matches!(&sheet, Sheet::Certificates { sign: true, certs, requester, .. } if certs.len() == 1 && requester == "Arca"));
    let a = r.b.press(key, Choice::Certificate(0), true);
    r.run(a);
    assert_eq!(r.to_driver(), vec![(TOKEN, ToDriver::SignDigest { ceremony: key, key_id: vec![1, 2], scheme: ECDSA, digest: digest.to_vec(), pin: None })]);

    let a = r.b.driver(TOKEN, FromDriver::NeedPin { ceremony: key, retries: 255 });
    r.run(a);
    assert!(r.out.contains(&Action::AskPin { key, device: TOKEN, retries: 255, wrong: false }));
    let a = r.b.pin(key, Pin(Zeroizing::new(b"0000".to_vec())));
    r.run(a);
    assert!(matches!(&r.to_driver()[..], [(TOKEN, ToDriver::SignDigest { pin: Some(_), .. })]));
    let a = r.b.driver(TOKEN, FromDriver::Error { ceremony: key, error: DriverError::PinInvalid { retries: 1 } });
    r.run(a);
    assert!(r.out.contains(&Action::AskPin { key, device: TOKEN, retries: 1, wrong: true }));
    let a = r.b.pin(key, Pin(Zeroizing::new(b"123456".to_vec())));
    r.run(a);
    let s: Signature = ec_key().sign_prehash(&digest).unwrap();
    let a = r.b.driver(TOKEN, FromDriver::Signed { ceremony: key, signature: s.to_bytes().to_vec() });
    r.run(a);
    assert_eq!(r.reply(), Some(Reply::CertificateSigned { ceremony: 9, signature: s.to_bytes().to_vec() }));
}

#[test]
fn a_signature_that_does_not_verify_is_refused() {
    let mut r = Rig::new(true);
    let digest = [7u8; 32];
    let a = r.b.request(1, &navigator(), sign(RSA_DER, SignScheme::RsaPkcs1(HashAlg::Sha256), &digest), 0);
    r.run(a);
    let a = r.b.driver(TOKEN, FromDriver::Certificates { ceremony: 1, token_label: "t".into(), certs: vec![tc(&[2], "rsa", RSA_DER)] });
    r.run(a);
    let (key, _) = r.sheet().unwrap();
    r.b.press(key, Choice::Certificate(0), true);
    // A real PKCS#1 signature, but over another digest.
    let a = r.b.driver(TOKEN, FromDriver::Signed { ceremony: key, signature: PKCS1_SIG.to_vec() });
    r.run(a);
    assert_eq!(r.reply(), Some(Reply::Refused { ceremony: 9, reason: Refusal::NotAllowed }));
    assert!(r.audit("does not verify"));

    // Over the right digest it passes.
    let mut r = Rig::new(true);
    let a = r.b.request(1, &navigator(), sign(RSA_DER, SignScheme::RsaPkcs1(HashAlg::Sha256), DIGEST), 0);
    r.run(a);
    let a = r.b.driver(TOKEN, FromDriver::Certificates { ceremony: 1, token_label: "t".into(), certs: vec![tc(&[2], "rsa", RSA_DER)] });
    r.run(a);
    let (key, _) = r.sheet().unwrap();
    r.b.press(key, Choice::Certificate(0), true);
    let a = r.b.driver(TOKEN, FromDriver::Signed { ceremony: key, signature: PKCS1_SIG.to_vec() });
    r.run(a);
    assert!(matches!(r.reply(), Some(Reply::CertificateSigned { .. })));
}

#[test]
fn who_may_ask() {
    let app = Label::App { user: "alice".into(), app: AppId { publisher: [1; 32], app_id: "com.example.mail".into() }, rp_ids: vec![] };
    for (label, want) in [(app, Refusal::Denied), (Label::Ostiarius, Refusal::Denied), (Label::Unknown, Refusal::UnknownPeer)] {
        let mut r = Rig::new(true);
        let a = r.b.request(1, &label, choose(vec![ECDSA]), 0);
        r.run(a);
        assert_eq!(r.reply(), Some(Reply::Refused { ceremony: 9, reason: want }));
        assert!(r.to_driver().is_empty());
    }
    let mut r = Rig::new(true);
    let bad = Request::ChooseCertificate(ChooseCertificate { ceremony: 9, timeout_ms: 0, purpose: CertPurpose::TlsClient { host: "10.0.0.1".into(), port: 443 }, schemes: vec![ECDSA] });
    let a = r.b.request(1, &navigator(), bad, 0);
    r.run(a);
    assert_eq!(r.reply(), Some(Reply::Refused { ceremony: 9, reason: Refusal::Malformed }));
    let a = r.b.request(1, &navigator(), choose(vec![]), 0);
    r.run(a);
    assert_eq!(r.reply(), Some(Reply::Refused { ceremony: 9, reason: Refusal::Malformed }));
    let a = r.b.request(1, &navigator(), sign(RSA_DER, ECDSA, &[0; 31]), 0);
    r.run(a);
    assert_eq!(r.reply(), Some(Reply::Refused { ceremony: 9, reason: Refusal::Malformed }));
}

#[test]
fn a_token_plugged_in_later_is_asked() {
    let mut r = Rig::new(false);
    let fido = r.b.driver(2, FromDriver::Hello { class: DeviceClass::Fido, vendor: 1, product: 1, name: "key".into(), fido: None });
    assert!(fido.iter().all(|a| matches!(a, Action::Audit(_))));
    let a = r.b.request(1, &navigator(), choose(vec![ECDSA]), 0);
    r.run(a);
    let (key, sheet) = r.sheet().unwrap();
    assert!(matches!(sheet, Sheet::Certificates { ref certs, .. } if certs.is_empty()), "plug in your token");
    assert!(r.to_driver().is_empty(), "a FIDO key isn't asked for certificates");
    let a = r.b.driver(TOKEN, FromDriver::Hello { class: DeviceClass::Ccid, vendor: 0x338c, product: 0x32, name: "InnaitKey DSC".into(), fido: None });
    r.run(a);
    assert_eq!(r.to_driver(), vec![(TOKEN, ToDriver::ListCertificates { ceremony: key })]);
    let a = r.b.driver(TOKEN, FromDriver::Certificates { ceremony: key, token_label: "t".into(), certs: vec![tc(&[1], "x", &ec_cert("Asha Rao"))] });
    r.run(a);
    assert!(matches!(r.sheet(), Some((_, Sheet::Certificates { certs, .. })) if certs.len() == 1));
    // Unplugged again: the sheet drops its certificate.
    let a = r.b.device_gone(TOKEN);
    r.run(a);
    assert!(matches!(r.sheet(), Some((_, Sheet::Certificates { certs, .. })) if certs.is_empty()));
    assert!(r.reply().is_none(), "the ceremony goes on");
}

#[test]
fn a_token_record_out_of_place_ends_the_ceremony() {
    let mut r = Rig::new(true);
    let a = r.b.request(1, &navigator(), sign(RSA_DER, SignScheme::RsaPkcs1(HashAlg::Sha256), DIGEST), 0);
    r.run(a);
    let a = r.b.driver(TOKEN, FromDriver::Certificates { ceremony: 1, token_label: "t".into(), certs: vec![tc(&[2], "rsa", RSA_DER)] });
    r.run(a);
    let (key, _) = r.sheet().unwrap();
    r.b.press(key, Choice::Certificate(0), true);
    let a = r.b.driver(TOKEN, FromDriver::Certificates { ceremony: key, token_label: "t".into(), certs: vec![] });
    r.run(a);
    assert_eq!(r.reply(), Some(Reply::Refused { ceremony: 9, reason: Refusal::NotAllowed }));
    assert!(r.audit("out of place"));
}
