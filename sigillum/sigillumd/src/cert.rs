//! The few fields of an X.509 certificate a token reports that the broker
//! needs (sigillum.md §10.1): names and expiry for the sheet, and the public
//! key, to check a signature the token returns before anyone gets it. The
//! driver's own words about a certificate (its label, its key kind) are
//! never used for either.
//!
//! A strict DER walk over at most [`cap::CERT`] bytes: definite lengths
//! only, every length checked against what holds it. Nothing here
//! validates a chain; the server that asked for the certificate does.

use p256::ecdsa::signature::hazmat::PrehashVerifier;
use rsa::traits::PublicKeyParts;
use sha2::{Digest, Sha256, Sha384, Sha512};
use sigillum_proto::driver::{cap, HashAlg, SignScheme};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    P256(Vec<u8>),
    P384(Vec<u8>),
    /// PKCS#1 `RSAPublicKey`, and the modulus size in bits.
    Rsa { der: Vec<u8>, bits: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    pub subject: String,
    pub issuer: String,
    /// `YYYY-MM-DD`.
    pub not_after: String,
    pub key: Key,
    pub sha256: [u8; 32],
}

/// Longest name shown on the sheet, in characters.
pub const MAX_NAME: usize = 64;

struct Der<'a> {
    b: &'a [u8],
}

impl<'a> Der<'a> {
    /// The next element: (tag, contents).
    fn next(&mut self) -> Result<(u8, &'a [u8]), &'static str> {
        let b = self.b;
        if b.len() < 2 {
            return Err("truncated");
        }
        let tag = b[0];
        if tag & 0x1F == 0x1F {
            return Err("high tag number");
        }
        let (len, hdr) = match b[1] {
            n if n < 0x80 => (n as usize, 2),
            0x81 => {
                let n = *b.get(2).ok_or("truncated")? as usize;
                if n < 0x80 {
                    return Err("non-minimal length");
                }
                (n, 3)
            }
            0x82 => {
                if b.len() < 4 {
                    return Err("truncated");
                }
                let n = u16::from_be_bytes([b[2], b[3]]) as usize;
                if n < 0x100 {
                    return Err("non-minimal length");
                }
                (n, 4)
            }
            _ => return Err("length form"),
        };
        if b.len() - hdr < len {
            return Err("truncated");
        }
        self.b = &b[hdr + len..];
        Ok((tag, &b[hdr..hdr + len]))
    }

    fn expect(&mut self, tag: u8) -> Result<&'a [u8], &'static str> {
        match self.next()? {
            (t, c) if t == tag => Ok(c),
            _ => Err("unexpected element"),
        }
    }

    fn done(&self) -> bool {
        self.b.is_empty()
    }
}

const SEQ: u8 = 0x30;
const SET: u8 = 0x31;
const OID: u8 = 0x06;
const BITS: u8 = 0x03;
const OID_CN: &[u8] = &[0x55, 0x04, 0x03];
const OID_O: &[u8] = &[0x55, 0x04, 0x0A];
const OID_EC: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01];
const OID_P256: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07];
const OID_P384: &[u8] = &[0x2B, 0x81, 0x04, 0x00, 0x22];
const OID_RSA: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x01];

/// A name's common name, else its organisation, as plain text.
fn name(n: &[u8]) -> Result<String, &'static str> {
    let (mut cn, mut o) = (None, None);
    let mut rdns = Der { b: n };
    while !rdns.done() {
        let mut set = Der { b: rdns.expect(SET)? };
        while !set.done() {
            let mut atv = Der { b: set.expect(SEQ)? };
            let oid = atv.expect(OID)?;
            let (tag, v) = atv.next()?;
            let text = match tag {
                // UTF8String, PrintableString, T61String (as Latin-1 would be
                // wrong as often as right: only its ASCII), IA5String.
                0x0C | 0x13 | 0x14 | 0x16 => String::from_utf8_lossy(v).into_owned(),
                _ => continue,
            };
            if oid == OID_CN && cn.is_none() {
                cn = Some(text);
            } else if oid == OID_O && o.is_none() {
                o = Some(text);
            }
        }
    }
    Ok(shown(&cn.or(o).unwrap_or_default()))
}

/// Text as the sheet may show it: no control or bidi characters, at most
/// [`MAX_NAME`] characters.
pub fn shown(s: &str) -> String {
    let mut t: String = s
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'))
        .take(MAX_NAME)
        .collect();
    if t.trim().is_empty() {
        t = "(no name)".into();
    }
    t
}

fn time(tag: u8, v: &[u8]) -> Result<String, &'static str> {
    let digits = |s: &[u8]| s.iter().all(u8::is_ascii_digit);
    let (y, rest) = match tag {
        0x17 if v.len() == 13 && digits(&v[..12]) => {
            let yy = (v[0] - b'0') as u32 * 10 + (v[1] - b'0') as u32;
            (if yy < 50 { 2000 + yy } else { 1900 + yy }, &v[2..])
        }
        0x18 if v.len() == 15 && digits(&v[..14]) => {
            let y = v[..4].iter().fold(0u32, |a, d| a * 10 + (d - b'0') as u32);
            (y, &v[4..])
        }
        _ => return Err("validity time"),
    };
    Ok(format!("{y:04}-{}-{}", std::str::from_utf8(&rest[..2]).unwrap(), std::str::from_utf8(&rest[2..4]).unwrap()))
}

