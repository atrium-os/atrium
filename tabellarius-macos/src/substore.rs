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

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use rand::RngCore;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Length of the master encryption key (same shape as
/// the vestibulum keystore — see that crate for the
/// threat model).
const MASTER_KEY_LEN: usize = 32;
/// XChaCha20-Poly1305 nonce length.
const NONCE_LEN: usize = 24;
/// AEAD authentication-tag length.
const TAG_LEN: usize = 16;

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
    /// The Insula app that owns this subscription, when
    /// known. Set from the app's `$ATRIUM_CONTAINER_DIR`
    /// at subscribe time (see libatrium); `None` for
    /// subscriptions minted by the `insula` CLI or any
    /// other non-sandboxed caller. Used by wake-on-push
    /// to decide whose triggered-background entry to
    /// spawn.
    pub app_id: Option<String>,
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
    master: [u8; MASTER_KEY_LEN],
    by_id: BTreeMap<String, Subscription>,
}

impl SubStore {
    /// Open (or create) a subscription store rooted at
    /// `dir`. Loads existing `.sub` files (encrypted or
    /// legacy plaintext) into memory. Master key is
    /// created on first open if missing.
    pub fn open(dir: PathBuf) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&dir)?.permissions();
            perms.set_mode(0o700);
            let _ = std::fs::set_permissions(&dir, perms);
        }
        let master = load_or_create_master(&dir)?;
        let cipher = XChaCha20Poly1305::new((&master).into());

        let mut by_id = BTreeMap::new();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("sub") {
                continue;
            }
            let bytes = std::fs::read(&path)?;
            if bytes.is_empty() {
                continue;
            }

            // Try encrypted format first (starts with a
            // 24-byte nonce; minimum total is nonce +
            // tag + 1B payload = 41 bytes). If decrypt
            // fails AND the file matches the legacy
            // shape, accept as plaintext and re-encrypt
            // on next write.
            let payload: Vec<u8> = if bytes.len() > NONCE_LEN + TAG_LEN {
                let nonce = XNonce::from_slice(&bytes[..NONCE_LEN]);
                match cipher.decrypt(nonce, &bytes[NONCE_LEN..]) {
                    Ok(plain) => plain,
                    Err(_) => {
                        // Maybe legacy plaintext.
                        if looks_like_legacy_sub(&bytes) {
                            bytes.clone()
                        } else {
                            eprintln!(
                                "tabellarius: decrypt failed for {}: \
                                 master.key changed, or file corrupted",
                                path.display(),
                            );
                            continue;
                        }
                    }
                }
            } else if looks_like_legacy_sub(&bytes) {
                bytes.clone()
            } else {
                continue;
            };

            // Inner payload formats:
            //   v1 (legacy): [1B plen | purpose | 32B sk]
            //   v2:          [1B plen | purpose |
            //                 1B alen | app_id | 32B sk]
            // After `purpose`, v1 has exactly 32 bytes
            // left; v2 has >= 33 (the alen byte + sk).
            let purpose_len = payload[0] as usize;
            if payload.len() < 1 + purpose_len + 32 {
                continue;
            }
            let purpose = match std::str::from_utf8(&payload[1..1 + purpose_len]) {
                Ok(s) => s.to_string(),
                Err(_) => continue,
            };
            let after_purpose = &payload[1 + purpose_len..];
            let (app_id, sk_slice) = if after_purpose.len() == 32 {
                // v1 legacy — no app_id.
                (None, after_purpose)
            } else {
                // v2 — [1B alen | app_id | 32B sk].
                let alen = after_purpose[0] as usize;
                if after_purpose.len() != 1 + alen + 32 {
                    continue;
                }
                let app_id = std::str::from_utf8(&after_purpose[1..1 + alen])
                    .ok()
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string());
                (app_id, &after_purpose[1 + alen..])
            };
            let mut sk_bytes = [0u8; 32];
            sk_bytes.copy_from_slice(sk_slice);
            let signing_key = SigningKey::from_bytes(&sk_bytes);
            let key_id = key_id_for(&signing_key.verifying_key().to_bytes());

            // File-name sanity check.
            let expected = format!("{}.sub", key_id);
            if path.file_name().and_then(|s| s.to_str()) != Some(expected.as_str()) {
                continue;
            }

            by_id.insert(
                key_id.clone(),
                Subscription { key_id, purpose, app_id, signing_key },
            );
        }

        Ok(SubStore { dir, master, by_id })
    }

    /// Mint a new subscription with the given purpose
    /// and (optionally) the owning app's id. Encrypted
    /// before persistence — the disk file is
    /// `[24B nonce | ciphertext | 16B tag]`.
    pub fn mint(&mut self, purpose: &str, app_id: Option<&str>)
        -> std::io::Result<&Subscription>
    {
        let sk = SigningKey::generate(&mut OsRng);
        let pk_bytes = sk.verifying_key().to_bytes();
        let key_id = key_id_for(&pk_bytes);

        if self.by_id.contains_key(&key_id) {
            return Ok(self.by_id.get(&key_id).unwrap());
        }

        // Inner payload (v2):
        //   [1B plen | purpose | 1B alen | app_id | 32B sk]
        let app_id_str = app_id.unwrap_or("");
        let mut inner = Vec::with_capacity(
            1 + purpose.len() + 1 + app_id_str.len() + 32,
        );
        let plen: u8 = purpose.len().try_into().unwrap_or(255);
        inner.push(plen);
        inner.extend_from_slice(&purpose.as_bytes()[..plen as usize]);
        let alen: u8 = app_id_str.len().try_into().unwrap_or(255);
        inner.push(alen);
        inner.extend_from_slice(&app_id_str.as_bytes()[..alen as usize]);
        inner.extend_from_slice(sk.as_bytes());

        let cipher = XChaCha20Poly1305::new((&self.master).into());
        let mut nonce_bytes = [0u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = XNonce::from_slice(&nonce_bytes);
        let ciphertext = cipher.encrypt(nonce, inner.as_slice())
            .map_err(|_| std::io::Error::new(
                std::io::ErrorKind::Other, "encrypt failed",
            ))?;
        let mut bytes = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        bytes.extend_from_slice(&nonce_bytes);
        bytes.extend_from_slice(&ciphertext);

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
                app_id: app_id.map(|s| s.to_string()),
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

/// Read `master.key` from `dir` or create it (32 random
/// bytes, mode 0o600) on first call.
fn load_or_create_master(dir: &Path)
    -> std::io::Result<[u8; MASTER_KEY_LEN]>
{
    let path = dir.join("master.key");
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut new_master = [0u8; MASTER_KEY_LEN];
            OsRng.fill_bytes(&mut new_master);
            std::fs::write(&path, new_master)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = std::fs::metadata(&path)?.permissions();
                perms.set_mode(0o600);
                let _ = std::fs::set_permissions(&path, perms);
            }
            return Ok(new_master);
        }
        Err(e) => return Err(e),
    };
    if bytes.len() != MASTER_KEY_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "master.key has wrong length: expected {} bytes, got {}",
                MASTER_KEY_LEN, bytes.len(),
            ),
        ));
    }
    let mut arr = [0u8; MASTER_KEY_LEN];
    arr.copy_from_slice(&bytes);
    Ok(arr)
}

