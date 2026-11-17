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
use cms::enveloped_data::{EncryptedContentInfo, EnvelopedData, KeyTransRecipientInfo, RecipientIdentifier, RecipientInfo, RecipientInfos};
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
    let ski = c.tbs_certificate.extensions.as_ref().and_then(|x| x.iter().find(|x| x.extn_id == der::asn1::ObjectIdentifier::new_unwrap("2.5.29.14"))).and_then(|x| x509_cert::ext::pkix::SubjectKeyIdentifier::from_der(x.extn_value.as_bytes()).ok());
    env.recip_infos.0.iter().find_map(|ri| match ri {
        RecipientInfo::Ktri(k) if k.key_enc_alg.oid == oid::RSA => {
            let mine = match &k.rid {
                RecipientIdentifier::IssuerAndSerialNumber(is) => is.issuer == c.tbs_certificate.issuer && is.serial_number == c.tbs_certificate.serial_number,
                RecipientIdentifier::SubjectKeyIdentifier(s) => ski.as_ref().is_some_and(|m| m.0 == s.0),
            };
            mine.then(|| k.enc_key.as_bytes().to_vec())
        }
        _ => None,
    })
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
