//! Encrypting to recipients' certificates and decrypting with a content key
//! someone else unwraps (CMS `EnvelopedData`, RFC 8551 §3.3).
//!
//! Sending: AES-256-CBC with a fresh key, wrapped for each recipient with
//! RSA PKCS#1 v1.5 (what every mail client reads). Recipients with EC keys
//! (key agreement) aren't supported yet and are refused by name.
//! Reading: AES-128/256-CBC; the wrapped key for the person's certificate
//! is handed out to be unwrapped by the token or Arca, never here.

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use cms::cert::IssuerAndSerialNumber;
use cms::content_info::{CmsVersion, ContentInfo};
pub use cms::enveloped_data::EnvelopedData;
use cms::enveloped_data::{EncryptedContentInfo, KeyTransRecipientInfo, RecipientIdentifier, RecipientInfo, RecipientInfos};
use der::asn1::{Any, OctetString, SetOfVec};
use der::{Decode, Encode};
use spki::AlgorithmIdentifierOwned;

use crate::{ber, cert, oid, MAX_CMS};

/// Most recipients one message is encrypted to.
pub const MAX_RECIPIENTS: usize = 100;

fn e(x: impl std::fmt::Display) -> String {
    x.to_string()
}

/// Encrypt `content` (a whole MIME entity) to each of `recipients`
/// (certificates, DER; the sender's own among them so the Sent copy can be
/// read). Returns the DER for `application/pkcs7-mime; smime-type=enveloped-data`.
pub fn encrypt(content: &[u8], recipients: &[Vec<u8>]) -> Result<Vec<u8>, String> {
    use rand_core::{OsRng, RngCore};
    use rsa::pkcs1::DecodeRsaPublicKey;
    if recipients.is_empty() || recipients.len() > MAX_RECIPIENTS {
        return Err("no recipients to encrypt to".into());
    }
    let mut key = [0u8; 32];
    let mut iv = [0u8; 16];
    OsRng.fill_bytes(&mut key);
    OsRng.fill_bytes(&mut iv);
    let mut infos = Vec::new();
    for r in recipients {
        let c = cert::parse(r)?;
        let spki = &c.tbs_certificate.subject_public_key_info;
        if spki.algorithm.oid != oid::RSA {
            let who = cert::emails(&c).into_iter().next().unwrap_or_default();
            return Err(format!("{who}'s certificate has a kind of key mail can't encrypt to yet"));
        }
        let pk = rsa::RsaPublicKey::from_pkcs1_der(spki.subject_public_key.raw_bytes()).map_err(|x| format!("certificate key: {x}"))?;
        let wrapped = pk.encrypt(&mut OsRng, rsa::Pkcs1v15Encrypt, &key).map_err(e)?;
        infos.push(RecipientInfo::Ktri(KeyTransRecipientInfo {
            version: CmsVersion::V0,
            rid: RecipientIdentifier::IssuerAndSerialNumber(IssuerAndSerialNumber { issuer: c.tbs_certificate.issuer.clone(), serial_number: c.tbs_certificate.serial_number.clone() }),
            key_enc_alg: AlgorithmIdentifierOwned { oid: oid::RSA, parameters: Some(Any::from(der::asn1::Null)) },
            enc_key: OctetString::new(wrapped).map_err(e)?,
        }));
    }
    let ct = cbc::Encryptor::<aes::Aes256>::new(&key.into(), &iv.into()).encrypt_padded_vec_mut::<Pkcs7>(content);
    key.fill(0);
    let env = EnvelopedData {
        version: CmsVersion::V0,
        originator_info: None,
        recip_infos: RecipientInfos(SetOfVec::try_from(infos).map_err(e)?),
        encrypted_content: EncryptedContentInfo {
            content_type: oid::DATA,
            content_enc_alg: AlgorithmIdentifierOwned { oid: oid::AES256_CBC, parameters: Some(Any::encode_from(&OctetString::new(iv.to_vec()).map_err(e)?).map_err(e)?) },
            encrypted_content: Some(OctetString::new(ct).map_err(e)?),
        },
        unprotected_attrs: None,
    };
    ContentInfo { content_type: oid::ENVELOPED_DATA, content: Any::encode_from(&env).map_err(e)? }.to_der().map_err(e)
}

