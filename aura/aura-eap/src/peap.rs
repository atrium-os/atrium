//! PEAPv0's extensions (MS-PEAP §2.2.8): the Result TLV that ends phase 2
//! and the Crypto-Binding TLV that ties the inner method to the tunnel,
//! with the key derivation hostapd and Windows share.

use hmac::{Hmac, Mac};
use sha1::Sha1;
use zeroize::Zeroizing;

pub const RESULT: u16 = 3;
pub const CRYPTO_BINDING: u16 = 12;
pub const MANDATORY: u16 = 0x8000;
pub const SUCCESS: u16 = 1;
pub const FAILURE: u16 = 2;
const BINDING_LEN: usize = 56;

#[derive(Debug, PartialEq, Eq)]
pub struct Tlv<'a> {
    pub typ: u16,
    pub mandatory: bool,
    pub value: &'a [u8],
    /// The TLV with its header, as sent.
    pub raw: &'a [u8],
}

/// Every TLV in `b`, or `None` when one runs past the end.
pub fn parse(mut b: &[u8]) -> Option<Vec<Tlv<'_>>> {
    let mut v = Vec::new();
    while !b.is_empty() {
        if b.len() < 4 {
            return None;
        }
        let t = u16::from_be_bytes([b[0], b[1]]);
        let len = u16::from_be_bytes([b[2], b[3]]) as usize;
        if 4 + len > b.len() {
            return None;
        }
        v.push(Tlv { typ: t & 0x3fff, mandatory: t & MANDATORY != 0, value: &b[4..4 + len], raw: &b[..4 + len] });
        b = &b[4 + len..];
    }
    Some(v)
}

/// A Result TLV.
pub fn result(status: u16) -> [u8; 6] {
    let mut r = [0u8; 6];
    r[..2].copy_from_slice(&(MANDATORY | RESULT).to_be_bytes());
    r[2..4].copy_from_slice(&2u16.to_be_bytes());
    r[4..].copy_from_slice(&status.to_be_bytes());
    r
}

fn hmac_sha1(key: &[u8], parts: &[&[u8]]) -> [u8; 20] {
    let mut m = <Hmac<Sha1> as Mac>::new_from_slice(key).expect("HMAC takes any key");
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().into()
}

/// PEAPv0's PRF+: T(n) = HMAC-SHA1(K, T(n-1) | label | seed | n | 0 | 0).
pub fn prf_plus(key: &[u8], label: &[u8], seed: &[u8], len: usize) -> Zeroizing<Vec<u8>> {
    let mut out = Zeroizing::new(Vec::with_capacity(len + 20));
    let mut t = Zeroizing::new([0u8; 20]);
    let mut n = 0u8;
    while out.len() < len {
        n += 1;
        let prev: &[u8] = if n == 1 { &[] } else { &t[..] };
        let next = hmac_sha1(key, &[prev, label, seed, &[n, 0, 0]]);
        *t = next;
        out.extend_from_slice(&t[..]);
    }
    out.truncate(len);
    out
}

/// IPMK and CMK from the tunnel key (the first 40 bytes of the TLS
/// "client EAP encryption" output) and the inner method's key.
pub fn compound_keys(tk: &[u8], isk: &[u8; 32]) -> (Zeroizing<[u8; 40]>, Zeroizing<[u8; 20]>) {
    let imck = prf_plus(&tk[..40], b"Inner Methods Compound Keys", isk, 60);
    let mut ipmk = Zeroizing::new([0u8; 40]);
    let mut cmk = Zeroizing::new([0u8; 20]);
    ipmk.copy_from_slice(&imck[..40]);
    cmk.copy_from_slice(&imck[40..]);
    (ipmk, cmk)
}

/// The MSK once crypto-binding ran (the label's NUL is Microsoft's).
pub fn bound_msk(ipmk: &[u8; 40]) -> Zeroizing<[u8; 64]> {
    let csk = prf_plus(ipmk, b"Session Key Generating Function", &[0], 128);
    let mut m = Zeroizing::new([0u8; 64]);
    m.copy_from_slice(&csk[..64]);
    m
}

fn compound_mac(cmk: &[u8; 20], tlv: &[u8]) -> [u8; 20] {
    let mut b = tlv.to_vec();
    b[4 + 36..4 + 56].fill(0);
    hmac_sha1(cmk, &[&b, &[crate::eap::typ::PEAP]])
}

/// Checks the server's Crypto-Binding request and builds our response:
/// the same header, sub-type response, the nonce plus one, our MAC.
pub fn binding_response(req: &Tlv<'_>, cmk: &[u8; 20]) -> Option<Vec<u8>> {
    let v = req.value;
    if v.len() != BINDING_LEN || v[1] != 0 || v[3] != 0 {
        return None;
    }
    let want = compound_mac(cmk, req.raw);
    let mut d = 0u8;
    for (a, b) in want.iter().zip(&v[36..56]) {
        d |= a ^ b;
    }
    if d != 0 {
        return None;
    }
    let mut r = req.raw.to_vec();
    r[4 + 2] = v[1];
    r[4 + 3] = 1;
    r[4 + 35] = r[4 + 35].wrapping_add(1);
    let mac = compound_mac(cmk, &r);
    r[4 + 36..].copy_from_slice(&mac);
    Some(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tlvs() {
        let r = result(SUCCESS);
        assert_eq!(r, [0x80, 3, 0, 2, 0, 1]);
        let v = parse(&r).unwrap();
        assert_eq!(v[0].typ, RESULT);
        assert!(v[0].mandatory);
        assert_eq!(v[0].value, &[0, 1]);
        assert_eq!(parse(&r[..5]), None);
    }

    #[test]
    fn binding_checks_the_mac() {
        let cmk = [9u8; 20];
        let mut req = vec![0, 12, 0, 56, 0, 0, 0, 0];
        req.extend_from_slice(&[0x55; 32]);
        req.extend_from_slice(&[0; 20]);
        let mac = compound_mac(&cmk, &req);
        req[40..].copy_from_slice(&mac);
        let t = &parse(&req).unwrap()[0];
        let r = binding_response(t, &cmk).unwrap();
        assert_eq!(r[7], 1, "a response");
        assert_eq!(r[39], 0x56, "nonce plus one");
        assert_eq!(&r[40..], &compound_mac(&cmk, &r));
        assert_eq!(binding_response(t, &[8; 20]), None, "wrong key");
        assert_eq!(prf_plus(b"k", b"l", b"s", 50).len(), 50);
    }
}
