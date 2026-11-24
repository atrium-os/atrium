//! A certificate request for an EAP-TLS key Arca holds (wifi.md §7.4 E3):
//! PKCS #10, P-256, signed by Arca over the request's SHA-256; the
//! network's IT team signs it. The same shape as cryptad's (vpn.md §9.3).

use sha2::{Digest, Sha256};

fn len(n: usize, out: &mut Vec<u8>) {
    match n {
        0..=0x7f => out.push(n as u8),
        0x80..=0xff => out.extend_from_slice(&[0x81, n as u8]),
        _ => out.extend_from_slice(&[0x82, (n >> 8) as u8, n as u8]),
    }
}

fn tlv(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut v = vec![tag];
    len(body.len(), &mut v);
    v.extend_from_slice(body);
    v
}

fn seq(parts: &[&[u8]]) -> Vec<u8> {
    tlv(0x30, &parts.concat())
}

const OID_CN: &[u8] = &[0x06, 0x03, 0x55, 0x04, 0x03];
const OID_EC: &[u8] = &[0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
const OID_P256: &[u8] = &[0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
const OID_ECDSA_SHA256: &[u8] = &[0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02];

/// The part that is signed, for `cn` and the SEC1 point `public`.
pub fn info(cn: &str, public: &[u8]) -> Vec<u8> {
    let name = seq(&[&tlv(0x31, &seq(&[OID_CN, &tlv(0x0c, cn.as_bytes())]))]);
    let mut bits = vec![0u8];
    bits.extend_from_slice(public);
    let spki = seq(&[&seq(&[OID_EC, OID_P256]), &tlv(0x03, &bits)]);
    seq(&[&[0x02, 0x01, 0x00], &name, &spki, &[0xa0, 0x00]])
}

pub fn digest(b: &[u8]) -> [u8; 32] {
    Sha256::digest(b).into()
}

/// The whole request, DER, from `info` and Arca's DER signature over it.
pub fn der(info: &[u8], sig: &[u8]) -> Vec<u8> {
    let mut bits = vec![0u8];
    bits.extend_from_slice(sig);
    seq(&[info, &seq(&[OID_ECDSA_SHA256]), &tlv(0x03, &bits)])
}

/// Whether the certificate `der` is for the key `public`: its SPKI carries
/// the point as a BIT STRING.
pub fn certifies(der: &[u8], public: &[u8]) -> bool {
    let mut bits = vec![0x03, (public.len() + 1) as u8, 0];
    bits.extend_from_slice(public);
    der.windows(bits.len()).any(|w| w == bits.as_slice())
}