fn spki(s: &[u8]) -> Result<Key, &'static str> {
    let mut d = Der { b: s };
    let mut alg = Der { b: d.expect(SEQ)? };
    let bits = d.expect(BITS)?;
    if bits.first() != Some(&0) {
        return Err("public key bit string");
    }
    let key = bits[1..].to_vec();
    let oid = alg.expect(OID)?;
    if oid == OID_EC {
        let curve = alg.expect(OID)?;
        return match curve {
            OID_P256 if key.len() == 65 && key[0] == 4 => Ok(Key::P256(key)),
            OID_P384 if key.len() == 97 && key[0] == 4 => Ok(Key::P384(key)),
            _ => Err("EC key not on P-256 or P-384, or compressed"),
        };
    }
    if oid == OID_RSA {
        let k = <rsa::RsaPublicKey as rsa::pkcs1::DecodeRsaPublicKey>::from_pkcs1_der(&key).map_err(|_| "RSA key")?;
        let bits = k.n().bits();
        if !(2048..=4096).contains(&bits) {
            return Err("RSA key outside 2048..4096 bits");
        }
        return Ok(Key::Rsa { der: key, bits });
    }
    Err("key algorithm")
}

pub fn parse(der: &[u8]) -> Result<Info, &'static str> {
    if der.len() > cap::CERT {
        return Err("too long");
    }
    let mut outer = Der { b: der };
    let mut cert = Der { b: outer.expect(SEQ)? };
    if !outer.done() {
        return Err("trailing data");
    }
    let mut tbs = Der { b: cert.expect(SEQ)? };
    let (mut tag, mut v) = tbs.next()?;
    if tag == 0xA0 {
        (tag, v) = tbs.next()?; // the version; then the serial
    }
    let _ = v;
    if tag != 0x02 {
        return Err("serial number");
    }
    tbs.expect(SEQ)?; // signature algorithm
    let issuer = name(tbs.expect(SEQ)?)?;
    let mut validity = Der { b: tbs.expect(SEQ)? };
    validity.next()?;
    let (t, na) = validity.next()?;
    let not_after = time(t, na)?;
    let subject = name(tbs.expect(SEQ)?)?;
    let key = spki(tbs.expect(SEQ)?)?;
    Ok(Info { subject, issuer, not_after, key, sha256: Sha256::digest(der).into() })
}

/// Can this key sign with this scheme?
pub fn fits(key: &Key, scheme: SignScheme) -> bool {
    matches!(
        (key, scheme),
        (Key::P256(_) | Key::P384(_), SignScheme::Ecdsa(_)) | (Key::Rsa { .. }, SignScheme::RsaPkcs1(_) | SignScheme::RsaPss(_))
    )
}

fn digest_info(h: HashAlg) -> Box<[u8]> {
    match h {
        HashAlg::Sha256 => Box::new([0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05, 0x00, 0x04, 0x20]),
        HashAlg::Sha384 => Box::new([0x30, 0x41, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02, 0x05, 0x00, 0x04, 0x30]),
        HashAlg::Sha512 => Box::new([0x30, 0x51, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x03, 0x05, 0x00, 0x04, 0x40]),
    }
}

/// Is `sig` this key's signature over `digest` with `scheme`? ECDSA
/// signatures are `r || s`, as PKCS#11 returns them.
pub fn verify(key: &Key, scheme: SignScheme, digest: &[u8], sig: &[u8]) -> bool {
    match (key, scheme) {
        (Key::P256(k), SignScheme::Ecdsa(_)) => {
            let (Ok(k), Ok(s)) = (p256::ecdsa::VerifyingKey::from_sec1_bytes(k), p256::ecdsa::Signature::from_slice(sig)) else { return false };
            k.verify_prehash(digest, &s).is_ok()
        }
        (Key::P384(k), SignScheme::Ecdsa(_)) => {
            let (Ok(k), Ok(s)) = (p384::ecdsa::VerifyingKey::from_sec1_bytes(k), p384::ecdsa::Signature::from_slice(sig)) else { return false };
            k.verify_prehash(digest, &s).is_ok()
        }
        (Key::Rsa { der, .. }, SignScheme::RsaPkcs1(h)) => {
            let Ok(k) = <rsa::RsaPublicKey as rsa::pkcs1::DecodeRsaPublicKey>::from_pkcs1_der(der) else { return false };
            let pad = rsa::Pkcs1v15Sign { hash_len: Some(h.len()), prefix: digest_info(h) };
            k.verify(pad, digest, sig).is_ok()
        }
        (Key::Rsa { der, .. }, SignScheme::RsaPss(h)) => {
            let Ok(k) = <rsa::RsaPublicKey as rsa::pkcs1::DecodeRsaPublicKey>::from_pkcs1_der(der) else { return false };
            let pss = match h {
                HashAlg::Sha256 => rsa::Pss::new_with_salt::<Sha256>(32),
                HashAlg::Sha384 => rsa::Pss::new_with_salt::<Sha384>(48),
                HashAlg::Sha512 => rsa::Pss::new_with_salt::<Sha512>(64),
            };
            k.verify(pss, digest, sig).is_ok()
        }
        _ => false,
    }
}
