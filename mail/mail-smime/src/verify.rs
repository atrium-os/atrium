//! Checking a detached signature (`multipart/signed`, RFC 8551 §3.5): the
//! signature itself, the chain from the signer's certificate to a root the
//! caller trusts, and that the certificate is for the address in From.
//! Runs in the parse jail; nothing here touches a file or the network.

use cms::cert::CertificateChoices;
use cms::content_info::ContentInfo;
use cms::signed_data::{SignedData, SignerIdentifier, SignerInfo};
use der::asn1::{ObjectIdentifier, OctetString};
use der::{Decode, Encode};
use x509_cert::ext::pkix::{ExtendedKeyUsage, SubjectKeyIdentifier};
use x509_cert::time::Time;
use x509_cert::Certificate;

use crate::cert::{self, CertInfo};
use crate::{ber, oid, MAX_CERTS, MAX_CMS};

/// What the check found, most important first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Signed by a certificate for the From address that chains to a
    /// trusted root, and the message is as it was signed.
    Valid,
    /// The signature matches, but the certificate doesn't chain to a root
    /// this system trusts (or has expired); why, for the details sheet.
    NotTrusted(String),
    /// The message was changed after it was signed, or the signature can't
    /// be read; why.
    Invalid(String),
    /// Signed and trusted, but by a certificate for other addresses.
    WrongAddress(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub state: State,
    /// The signer's certificate, when it was found.
    pub signer: Option<CertInfo>,
    /// Its DER: what later encryption to this sender uses.
    pub signer_cert: Option<Vec<u8>>,
    /// The issuers above the signer, nearest first, as far as they were found.
    pub chain: Vec<CertInfo>,
    /// When the signature says it was made (unix seconds), if it says.
    pub signed_at: Option<i64>,
}

impl Check {
    fn invalid(why: impl Into<String>) -> Check {
        Check { state: State::Invalid(why.into()), signer: None, signer_cert: None, chain: Vec::new(), signed_at: None }
    }
}

const EMAIL_PROTECTION: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.6.1.5.5.7.3.4");
const ANY_EKU: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.5.29.37.0");
const EKU: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.5.29.37");
const SKI: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.5.29.14");
/// Longest chain followed.
const MAX_DEPTH: usize = 8;

/// Read a CMS `ContentInfo` holding `SignedData`, BER or DER.
pub fn signed_data(cms_bytes: &[u8]) -> Result<SignedData, String> {
    if cms_bytes.len() > MAX_CMS {
        return Err("the signature is too large".into());
    }
    let der = ber::to_der(cms_bytes)?;
    let ci = ContentInfo::from_der(&der).map_err(|e| format!("signature: {e}"))?;
    if ci.content_type != oid::SIGNED_DATA {
        return Err("not a signature".into());
    }
    ci.content.decode_as::<SignedData>().map_err(|e| format!("signature: {e}"))
}

fn digest_of(alg: &ObjectIdentifier, data: &[u8]) -> Option<Vec<u8>> {
    let a = match *alg {
        a if a == oid::SHA256 => &ring::digest::SHA256,
        a if a == oid::SHA384 => &ring::digest::SHA384,
        a if a == oid::SHA512 => &ring::digest::SHA512,
        _ => return None,
    };
    Some(ring::digest::digest(a, data).as_ref().to_vec())
}

fn ski(c: &Certificate) -> Option<Vec<u8>> {
    let exts = c.tbs_certificate.extensions.as_ref()?;
    let e = exts.iter().find(|e| e.extn_id == SKI)?;
    SubjectKeyIdentifier::from_der(e.extn_value.as_bytes()).ok().map(|s| s.0.as_bytes().to_vec())
}

fn is_signer(sid: &SignerIdentifier, c: &Certificate) -> bool {
    match sid {
        SignerIdentifier::IssuerAndSerialNumber(is) => is.issuer == c.tbs_certificate.issuer && is.serial_number == c.tbs_certificate.serial_number,
        SignerIdentifier::SubjectKeyIdentifier(k) => ski(c).is_some_and(|s| s == k.0.as_bytes()),
    }
}

/// Whether the certificate may sign mail: no extended key usage, or one
/// that includes email protection.
fn for_mail(c: &Certificate) -> bool {
    let Some(exts) = &c.tbs_certificate.extensions else { return true };
    let Some(e) = exts.iter().find(|e| e.extn_id == EKU) else { return true };
    ExtendedKeyUsage::from_der(e.extn_value.as_bytes()).is_ok_and(|u| u.0.iter().any(|o| *o == EMAIL_PROTECTION || *o == ANY_EKU))
}

fn time_of(t: &Time) -> i64 {
    t.to_unix_duration().as_secs() as i64
}

/// The one attribute `which` among the signed attributes, if exactly one
/// value is given.
fn attr<'a>(si: &'a SignerInfo, which: ObjectIdentifier) -> Option<&'a der::Any> {
    let a = si.signed_attrs.as_ref()?.iter().find(|a| a.oid == which)?;
    if a.values.len() != 1 {
        return None;
    }
    a.values.iter().next()
}

