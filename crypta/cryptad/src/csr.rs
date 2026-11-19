//! A certificate request for an AnyConnect client key Arca holds (vpn.md
//! §9.3): PKCS #10, P-256, signed by Arca over the request's SHA-256. The
//! IT team signs it; `certificate` brings the result back.

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

/// The whole request, PEM, from `info` and Arca's DER signature over it.
pub fn pem(info: &[u8], sig: &[u8]) -> String {
    let mut bits = vec![0u8];
    bits.extend_from_slice(sig);
    let der = seq(&[info, &seq(&[OID_ECDSA_SHA256]), &tlv(0x03, &bits)]);
    let b = crypta_proto::b64::encode(&der);
    let mut s = String::from("-----BEGIN CERTIFICATE REQUEST-----\n");
    for c in b.as_bytes().chunks(64) {
        s.push_str(std::str::from_utf8(c).unwrap_or(""));
        s.push('\n');
    }
    s.push_str("-----END CERTIFICATE REQUEST-----\n");
    s
}

/// Whether the certificate `der` is for the key `public`: its SPKI carries
/// the point as a BIT STRING.
pub fn certifies(der: &[u8], public: &[u8]) -> bool {
    let mut bits = vec![0x03, (public.len() + 1) as u8, 0];
    bits.extend_from_slice(public);
    der.windows(bits.len()).any(|w| w == bits.as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request openssl reads and verifies: the key is made here, the
    /// signature by p256 the way arcad makes it.
    #[test]
    fn a_request_openssl_verifies() {
        use p256::ecdsa::{signature::hazmat::PrehashSigner, Signature, SigningKey};
        let k = SigningKey::from_bytes(&[7u8; 32].into()).unwrap();
        let public = k.verifying_key().to_encoded_point(false).as_bytes().to_vec();
        let i = info("carol", &public);
        let sig: Signature = k.sign_prehash(&digest(&i)).unwrap();
        let p = pem(&i, sig.to_der().as_bytes());
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("r.csr");
        std::fs::write(&f, &p).unwrap();
        let Ok(o) = std::process::Command::new("openssl").args(["req", "-verify", "-noout", "-subject", "-in"]).arg(&f).output() else { return };
        let out = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
        assert!(o.status.success(), "{out}");
        assert!(out.contains("carol"), "{out}");
        assert!(certifies(&{
            let mut b = vec![0x03, 66, 0];
            b.extend_from_slice(&public);
            b
        }, &public));
    }
}
