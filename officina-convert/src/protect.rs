//! Sheet and structure protection passwords (docs/spec/officina.md §16.149):
//! making Excel's verifier and checking a password against the verifier a
//! file carries.
//!
//! Excel 2010 and later: `algorithmName`, base64 `saltValue` and
//! `hashValue`, `spinCount`: H0 = H(salt + UTF-16LE password), then
//! Hn = H(Hn-1 + n as u32 LE) for each spin (the iterator after the hash,
//! unlike the agile encryption's). Older files: the 16-bit `password` hash.
//! ODF: `table:protection-key`, base64 SHA-256 (or SHA-1) of the UTF-8
//! password.

use officina_grid::protection::{ProtectionFormat, SheetProtection};
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha384, Sha512};

/// The most spins a file may ask for; Excel writes 100,000.
pub const MAX_SPIN: u32 = 10_000_000;
/// The spins a new verifier uses, as Excel's.
pub const SPIN: u32 = 100_000;
/// The longest password Excel takes.
pub const MAX_PASSWORD: usize = 255;

fn utf16(p: &str) -> Vec<u8> {
    p.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

fn spun<D: Digest>(salt: &[u8], password: &str, spin: u32) -> Vec<u8> {
    let mut x = D::new().chain_update(salt).chain_update(utf16(password)).finalize().to_vec();
    for i in 0..spin {
        x = D::new().chain_update(&x).chain_update(i.to_le_bytes()).finalize().to_vec();
    }
    x
}

fn hash(algorithm: &str, salt: &[u8], password: &str, spin: u32) -> Option<Vec<u8>> {
    Some(match algorithm.to_ascii_uppercase().replace('-', "").as_str() {
        "SHA512" => spun::<Sha512>(salt, password, spin),
        "SHA384" => spun::<Sha384>(salt, password, spin),
        "SHA256" => spun::<Sha256>(salt, password, spin),
        "SHA1" => spun::<Sha1>(salt, password, spin),
        _ => return None,
    })
}

/// Excel's 16-bit password hash of old files, as four hex digits.
pub fn legacy_hash(password: &str) -> String {
    // The low bytes of the characters, at most 15, as Excel takes them.
    let bytes: Vec<u16> = password.encode_utf16().take(15).map(|u| u & 0xff).collect();
    let mut h: u16 = 0;
    for &c in bytes.iter().rev() {
        h = ((h >> 14) & 1) | ((h << 1) & 0x7fff);
        h ^= c;
    }
    h = ((h >> 14) & 1) | ((h << 1) & 0x7fff);
    h ^= bytes.len() as u16;
    h ^= 0xCE4B;
    format!("{h:04X}")
}

/// The settings for a new Excel verifier of `password` with `salt`.
pub fn verifier(password: &str, salt: &[u8; 16]) -> Vec<(&'static str, String)> {
    let h = spun::<Sha512>(salt, password, SPIN);
    vec![
        ("algorithmName", "SHA-512".to_string()),
        ("hashValue", crate::crypt::b64e(&h)),
        ("saltValue", crate::crypt::b64e(salt)),
        ("spinCount", SPIN.to_string()),
    ]
}

/// Whether `p` asks for a password at all.
pub fn has_password(p: &SheetProtection) -> bool {
    p.settings().any(|(k, v)| matches!(k, "hashValue" | "password" | "workbookHashValue" | "workbookPassword" | "table:protection-key") && !v.is_empty())
}

/// Whether `password` opens `p`. Err when its verifier can't be checked
/// here (an unknown algorithm, too many spins, damaged base64).
pub fn check(p: &SheetProtection, password: &str) -> Result<bool, String> {
    let get = |k: &str| p.settings().find(|(key, _)| *key == k).map(|(_, v)| v.to_string());
    if !has_password(p) {
        return Ok(true);
    }
    if password.chars().count() > MAX_PASSWORD {
        return Ok(false);
    }
    let prefix = if p.format() == ProtectionFormat::ExcelWorkbook { "workbook" } else { "" };
    let key = |k: &str| if prefix.is_empty() { k.to_string() } else { format!("{prefix}{}{}", k[..1].to_ascii_uppercase(), &k[1..]) };
    if p.format() == ProtectionFormat::Ods {
        let want = get("table:protection-key").unwrap_or_default();
        let want = crate::crypt::b64(&want).map_err(|_| "The sheet's password record is damaged.".to_string())?;
        let alg = get("loext:protection-key-digest-algorithm-2").or_else(|| get("table:protection-key-digest-algorithm")).unwrap_or_default();
        let have = if alg.ends_with("sha256") { Sha256::digest(password.as_bytes()).to_vec() } else if alg.is_empty() || alg.ends_with("sha1") { Sha1::digest(password.as_bytes()).to_vec() } else { return Err("The sheet's password uses a method this can't check.".into()) };
        return Ok(have == want);
    }
    if let Some(want) = get(&key("hashValue")).filter(|v| !v.is_empty()) {
        let alg = get(&key("algorithmName")).unwrap_or_default();
        let salt = crate::crypt::b64(&get(&key("saltValue")).unwrap_or_default()).map_err(|_| "The sheet's password record is damaged.".to_string())?;
        let spin: u32 = get(&key("spinCount")).and_then(|s| s.parse().ok()).unwrap_or(0);
        if spin > MAX_SPIN {
            return Err("The sheet's password record asks for too much work to check.".into());
        }
        let want = crate::crypt::b64(&want).map_err(|_| "The sheet's password record is damaged.".to_string())?;
        let have = hash(&alg, &salt, password, spin).ok_or("The sheet's password uses a method this can't check.")?;
        return Ok(have == want);
    }
    let want = get(&key("password")).unwrap_or_default();
    Ok(legacy_hash(password).eq_ignore_ascii_case(want.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn excel(settings: &[(&str, String)]) -> SheetProtection {
        let mut p = SheetProtection::new(ProtectionFormat::Excel);
        for (k, v) in settings {
            assert!(p.keep(k, v));
        }
        p
    }

    #[test]
    fn a_new_verifier_checks_its_password_only() {
        let p = excel(&verifier("Secret 1", &[7; 16]));
        assert_eq!(check(&p, "Secret 1"), Ok(true));
        assert_eq!(check(&p, "secret 1"), Ok(false));
        assert_eq!(check(&p, ""), Ok(false));
        assert!(has_password(&p));
        assert_eq!(check(&SheetProtection::new(ProtectionFormat::Excel), ""), Ok(true));
    }

    #[test]
    fn old_and_foreign_verifiers() {
        // The empty password hashes to the constant alone.
        assert_eq!(legacy_hash(""), "CE4B");
        let p = excel(&[("password", legacy_hash("abc"))]);
        assert_eq!(check(&p, "abc"), Ok(true));
        assert_eq!(check(&p, "abd"), Ok(false));
        // The iterator goes after the hash: a verifier made with it first doesn't match.
        let salt = [1u8; 16];
        let mut x = Sha512::new().chain_update(salt).chain_update(utf16("pw")).finalize().to_vec();
        for i in 0..3u32 {
            x = Sha512::new().chain_update(i.to_le_bytes()).chain_update(&x).finalize().to_vec();
        }
        let prepend = excel(&[("algorithmName", "SHA-512".into()), ("hashValue", crate::crypt::b64e(&x)), ("saltValue", crate::crypt::b64e(&salt)), ("spinCount", "3".into())]);
        assert_eq!(check(&prepend, "pw"), Ok(false));
        let append = excel(&[("algorithmName", "SHA-512".into()), ("hashValue", crate::crypt::b64e(&spun::<Sha512>(&salt, "pw", 3))), ("saltValue", crate::crypt::b64e(&salt)), ("spinCount", "3".into())]);
        assert_eq!(check(&append, "pw"), Ok(true));
        let greedy = excel(&[("algorithmName", "SHA-512".into()), ("hashValue", "AA==".into()), ("saltValue", "AA==".into()), ("spinCount", "4000000000".into())]);
        assert!(check(&greedy, "pw").is_err());
        let odd = excel(&[("algorithmName", "MD4".into()), ("hashValue", "AA==".into()), ("saltValue", "AA==".into()), ("spinCount", "1".into())]);
        assert!(check(&odd, "pw").is_err());
        // ODF: SHA-256 of the UTF-8 password.
        let mut ods = SheetProtection::new(ProtectionFormat::Ods);
        assert!(ods.keep("table:protection-key", &crate::crypt::b64e(&Sha256::digest(b"odf"))));
        assert!(ods.keep("table:protection-key-digest-algorithm", "http://www.w3.org/2000/09/xmldsig#sha256"));
        assert_eq!(check(&ods, "odf"), Ok(true));
        assert_eq!(check(&ods, "ODF"), Ok(false));
    }
}
