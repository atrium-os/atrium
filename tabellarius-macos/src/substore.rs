//! Disk-backed subscription store.
//!
//! One file per subscription at `<store_dir>/<key_id>.sub`
//! containing the raw 32-byte ed25519 secret. The key_id
//! is the SHA-256-prefix derived from the pubkey, which
//! makes it both stable and self-describing — a caller
//! can verify the key_id matches a pubkey without going
//! through us.
//!
//! v0 is plaintext on disk just like vestibulum's
//! keystore; integration with macOS Keychain Services is
//! future polish, shared with the vestibulum work.

use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Length of a subscription key_id in hex characters.
/// 16 hex chars = 8 bytes of pubkey-prefix, enough to be
/// human-skimmable in the `insula` CLI / logs without
/// being a security-relevant identifier in its own
/// right (collisions are not a threat model here — the
/// pubkey is the ground truth).
pub const KEY_ID_HEX_LEN: usize = 16;

/// One subscription: key_id + the keypair it identifies.
pub struct Subscription {
    pub key_id: String,
    pub purpose: String,
    pub signing_key: SigningKey,
}

impl Subscription {
    pub fn pubkey_bytes(&self) -> [u8; 32] {
        self.signing_key.verifying_key().to_bytes()
    }
}

/// In-memory mirror of the on-disk subscription store.
pub struct SubStore {
    dir: PathBuf,
    by_id: BTreeMap<String, Subscription>,
}

impl SubStore {
    /// Open (or create) a subscription store rooted at
    /// `dir`. Loads any existing `.sub` files into
    /// memory.
    pub fn open(dir: PathBuf) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&dir)?.permissions();
            perms.set_mode(0o700);
            let _ = std::fs::set_permissions(&dir, perms);
        }

        let mut by_id = BTreeMap::new();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("sub") {
                continue;
            }
            let bytes = std::fs::read(&path)?;
            // Format: [1B purpose_len | purpose | 32B sk]
            if bytes.is_empty() {
                continue;
            }
            let purpose_len = bytes[0] as usize;
            if bytes.len() < 1 + purpose_len + 32 {
                continue;
            }
            let purpose = match std::str::from_utf8(&bytes[1..1 + purpose_len]) {
                Ok(s) => s.to_string(),
                Err(_) => continue,
            };
            let mut sk_bytes = [0u8; 32];
            sk_bytes.copy_from_slice(&bytes[1 + purpose_len..1 + purpose_len + 32]);
            let signing_key = SigningKey::from_bytes(&sk_bytes);
            let key_id = key_id_for(&signing_key.verifying_key().to_bytes());

            // Skip files whose name doesn't match the key_id
            // (corrupted / hand-edited). Don't crash.
            let expected = format!("{}.sub", key_id);
            if path.file_name().and_then(|s| s.to_str()) != Some(expected.as_str()) {
                continue;
            }

            by_id.insert(
                key_id.clone(),
                Subscription { key_id, purpose, signing_key },
            );
        }

        Ok(SubStore { dir, by_id })
    }

    /// Mint a new subscription with the given purpose.
    /// Persists to disk before returning, so the caller
    /// can publish the returned pubkey knowing the
    /// keypair survives daemon restart.
    pub fn mint(&mut self, purpose: &str) -> std::io::Result<&Subscription> {
        let sk = SigningKey::generate(&mut OsRng);
        let pk_bytes = sk.verifying_key().to_bytes();
        let key_id = key_id_for(&pk_bytes);

        // On collision (astronomically unlikely), just
        // return the existing entry — they'd be the same
        // pubkey so the caller is none the wiser.
        if self.by_id.contains_key(&key_id) {
            return Ok(self.by_id.get(&key_id).unwrap());
        }

        let mut bytes = Vec::with_capacity(1 + purpose.len() + 32);
        let plen: u8 = purpose.len().try_into().unwrap_or(255);
        bytes.push(plen);
        bytes.extend_from_slice(&purpose.as_bytes()[..plen as usize]);
        bytes.extend_from_slice(sk.as_bytes());

        let path = self.dir.join(format!("{}.sub", key_id));
        std::fs::write(&path, &bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path)?.permissions();
            perms.set_mode(0o600);
            let _ = std::fs::set_permissions(&path, perms);
        }

        self.by_id.insert(
            key_id.clone(),
            Subscription {
                key_id: key_id.clone(),
                purpose: purpose.to_string(),
                signing_key: sk,
            },
        );
        Ok(self.by_id.get(&key_id).unwrap())
    }

    /// Remove a subscription. Returns `true` if it was
    /// present, `false` if the key_id was unknown.
    pub fn remove(&mut self, key_id: &str) -> bool {
        let path = self.dir.join(format!("{}.sub", key_id));
        let on_disk = path.exists();
        let _ = std::fs::remove_file(&path);
        self.by_id.remove(key_id).is_some() || on_disk
    }

    pub fn iter(&self) -> impl Iterator<Item = &Subscription> {
        self.by_id.values()
    }
}

fn key_id_for(pubkey: &[u8; 32]) -> String {
    let mut out = String::with_capacity(KEY_ID_HEX_LEN);
    for b in &pubkey[..KEY_ID_HEX_LEN / 2] {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

pub fn resolve_substore_path() -> PathBuf {
    if let Ok(p) = std::env::var("INSULA_TABELLARIUSD_STORE") {
        return PathBuf::from(p);
    }
    let base = std::env::var("XDG_RUNTIME_DIR")
        .or_else(|_| std::env::var("TMPDIR"))
        .unwrap_or_else(|_| "/tmp".to_string());
    Path::new(&base).join("tabellarius-macos").join("subs")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_persists_and_reload_finds() {
        let dir = tempfile::tempdir().unwrap();
        let mut s1 = SubStore::open(dir.path().to_path_buf()).unwrap();
        let sub = s1.mint("primary").unwrap();
        let key_id = sub.key_id.clone();
        let pk_before = sub.pubkey_bytes();

        // Reopen.
        let s2 = SubStore::open(dir.path().to_path_buf()).unwrap();
        let reloaded = s2.iter().find(|s| s.key_id == key_id).unwrap();
        assert_eq!(reloaded.pubkey_bytes(), pk_before,
                   "pubkey must survive store reopen");
        assert_eq!(reloaded.purpose, "primary");
    }

    #[test]
    fn remove_drops_from_disk_and_memory() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = SubStore::open(dir.path().to_path_buf()).unwrap();
        let key_id = s.mint("p").unwrap().key_id.clone();

        assert!(s.remove(&key_id));
        assert!(s.iter().next().is_none());

        let s2 = SubStore::open(dir.path().to_path_buf()).unwrap();
        assert!(s2.iter().next().is_none(),
                "removed sub must not reappear on reopen");
    }

    #[test]
    fn remove_unknown_key_id_returns_false() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = SubStore::open(dir.path().to_path_buf()).unwrap();
        assert!(!s.remove("0123456789abcdef"));
    }

    #[test]
    fn key_id_matches_pubkey_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = SubStore::open(dir.path().to_path_buf()).unwrap();
        let sub = s.mint("x").unwrap();
        let pk = sub.pubkey_bytes();
        let expected: String = pk[..KEY_ID_HEX_LEN / 2]
            .iter().map(|b| format!("{:02x}", b)).collect();
        assert_eq!(sub.key_id, expected);
    }
}
