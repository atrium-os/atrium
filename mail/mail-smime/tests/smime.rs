//! Sign and verify with a test CA (P-256 and RSA), and encrypt and decrypt.

use mail_smime::verify::{self, State};
use mail_smime::{envelope, sign};
use p256::ecdsa::signature::hazmat::PrehashSigner;
use p256::pkcs8::DecodePrivateKey;
use rcgen::{BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair, SanType};

const NOW: i64 = 1_790_000_000; // 2026-09
const CONTENT: &[u8] = b"Content-Type: text/plain; charset=utf-8\r\n\r\nHello there.\r\n";

struct Ca {
    cert: rcgen::Certificate,
    key: KeyPair,
}

fn ca(name: &str) -> Ca {
    let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let mut p = CertificateParams::new(Vec::<String>::new()).unwrap();
    p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, name);
    dn.push(DnType::OrganizationName, format!("{name} Org"));
    p.distinguished_name = dn;
    Ca { cert: p.self_signed(&key).unwrap(), key }
}

fn leaf(issuer: &Ca, email: &str, key: &KeyPair, eku: ExtendedKeyUsagePurpose, expired: bool) -> Vec<u8> {
    let mut p = CertificateParams::new(Vec::<String>::new()).unwrap();
    p.subject_alt_names = vec![SanType::Rfc822Name(email.try_into().unwrap())];
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "Ada Lovelace");
    p.distinguished_name = dn;
    p.extended_key_usages = vec![eku];
    if expired {
        p.not_before = rcgen::date_time_ymd(2019, 1, 1);
        p.not_after = rcgen::date_time_ymd(2020, 1, 1);
    }
    p.signed_by(key, &issuer.cert, &issuer.key).unwrap().der().to_vec()
}

fn ec_signer(key: &KeyPair) -> impl FnOnce(&[u8; 32]) -> Result<Vec<u8>, String> + '_ {
    move |d| {
        let sk = p256::ecdsa::SigningKey::from_pkcs8_der(&key.serialize_der()).unwrap();
        let sig: p256::ecdsa::Signature = sk.sign_prehash(d).unwrap();
        Ok(sig.to_bytes().to_vec())
    }
}

#[test]
fn ec_signature_round_trip() {
    let root = ca("Test Root");
    let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let cert = leaf(&root, "ada@example.org", &key, ExtendedKeyUsagePurpose::EmailProtection, false);
    let sig = sign::detached(CONTENT, &cert, &[], NOW - 60, ec_signer(&key)).unwrap();
    let roots = vec![root.cert.der().to_vec()];

    let c = verify::detached(CONTENT, &sig, &roots, NOW, "Ada@Example.org");
    assert_eq!(c.state, State::Valid, "{c:?}");
    let s = c.signer.unwrap();
    assert_eq!(s.name, "Ada Lovelace");
    assert_eq!(s.emails, vec!["ada@example.org".to_string()]);
    assert_eq!(s.issuer, "Test Root Org");
    assert_eq!(c.signer_cert.as_deref(), Some(&cert[..]));
    assert_eq!(c.chain.len(), 1);
    assert_eq!(c.signed_at, Some(NOW - 60));

    // Changed after signing.
    let mut changed = CONTENT.to_vec();
    changed[50] ^= 1;
    assert!(matches!(verify::detached(&changed, &sig, &roots, NOW, "ada@example.org").state, State::Invalid(_)));
    // Someone else's address.
    assert_eq!(verify::detached(CONTENT, &sig, &roots, NOW, "eve@example.org").state, State::WrongAddress(vec!["ada@example.org".into()]));
    // A root this system doesn't trust.
    let other = ca("Other Root");
    assert!(matches!(verify::detached(CONTENT, &sig, &[other.cert.der().to_vec()], NOW, "ada@example.org").state, State::NotTrusted(_)));
    // Garbage.
    assert!(matches!(verify::detached(CONTENT, b"\x30\x03\x02\x01\x00", &roots, NOW, "ada@example.org").state, State::Invalid(_)));
}

#[test]
fn intermediate_expired_and_wrong_usage() {
    let root = ca("Test Root");
    let ikey = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let mut ip = CertificateParams::new(Vec::<String>::new()).unwrap();
    ip.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ip.distinguished_name.push(DnType::CommonName, "Test Mail CA");
    let inter_cert = ip.signed_by(&ikey, &root.cert, &root.key).unwrap();
    let inter = Ca { cert: inter_cert, key: ikey };
    let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let cert = leaf(&inter, "ada@example.org", &key, ExtendedKeyUsagePurpose::EmailProtection, false);
    let roots = vec![root.cert.der().to_vec()];

    // Without the intermediate the chain stops short.
    let sig = sign::detached(CONTENT, &cert, &[], NOW, ec_signer(&key)).unwrap();
    assert!(matches!(verify::detached(CONTENT, &sig, &roots, NOW, "ada@example.org").state, State::NotTrusted(_)));
    let sig = sign::detached(CONTENT, &cert, &[inter.cert.der().to_vec()], NOW, ec_signer(&key)).unwrap();
    let c = verify::detached(CONTENT, &sig, &roots, NOW, "ada@example.org");
    assert_eq!(c.state, State::Valid, "{c:?}");
    assert_eq!(c.chain.len(), 2);

    // Expired, judged at the signing time: good if signed while valid.
    let cert = leaf(&root, "ada@example.org", &key, ExtendedKeyUsagePurpose::EmailProtection, true);
    let sig = sign::detached(CONTENT, &cert, &[], 1_560_000_000, ec_signer(&key)).unwrap();
    assert_eq!(verify::detached(CONTENT, &sig, &roots, NOW, "ada@example.org").state, State::Valid);
    let sig = sign::detached(CONTENT, &cert, &[], NOW, ec_signer(&key)).unwrap();
    assert!(matches!(verify::detached(CONTENT, &sig, &roots, NOW, "ada@example.org").state, State::NotTrusted(_)));

    // A TLS-only certificate isn't for mail.
    let cert = leaf(&root, "ada@example.org", &key, ExtendedKeyUsagePurpose::ServerAuth, false);
    let sig = sign::detached(CONTENT, &cert, &[], NOW, ec_signer(&key)).unwrap();
    assert!(matches!(verify::detached(CONTENT, &sig, &roots, NOW, "ada@example.org").state, State::NotTrusted(_)));

    // The wrong key's signature is caught before sending.
    let other = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let cert = leaf(&root, "ada@example.org", &key, ExtendedKeyUsagePurpose::EmailProtection, false);
    assert!(sign::detached(CONTENT, &cert, &[], NOW, ec_signer(&other)).is_err());
}

