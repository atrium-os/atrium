//! PIN/UV auth protocols 2 (CTAP 2.1 §6.5.7) and 1 (§6.5.6, CTAP 2.0's
//! only one), both sides: the platform (`sigillum-fido`) and the
//! authenticator (the soft key). Keys from before CTAP 2.1 (a YubiKey 5 on
//! firmware 5.4, say) list only protocol 1 and know only `getPinToken`.
//!
//! Protocol 1: the shared secret is `SHA-256(Z)`, one key for both AES and
//! HMAC; `encrypt` is AES-256-CBC with an all-zero IV (none sent);
//! `authenticate` is the first 16 bytes of HMAC-SHA-256.
//!
//! Protocol 2:
//! The shared secret is `HKDF-SHA-256(salt = 32 zero bytes, IKM = Z,
//! info = "CTAP2 HMAC key") || HKDF(..., info = "CTAP2 AES key")`, where Z is
//! the x coordinate of the ECDH point. `encrypt` is AES-256-CBC with a
//! random IV prepended, no padding; `authenticate` is the full 32-byte
//! HMAC-SHA-256.

use aes::cipher::{block_padding::NoPadding, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::cose::{alg, CoseKey};
use crate::Error;

pub const PROTOCOL: u8 = 2;
/// CTAP 2.0's protocol.
pub const PROTOCOL_ONE: u8 = 1;

pub mod permission {
    pub const MAKE_CREDENTIAL: u8 = 0x01;
    pub const GET_ASSERTION: u8 = 0x02;
}

/// The two keys derived from one ECDH exchange.
pub struct Shared {
    hmac_key: Zeroizing<[u8; 32]>,
    aes_key: Zeroizing<[u8; 32]>,
    proto: u8,
}

impl Shared {
    pub fn from_z(z: &[u8]) -> Shared {
        let hk = hkdf::Hkdf::<Sha256>::new(Some(&[0u8; 32]), z);
        let mut hmac_key = Zeroizing::new([0u8; 32]);
        let mut aes_key = Zeroizing::new([0u8; 32]);
        hk.expand(b"CTAP2 HMAC key", &mut *hmac_key).expect("32 bytes is a valid HKDF length");
        hk.expand(b"CTAP2 AES key", &mut *aes_key).expect("32 bytes is a valid HKDF length");
        Shared { hmac_key, aes_key, proto: PROTOCOL }
    }

    /// Protocol 1's secret: `SHA-256(Z)` for both keys.
    pub fn from_z_one(z: &[u8]) -> Shared {
        let k: [u8; 32] = Sha256::digest(z).into();
        Shared { hmac_key: Zeroizing::new(k), aes_key: Zeroizing::new(k), proto: PROTOCOL_ONE }
    }

    /// ECDH between our secret and the peer's key agreement key (protocol 2).
    pub fn agree(ours: &p256::SecretKey, peer: &CoseKey) -> Result<Shared, Error> {
        Shared::agree_with(PROTOCOL, ours, peer)
    }

    /// The same for either protocol.
    pub fn agree_with(proto: u8, ours: &p256::SecretKey, peer: &CoseKey) -> Result<Shared, Error> {
        let pk = peer.p256().ok_or(Error::Protocol("key agreement key is not P-256"))?;
        let z = p256::ecdh::diffie_hellman(ours.to_nonzero_scalar(), pk.as_affine());
        match proto {
            PROTOCOL => Ok(Shared::from_z(z.raw_secret_bytes())),
            PROTOCOL_ONE => Ok(Shared::from_z_one(z.raw_secret_bytes())),
            _ => Err(Error::Request("unknown PIN/UV auth protocol")),
        }
    }

    pub fn protocol(&self) -> u8 {
        self.proto
    }

    /// `iv[16] | AES-256-CBC(plaintext)`; the plaintext must be a multiple
    /// of 16 bytes.
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, Error> {
        if plaintext.len() % 16 != 0 {
            return Err(Error::Request("PIN protocol plaintext not block aligned"));
        }
        let mut iv = [0u8; 16];
        if self.proto == PROTOCOL_ONE {
            let mut out = plaintext.to_vec();
            let enc = cbc::Encryptor::<aes::Aes256>::new(self.aes_key.as_ref().into(), &iv.into());
            enc.encrypt_padded_mut::<NoPadding>(&mut out, plaintext.len()).map_err(|_| Error::Request("PIN protocol encrypt"))?;
            return Ok(out);
        }
        getrandom::getrandom(&mut iv).map_err(|e| Error::Io(e.to_string()))?;
        let mut out = iv.to_vec();
        out.extend_from_slice(plaintext);
        let enc = cbc::Encryptor::<aes::Aes256>::new(self.aes_key.as_ref().into(), &iv.into());
        enc.encrypt_padded_mut::<NoPadding>(&mut out[16..], plaintext.len())
            .map_err(|_| Error::Request("PIN protocol encrypt"))?;
        Ok(out)
    }

    pub fn decrypt(&self, ct: &[u8]) -> Result<Zeroizing<Vec<u8>>, Error> {
        if self.proto == PROTOCOL_ONE {
            if ct.is_empty() || ct.len() % 16 != 0 {
                return Err(Error::Protocol("PIN protocol ciphertext length"));
            }
            let dec = cbc::Decryptor::<aes::Aes256>::new(self.aes_key.as_ref().into(), &[0u8; 16].into());
            let mut buf = Zeroizing::new(ct.to_vec());
            dec.decrypt_padded_mut::<NoPadding>(&mut buf).map_err(|_| Error::Protocol("PIN protocol decrypt"))?;
            return Ok(buf);
        }
        if ct.len() < 16 || (ct.len() - 16) % 16 != 0 {
            return Err(Error::Protocol("PIN protocol ciphertext length"));
        }
        let iv: [u8; 16] = ct[..16].try_into().unwrap();
        let dec = cbc::Decryptor::<aes::Aes256>::new(self.aes_key.as_ref().into(), &iv.into());
        let mut buf = Zeroizing::new(ct[16..].to_vec());
        dec.decrypt_padded_mut::<NoPadding>(&mut buf).map_err(|_| Error::Protocol("PIN protocol decrypt"))?;
        Ok(buf)
    }

    /// `authenticate(sharedSecret, message)`: keyed with the HMAC half.
    pub fn authenticate(&self, msg: &[u8]) -> [u8; 32] {
        authenticate(&*self.hmac_key, msg)
    }
}

/// `authenticate(key, message)` for `proto`: all 32 bytes of HMAC-SHA-256
/// (protocol 2), the first 16 (protocol 1).
pub fn authenticate_with(proto: u8, key: &[u8], msg: &[u8]) -> Vec<u8> {
    let t = authenticate(key, msg);
    if proto == PROTOCOL_ONE { t[..16].to_vec() } else { t.to_vec() }
}

/// Constant-time check of an `authenticate_with` result.
pub fn verify_with(proto: u8, key: &[u8], msg: &[u8], tag: &[u8]) -> bool {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC takes any key length");
    m.update(msg);
    match proto {
        PROTOCOL_ONE if tag.len() == 16 => m.verify_truncated_left(tag).is_ok(),
        PROTOCOL => m.verify_slice(tag).is_ok(),
        _ => false,
    }
}

/// `authenticate(key, message)` = HMAC-SHA-256, all 32 bytes.
pub fn authenticate(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC takes any key length");
    m.update(msg);
    m.finalize().into_bytes().into()
}

/// Constant-time check of an `authenticate` result.
pub fn verify(key: &[u8], msg: &[u8], tag: &[u8]) -> bool {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC takes any key length");
    m.update(msg);
    m.verify_slice(tag).is_ok()
}

/// A fresh platform key agreement pair and the COSE key to send.
pub fn platform_key() -> (p256::SecretKey, CoseKey) {
    let sk = p256::SecretKey::random(&mut p256::elliptic_curve::rand_core::OsRng);
    let pk = CoseKey::from_p256(alg::ECDH_ES_HKDF_256, &sk.public_key());
    (sk, pk)
}

/// `LEFT(SHA-256(pin), 16)`.
pub fn pin_hash(pin: &[u8]) -> Zeroizing<[u8; 16]> {
    let d = Sha256::digest(pin);
    let mut out = Zeroizing::new([0u8; 16]);
    out.copy_from_slice(&d[..16]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_sides_agree() {
        let (a_sk, a_pk) = platform_key();
        let (b_sk, b_pk) = platform_key();
        let a = Shared::agree(&a_sk, &b_pk).unwrap();
        let b = Shared::agree(&b_sk, &a_pk).unwrap();
        let ct = a.encrypt(&[5u8; 32]).unwrap();
        assert_eq!(&*b.decrypt(&ct).unwrap(), &[5u8; 32]);
        assert_eq!(a.authenticate(b"m"), b.authenticate(b"m"));
        assert!(a.encrypt(&[0u8; 15]).is_err());
        assert!(b.decrypt(&ct[..20]).is_err());
    }

    #[test]
    fn known_answers() {
        // Computed independently with Python's hmac/hashlib:
        // prk = HMAC(b"\0"*32, z); k = HMAC(prk, b"CTAP2 HMAC key\x01");
        // HMAC(k, b"abc").
        let s = Shared::from_z(&[0x11u8; 32]);
        assert_eq!(hex(&s.authenticate(b"abc")), "f7105ee8cac42dae1588ae8ad44f15870f784936ad657f9bcf80e14a5056652a");
        assert_eq!(hex(&*pin_hash(b"1234")), "03ac674216f3e15c761ee1a5e255f067");
    }

    #[test]
    fn protocol_one() {
        let (a_sk, a_pk) = platform_key();
        let (b_sk, b_pk) = platform_key();
        let a = Shared::agree_with(PROTOCOL_ONE, &a_sk, &b_pk).unwrap();
        let b = Shared::agree_with(PROTOCOL_ONE, &b_sk, &a_pk).unwrap();
        let ct = a.encrypt(&[7u8; 16]).unwrap();
        assert_eq!(ct.len(), 16, "no IV on the wire");
        assert_eq!(&*b.decrypt(&ct).unwrap(), &[7u8; 16]);
        // Deterministic: zero IV.
        assert_eq!(a.encrypt(&[7u8; 16]).unwrap(), ct);
        let tag = authenticate_with(PROTOCOL_ONE, b"k", b"m");
        assert_eq!(tag.len(), 16);
        assert!(verify_with(PROTOCOL_ONE, b"k", b"m", &tag));
        assert!(!verify_with(PROTOCOL, b"k", b"m", &tag));
        assert!(!verify_with(PROTOCOL_ONE, b"k", b"n", &tag));
    }

    #[test]
    fn protocol_one_known_answers() {
        // Computed independently with Python: k = sha256(b"\x11"*32);
        // AES-256-CBC(k, iv=0, b"\x00"*16); hmac(k, b"abc")[:16].
        let s = Shared::from_z_one(&[0x11u8; 32]);
        assert_eq!(hex(&s.encrypt(&[0u8; 16]).unwrap()), "4eba0cf44ba2422d95768fe7c6508817");
        assert_eq!(hex(&authenticate_with(PROTOCOL_ONE, &s.hmac_key[..], b"abc")), "efc2b084bbdcaaeaeb480b40472b2e82");
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
}
