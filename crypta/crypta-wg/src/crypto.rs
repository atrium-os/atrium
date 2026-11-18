//! WireGuard's primitives (protocol page, "Cryptographic primitives"):
//! HASH = BLAKE2s-256, MAC = keyed BLAKE2s-128, HMAC over BLAKE2s, the
//! KDF chain, AEAD = ChaCha20-Poly1305 with a 64-bit counter nonce, XAEAD
//! = XChaCha20-Poly1305 for cookies, and TAI64N.

use blake2::digest::{FixedOutput, KeyInit, Mac, Update};
use blake2::{Blake2s256, Blake2sMac, Digest};
use chacha20poly1305::aead::AeadInPlace;
use chacha20poly1305::{ChaCha20Poly1305, Nonce, Tag, XChaCha20Poly1305, XNonce};
use hmac::SimpleHmac;
use zeroize::Zeroizing;

pub const CONSTRUCTION: &[u8] = b"Noise_IKpsk2_25519_ChaChaPoly_BLAKE2s";
pub const IDENTIFIER: &[u8] = b"WireGuard v1 zx2c4 Jason@zx2c4.com";
pub const LABEL_MAC1: &[u8] = b"mac1----";
pub const LABEL_COOKIE: &[u8] = b"cookie--";
pub const TAG_LEN: usize = 16;

pub type Key = Zeroizing<[u8; 32]>;

pub fn hash(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Blake2s256::new();
    for p in parts {
        Digest::update(&mut h, p);
    }
    h.finalize().into()
}

/// Keyed BLAKE2s with a 16-byte output.
pub fn mac(key: &[u8], parts: &[&[u8]]) -> [u8; 16] {
    let mut m = <Blake2sMac<blake2::digest::consts::U16> as KeyInit>::new_from_slice(key).expect("key of at most 32 bytes");
    for p in parts {
        Mac::update(&mut m, p);
    }
    m.finalize_fixed().into()
}

fn hmac(key: &[u8], parts: &[&[u8]]) -> Key {
    let mut m = <SimpleHmac<Blake2s256> as KeyInit>::new_from_slice(key).expect("any key length");
    for p in parts {
        Update::update(&mut m, p);
    }
    Zeroizing::new(m.finalize_fixed().into())
}

/// KDF_n(key, input): τ0 = HMAC(key, input), τi = HMAC(τ0, τ(i-1) || i).
fn kdf<const N: usize>(key: &[u8], input: &[u8]) -> [Key; N] {
    let t0 = hmac(key, &[input]);
    let mut out: [Key; N] = std::array::from_fn(|_| Zeroizing::new([0u8; 32]));
    let mut prev: Option<Key> = None;
    for (i, o) in out.iter_mut().enumerate() {
        let n = [i as u8 + 1];
        let t = match &prev {
            None => hmac(&t0[..], &[&n]),
            Some(p) => hmac(&t0[..], &[&p[..], &n]),
        };
        *o = t.clone();
        prev = Some(t);
    }
    out
}

pub fn kdf1(key: &[u8], input: &[u8]) -> Key {
    let [a] = kdf::<1>(key, input);
    a
}

pub fn kdf2(key: &[u8], input: &[u8]) -> (Key, Key) {
    let [a, b] = kdf::<2>(key, input);
    (a, b)
}

pub fn kdf3(key: &[u8], input: &[u8]) -> (Key, Key, Key) {
    let [a, b, c] = kdf::<3>(key, input);
    (a, b, c)
}

fn nonce(counter: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&counter.to_le_bytes());
    Nonce::from(n)
}

/// Seal `buf[..len]` in place and append the tag; `buf` must have
/// `len + TAG_LEN` bytes.
pub fn seal(key: &[u8; 32], counter: u64, aad: &[u8], buf: &mut [u8], len: usize) {
    let c = ChaCha20Poly1305::new(key.into());
    let tag = c.encrypt_in_place_detached(&nonce(counter), aad, &mut buf[..len]).expect("in-place seal");
    buf[len..len + TAG_LEN].copy_from_slice(&tag);
}