#[test]
fn rsa_sign_encrypt_decrypt() {
    use rsa::pkcs8::EncodePrivateKey;
    let root = ca("Test Root");
    let rsa_key = rsa::RsaPrivateKey::new(&mut rand_core::OsRng, 2048).unwrap();
    let pkcs8 = rsa_key.to_pkcs8_der().unwrap();
    let kp = KeyPair::from_pkcs8_der_and_sign_algo(&pkcs8.as_bytes().into(), &rcgen::PKCS_RSA_SHA256).unwrap();
    let cert = leaf(&root, "ada@example.org", &kp, ExtendedKeyUsagePurpose::EmailProtection, false);
    let roots = vec![root.cert.der().to_vec()];
    assert_eq!(sign::scheme_for(&cert).unwrap(), sign::Scheme::RsaPkcs1Sha256);

    // A token signs the DigestInfo; here rsa does the same.
    let sig = sign::detached(CONTENT, &cert, &[], NOW, |d| rsa_key.sign(rsa::Pkcs1v15Sign::new::<sha2::Sha256>(), d).map_err(|e| e.to_string())).unwrap();
    assert_eq!(verify::detached(CONTENT, &sig, &roots, NOW, "ada@example.org").state, State::Valid);

    // Encrypt to Ada; an EC recipient is refused by name.
    let env = envelope::encrypt(CONTENT, &[cert.clone()]).unwrap();
    let ec_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let ec_cert = leaf(&root, "bob@example.org", &ec_key, ExtendedKeyUsagePurpose::EmailProtection, false);
    let e = envelope::encrypt(CONTENT, &[cert.clone(), ec_cert.clone()]).unwrap_err();
    assert!(e.contains("bob@example.org"), "{e}");

    let ed = envelope::enveloped_data(&env).unwrap();
    assert!(envelope::wrapped_key(&ed, &ec_cert).is_none());
    let wrapped = envelope::wrapped_key(&ed, &cert).unwrap();
    let cek = rsa_key.decrypt(rsa::Pkcs1v15Encrypt, &wrapped).unwrap();
    assert_eq!(envelope::decrypt(&ed, &cek).unwrap(), CONTENT);
    assert!(envelope::decrypt(&ed, &[0u8; 32]).map(|p| p != CONTENT).unwrap_or(true));
}

/// What another implementation sends: OpenSSL's streamed (indefinite
/// length BER) signature and envelope.
#[test]
fn openssl_ber_fixtures() {
    use rsa::pkcs8::DecodePrivateKey;
    let d = |f: &str| std::fs::read(format!("{}/tests/data/{f}", env!("CARGO_MANIFEST_DIR"))).unwrap();
    let content = d("content.txt");
    let roots = vec![d("ca.der")];
    let at = 1_800_000_000;
    let c = verify::detached(&content, &d("sig.der"), &roots, at, "grace@example.net");
    assert_eq!(c.state, State::Valid, "{c:?}");
    assert_eq!(c.signer.unwrap().name, "Grace Hopper");
    assert!(matches!(verify::detached(b"other", &d("sig.der"), &roots, at, "grace@example.net").state, State::Invalid(_)));

    let key = rsa::RsaPrivateKey::from_pkcs8_der(&d("leaf.p8")).unwrap();
    let ed = envelope::enveloped_data(&d("env.der")).unwrap();
    let cek = key.decrypt(rsa::Pkcs1v15Encrypt, &envelope::wrapped_key(&ed, &d("leaf.der")).unwrap()).unwrap();
    assert_eq!(envelope::decrypt(&ed, &cek).unwrap(), content);
}

/// Writes what we make for an outside check (`openssl cms -verify`/`-decrypt`);
/// run by hand with MAIL_SMIME_OUT set.
#[test]
fn write_for_openssl() {
    use rsa::pkcs8::DecodePrivateKey;
    let Ok(dir) = std::env::var("MAIL_SMIME_OUT") else { return };
    let d = |f: &str| std::fs::read(format!("{}/tests/data/{f}", env!("CARGO_MANIFEST_DIR"))).unwrap();
    let key = rsa::RsaPrivateKey::from_pkcs8_der(&d("leaf.p8")).unwrap();
    let sig = sign::detached(CONTENT, &d("leaf.der"), &[], NOW, |h| key.sign(rsa::Pkcs1v15Sign::new::<sha2::Sha256>(), h).map_err(|e| e.to_string())).unwrap();
    std::fs::write(format!("{dir}/ours.sig"), sig).unwrap();
    std::fs::write(format!("{dir}/ours.txt"), CONTENT).unwrap();
    std::fs::write(format!("{dir}/ours.env"), envelope::encrypt(CONTENT, &[d("leaf.der")]).unwrap()).unwrap();
}
