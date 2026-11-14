//! Host keys, key derivation and the packet cipher.

use aes_gcm::aead::{AeadInPlace, KeyInit};
use aes_gcm::{Aes128Gcm, Aes256Gcm, Nonce, Tag};
use base64::Engine;
use sha2::{Digest, Sha256, Sha512};
use zeroize::Zeroizing;

use crate::wire::{Reader, Writer};
use crate::Error;

/// The smallest RSA host key accepted (network-shares.md §5.1).
pub const RSA_MIN_BITS: usize = 3072;

/// OpenSSH's fingerprint of a host key blob: "SHA256:" and unpadded base64.
pub fn fingerprint(blob: &[u8]) -> String {
    let d = Sha256::digest(blob);
    format!("SHA256:{}", base64::engine::general_purpose::STANDARD_NO_PAD.encode(d))
}

/// The host key's algorithm name, from its blob.
pub fn key_type(blob: &[u8]) -> Result<&str, Error> {
    Reader::new(blob).utf8()
}

/// Checks `sig` (an SSH signature blob made with `alg`) over `data` with the
/// host key `blob`. Only the algorithms we offer are accepted.
pub fn verify_host_sig(alg: &str, blob: &[u8], data: &[u8], sig: &[u8]) -> Result<(), Error> {
    let mut k = Reader::new(blob);
    let ktype = k.utf8()?;
    let mut s = Reader::new(sig);
    let sname = s.utf8()?;
    if sname != alg {
        return Err(Error::HostKey("signature algorithm differs from the one negotiated"));
    }
    let sbody = s.string()?;
    if !s.done() {
        return Err(Error::Protocol("trailing bytes after the signature"));
    }
    match alg {
        "ssh-ed25519" => {
            if ktype != "ssh-ed25519" {
                return Err(Error::HostKey("key type differs from the negotiated algorithm"));
            }
            let pk: [u8; 32] = k.string()?.try_into().map_err(|_| Error::HostKey("Ed25519 key is not 32 bytes"))?;
            let sb: [u8; 64] = sbody.try_into().map_err(|_| Error::HostKey("Ed25519 signature is not 64 bytes"))?;
            let vk = ed25519_dalek::VerifyingKey::from_bytes(&pk).map_err(|_| Error::HostKey("bad Ed25519 key"))?;
            vk.verify_strict(data, &ed25519_dalek::Signature::from_bytes(&sb))
                .map_err(|_| Error::HostKey("host signature does not verify"))
        }
        "ecdsa-sha2-nistp256" => {
            if ktype != alg || k.utf8()? != "nistp256" {
                return Err(Error::HostKey("key type differs from the negotiated algorithm"));
            }
            let q = k.string()?;
            let vk = p256::ecdsa::VerifyingKey::from_sec1_bytes(q).map_err(|_| Error::HostKey("bad P-256 key"))?;
            let mut rs = Reader::new(sbody);
            let (r, s2) = (rs.mpint_pos()?, rs.mpint_pos()?);
            if r.len() > 32 || s2.len() > 32 {
                return Err(Error::HostKey("ECDSA signature too long"));
            }
            let mut raw = [0u8; 64];
            raw[32 - r.len()..32].copy_from_slice(r);
            raw[64 - s2.len()..].copy_from_slice(s2);
            let sig = p256::ecdsa::Signature::from_slice(&raw).map_err(|_| Error::HostKey("bad ECDSA signature"))?;
            use p256::ecdsa::signature::Verifier;
            vk.verify(data, &sig).map_err(|_| Error::HostKey("host signature does not verify"))
        }
        "rsa-sha2-512" | "rsa-sha2-256" => {
            if ktype != "ssh-rsa" {
                return Err(Error::HostKey("key type differs from the negotiated algorithm"));
            }
            let e = rsa::BigUint::from_bytes_be(k.mpint_pos()?);
            let n = rsa::BigUint::from_bytes_be(k.mpint_pos()?);
            let pk = rsa::RsaPublicKey::new(n, e).map_err(|_| Error::HostKey("bad RSA key"))?;
            use rsa::traits::PublicKeyParts;
            if pk.n().bits() < RSA_MIN_BITS {
                return Err(Error::HostKey("RSA host key shorter than 3072 bits"));
            }
            let res = if alg == "rsa-sha2-512" {
                pk.verify(rsa::Pkcs1v15Sign::new::<Sha512>(), &Sha512::digest(data), sbody)
            } else {
                pk.verify(rsa::Pkcs1v15Sign::new::<Sha256>(), &Sha256::digest(data), sbody)
            };
            res.map_err(|_| Error::HostKey("host signature does not verify"))
        }
        _ => Err(Error::HostKey("host key algorithm not offered")),
    }
}

/// RFC 4253 §7.2: HASH(K || H || letter || session_id), extended to `len`.
pub fn derive(k: &[u8], h: &[u8], letter: u8, session_id: &[u8], len: usize) -> Zeroizing<Vec<u8>> {
    let mut kw = Writer::default();
    kw.mpint(k);
    let mut out = Zeroizing::new(Vec::with_capacity(len + 32));
    let mut d = Sha256::new();
    d.update(&kw.0);
    d.update(h);
    d.update([letter]);
    d.update(session_id);
    out.extend_from_slice(&d.finalize());
    while out.len() < len {
        let mut d = Sha256::new();
        d.update(&kw.0);
        d.update(h);
        d.update(&out[..]);
        out.extend_from_slice(&d.finalize());
    }
    out.truncate(len);
    out
}

