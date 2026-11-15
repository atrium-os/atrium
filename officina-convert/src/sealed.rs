//! Native files with a password (§16.40 O7a): the native bytes sealed with
//! a random 256-bit data key, the data key wrapped by a key Argon2id makes
//! from the password.
//!
//! Layout (little-endian):
//!
//! | bytes | what |
//! |---|---|
//! | 16 | `OFFICINA-SEALED\0` |
//! | 4 | version, 1 |
//! | 12 | Argon2id passes, memory (KiB), lanes |
//! | 16 | salt |
//! | 12 + 48 | wrap nonce; data key under AES-256-GCM with the password key |
//! | 8 | file id: random per seal, the first half of every segment nonce |
//! | 4 | segment size |
//! | 8 | plain length |
//! | … | segments: AES-256-GCM, nonce = file id ‖ index (u32), additional data = SHA-256 of the header |
//!
//! Changing the password rewraps the key; each save takes a new file id, so
//! no nonce repeats under one key. The same [`Sealer`] seals the autosave
//! journal of a protected document, so nothing of it reaches the disk in
//! the clear.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use sha2::{Digest, Sha256};

use crate::crypt::{random, CryptError};

const MAGIC: &[u8; 16] = b"OFFICINA-SEALED\0";
const VERSION: u32 = 1;
const SEGMENT: usize = 1 << 20;
const HEADER: usize = 16 + 4 + 12 + 16 + 12 + 48 + 8 + 4 + 8;
/// The Argon2id cost of a new password: 3 passes over 64 MiB, 4 lanes
/// (RFC 9106's second recommended option).
const PASSES: u32 = 3;
const MEMORY: u32 = 64 * 1024;
const LANES: u32 = 4;
const MAX_MEMORY: u32 = 1 << 20;
const MAX_PASSES: u32 = 64;
/// The largest sealed file opened (the native formats' own ceilings are
/// lower).
const MAX_PLAIN: u64 = 16 << 30;

/// Whether `b` is a sealed native file.
pub fn is_sealed(b: &[u8]) -> bool {
    b.len() >= HEADER && &b[..16] == MAGIC
}

/// A document's key: seals its saves and journal.
#[derive(Clone)]
pub struct Sealer {
    key: [u8; 32],
    /// Argon2 parameters, salt and wrapped key, as written.
    wrap: Vec<u8>,
}

impl std::fmt::Debug for Sealer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Sealer(..)")
    }
}

fn kek(password: &str, salt: &[u8], (t, m, p): (u32, u32, u32)) -> Result<[u8; 32], CryptError> {
    if t == 0 || t > MAX_PASSES || m < 8 * p || m > MAX_MEMORY || p == 0 || p > 64 {
        return Err(CryptError::Damaged("Argon2 parameters".into()));
    }
    let params = argon2::Params::new(m, t, p, Some(32)).map_err(|_| CryptError::Damaged("Argon2 parameters".into()))?;
    let mut k = [0u8; 32];
    argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
        .hash_password_into(password.as_bytes(), salt, &mut k)
        .map_err(|_| CryptError::Damaged("Argon2 parameters".into()))?;
    Ok(k)
}

impl Sealer {
    /// A new random key, wrapped under `password`.
    pub fn new(password: &str) -> Sealer {
        Self::with_cost(password, (PASSES, MEMORY, LANES))
    }

    /// [`Sealer::new`] at another Argon2id cost (tests use less).
    pub fn with_cost(password: &str, cost: (u32, u32, u32)) -> Sealer {
        let key: [u8; 32] = random(32).try_into().unwrap();
        let mut s = Sealer { key, wrap: Vec::new() };
        s.rewrap(password, cost);
        s
    }

    /// The same key, wrapped under a new password.
    pub fn rewrap(&mut self, password: &str, cost: (u32, u32, u32)) {
        let salt = random(16);
        let k = kek(password, &salt, cost).expect("our own parameters");
        let nonce = random(12);
        let wrapped = Aes256Gcm::new_from_slice(&k).unwrap().encrypt(Nonce::from_slice(&nonce), &self.key[..]).expect("in-memory encryption");
        let mut w = Vec::with_capacity(12 + 16 + 12 + 48);
        for v in [cost.0, cost.1, cost.2] {
            w.extend_from_slice(&v.to_le_bytes());
        }
        w.extend_from_slice(&salt);
        w.extend_from_slice(&nonce);
        w.extend_from_slice(&wrapped);
        self.wrap = w;
    }

    /// `plain` sealed.
    pub fn seal(&self, plain: &[u8]) -> Vec<u8> {
        let id = random(8);
        let mut h = Vec::with_capacity(HEADER + plain.len() + plain.len() / SEGMENT * 16 + 16);
        h.extend_from_slice(MAGIC);
        h.extend_from_slice(&VERSION.to_le_bytes());
        h.extend_from_slice(&self.wrap);
        h.extend_from_slice(&id);
        h.extend_from_slice(&(SEGMENT as u32).to_le_bytes());
        h.extend_from_slice(&(plain.len() as u64).to_le_bytes());
        let aad = Sha256::digest(&h);
        let c = Aes256Gcm::new_from_slice(&self.key).unwrap();
        // An empty file is one empty segment, so it is authenticated too.
        let chunks: Vec<&[u8]> = if plain.is_empty() { vec![&[][..]] } else { plain.chunks(SEGMENT).collect() };
        for (i, chunk) in chunks.into_iter().enumerate() {
            let mut n = id.clone();
            n.extend_from_slice(&(i as u32).to_le_bytes());
            let ct = c.encrypt(Nonce::from_slice(&n), Payload { msg: chunk, aad: &aad }).expect("in-memory encryption");
            h.extend_from_slice(&ct);
        }
        h
    }
}

