//! What a certificate says (read from the certificate itself), and checking
//! a signature made with its key.

use der::asn1::{Ia5StringRef, ObjectIdentifier};
use der::{Decode, Encode};
use ring::signature as rs;
use spki::SubjectPublicKeyInfoOwned;
use x509_cert::ext::pkix::name::GeneralName;
use x509_cert::ext::pkix::{BasicConstraints, SubjectAltName};
use x509_cert::name::Name;
use x509_cert::Certificate;

use crate::oid;

/// A certificate as the reader shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertInfo {
    /// The subject's common name, else its first address.
    pub name: String,
    /// Addresses the certificate is for (SAN `rfc822Name`, else the
    /// subject's `emailAddress`), lower case.
    pub emails: Vec<String>,
    /// Who issued it: the issuer's organisation, else its common name.
    pub issuer: String,
    pub not_before: i64,
    pub not_after: i64,
    pub sha256: [u8; 32],
}

/// The SHA-256 of a certificate's DER (how Sigillum names one).
pub fn sha256(der: &[u8]) -> [u8; 32] {
    let mut h = [0u8; 32];
    h.copy_from_slice(ring::digest::digest(&ring::digest::SHA256, der).as_ref());
    h
}

pub fn parse(der: &[u8]) -> Result<Certificate, String> {
    Certificate::from_der(der).map_err(|e| format!("certificate: {e}"))
}

fn attr(name: &Name, which: ObjectIdentifier) -> Option<String> {
    for rdn in name.0.iter() {
        for a in rdn.0.iter() {
            if a.oid == which {
                let v = &a.value;
                if let Ok(s) = v.decode_as::<der::asn1::Utf8StringRef>() {
                    return Some(s.as_str().to_string());
                }
                if let Ok(s) = v.decode_as::<der::asn1::PrintableStringRef>() {
                    return Some(s.as_str().to_string());
                }
                if let Ok(s) = v.decode_as::<Ia5StringRef>() {
                    return Some(s.as_str().to_string());
                }
                if let Ok(s) = v.decode_as::<der::asn1::TeletexStringRef>() {
                    return Some(s.as_str().to_string());
                }
            }
        }
    }
    None
}

fn clean(s: &str) -> String {
    s.chars().filter(|c| !c.is_control() && !matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')).take(128).collect()
}

/// The addresses the certificate is for.
pub fn emails(c: &Certificate) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(exts) = &c.tbs_certificate.extensions {
        for e in exts.iter().filter(|e| e.extn_id == oid::SAN) {
            if let Ok(san) = SubjectAltName::from_der(e.extn_value.as_bytes()) {
                for g in san.0 {
                    if let GeneralName::Rfc822Name(m) = g {
                        out.push(m.as_str().to_ascii_lowercase());
                    }
                }
            }
        }
    }
    if out.is_empty() {
        if let Some(m) = attr(&c.tbs_certificate.subject, oid::EMAIL) {
            out.push(m.to_ascii_lowercase());
        }
    }
    out.truncate(16);
    out.iter().map(|m| clean(m)).collect()
}

pub fn is_ca(c: &Certificate) -> bool {
    c.tbs_certificate.extensions.as_ref().is_some_and(|exts| {
        exts.iter().filter(|e| e.extn_id == oid::BASIC_CONSTRAINTS).any(|e| BasicConstraints::from_der(e.extn_value.as_bytes()).is_ok_and(|b| b.ca))
    })
}

pub fn validity(c: &Certificate) -> (i64, i64) {
    let v = &c.tbs_certificate.validity;
    (v.not_before.to_unix_duration().as_secs() as i64, v.not_after.to_unix_duration().as_secs() as i64)
}

