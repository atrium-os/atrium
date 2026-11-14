//! What the share server tells about a server's TLS certificate (S2): the
//! digest of its public key, which a self-signed server is pinned by, and
//! names and dates for the user to read. A minimal DER walk over the
//! fields that say so; the certificate's signature and chain are rustls's
//! (webpki's) business, never this module's.

use sha2::{Digest, Sha256};

/// One DER element: (tag, contents, rest).
fn tlv(b: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let tag = *b.first()?;
    let l0 = *b.get(1)?;
    let (len, hdr) = if l0 < 0x80 {
        (l0 as usize, 2)
    } else {
        let n = (l0 & 0x7f) as usize;
        if n == 0 || n > 4 {
            return None;
        }
        let mut len = 0usize;
        for i in 0..n {
            len = (len << 8) | *b.get(2 + i)? as usize;
        }
        (len, 2 + n)
    };
    let end = hdr.checked_add(len)?;
    Some((tag, b.get(hdr..end)?, b.get(end..)?))
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Summary {
    /// SHA-256 of the SubjectPublicKeyInfo (what a pin holds).
    pub spki_sha256: [u8; 32],
    pub subject: String,
    pub issuer: String,
    /// As the certificate writes it (UTCTime or GeneralizedTime).
    pub not_after: String,
}

const CN: &[u8] = &[0x55, 0x04, 0x03];
const O: &[u8] = &[0x55, 0x04, 0x0a];

/// A Name's common name, else its organisation; printable characters only.
fn name_text(name: &[u8]) -> String {
    let mut cn = None;
    let mut org = None;
    let mut rdns = name;
    while let Some((0x31, set, rest)) = tlv(rdns) {
        rdns = rest;
        let mut atvs = set;
        while let Some((0x30, atv, more)) = tlv(atvs) {
            atvs = more;
            let Some((0x06, oid, v)) = tlv(atv) else { continue };
            let Some((_, val, _)) = tlv(v) else { continue };
            let s: String = String::from_utf8_lossy(val).chars().filter(|c| !c.is_control()).take(128).collect();
            if oid == CN {
                cn = Some(s);
            } else if oid == O {
                org = Some(s);
            }
        }
    }
    cn.or(org).unwrap_or_default()
}

/// Read a certificate (DER).
pub fn summary(der: &[u8]) -> Option<Summary> {
    let (0x30, cert, _) = tlv(der)? else { return None };
    let (0x30, tbs, _) = tlv(cert)? else { return None };
    let mut f = tbs;
    // [0] version, if there.
    if f.first() == Some(&0xa0) {
        f = tlv(f)?.2;
    }
    let (_, _serial, f) = tlv(f)?;
    let (_, _sig, f) = tlv(f)?;
    let (0x30, issuer, f) = tlv(f)? else { return None };
    let (0x30, validity, f) = tlv(f)? else { return None };
    let (0x30, subject, f) = tlv(f)? else { return None };
    // The SPKI, whole (tag and length included), as it is hashed.
    let (0x30, _, after) = tlv(f)? else { return None };
    let spki = &f[..f.len() - after.len()];
    let (_, _not_before, v) = tlv(validity)?;
    let (_, not_after, _) = tlv(v)?;
    Some(Summary {
        spki_sha256: Sha256::digest(spki).into(),
        subject: name_text(subject),
        issuer: name_text(issuer),
        not_after: String::from_utf8_lossy(not_after).chars().filter(|c| c.is_ascii_alphanumeric()).collect(),
    })
}

/// `SHA256:` and unpadded base64, as SSH fingerprints are shown.
pub fn fingerprint(d: &[u8; 32]) -> String {
    use base64::Engine;
    format!("SHA256:{}", base64::engine::general_purpose::STANDARD_NO_PAD.encode(d))
}

/// The digest in a [`fingerprint`].
pub fn key_of(fp: &str) -> Option<[u8; 32]> {
    use base64::Engine;
    let b = fp.strip_prefix("SHA256:")?;
    if b.len() != 43 {
        return None;
    }
    base64::engine::general_purpose::STANDARD_NO_PAD.decode(b).ok()?.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_generated_certificate() {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut p = rcgen::CertificateParams::new(vec!["nas.lan".into()]).unwrap();
        p.distinguished_name.push(rcgen::DnType::CommonName, "nas.lan");
        p.distinguished_name.push(rcgen::DnType::OrganizationName, "Home");
        let c = p.self_signed(&key).unwrap();
        let s = summary(c.der()).unwrap();
        assert_eq!((s.subject.as_str(), s.issuer.as_str()), ("nas.lan", "nas.lan"));
        assert_eq!(s.spki_sha256, <[u8; 32]>::from(Sha256::digest(key.public_key_der())));
        assert!(!s.not_after.is_empty());
        assert_eq!(key_of(&fingerprint(&s.spki_sha256)), Some(s.spki_sha256));
        assert_eq!(key_of("SHA256:short"), None);
        // Truncated or wrong, nothing.
        assert_eq!(summary(&c.der()[..c.der().len() / 2]), None);
        assert_eq!(summary(b"\x30\x84\xff\xff\xff\xff"), None);
        assert_eq!(summary(b""), None);
    }
}
