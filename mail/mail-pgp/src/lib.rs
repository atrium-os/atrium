//! OpenPGP for Mail (mail.md §10.2), over rpgp (MIT/Apache; Sequoia is
//! LGPL and out): what a public key is and who it is for, Autocrypt
//! headers (how keys arrive), checking a PGP/MIME signature against the
//! keys Mail has for the sender, and encrypting to recipients' keys.
//! Mail never holds a private key: signing and reading encrypted mail are
//! Arca's to do (keys in Arca, use-not-read), which is still to come.

use std::io::Cursor;

use pgp::composed::{ArmorOptions, Deserializable, DetachedSignature, MessageBuilder, SignedPublicKey};
use pgp::crypto::sym::SymmetricKeyAlgorithm;
use pgp::types::KeyDetails;

/// The largest key accepted (bytes, armored or not).
pub const KEY_MAX: usize = 64 << 10;

/// What a public key is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyInfo {
    /// Upper-case hex.
    pub fingerprint: String,
    /// The addresses of its user ids, lower case.
    pub emails: Vec<String>,
    pub can_encrypt: bool,
    /// When the key stops being valid (seconds since 1970), if it does.
    pub expires: Option<i64>,
    /// Its owner revoked it (a revocation signature that checks).
    pub revoked: bool,
}

impl KeyInfo {
    /// Fit to encrypt to at `now`: not revoked, not expired, with a key
    /// that encrypts.
    pub fn usable(&self, now: i64) -> bool {
        self.can_encrypt && !self.revoked && self.expires.is_none_or(|e| e > now)
    }

    /// "1A2B 3C4D …": the last 16 hex digits in fours, for showing.
    pub fn short(&self) -> String {
        let f = &self.fingerprint[self.fingerprint.len().saturating_sub(16)..];
        f.as_bytes().chunks(4).map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join(" ")
    }
}

fn email_of(uid: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(uid);
    let a = match (s.rfind('<'), s.rfind('>')) {
        (Some(i), Some(j)) if j > i + 1 => s[i + 1..j].to_string(),
        _ => s.trim().to_string(),
    };
    let a = a.trim().to_ascii_lowercase();
    (a.contains('@') && !a.contains(char::is_whitespace)).then_some(a)
}

/// A public key from its bytes (armored or binary), its self-signatures
/// checked.
pub fn read_key(bytes: &[u8]) -> Result<SignedPublicKey, String> {
    if bytes.len() > KEY_MAX {
        return Err("key too large".into());
    }
    let (k, _) = SignedPublicKey::from_reader_single(Cursor::new(bytes)).map_err(|e| format!("not a public key: {e}"))?;
    k.verify_bindings().map_err(|e| format!("the key's own signatures don't check: {e}"))?;
    Ok(k)
}

/// When a key made at `created` expires, by the newest of `sigs` (its
/// self-signatures) that says.
fn expiry(created: u32, sigs: &[&pgp::packet::Signature]) -> Option<i64> {
    let newest = sigs.iter().max_by_key(|s| s.created().map(|t| t.as_secs()).unwrap_or(0))?;
    let d: std::time::Duration = newest.key_expiration_time()?.into();
    (d.as_secs() > 0).then(|| created as i64 + d.as_secs() as i64)
}

/// When the primary key of `k` expires.
fn key_expiry(k: &SignedPublicKey) -> Option<i64> {
    let sigs: Vec<&pgp::packet::Signature> = k.details.users.iter().flat_map(|u| &u.signatures).chain(&k.details.direct_signatures).collect();
    expiry(k.primary_key.created_at().as_secs(), &sigs)
}

/// The subkeys of `k` that can be encrypted to at `now`.
fn live_encryption_subkeys(k: &SignedPublicKey, now: i64) -> impl Iterator<Item = &pgp::composed::SignedPublicSubKey> {
    k.public_subkeys.iter().filter(move |s| {
        let revoked = s.signatures.iter().any(|g| g.typ() == Some(pgp::packet::SignatureType::SubkeyRevocation));
        let sigs: Vec<&pgp::packet::Signature> = s.signatures.iter().collect();
        s.key.algorithm().can_encrypt() && !revoked && expiry(s.key.created_at().as_secs(), &sigs).is_none_or(|e| e > now)
    })
}