pub fn info(c: &Certificate) -> CertInfo {
    let emails = emails(c);
    let name = attr(&c.tbs_certificate.subject, oid::CN).map(|s| clean(&s)).or_else(|| emails.first().cloned()).unwrap_or_default();
    let issuer = attr(&c.tbs_certificate.issuer, oid::O_).or_else(|| attr(&c.tbs_certificate.issuer, oid::CN)).map(|s| clean(&s)).unwrap_or_default();
    let (not_before, not_after) = validity(c);
    let der = c.to_der().unwrap_or_default();
    let mut sha256 = [0u8; 32];
    sha256.copy_from_slice(ring::digest::digest(&ring::digest::SHA256, &der).as_ref());
    CertInfo { name, emails, issuer, not_before, not_after, sha256 }
}

/// Whether `sig` over `msg` was made with `key`, by signature algorithm
/// `alg` (or a key algorithm with `digest`, as CMS allows).
pub fn verify_sig(key: &SubjectPublicKeyInfoOwned, alg: &ObjectIdentifier, digest: Option<&ObjectIdentifier>, msg: &[u8], sig: &[u8]) -> Result<(), String> {
    let hash = match *alg {
        a if a == oid::RSA_SHA256 || a == oid::ECDSA_SHA256 => oid::SHA256,
        a if a == oid::RSA_SHA384 || a == oid::ECDSA_SHA384 => oid::SHA384,
        a if a == oid::RSA_SHA512 || a == oid::ECDSA_SHA512 => oid::SHA512,
        a if a == oid::RSA || a == oid::EC_PUBLIC_KEY || a == oid::RSA_PSS => *digest.ok_or("no digest algorithm")?,
        _ => return Err("an unknown signature algorithm".into()),
    };
    if hash == oid::SHA1 {
        return Err("signed with SHA-1, which is no longer safe".into());
    }
    let pk = key.subject_public_key.raw_bytes();
    let algo: &dyn rs::VerificationAlgorithm = if key.algorithm.oid == oid::RSA {
        let pss = *alg == oid::RSA_PSS;
        match (hash, pss) {
            (h, false) if h == oid::SHA256 => &rs::RSA_PKCS1_2048_8192_SHA256,
            (h, false) if h == oid::SHA384 => &rs::RSA_PKCS1_2048_8192_SHA384,
            (h, false) if h == oid::SHA512 => &rs::RSA_PKCS1_2048_8192_SHA512,
            (h, true) if h == oid::SHA256 => &rs::RSA_PSS_2048_8192_SHA256,
            (h, true) if h == oid::SHA384 => &rs::RSA_PSS_2048_8192_SHA384,
            (h, true) if h == oid::SHA512 => &rs::RSA_PSS_2048_8192_SHA512,
            _ => return Err("an unknown digest".into()),
        }
    } else if key.algorithm.oid == oid::EC_PUBLIC_KEY {
        let curve = key.algorithm.parameters.as_ref().and_then(|p| p.decode_as::<ObjectIdentifier>().ok()).ok_or("an EC key without a curve")?;
        match (curve, hash) {
            (c, h) if c == oid::P256 && h == oid::SHA256 => &rs::ECDSA_P256_SHA256_ASN1,
            (c, h) if c == oid::P256 && h == oid::SHA384 => &rs::ECDSA_P256_SHA384_ASN1,
            (c, h) if c == oid::P384 && h == oid::SHA384 => &rs::ECDSA_P384_SHA384_ASN1,
            (c, h) if c == oid::P384 && h == oid::SHA256 => &rs::ECDSA_P384_SHA256_ASN1,
            _ => return Err("an unsupported curve".into()),
        }
    } else {
        return Err("an unsupported key".into());
    };
    rs::UnparsedPublicKey::new(algo, pk).verify(msg, sig).map_err(|_| "the signature doesn't match".to_string())
}

/// Whether `child` was issued by `parent` (names and signature).
pub fn issued_by(child: &Certificate, parent: &Certificate) -> bool {
    if child.tbs_certificate.issuer != parent.tbs_certificate.subject {
        return false;
    }
    let Ok(tbs) = child.tbs_certificate.to_der() else { return false };
    verify_sig(&parent.tbs_certificate.subject_public_key_info, &child.signature_algorithm.oid, None, &tbs, child.signature.raw_bytes()).is_ok()
}