/// Open `buf` (ciphertext || tag) in place; the plaintext length, or None.
pub fn open(key: &[u8; 32], counter: u64, aad: &[u8], buf: &mut [u8]) -> Option<usize> {
    let len = buf.len().checked_sub(TAG_LEN)?;
    let (data, tag) = buf.split_at_mut(len);
    let c = ChaCha20Poly1305::new(key.into());
    c.decrypt_in_place_detached(&nonce(counter), aad, data, Tag::from_slice(tag)).ok()?;
    Some(len)
}

/// AEAD of a small value into a fresh vector.
pub fn seal_vec(key: &[u8; 32], counter: u64, aad: &[u8], plain: &[u8]) -> Vec<u8> {
    let mut v = vec![0u8; plain.len() + TAG_LEN];
    v[..plain.len()].copy_from_slice(plain);
    seal(key, counter, aad, &mut v, plain.len());
    v
}

pub fn open_vec(key: &[u8; 32], counter: u64, aad: &[u8], sealed: &[u8]) -> Option<Zeroizing<Vec<u8>>> {
    let mut v = Zeroizing::new(sealed.to_vec());
    let n = open(key, counter, aad, &mut v)?;
    v.truncate(n);
    Some(v)
}

pub fn xseal(key: &[u8; 32], nonce: &[u8; 24], aad: &[u8], plain: &[u8; 16]) -> [u8; 32] {
    let c = XChaCha20Poly1305::new(key.into());
    let mut out = [0u8; 32];
    out[..16].copy_from_slice(plain);
    let tag = c.encrypt_in_place_detached(XNonce::from_slice(nonce), aad, &mut out[..16]).expect("in-place seal");
    out[16..].copy_from_slice(&tag);
    out
}

pub fn xopen(key: &[u8; 32], nonce: &[u8; 24], aad: &[u8], sealed: &[u8; 32]) -> Option<[u8; 16]> {
    let c = XChaCha20Poly1305::new(key.into());
    let mut out = [0u8; 16];
    out.copy_from_slice(&sealed[..16]);
    c.decrypt_in_place_detached(XNonce::from_slice(nonce), aad, &mut out, Tag::from_slice(&sealed[16..])).ok()?;
    Some(out)
}

/// TAI64N of the wall clock: 8 bytes of seconds + 2^62, 4 of nanoseconds,
/// big-endian.
pub fn tai64n(since_unix: std::time::Duration) -> [u8; 12] {
    let mut t = [0u8; 12];
    t[..8].copy_from_slice(&(since_unix.as_secs() + (1u64 << 62)).to_be_bytes());
    t[8..].copy_from_slice(&since_unix.subsec_nanos().to_be_bytes());
    t
}

pub fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::getrandom(&mut b).expect("random");
    b
}

/// Constant-time comparison.
pub fn eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_initial_chaining_key_and_hash_are_the_protocols() {
        // Ci = HASH(CONSTRUCTION); Hi = HASH(Ci || IDENTIFIER). Values as the
        // WireGuard implementations precompute them.
        let c = hash(&[CONSTRUCTION]);
        let h = hash(&[&c, IDENTIFIER]);
        assert_eq!(hex(&c), "60e26daef327efc02ec335e2a025d2d016eb4206f87277f52d38d1988b78cd36");
        assert_eq!(hex(&h), "2211b361081ac566691243db458ad5322d9c6c662293e8b70ee19c65ba079ef3");
    }

    #[test]
    fn aead_round_trips_and_refuses_a_flipped_bit() {
        let k = [7u8; 32];
        let mut s = seal_vec(&k, 3, b"ad", b"hello");
        assert_eq!(&open_vec(&k, 3, b"ad", &s).unwrap()[..], b"hello");
        assert!(open_vec(&k, 4, b"ad", &s).is_none());
        s[0] ^= 1;
        assert!(open_vec(&k, 3, b"ad", &s).is_none());
        let n = [1u8; 24];
        let x = xseal(&k, &n, b"m", &[9u8; 16]);
        assert_eq!(xopen(&k, &n, b"m", &x), Some([9u8; 16]));
        assert_eq!(xopen(&k, &n, b"n", &x), None);
    }

    #[test]
    fn tai64n_is_ordered() {
        let a = tai64n(std::time::Duration::new(1, 999_999_999));
        let b = tai64n(std::time::Duration::new(2, 0));
        assert!(a < b);
        assert_eq!(&a[..8], &(1u64 + (1 << 62)).to_be_bytes());
    }

    pub fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
}