pub fn info(k: &SignedPublicKey) -> KeyInfo {
    let mut emails: Vec<String> = k.details.users.iter().filter_map(|u| email_of(u.id.id())).collect();
    emails.sort();
    emails.dedup();
    let can_encrypt = k.primary_key.algorithm().can_encrypt() || k.public_subkeys.iter().any(|s| s.key.algorithm().can_encrypt());
    let revoked = !k.details.revocation_signatures.is_empty();
    KeyInfo { fingerprint: format!("{:X}", k.primary_key.fingerprint()), emails, can_encrypt, expires: key_expiry(k), revoked }
}

pub fn key_info(bytes: &[u8]) -> Result<KeyInfo, String> {
    read_key(bytes).map(|k| info(&k))
}

/// The key in binary form (how Mail keeps it).
pub fn to_binary(k: &SignedPublicKey) -> Result<Vec<u8>, String> {
    use pgp::ser::Serialize;
    k.to_bytes().map_err(|e| e.to_string())
}

/// An `Autocrypt:` header's address and key (binary), if it reads and the
/// key is for that address. Attributes Mail doesn't know that start with
/// `_` are ignored; any other unknown one makes the header not count.
pub fn autocrypt(value: &str) -> Option<(String, Vec<u8>)> {
    let (mut addr, mut keydata) = (None, None);
    for attr in value.split(';') {
        let (k, v) = attr.split_once('=')?;
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
        match k.as_str() {
            "addr" => addr = Some(v.to_ascii_lowercase()),
            "keydata" => keydata = Some(v.chars().filter(|c| !c.is_whitespace()).collect::<String>()),
            "prefer-encrypt" => {}
            _ if k.starts_with('_') => {}
            _ => return None,
        }
    }
    let (addr, keydata) = (addr?, keydata?);
    let bytes = base64(&keydata)?;
    let k = read_key(&bytes).ok()?;
    if !info(&k).emails.contains(&addr) {
        return None;
    }
    Some((addr, to_binary(&k).ok()?))
}

