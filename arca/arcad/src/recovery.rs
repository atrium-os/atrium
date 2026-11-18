//! Recovery codes for sealed sticks (tessera-encryption.md §7.4).
//!
//! The code is 128 random bits and a 22-bit checksum, written as 6 groups
//! of 5 base-32 characters. Argon2id (arcad's cost, stored in the slot)
//! turns the 16 secret bytes into a key that wraps the volume key with
//! XChaCha20-Poly1305, bound to the volume's uuid and key id. The result is
//! a 256-byte `tessera_key_slot_t` of type 3 that the kernel writes into the
//! superblock, under its HMAC.
//!
//! Slot layout (format.h): `slot_type` 3 | `kdf_algorithm` 1 | `flags` u16 0
//! | `kdf_iterations` u32 (t_cost) | `kdf_salt[16]` | `wrapped_key[80]`
//! (nonce 24, ct 32, tag 16, 8 zero) | 40 zero (TPM fields) |
//! `credential_id[16]` = the key id | `reserved[96]`: m_kib u32, p_cost u32,
//! the stick's name sealed under the same key (`len u8 | name[63]`, zero
//! padded so its length doesn't show, then a 16-byte tag; the nonce is the
//! key's with its last byte flipped), then 8 zero. A slot made before the
//! name was added has zeros there, and opens with no name.

use arca_store::secret::LockedBuf;
use arca_store::{KdfParams, RootKey};
use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{Tag, XChaCha20Poly1305, XNonce};
use sha2::{Digest, Sha256};

pub const SLOT_LEN: usize = 256;
pub const SLOT_RECOVERY: u8 = 3;
const KDF_ARGON2ID: u8 = 1;
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const OFF_T: usize = 4;
const OFF_SALT: usize = 8;
const OFF_WRAP: usize = 24;
const OFF_CRED: usize = 144;
const OFF_M: usize = 160;
const OFF_P: usize = 164;
const OFF_NAME: usize = 168;
const NAME_LEN: usize = 64;
/// The longest name a slot carries, in bytes.
pub const NAME_MAX: usize = NAME_LEN - 1;
/// The most a slot may ask of arcad: a slot is read from a stick anyone
/// could have written.
const MAX_M_KIB: u32 = 1024 * 1024;
const MAX_T: u32 = 16;

fn checksum(secret: &[u8; 16]) -> u32 {
    let h = Sha256::digest([b"tessera recovery code".as_slice(), secret].concat());
    u32::from_be_bytes([0, h[0], h[1], h[2]]) >> 2
}

/// `XXXXX-XXXXX-XXXXX-XXXXX-XXXXX-XXXXX`.
pub fn encode(secret: &[u8; 16]) -> String {
    // 150 bits: the secret, then the 22-bit checksum.
    let mut bits = u128::from_be_bytes(*secret);
    let ck = checksum(secret);
    let mut out = String::with_capacity(35);
    let mut acc: u32 = 0;
    let mut n = 0;
    let mut chars = Vec::with_capacity(30);
    for _ in 0..128 {
        acc = (acc << 1) | (bits >> 127) as u32;
        bits <<= 1;
        n += 1;
        if n == 5 {
            chars.push(ALPHABET[acc as usize]);
            acc = 0;
            n = 0;
        }
    }
    for i in (0..22).rev() {
        acc = (acc << 1) | ((ck >> i) & 1);
        n += 1;
        if n == 5 {
            chars.push(ALPHABET[acc as usize]);
            acc = 0;
            n = 0;
        }
    }
    debug_assert_eq!(n, 0);
    for (i, c) in chars.iter().enumerate() {
        if i > 0 && i % 5 == 0 {
            out.push('-');
        }
        out.push(*c as char);
    }
    out
}

/// The secret, if `code` is a well-formed code (case, spaces and dashes
/// ignored; O is read as 0, I and L as 1).
pub fn decode(code: &str) -> Option<[u8; 16]> {
    let mut vals = Vec::with_capacity(30);
    for c in code.chars() {
        let c = match c.to_ascii_uppercase() {
            ' ' | '-' => continue,
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        };
        vals.push(ALPHABET.iter().position(|&a| a as char == c)? as u32);
    }
    if vals.len() != 30 {
        return None;
    }
    let mut bits: u128 = 0;
    let mut ck: u32 = 0;
    let mut i = 0;
    for v in vals {
        for b in (0..5).rev() {
            let bit = (v >> b) & 1;
            if i < 128 {
                bits = (bits << 1) | bit as u128;
            } else {
                ck = (ck << 1) | bit;
            }
            i += 1;
        }
    }
    let secret = bits.to_be_bytes();
    (checksum(&secret) == ck).then_some(secret)
}

fn aad(uuid: &[u8; 16], key_id: &[u8; 16]) -> Vec<u8> {
    [b"tessera recovery slot".as_slice(), uuid, key_id].concat()
}