/// Read a CMS `ContentInfo` holding `EnvelopedData`, BER or DER.
pub fn enveloped_data(bytes: &[u8]) -> Result<EnvelopedData, String> {
    if bytes.len() > MAX_CMS {
        return Err("the encrypted message is too large".into());
    }
    let der = ber::to_der(bytes)?;
    let ci = ContentInfo::from_der(&der).map_err(|x| format!("encrypted message: {x}"))?;
    if ci.content_type != oid::ENVELOPED_DATA {
        return Err("not an encrypted message".into());
    }
    ci.content.decode_as::<EnvelopedData>().map_err(|x| format!("encrypted message: {x}"))
}

/// The content key wrapped for `my_cert` (DER), to be unwrapped (RSA
/// PKCS#1 v1.5) by whoever holds its private key. None when the message
/// wasn't encrypted to that certificate.
pub fn wrapped_key(env: &EnvelopedData, my_cert: &[u8]) -> Option<Vec<u8>> {
    let c = cert::parse(my_cert).ok()?;
    let recipient = recipients_for(env, &c).find(|k| k.key_enc_alg.oid == oid::RSA)?;
    Some(recipient.enc_key.as_bytes().to_vec())
}

fn recipients_for<'a>(env: &'a EnvelopedData, cert: &'a x509_cert::Certificate) -> impl Iterator<Item = &'a KeyTransRecipientInfo> {
    let tbs = &cert.tbs_certificate;
    let ski = tbs.extensions.as_ref()
        .and_then(|ext| ext.iter().find(|x| x.extn_id == der::asn1::ObjectIdentifier::new_unwrap("2.5.29.14")))
        .and_then(|x| x509_cert::ext::pkix::SubjectKeyIdentifier::from_der(x.extn_value.as_bytes()).ok());
    env.recip_infos.0.iter().filter_map(move |ri| {
        let RecipientInfo::Ktri(k) = ri else { return None };
        let mine = match &k.rid {
            RecipientIdentifier::IssuerAndSerialNumber(is) => is.issuer == tbs.issuer && is.serial_number == tbs.serial_number,
            RecipientIdentifier::SubjectKeyIdentifier(s) => ski.as_ref().is_some_and(|m| m.0 == s.0),
        };
        mine.then_some(k)
    })
}

fn oaep_sha256(alg: &AlgorithmIdentifierOwned) -> Option<()> {
    use der::asn1::OctetStringRef;
    use rsa::pkcs1::RsaOaepParams;
    use spki::AlgorithmIdentifierRef;

    fn sha256(alg: AlgorithmIdentifierRef<'_>) -> bool {
        alg.oid == oid::SHA256 && alg.parameters.map_or(true, |p| p == der::asn1::AnyRef::NULL)
    }
    if alg.oid != oid::RSA_OAEP {
        return None;
    }
    let p = alg.parameters.as_ref()?.decode_as::<RsaOaepParams<'_>>().ok()?;
    let empty = p.p_source.parameters?.decode_as::<OctetStringRef<'_>>().ok()?.as_bytes().is_empty();
    (sha256(p.hash) && p.mask_gen.oid == oid::MGF1 && sha256(p.mask_gen.parameters?)
        && p.p_source.oid == oid::PSPECIFIED && empty).then_some(())
}

fn rsa_public_key(c: &x509_cert::Certificate) -> Option<rsa::RsaPublicKey> {
    use rsa::{pkcs1::DecodeRsaPublicKey, traits::PublicKeyParts};

    let spki = &c.tbs_certificate.subject_public_key_info;
    // This profile uses conventional rsaEncryption certificates. Canonical
    // encoding below matches the public SPKI emitted by Arca from PKCS#8.
    if spki.algorithm.oid != oid::RSA || !spki.algorithm.parameters.as_ref().map_or(true, |p| p.is_null()) {
        return None;
    }
    let key = rsa::RsaPublicKey::from_pkcs1_der(spki.subject_public_key.as_bytes()?).ok()?;
    matches!(key.n().bits(), 2048 | 3072 | 4096).then_some(key)
}