/// The native bytes in `file`, a sealed file, and its [`Sealer`] (to seal
/// the next save with).
pub fn unseal(file: &[u8], password: &str) -> Result<(Vec<u8>, Sealer), CryptError> {
    if !is_sealed(file) {
        return Err(CryptError::Damaged("not a sealed file".into()));
    }
    let u32_at = |o: usize| u32::from_le_bytes(file[o..o + 4].try_into().unwrap());
    if u32_at(16) != VERSION {
        return Err(CryptError::Unsupported(format!("sealed file version {}", u32_at(16))));
    }
    let cost = (u32_at(20), u32_at(24), u32_at(28));
    let salt = &file[32..48];
    let wrap_nonce = &file[48..60];
    let wrapped = &file[60..108];
    let id = &file[108..116];
    let seg = u32_at(116) as usize;
    let len = u64::from_le_bytes(file[120..128].try_into().unwrap());
    if seg == 0 || seg > 64 << 20 || len > MAX_PLAIN {
        return Err(CryptError::Damaged("sealed file header".into()));
    }
    let k = kek(password, salt, cost)?;
    let key: [u8; 32] = Aes256Gcm::new_from_slice(&k)
        .unwrap()
        .decrypt(Nonce::from_slice(wrap_nonce), wrapped)
        .map_err(|_| CryptError::WrongPassword)?
        .try_into()
        .map_err(|_| CryptError::Damaged("wrapped key".into()))?;
    let aad = Sha256::digest(&file[..HEADER]);
    let c = Aes256Gcm::new_from_slice(&key).unwrap();
    let segments = (len as usize).div_ceil(seg).max(1);
    let body = &file[HEADER..];
    if body.len() as u64 != len + segments as u64 * 16 {
        return Err(CryptError::Damaged("sealed file length".into()));
    }
    let mut out = Vec::with_capacity(len as usize);
    for (i, ct) in body.chunks(seg + 16).enumerate() {
        let mut n = id.to_vec();
        n.extend_from_slice(&(i as u32).to_le_bytes());
        let p = c.decrypt(Nonce::from_slice(&n), Payload { msg: ct, aad: &aad }).map_err(|_| CryptError::Damaged("a sealed segment fails its check".into()))?;
        out.extend_from_slice(&p);
    }
    Ok((out, Sealer { key, wrap: file[20..108].to_vec() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHEAP: (u32, u32, u32) = (1, 64, 1);

    #[test]
    fn seal_and_unseal() {
        let s = Sealer::with_cost("hunter2", CHEAP);
        for n in [0usize, 1, SEGMENT - 1, SEGMENT, SEGMENT + 5, 3 * SEGMENT] {
            let plain: Vec<u8> = (0..n).map(|i| (i * 7 % 251) as u8).collect();
            let f = s.seal(&plain);
            assert!(is_sealed(&f));
            let (back, s2) = unseal(&f, "hunter2").unwrap();
            assert_eq!(back, plain, "{n}");
            // The key comes back: the next save is sealed the same way.
            assert_eq!(unseal(&s2.seal(b"next"), "hunter2").unwrap().0, b"next");
            assert!(matches!(unseal(&f, "hunter3"), Err(CryptError::WrongPassword)));
        }
        // Two seals of one text differ (a new file id each time).
        assert_ne!(s.seal(b"same"), s.seal(b"same"));
    }

    #[test]
    fn a_new_password_keeps_the_key() {
        let mut s = Sealer::with_cost("old", CHEAP);
        let before = s.seal(b"text");
        s.rewrap("new", CHEAP);
        let after = s.seal(b"text");
        assert_eq!(unseal(&after, "new").unwrap().0, b"text");
        assert!(matches!(unseal(&after, "old"), Err(CryptError::WrongPassword)));
        assert_eq!(unseal(&before, "old").unwrap().0, b"text");
    }

    #[test]
    fn tampering_is_caught() {
        let s = Sealer::with_cost("pw", CHEAP);
        let plain = vec![9u8; SEGMENT * 2 + 10];
        let f = s.seal(&plain);
        for at in [20, 40, 110, 117, 125, HEADER + 3, HEADER + SEGMENT + 20, f.len() - 1] {
            let mut g = f.clone();
            g[at] ^= 1;
            assert!(unseal(&g, "pw").is_err(), "byte {at}");
        }
        // Truncated by a whole segment, or with segments swapped.
        assert!(unseal(&f[..HEADER + SEGMENT + 16], "pw").is_err());
        let mut g = f.clone();
        let (a, b) = (HEADER, HEADER + SEGMENT + 16);
        let first = g[a..b].to_vec();
        g.copy_within(b..b + SEGMENT + 16, a);
        g[b..b + SEGMENT + 16].copy_from_slice(&first);
        assert!(unseal(&g, "pw").is_err());
        // Absurd Argon2 costs are refused before any work.
        let mut g = f.clone();
        g[24..28].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(unseal(&g, "pw"), Err(CryptError::Damaged(_))));
    }
}
