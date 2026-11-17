//! Making a detached signature around a signature someone else makes: the
//! token through Sigillum, or a key Arca holds. Only the 32-byte SHA-256
//! of the signed attributes goes to them.

use cms::cert::{CertificateChoices, IssuerAndSerialNumber};
use cms::content_info::{CmsVersion, ContentInfo};
use cms::signed_data::{CertificateSet, EncapsulatedContentInfo, SignedData, SignerIdentifier, SignerInfo, SignerInfos};
use der::asn1::{Any, OctetString, SetOfVec, UintRef, UtcTime};
use der::{Decode, Encode, Sequence};
use spki::AlgorithmIdentifierOwned;
use x509_cert::attr::Attribute;
use x509_cert::time::Time;

use crate::{cert, oid, MAX_CERTS};

/// How the key signs, read from the certificate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    /// RSASSA-PKCS1-v1_5 over SHA-256 (the signer wraps it in a DigestInfo).
    RsaPkcs1Sha256,
    /// ECDSA over SHA-256; the signer returns `r || s`.
    EcdsaSha256,
}

/// The scheme the certificate's key signs mail with.
pub fn scheme_for(signer_cert: &[u8]) -> Result<Scheme, String> {
    let c = cert::parse(signer_cert)?;
    let alg = c.tbs_certificate.subject_public_key_info.algorithm.oid;
    if alg == oid::RSA {
        Ok(Scheme::RsaPkcs1Sha256)
    } else if alg == oid::EC_PUBLIC_KEY {
        Ok(Scheme::EcdsaSha256)
    } else {
        Err("this certificate's key can't sign mail".into())
    }
}

#[derive(Sequence)]
struct EcdsaSig<'a> {
    r: UintRef<'a>,
    s: UintRef<'a>,
}

fn alg(o: der::asn1::ObjectIdentifier, null: bool) -> AlgorithmIdentifierOwned {
    AlgorithmIdentifierOwned { oid: o, parameters: null.then(|| Any::from(der::asn1::Null)) }
}

fn attribute(o: der::asn1::ObjectIdentifier, v: &impl Encode) -> Result<Attribute, String> {
    let any = Any::from_der(&v.to_der().map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    Ok(Attribute { oid: o, values: SetOfVec::try_from(vec![any]).map_err(|e| e.to_string())? })
}

/// Sign `content` (the MIME entity that becomes the first part of
/// `multipart/signed`, exactly as it will be sent) with the key of
/// `signer_cert`, `extra` certificates (its issuers) included so the
/// reader can build the chain. `sign` gets the digest to sign and returns
/// the signature. Returns the DER for `application/pkcs7-signature`.
pub fn detached(content: &[u8], signer_cert: &[u8], extra: &[Vec<u8>], signing_time: i64, sign: impl FnOnce(&[u8; 32]) -> Result<Vec<u8>, String>) -> Result<Vec<u8>, String> {
    let scheme = scheme_for(signer_cert)?;
    let signer = cert::parse(signer_cert)?;
    let mut digest = [0u8; 32];
    digest.copy_from_slice(ring::digest::digest(&ring::digest::SHA256, content).as_ref());

    let when = UtcTime::from_unix_duration(std::time::Duration::from_secs(signing_time.max(0) as u64)).map_err(|e| e.to_string())?;
    let attrs: SetOfVec<Attribute> = SetOfVec::try_from(vec![
        attribute(oid::CONTENT_TYPE, &oid::DATA)?,
        attribute(oid::SIGNING_TIME, &Time::UtcTime(when))?,
        attribute(oid::MESSAGE_DIGEST, &OctetString::new(digest.to_vec()).map_err(|e| e.to_string())?)?,
    ])
    .map_err(|e| e.to_string())?;
    let to_sign = attrs.to_der().map_err(|e| e.to_string())?;
    let mut d = [0u8; 32];
    d.copy_from_slice(ring::digest::digest(&ring::digest::SHA256, &to_sign).as_ref());
    let raw = sign(&d)?;
    let (sig_alg, sig) = match scheme {
        Scheme::RsaPkcs1Sha256 => (alg(oid::RSA, true), raw),
        Scheme::EcdsaSha256 => {
            if raw.len() != 64 {
                return Err("the token's signature has the wrong length".into());
            }
            let trim = |b: &[u8]| {
                let i = b.iter().position(|&x| x != 0).unwrap_or(b.len() - 1);
                b[i..].to_vec()
            };
            let (r, s) = (trim(&raw[..32]), trim(&raw[32..]));
            let der = EcdsaSig { r: UintRef::new(&r).map_err(|e| e.to_string())?, s: UintRef::new(&s).map_err(|e| e.to_string())? }.to_der().map_err(|e| e.to_string())?;
            (alg(oid::ECDSA_SHA256, false), der)
        }
    };
    // Check before sending: a wrong PIN slot or key gives a signature that
    // no reader would accept.
    cert::verify_sig(&signer.tbs_certificate.subject_public_key_info, &sig_alg.oid, Some(&oid::SHA256), &to_sign, &sig)?;

    let si = SignerInfo {
        version: CmsVersion::V1,
        sid: SignerIdentifier::IssuerAndSerialNumber(IssuerAndSerialNumber { issuer: signer.tbs_certificate.issuer.clone(), serial_number: signer.tbs_certificate.serial_number.clone() }),
        digest_alg: alg(oid::SHA256, false),
        signed_attrs: Some(attrs),
        signature_algorithm: sig_alg,
        signature: OctetString::new(sig).map_err(|e| e.to_string())?,
        unsigned_attrs: None,
    };
    let mut certs = vec![CertificateChoices::Certificate(signer)];
    for e in extra.iter().take(MAX_CERTS - 1) {
        certs.push(CertificateChoices::Certificate(x509_cert::Certificate::from_der(e).map_err(|e| format!("certificate: {e}"))?));
    }
    let sd = SignedData {
        version: CmsVersion::V1,
        digest_algorithms: SetOfVec::try_from(vec![alg(oid::SHA256, false)]).map_err(|e| e.to_string())?,
        encap_content_info: EncapsulatedContentInfo { econtent_type: oid::DATA, econtent: None },
        certificates: Some(CertificateSet(SetOfVec::try_from(certs).map_err(|e| e.to_string())?)),
        crls: None,
        signer_infos: SignerInfos(SetOfVec::try_from(vec![si]).map_err(|e| e.to_string())?),
    };
    let ci = ContentInfo { content_type: oid::SIGNED_DATA, content: Any::encode_from(&sd).map_err(|e| e.to_string())? };
    ci.to_der().map_err(|e| e.to_string())
}