/// Check `signature` (the `application/pkcs7-signature` part) over
/// `content` (the first part of `multipart/signed`, its headers and body
/// exactly as sent) against `roots` (DER) at `now`, for a message from
/// `from`.
pub fn detached(content: &[u8], signature: &[u8], roots: &[Vec<u8>], now: i64, from: &str) -> Check {
    let sd = match signed_data(signature) {
        Ok(sd) => sd,
        Err(e) => return Check::invalid(e),
    };
    if sd.encap_content_info.econtent.is_some() {
        return Check::invalid("not a detached signature");
    }
    let mut certs: Vec<(Certificate, Vec<u8>)> = Vec::new();
    for c in sd.certificates.iter().flat_map(|s| s.0.iter()).take(MAX_CERTS) {
        if let CertificateChoices::Certificate(c) = c {
            if let Ok(der) = c.to_der() {
                certs.push((c.clone(), der));
            }
        }
    }
    // One signer is what mail sends; with several, the first one whose
    // certificate is here decides.
    let Some((si, (signer, signer_der))) = sd.signer_infos.0.iter().find_map(|si| certs.iter().find(|(c, _)| is_signer(&si.sid, c)).map(|c| (si, c.clone()))) else {
        return Check::invalid("the signer's certificate isn't included");
    };
    let mut check = Check { state: State::Valid, signer: Some(cert::info(&signer)), signer_cert: Some(signer_der.clone()), chain: Vec::new(), signed_at: None };

    // The signature itself.
    let Some(digest) = digest_of(&si.digest_alg.oid, content) else {
        check.state = State::Invalid(if si.digest_alg.oid == oid::SHA1 { "signed with SHA-1, which is no longer safe".into() } else { "an unknown digest".into() });
        return check;
    };
    let signed_bytes = match &si.signed_attrs {
        Some(attrs) => {
            match attr(si, oid::CONTENT_TYPE).and_then(|v| v.decode_as::<ObjectIdentifier>().ok()) {
                Some(t) if t == oid::DATA => {}
                _ => {
                    check.state = State::Invalid("the signature's content type is wrong".into());
                    return check;
                }
            }
            match attr(si, oid::MESSAGE_DIGEST).and_then(|v| v.decode_as::<OctetString>().ok()) {
                Some(d) if d.as_bytes() == digest.as_slice() => {}
                _ => {
                    check.state = State::Invalid("the message was changed after it was signed".into());
                    return check;
                }
            }
            check.signed_at = attr(si, oid::SIGNING_TIME).and_then(|v| v.to_der().ok().and_then(|b| Time::from_der(&b).ok())).map(|t| time_of(&t));
            match attrs.to_der() {
                Ok(b) => b,
                Err(e) => {
                    check.state = State::Invalid(format!("signature: {e}"));
                    return check;
                }
            }
        }
        None => content.to_vec(),
    };
    if let Err(e) = cert::verify_sig(&signer.tbs_certificate.subject_public_key_info, &si.signature_algorithm.oid, Some(&si.digest_alg.oid), &signed_bytes, si.signature.as_bytes()) {
        check.state = State::Invalid(if e == "the signature doesn't match" { "the message was changed after it was signed".into() } else { e });
        return check;
    }

    // The chain. Certificates are judged at the time the signature says it
    // was made, unless that is in the future: a signature made while the
    // certificate was good stays good after it expires.
    let at = check.signed_at.filter(|&t| t <= now + 300).unwrap_or(now);
    let roots: Vec<Certificate> = roots.iter().filter_map(|r| cert::parse(r).ok()).collect();
    let valid_at = |c: &Certificate| {
        let (nb, na) = cert::validity(c);
        nb <= at && at <= na
    };
    let mut trust: Result<(), String> = Ok(());
    if !valid_at(&signer) {
        trust = Err("the certificate had expired or wasn't yet valid".into());
    } else if !for_mail(&signer) {
        trust = Err("the certificate isn't for mail".into());
    }
    let mut cur = signer.clone();
    let mut found_root = false;
    for _ in 0..MAX_DEPTH {
        if let Some(r) = roots.iter().find(|r| r.tbs_certificate == cur.tbs_certificate || cert::issued_by(&cur, r)) {
            if r.tbs_certificate != cur.tbs_certificate {
                check.chain.push(cert::info(r));
            }
            if trust.is_ok() && !valid_at(r) {
                trust = Err("the issuer's certificate had expired".into());
            }
            found_root = true;
            break;
        }
        let Some((next, _)) = certs.iter().find(|(c, _)| c.tbs_certificate != cur.tbs_certificate && cert::is_ca(c) && cert::issued_by(&cur, c)) else {
            break;
        };
        check.chain.push(cert::info(next));
        if trust.is_ok() && !valid_at(next) {
            trust = Err("an issuer's certificate had expired".into());
        }
        cur = next.clone();
    }
    if !found_root && trust.is_ok() {
        trust = Err(if cert::issued_by(&cur, &cur) { "the certificate is self-signed".into() } else { "the issuer isn't trusted on this system".into() });
    }
    if let Err(why) = trust {
        check.state = State::NotTrusted(why);
        return check;
    }

    let from = from.trim().to_ascii_lowercase();
    let emails = check.signer.as_ref().map(|s| s.emails.clone()).unwrap_or_default();
    if !emails.iter().any(|e| *e == from) {
        check.state = State::WrongAddress(emails);
    }
    check
}