fn name_nonce(slot: &[u8]) -> XNonce {
    let mut n = [0u8; 24];
    n.copy_from_slice(&slot[OFF_WRAP..OFF_WRAP + 24]);
    n[23] ^= 0xff;
    *XNonce::from_slice(&n)
}

fn name_aad(uuid: &[u8; 16], key_id: &[u8; 16]) -> Vec<u8> {
    [b"tessera recovery name".as_slice(), uuid, key_id].concat()
}

/// `name` cut to `NAME_MAX` bytes on a character boundary.
fn clip(name: &str) -> &str {
    let mut end = name.len().min(NAME_MAX);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
}

fn kek(secret: &[u8; 16], salt: &[u8; 16], kdf: KdfParams) -> Option<XChaCha20Poly1305> {
    let k = RootKey::derive(secret, salt, kdf).ok()?;
    XChaCha20Poly1305::new_from_slice(&k.as_bytes()[..32]).ok()
}

/// A new code and the slot that opens `vk` with it, carrying the stick's
/// `name` (at most `NAME_MAX` bytes kept) for whoever opens it with the code.
pub fn make(vk: &[u8], uuid: &[u8; 16], key_id: &[u8; 16], name: &str, kdf: KdfParams) -> Option<(String, [u8; SLOT_LEN])> {
    if vk.len() != 32 {
        return None;
    }
    let mut secret = [0u8; 16];
    let mut salt = [0u8; 16];
    let mut nonce = [0u8; 24];
    getrandom::getrandom(&mut secret).ok()?;
    getrandom::getrandom(&mut salt).ok()?;
    getrandom::getrandom(&mut nonce).ok()?;
    let cipher = kek(&secret, &salt, kdf)?;
    let mut s = [0u8; SLOT_LEN];
    s[0] = SLOT_RECOVERY;
    s[1] = KDF_ARGON2ID;
    s[OFF_T..OFF_T + 4].copy_from_slice(&kdf.t_cost.to_le_bytes());
    s[OFF_SALT..OFF_SALT + 16].copy_from_slice(&salt);
    s[OFF_WRAP..OFF_WRAP + 24].copy_from_slice(&nonce);
    s[OFF_WRAP + 24..OFF_WRAP + 56].copy_from_slice(vk);
    let tag = cipher.encrypt_in_place_detached(XNonce::from_slice(&nonce), &aad(uuid, key_id), &mut s[OFF_WRAP + 24..OFF_WRAP + 56]).ok()?;
    s[OFF_WRAP + 56..OFF_WRAP + 72].copy_from_slice(&tag);
    s[OFF_CRED..OFF_CRED + 16].copy_from_slice(key_id);
    s[OFF_M..OFF_M + 4].copy_from_slice(&kdf.m_kib.to_le_bytes());
    s[OFF_P..OFF_P + 4].copy_from_slice(&kdf.p_cost.to_le_bytes());
    let name = clip(name);
    s[OFF_NAME] = name.len() as u8;
    s[OFF_NAME + 1..OFF_NAME + 1 + name.len()].copy_from_slice(name.as_bytes());
    let nonce = name_nonce(&s);
    let tag = cipher.encrypt_in_place_detached(&nonce, &name_aad(uuid, key_id), &mut s[OFF_NAME..OFF_NAME + NAME_LEN]).ok()?;
    s[OFF_NAME + NAME_LEN..OFF_NAME + NAME_LEN + 16].copy_from_slice(&tag);
    let code = encode(&secret);
    secret.fill(0);
    Some((code, s))
}

/// Why a slot didn't open.
#[derive(Debug, PartialEq, Eq)]
pub enum OpenError {
    /// Not a recovery slot arcad can read, or for another volume.
    BadSlot,
    /// Not a code at all (a typo the checksum caught).
    BadCode,
    /// A code, but not this slot's.
    WrongCode,
}