/// The fingerprint Arca uses and the exact wrapped-key length for this
/// known certificate. Derive this from Mail’s certificate, not a worker reply.
pub fn oaep_recipient(my_cert: &[u8]) -> Option<([u8; 32], usize)> {
    rsa_recipient(my_cert)
}

fn rsa_recipient(my_cert: &[u8]) -> Option<([u8; 32], usize)> {
    use rsa::{pkcs8::EncodePublicKey, traits::PublicKeyParts};

    let key = rsa_public_key(&cert::parse(my_cert).ok()?)?;
    let spki = key.to_public_key_der().ok()?;
    Some((cert::sha256(spki.as_bytes()), key.size()))
}

/// A wrapped key Mail may send to Arca. Validate the CMS parameters and
/// all the supported content fields before making a private-key request.
pub fn oaep_wrapped_key(env: &EnvelopedData, my_cert: &[u8]) -> Option<(u8, Vec<u8>)> {
    use rsa::traits::PublicKeyParts;
    let eci = &env.encrypted_content;
    let key_bytes = match eci.content_enc_alg.oid {
        oid::AES128_CBC => 16,
        oid::AES256_CBC => 32,
        _ => return None,
    };
    let iv = eci.content_enc_alg.parameters.as_ref()?.decode_as::<der::asn1::OctetStringRef<'_>>().ok()?;
    let content = eci.encrypted_content.as_ref()?.as_bytes();
    if eci.content_type != oid::DATA || iv.as_bytes().len() != 16 || content.is_empty() || content.len() % 16 != 0 {
        return None;
    }
    let c = cert::parse(my_cert).ok()?;
    let size = rsa_public_key(&c)?.size();
    let mut matches = recipients_for(env, &c).filter(|k| k.key_enc_alg.oid == oid::RSA_OAEP);
    let recipient = matches.next()?;
    if matches.next().is_some() || recipient.enc_key.as_bytes().len() != size {
        return None;
    }
    oaep_sha256(&recipient.key_enc_alg)?;
    Some((key_bytes, recipient.enc_key.as_bytes().to_vec()))
}

/// A bounded, unambiguous legacy match for the selected certificate.
#[cfg(feature = "legacy-receive-prototype")]
pub fn legacy_recipient(
    env: &EnvelopedData,
    certificate: &[u8],
) -> Option<([u8; 32], usize, Vec<u8>)> {
    let (fingerprint, modulus) = rsa_recipient(certificate)?;
    let certificate = cert::parse(certificate).ok()?;
    let mut matching = recipients_for(env, &certificate);
    let recipient = matching.next()?;
    if matching.next().is_some()
        || recipient.key_enc_alg.oid != oid::RSA
        || recipient.key_enc_alg.parameters.as_ref()?.decode_as::<der::asn1::Null>().is_err()
        || recipient.enc_key.as_bytes().len() != modulus
        || env.recip_infos.0.len() > MAX_RECIPIENTS
    {
        return None;
    }
    let content = &env.encrypted_content;
    let key_bytes = match content.content_enc_alg.oid {
        oid::AES128_CBC => 16,
        oid::AES256_CBC => 32,
        _ => return None,
    };
    let iv = content.content_enc_alg.parameters.as_ref()?.decode_as::<der::asn1::OctetStringRef<'_>>().ok()?;
    let encrypted = content.encrypted_content.as_ref()?.as_bytes();
    if content.content_type != oid::DATA || iv.as_bytes().len() != 16
        || encrypted.is_empty() || encrypted.len() % 16 != 0
    {
        return None;
    }
    Some((fingerprint, key_bytes, recipient.enc_key.as_bytes().to_vec()))
}