/// AES-GCM as OpenSSH does it (PROTOCOL, RFC 5647): the length is sent in
/// the clear as associated data, the nonce is a 4-byte fixed field and a
/// 64-bit invocation counter that goes up by one per packet.
pub enum Gcm {
    A128(Box<Aes128Gcm>),
    A256(Box<Aes256Gcm>),
}

pub struct PacketCipher {
    gcm: Gcm,
    iv: [u8; 12],
}

impl PacketCipher {
    pub fn new(name: &str, key: &[u8], iv: &[u8]) -> Result<Self, Error> {
        let gcm = match name {
            "aes256-gcm@openssh.com" => Gcm::A256(Box::new(Aes256Gcm::new_from_slice(key).map_err(|_| Error::Protocol("key length"))?)),
            "aes128-gcm@openssh.com" => Gcm::A128(Box::new(Aes128Gcm::new_from_slice(&key[..16]).map_err(|_| Error::Protocol("key length"))?)),
            _ => return Err(Error::Protocol("cipher not offered")),
        };
        let mut v = [0u8; 12];
        v.copy_from_slice(&iv[..12]);
        Ok(PacketCipher { gcm, iv: v })
    }
    pub fn key_len(name: &str) -> usize {
        if name == "aes128-gcm@openssh.com" { 16 } else { 32 }
    }
    fn bump(&mut self) {
        let mut c = u64::from_be_bytes(self.iv[4..].try_into().unwrap());
        c = c.wrapping_add(1);
        self.iv[4..].copy_from_slice(&c.to_be_bytes());
    }
    /// Encrypts `body` (padding length, payload, padding) in place; returns the tag.
    pub fn seal(&mut self, len: &[u8; 4], body: &mut [u8]) -> [u8; 16] {
        let n = Nonce::from(self.iv);
        let tag = match &self.gcm {
            Gcm::A128(c) => c.encrypt_in_place_detached(&n, len, body),
            Gcm::A256(c) => c.encrypt_in_place_detached(&n, len, body),
        }
        .expect("AES-GCM encryption of a bounded packet");
        self.bump();
        tag.into()
    }
    pub fn open(&mut self, len: &[u8; 4], body: &mut [u8], tag: &[u8]) -> Result<(), Error> {
        let n = Nonce::from(self.iv);
        let t: [u8; 16] = tag.try_into().map_err(|_| Error::Protocol("tag length"))?;
        let t = Tag::from(t);
        let r = match &self.gcm {
            Gcm::A128(c) => c.decrypt_in_place_detached(&n, len, body, &t),
            Gcm::A256(c) => c.decrypt_in_place_detached(&n, len, body, &t),
        };
        r.map_err(|_| Error::Protocol("packet does not authenticate"))?;
        self.bump();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_openssh_style() {
        // OpenSSH prints the same for this blob: ssh-keygen -lf on a key
        // whose blob is "ssh-ed25519" + 32 zero bytes.
        let mut w = Writer::default();
        w.string(b"ssh-ed25519").string(&[0u8; 32]);
        let f = fingerprint(&w.0);
        assert!(f.starts_with("SHA256:") && !f.ends_with('='));
        assert_eq!(f.len(), 7 + 43);
    }

    #[test]
    fn gcm_round_trip_and_counter() {
        let key = [7u8; 32];
        let iv = [1u8; 12];
        let mut a = PacketCipher::new("aes256-gcm@openssh.com", &key, &iv).unwrap();
        let mut b = PacketCipher::new("aes256-gcm@openssh.com", &key, &iv).unwrap();
        for i in 0..3u8 {
            let len = [0, 0, 0, 16];
            let mut body = [i; 16];
            let tag = a.seal(&len, &mut body);
            b.open(&len, &mut body, &tag).unwrap();
            assert_eq!(body, [i; 16]);
        }
        let len = [0, 0, 0, 16];
        let mut body = [9u8; 16];
        let tag = a.seal(&len, &mut body);
        body[0] ^= 1;
        assert!(b.open(&len, &mut body, &tag).is_err(), "a flipped bit must not authenticate");
    }

    #[test]
    fn ed25519_host_signature() {
        use ed25519_dalek::Signer;
        let sk = ed25519_dalek::SigningKey::from_bytes(&[3u8; 32]);
        let mut blob = Writer::default();
        blob.string(b"ssh-ed25519").string(sk.verifying_key().as_bytes());
        let h = [5u8; 32];
        let mut sig = Writer::default();
        sig.string(b"ssh-ed25519").string(&sk.sign(&h).to_bytes());
        verify_host_sig("ssh-ed25519", &blob.0, &h, &sig.0).unwrap();
        assert!(verify_host_sig("ssh-ed25519", &blob.0, &[6u8; 32], &sig.0).is_err());
        assert!(verify_host_sig("rsa-sha2-512", &blob.0, &h, &sig.0).is_err());
    }
}