/// Heuristic: does `bytes` look like the legacy
/// pre-encryption format `[1B plen | purpose | 32B sk]`?
/// The trailing 32 bytes have no fixed signature, so we
/// can only check structural constraints (length +
/// purpose UTF-8 validity).
fn looks_like_legacy_sub(bytes: &[u8]) -> bool {
    if bytes.is_empty() { return false; }
    let plen = bytes[0] as usize;
    if bytes.len() != 1 + plen + 32 { return false; }
    std::str::from_utf8(&bytes[1..1 + plen]).is_ok()
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
        let sub = s1.mint("primary", None).unwrap();
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
        let key_id = s.mint("p", None).unwrap().key_id.clone();

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
        let sub = s.mint("x", None).unwrap();
        let pk = sub.pubkey_bytes();
        let expected: String = pk[..KEY_ID_HEX_LEN / 2]
            .iter().map(|b| format!("{:02x}", b)).collect();
        assert_eq!(sub.key_id, expected);
    }

    #[test]
    fn on_disk_files_are_encrypted_not_raw_payload() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = SubStore::open(dir.path().to_path_buf()).unwrap();
        let sub = s.mint("primary", None).unwrap();
        let sk_bytes = sub.signing_key.to_bytes();
        let purpose = sub.purpose.clone();
        let key_id = sub.key_id.clone();

        let path = dir.path().join(format!("{}.sub", key_id));
        let bytes = std::fs::read(&path).unwrap();

        // Expect: nonce (24) + ciphertext (1 + len(purpose) + 32) + tag (16).
        // Inner v2 payload: 1B plen | purpose | 1B alen
        // (=0, no app_id) | 32B sk. Encrypted = nonce +
        // inner + tag.
        let expected_len =
            NONCE_LEN + (1 + purpose.len() + 1 + 0 + 32) + TAG_LEN;
        assert_eq!(bytes.len(), expected_len,
                   "encrypted sub file should be {} bytes; got {}",
                   expected_len, bytes.len());

        // The raw secret must not appear in the file.
        for window in bytes.windows(32) {
            assert!(window != sk_bytes,
                    "raw secret bytes leaked into the on-disk file");
        }
        // The purpose string must not appear in cleartext.
        assert!(!bytes.windows(purpose.len())
                    .any(|w| w == purpose.as_bytes()),
                "purpose string leaked into the on-disk file");
    }

    #[test]
    fn master_key_governs_decryption() {
        let dir = tempfile::tempdir().unwrap();
        let pk_first = {
            let mut s = SubStore::open(dir.path().to_path_buf()).unwrap();
            s.mint("master-test", None).unwrap().pubkey_bytes()
        };

        // Tamper with master.key.
        let mut bogus = [0u8; 32];
        bogus[0] = 0xff;
        std::fs::write(dir.path().join("master.key"), bogus).unwrap();

        let s = SubStore::open(dir.path().to_path_buf()).unwrap();
        assert_eq!(s.iter().count(), 0,
                   "wrong master should drop the entry, not panic");
        // pk_first is unrecoverable now, which is the
        // whole point.
        let _ = pk_first;
    }

    #[test]
    fn legacy_plaintext_files_are_accepted_on_read() {
        // Backward compat: a pre-encryption .sub file
        // [1B plen | purpose | 32B sk] should still load.
        let dir = tempfile::tempdir().unwrap();
        let legacy_sk = SigningKey::generate(&mut OsRng);
        let legacy_pk = legacy_sk.verifying_key().to_bytes();
        let key_id = key_id_for(&legacy_pk);

        let purpose = "legacy-purpose";
        let mut bytes = Vec::new();
        bytes.push(purpose.len() as u8);
        bytes.extend_from_slice(purpose.as_bytes());
        bytes.extend_from_slice(&legacy_sk.to_bytes());

        std::fs::write(dir.path().join(format!("{}.sub", key_id)), &bytes).unwrap();

        let s = SubStore::open(dir.path().to_path_buf()).unwrap();
        assert_eq!(s.iter().count(), 1, "legacy file should load");
        let loaded = s.iter().next().unwrap();
        assert_eq!(loaded.key_id, key_id);
        assert_eq!(loaded.purpose, purpose);
        assert_eq!(loaded.signing_key.to_bytes(), legacy_sk.to_bytes());
    }
}