/// The volume key and the stick's name (empty if the slot has none), if
/// `code` opens `slot` for this volume.
pub fn open(slot: &[u8], uuid: &[u8; 16], key_id: &[u8; 16], code: &str) -> Result<(LockedBuf, String), OpenError> {
    if slot.len() != SLOT_LEN || slot[0] != SLOT_RECOVERY || slot[1] != KDF_ARGON2ID || slot[OFF_CRED..OFF_CRED + 16] != key_id[..] {
        return Err(OpenError::BadSlot);
    }
    let u32_at = |o: usize| u32::from_le_bytes(slot[o..o + 4].try_into().unwrap());
    let kdf = KdfParams { m_kib: u32_at(OFF_M), t_cost: u32_at(OFF_T), p_cost: u32_at(OFF_P) };
    if kdf.m_kib > MAX_M_KIB || kdf.t_cost == 0 || kdf.t_cost > MAX_T || kdf.p_cost != 1 {
        return Err(OpenError::BadSlot);
    }
    let mut secret = decode(code).ok_or(OpenError::BadCode)?;
    let salt: [u8; 16] = slot[OFF_SALT..OFF_SALT + 16].try_into().unwrap();
    let cipher = kek(&secret, &salt, kdf);
    secret.fill(0);
    let cipher = cipher.ok_or(OpenError::BadSlot)?;
    let mut vk = LockedBuf::from_slice(&slot[OFF_WRAP + 24..OFF_WRAP + 56]).map_err(|_| OpenError::BadSlot)?;
    let nonce = XNonce::from_slice(&slot[OFF_WRAP..OFF_WRAP + 24]);
    let tag = Tag::from_slice(&slot[OFF_WRAP + 56..OFF_WRAP + 72]);
    cipher.decrypt_in_place_detached(nonce, &aad(uuid, key_id), vk.as_mut(), tag).map_err(|_| OpenError::WrongCode)?;
    // The name: a slot without one (or a damaged one) opens all the same.
    let mut n = [0u8; NAME_LEN];
    n.copy_from_slice(&slot[OFF_NAME..OFF_NAME + NAME_LEN]);
    let ntag = Tag::from_slice(&slot[OFF_NAME + NAME_LEN..OFF_NAME + NAME_LEN + 16]);
    let name = match cipher.decrypt_in_place_detached(&name_nonce(slot), &name_aad(uuid, key_id), &mut n, ntag) {
        Ok(()) if (n[0] as usize) <= NAME_MAX => String::from_utf8(n[1..1 + n[0] as usize].to_vec()).unwrap_or_default(),
        _ => String::new(),
    };
    Ok((vk, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_round_trip_and_catch_typos() {
        let s = [0x5au8; 16];
        let c = encode(&s);
        assert_eq!(c.len(), 35);
        assert_eq!(c.split('-').count(), 6);
        assert_eq!(decode(&c), Some(s));
        assert_eq!(decode(&c.to_lowercase().replace('-', " ")), Some(s));
        let mut typo: Vec<char> = c.chars().collect();
        typo[3] = if typo[3] == '7' { '8' } else { '7' };
        assert_eq!(decode(&typo.iter().collect::<String>()), None);
        assert_eq!(decode("ABCDE"), None);
        for _ in 0..50 {
            let mut r = [0u8; 16];
            getrandom::getrandom(&mut r).unwrap();
            assert_eq!(decode(&encode(&r)), Some(r));
        }
    }

    #[test]
    fn a_slot_opens_with_its_code_only() {
        let vk = [7u8; 32];
        let (uuid, kid) = ([1u8; 16], [2u8; 16]);
        let (code, slot) = make(&vk, &uuid, &kid, "Holiday", KdfParams::TEST).unwrap();
        assert_eq!(slot[0], 3);
        assert_eq!(&slot[OFF_WRAP + 24..OFF_WRAP + 56] == vk.as_slice(), false, "wrapped, not clear");
        let (k, name) = open(&slot, &uuid, &kid, &code).unwrap();
        assert_eq!((k.as_ref(), name.as_str()), (vk.as_slice(), "Holiday"));
        assert!(!slot.windows(7).any(|w| w == b"Holiday"), "the name is sealed too");
        let (other, _) = make(&vk, &uuid, &kid, "", KdfParams::TEST).unwrap();
        assert_eq!(open(&slot, &uuid, &kid, &other).err(), Some(OpenError::WrongCode));
        assert_eq!(open(&slot, &uuid, &kid, "nope").err(), Some(OpenError::BadCode));
        // Bound to the volume: another uuid, or key id, doesn't open it.
        assert_eq!(open(&slot, &[9; 16], &kid, &code).err(), Some(OpenError::WrongCode));
        assert_eq!(open(&slot, &uuid, &[9; 16], &code).err(), Some(OpenError::BadSlot));
        // A slot that asks for too much work is refused before any.
        let mut greedy = slot;
        greedy[OFF_M..OFF_M + 4].copy_from_slice(&(MAX_M_KIB + 1).to_le_bytes());
        assert_eq!(open(&greedy, &uuid, &kid, &code).err(), Some(OpenError::BadSlot));
        let mut flipped = slot;
        flipped[OFF_WRAP + 30] ^= 1;
        assert_eq!(open(&flipped, &uuid, &kid, &code).err(), Some(OpenError::WrongCode));
        // A slot from before names: zeros there, so it opens without one.
        let mut old = slot;
        old[OFF_NAME..OFF_NAME + NAME_LEN + 16].fill(0);
        let (k, name) = open(&old, &uuid, &kid, &code).unwrap();
        assert_eq!((k.as_ref(), name.as_str()), (vk.as_slice(), ""));
        // A long name is cut on a character boundary.
        let long = "é".repeat(40);
        let (code, slot) = make(&vk, &uuid, &kid, &long, KdfParams::TEST).unwrap();
        assert_eq!(open(&slot, &uuid, &kid, &code).unwrap().1, "é".repeat(31));
    }
}