fn base64(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let s = s.trim_end_matches('=').as_bytes();
    if s.len() > KEY_MAX * 2 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for c in s.chunks(4) {
        let mut n = 0u32;
        for (i, b) in c.iter().enumerate() {
            n |= val(*b)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if c.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if c.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

/// What a signature check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Made by a key Mail has for the sender, and the key is for From.
    Valid,
    /// Made by a key Mail has, but the key isn't for the From address.
    NotForFrom,
    /// No key Mail has made it: who signed is unknown.
    NoKey,
    /// The signature doesn't match the message (or doesn't read).
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub state: State,
    pub signer: Option<KeyInfo>,
}

/// Check a detached signature (`application/pgp-signature`, usually
/// armored) over `content` (the signed MIME part, CRLF line ends, as
/// sent) against `keys`, for the `from` address.
pub fn verify(content: &[u8], signature: &[u8], keys: &[Vec<u8>], from: &str) -> Check {
    let invalid = |why: String| Check { state: State::Invalid(why), signer: None };
    let sig = match DetachedSignature::from_reader_single(Cursor::new(signature)) {
        Ok((s, _)) => s,
        Err(e) => return invalid(format!("the signature doesn't read: {e}")),
    };
    let fps: Vec<String> = sig.signature.issuer_fingerprint().iter().map(|f| format!("{f:X}")).collect();
    let ids: Vec<String> = sig.signature.issuer_key_id().iter().map(|k| format!("{k:?}")).collect();
    for kb in keys {
        let Ok(k) = read_key(kb) else { continue };
        let i = info(&k);
        let names = |fp: String, id: String| fps.contains(&fp) || ids.contains(&id) || (fps.is_empty() && ids.is_empty());
        let primary = names(format!("{:X}", k.primary_key.fingerprint()), format!("{:?}", k.primary_key.legacy_key_id()));
        let result = if primary {
            Some(sig.verify(&k.primary_key, content))
        } else {
            k.public_subkeys.iter().find(|s| names(format!("{:X}", s.key.fingerprint()), format!("{:?}", s.key.legacy_key_id()))).map(|s| sig.verify(&s.key, content))
        };
        match result {
            Some(Ok(())) => {
                let state = if i.emails.iter().any(|e| e.eq_ignore_ascii_case(from.trim())) { State::Valid } else { State::NotForFrom };
                return Check { state, signer: Some(i) };
            }
            Some(Err(e)) if !fps.is_empty() || !ids.is_empty() => return Check { state: State::Invalid(format!("the signature doesn't match: {e}")), signer: Some(i) },
            _ => {}
        }
    }
    Check { state: State::NoKey, signer: None }
}

/// `content` (a MIME entity) encrypted to every one of `keys`, armored,
/// for a PGP/MIME `multipart/encrypted` message.
pub fn encrypt(content: &[u8], keys: &[Vec<u8>]) -> Result<Vec<u8>, String> {
    let mut rng = rand::rngs::OsRng;
    let mut b = MessageBuilder::from_bytes("", content.to_vec()).seipd_v1(&mut rng, SymmetricKeyAlgorithm::AES256);
    if keys.is_empty() {
        return Err("no one to encrypt to".into());
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    for kb in keys {
        let k = read_key(kb)?;
        let i = info(&k);
        if i.revoked {
            return Err(format!("the key {} was revoked by its owner", i.short()));
        }
        if i.expires.is_some_and(|e| e <= now) {
            return Err(format!("the key {} has expired", i.short()));
        }
        let sub = live_encryption_subkeys(&k, now).next();
        if let Some(s) = sub {
            b.encrypt_to_key(&mut rng, &s.key).map_err(|e| e.to_string())?;
        } else if k.primary_key.algorithm().can_encrypt() {
            b.encrypt_to_key(&mut rng, &k.primary_key).map_err(|e| e.to_string())?;
        } else {
            return Err(format!("the key {} can't encrypt", i.short()));
        }
    }
    b.to_armored_string(&mut rng, ArmorOptions::default()).map(String::into_bytes).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgp::composed::{EncryptionCaps, KeyType, Message, SecretKeyParamsBuilder, SignedSecretKey, SubkeyParamsBuilder};
    use pgp::crypto::{ecc_curve::ECCCurve, hash::HashAlgorithm};
    use pgp::types::Password;

    #[test]
    fn expired_and_revoked_keys() {
        // Made by GnuPG 2.4: one that expired 2 s after it was made, one
        // that lasts two years, one revoked.
        let expired = key_info(include_bytes!("../tests/keys/expired.asc")).unwrap();
        assert!(expired.expires.is_some() && !expired.revoked);
        assert!(!expired.usable(expired.expires.unwrap()) && expired.usable(expired.expires.unwrap() - 1));
        let later = key_info(include_bytes!("../tests/keys/expiring.asc")).unwrap();
        let e = later.expires.unwrap();
        assert!(e > 1_795_000_000 + 600 * 86_400, "about two years after 2026-11-19: {e}");
        let gone = key_info(include_bytes!("../tests/keys/revoked.asc")).unwrap();
        assert!(gone.revoked && gone.expires.is_none() && !gone.usable(0));
        assert_eq!(gone.emails, ["gone@example.com"]);
        // Never encrypted to.
        let bin = |a: &[u8]| to_binary(&read_key(a).unwrap()).unwrap();
        assert!(encrypt(b"x", &[bin(include_bytes!("../tests/keys/expired.asc"))]).unwrap_err().contains("has expired"));
        assert!(encrypt(b"x", &[bin(include_bytes!("../tests/keys/revoked.asc"))]).unwrap_err().contains("revoked"));
        assert!(encrypt(b"x", &[bin(include_bytes!("../tests/keys/expiring.asc"))]).is_ok());
    }

    fn key(uid: &str) -> SignedSecretKey {
        let mut p = SecretKeyParamsBuilder::default();
        p.key_type(KeyType::Ed25519Legacy)
            .can_certify(true)
            .can_sign(true)
            .primary_user_id(uid.into())
            .subkeys(vec![SubkeyParamsBuilder::default().key_type(KeyType::ECDH(ECCCurve::Curve25519Legacy)).can_encrypt(EncryptionCaps::All).build().unwrap()]);
        p.build().unwrap().generate(rand::thread_rng()).unwrap()
    }

    fn b64(data: &[u8]) -> String {
        const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut s = String::new();
        for c in data.chunks(3) {
            let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
            for i in 0..4 {
                if i <= c.len() { s.push(T[(n >> (18 - 6 * i) & 63) as usize] as char) } else { s.push('=') }
            }
        }
        s
    }

    #[test]
    fn keys_autocrypt_verify_encrypt() {
        let priya = key("Priya Nair <Priya@Example.com>");
        let public = priya.to_public_key();
        let bin = to_binary(&public).unwrap();
        let i = key_info(&bin).unwrap();
        assert_eq!(i.emails, vec!["priya@example.com".to_string()]);
        assert!(i.can_encrypt);
        assert_eq!(i.fingerprint.len(), 40);
        let armored = public.to_armored_bytes(ArmorOptions::default()).unwrap();
        assert_eq!(key_info(&armored).unwrap(), i);

        // Autocrypt: the key for its address; a key for another address, or an unknown attribute, doesn't count.
        let h = format!("addr=priya@example.com; prefer-encrypt=mutual; keydata={}", b64(&bin));
        assert_eq!(autocrypt(&h), Some(("priya@example.com".into(), bin.clone())));
        assert_eq!(autocrypt(&h.replace("addr=priya", "addr=mallory")), None);
        assert_eq!(autocrypt(&format!("{h}; critical=1")), None);
        assert!(autocrypt(&format!("{h}; _ignored=1")).is_some());

        // A detached signature over a MIME part.
        let content = b"Content-Type: text/plain\r\n\r\nThe numbers.\r\n";
        let sig = DetachedSignature::sign_binary_data(rand::thread_rng(), &priya.primary_key, &Password::empty(), HashAlgorithm::Sha256, &content[..]).unwrap();
        let sig = sig.to_armored_bytes(ArmorOptions::default()).unwrap();
        let ok = verify(content, &sig, &[bin.clone()], "priya@example.com");
        assert_eq!(ok.state, State::Valid);
        assert_eq!(ok.signer.as_ref().unwrap().fingerprint, i.fingerprint);
        assert_eq!(verify(content, &sig, &[bin.clone()], "boss@example.com").state, State::NotForFrom);
        assert_eq!(verify(content, &sig, &[], "priya@example.com").state, State::NoKey);
        let other = to_binary(&key("Other <o@example.com>").to_public_key()).unwrap();
        assert_eq!(verify(content, &sig, &[other], "priya@example.com").state, State::NoKey);
        assert!(matches!(verify(b"Content-Type: text/plain\r\n\r\nOther numbers.\r\n", &sig, &[bin.clone()], "priya@example.com").state, State::Invalid(_)));
        assert!(matches!(verify(content, b"junk", &[bin.clone()], "priya@example.com").state, State::Invalid(_)));

        // Encrypted to her: she can read it, with her key.
        let enc = encrypt(content, &[bin.clone()]).unwrap();
        assert!(enc.starts_with(b"-----BEGIN PGP MESSAGE-----"));
        let (m, _) = Message::from_armor(Cursor::new(&enc)).unwrap();
        let mut d = m.decrypt(&Password::empty(), &priya).unwrap();
        if d.is_compressed() {
            d = d.decompress().unwrap();
        }
        assert_eq!(d.as_data_vec().unwrap(), content);
        assert!(encrypt(content, &[]).is_err());
    }
}