/// Decrypt the content with the unwrapped content key.
pub fn decrypt(env: &EnvelopedData, key: &[u8]) -> Result<Vec<u8>, String> {
    let eci = &env.encrypted_content;
    let ct = eci.encrypted_content.as_ref().ok_or("the encrypted message is empty")?.as_bytes();
    let iv = eci.content_enc_alg.parameters.as_ref().and_then(|p| p.decode_as::<OctetString>().ok()).ok_or("the encrypted message has no IV")?;
    let iv: [u8; 16] = iv.as_bytes().try_into().map_err(|_| "the encrypted message's IV is wrong")?;
    let bad = |_| "the message couldn't be decrypted".to_string();
    match eci.content_enc_alg.oid {
        o if o == oid::AES256_CBC => {
            let k: [u8; 32] = key.try_into().map_err(|_| "the content key has the wrong length")?;
            cbc::Decryptor::<aes::Aes256>::new(&k.into(), &iv.into()).decrypt_padded_vec_mut::<Pkcs7>(ct).map_err(bad)
        }
        o if o == oid::AES128_CBC => {
            let k: [u8; 16] = key.try_into().map_err(|_| "the content key has the wrong length")?;
            cbc::Decryptor::<aes::Aes128>::new(&k.into(), &iv.into()).decrypt_padded_vec_mut::<Pkcs7>(ct).map_err(bad)
        }
        _ => Err("encrypted with a cipher mail can't read".into()),
    }
}

#[cfg(all(test, feature = "legacy-receive-prototype"))]
mod legacy_tests {
    use super::*;

    const CERT: &[u8] = include_bytes!("../tests/data/leaf.der");
    const ENVELOPE: &[u8] = include_bytes!("../tests/data/env.der");

    #[test]
    fn legacy_routes_only_an_unambiguous_legacy_recipient() {
        let mut envelope = enveloped_data(ENVELOPE).unwrap();
        assert!(legacy_recipient(&envelope, CERT).is_some());
        assert!(oaep_wrapped_key(&envelope, CERT).is_none());

        let RecipientInfo::Ktri(mut oaep) = envelope.recip_infos.0.iter().next().unwrap().clone() else {
            panic!("expected RSA key transport in the fixture");
        };
        oaep.key_enc_alg.oid = oid::RSA_OAEP;
        let mut mixed = envelope.recip_infos.0.clone();
        mixed.insert(RecipientInfo::Ktri(oaep.clone())).unwrap();
        envelope.recip_infos = RecipientInfos(mixed);
        assert!(legacy_recipient(&envelope, CERT).is_none());

        envelope.recip_infos = RecipientInfos(SetOfVec::try_from(vec![RecipientInfo::Ktri(oaep)]).unwrap());
        assert!(oaep_wrapped_key(&envelope, CERT).is_none(), "unsupported OAEP parameters");
        assert!(legacy_recipient(&envelope, CERT).is_none(), "OAEP failure cannot select legacy");
    }
}

#[cfg(test)]
mod oaep_tests {
    use super::*;
    use rsa::pkcs1::RsaOaepParams;
    use sha2::Sha256;

    #[test]
    fn oaep_requires_sha256_mgf1_sha256_and_the_empty_label() {
        let supported = RsaOaepParams::new::<Sha256>();
        let wrong_digest = RsaOaepParams::default();
        let wrong_mgf = RsaOaepParams { mask_gen: wrong_digest.mask_gen.clone(), ..supported.clone() };
        let wrong_label = RsaOaepParams::new_with_label::<Sha256>(b"other");
        for (p, accepted) in [(supported, true), (wrong_digest, false), (wrong_mgf, false), (wrong_label, false)] {
            let alg = AlgorithmIdentifierOwned { oid: oid::RSA_OAEP, parameters: Some(Any::encode_from(&p).unwrap()) };
            assert_eq!(oaep_sha256(&alg).is_some(), accepted);
        }
        assert!(oaep_sha256(&AlgorithmIdentifierOwned { oid: oid::RSA_OAEP, parameters: None }).is_none());
    }
}
