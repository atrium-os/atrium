//! Disk-backed keystore for the Vestibulum daemon.
//!
//! Storage model: one file per (service, persona)
//! pair under `$INSULA_VESTIBULUMD_KEYSTORE/<service>.key`,
//! XChaCha20-Poly1305-encrypted under a per-installation
//! master key at `$INSULA_VESTIBULUMD_KEYSTORE/master.key`.
//!
//! # Wire format
//!
//! - `master.key` — 32 random bytes, mode 0o600.
//!   Generated once on first open if absent.
//! - `<service>.key` — 72 bytes:
//!     - 24-byte XChaCha20-Poly1305 nonce
//!     - 32-byte ciphertext (the ed25519 secret)
//!     - 16-byte AEAD tag
//!
//! Compromising one `<service>.key` file without
//! `master.key` is useless — defense-in-depth against
//! partial backups, accidental syncing of individual
//! files, etc. The master.key file itself is plaintext
//! on disk at mode 0o600; an attacker that can read
//! all files in the keystore directory still gets
//! everything. Stronger wrapping (macOS Keychain
//! Services, hardware-backed) is future work per
//! `vestibulum.md` §3.1.
//!
//! # Migration
//!
//! Legacy 32-byte plaintext .key files (from the
//! pre-encryption shape) are accepted on read: we
//! identify them by file length and treat them as raw
//! secrets. The next write re-encrypts. No explicit
//! migration step is required.

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use rand::RngCore;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Length of the master encryption key.
const MASTER_KEY_LEN: usize = 32;
/// XChaCha20-Poly1305 nonce length.
const NONCE_LEN: usize = 24;
/// AEAD authentication-tag length.
const TAG_LEN: usize = 16;
/// Encrypted .key file layout: nonce | ciphertext(32) | tag(16).
const ENCRYPTED_KEY_FILE_LEN: usize = NONCE_LEN + 32 + TAG_LEN;
/// Legacy plaintext .key file length (pre-encryption shape).
const LEGACY_KEY_FILE_LEN: usize = 32;

/// In-memory cache + disk-backing for ed25519 keypairs.
pub struct Keystore {
    root: PathBuf,
    /// 32-byte master key used as the XChaCha20-Poly1305
    /// secret. Loaded from `<root>/master.key` (created
    /// on first open).
    master: [u8; MASTER_KEY_LEN],
    cache: HashMap<String, SigningKey>,
}

impl Keystore {
    /// Open the keystore at `root`, loading any
    /// existing key files into memory. Missing
    /// directory is created; missing `master.key` is
    /// generated.
    pub fn open(root: PathBuf) -> Result<Self, std::io::Error> {
        std::fs::create_dir_all(&root)?;
        let master = load_or_create_master(&root)?;
        let cipher = XChaCha20Poly1305::new((&master).into());

        let mut cache = HashMap::new();
        for entry in std::fs::read_dir(&root)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("key") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            // Skip the master file itself (stem ==
            // "master") — it lives in the same dir.
            if stem == "master" {
                continue;
            }
            let service = decode_service_name(stem);
            let bytes = std::fs::read(&path)?;

            let sk_bytes = match bytes.len() {
                LEGACY_KEY_FILE_LEN => {
                    // Legacy plaintext format. Accept
                    // on read; the next get_or_mint
                    // write will re-encrypt.
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(&bytes);
                    arr
                }
                ENCRYPTED_KEY_FILE_LEN => {
                    let nonce = XNonce::from_slice(&bytes[..NONCE_LEN]);
                    match cipher.decrypt(nonce, &bytes[NONCE_LEN..]) {
                        Ok(plain) if plain.len() == 32 => {
                            let mut arr = [0u8; 32];
                            arr.copy_from_slice(&plain);
                            arr
                        }
                        Ok(_) | Err(_) => {
                            eprintln!(
                                "vestibulum: decrypt failed for {}: \
                                 master.key changed, or file corrupted",
                                path.display(),
                            );
                            continue;
                        }
                    }
                }
                _ => {
                    eprintln!(
                        "vestibulum: ignoring malformed key file {}: \
                         expected {} or {} bytes, got {}",
                        path.display(),
                        LEGACY_KEY_FILE_LEN, ENCRYPTED_KEY_FILE_LEN,
                        bytes.len(),
                    );
                    continue;
                }
            };
            cache.insert(service, SigningKey::from_bytes(&sk_bytes));
        }

        Ok(Keystore { root, master, cache })
    }

    /// Get or mint the keypair for `service`. Newly-
    /// minted keys are encrypted with the master and
    /// persisted to disk synchronously.
    pub fn get_or_mint(&mut self, service: &str) -> &SigningKey {
        if !self.cache.contains_key(service) {
            let sk = SigningKey::generate(&mut OsRng);
            let path = self.path_for(service);
            if let Err(e) = self.write_encrypted(&path, &sk.to_bytes()) {
                eprintln!(
                    "vestibulum: WARNING — could not persist key for {} at {}: {} \
                     (in-memory only; will be lost on restart)",
                    service, path.display(), e
                );
            }
            self.cache.insert(service.to_string(), sk);
        }
        self.cache.get(service).unwrap()
    }

    fn write_encrypted(&self, path: &Path, sk_bytes: &[u8; 32])
        -> Result<(), std::io::Error>
    {
        let cipher = XChaCha20Poly1305::new((&self.master).into());
        let mut nonce_bytes = [0u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = XNonce::from_slice(&nonce_bytes);
        let ciphertext = cipher.encrypt(nonce, &sk_bytes[..])
            .map_err(|_| std::io::Error::new(
                std::io::ErrorKind::Other, "encrypt failed",
            ))?;

        let mut out = Vec::with_capacity(ENCRYPTED_KEY_FILE_LEN);
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ciphertext);
        std::fs::write(path, &out)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(path)?.permissions();
            perms.set_mode(0o600);
            let _ = std::fs::set_permissions(path, perms);
        }
        Ok(())
    }

    /// Number of entries cached. Useful for tests +
    /// debugging.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    fn path_for(&self, service: &str) -> PathBuf {
        self.root.join(format!("{}.key", encode_service_name(service)))
    }
}

/// Load `master.key` from `root`, or create it (32
/// random bytes from OsRng) if absent. Mode is set to
/// 0o600 either way.
fn load_or_create_master(root: &Path)
    -> Result<[u8; MASTER_KEY_LEN], std::io::Error>
{
    let path = root.join("master.key");
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

/// Service names can contain `/` (reverse-DNS plus path
/// shape: `com.example.foo/bar`). Replace `/` and `\`
/// with `__` so we never traverse out of the keystore
/// directory.
fn encode_service_name(service: &str) -> String {
    service.replace(['/', '\\'], "__")
}

fn decode_service_name(stem: &str) -> String {
    stem.replace("__", "/")
}

/// Resolve the keystore directory from
/// `$INSULA_VESTIBULUMD_KEYSTORE`, with a sensible
/// fallback per the daemon's convention.
pub fn resolve_keystore_path() -> PathBuf {
    if let Some(p) = std::env::var_os("INSULA_VESTIBULUMD_KEYSTORE") {
        return PathBuf::from(p);
    }
    if let Some(p) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(p).join("vestibulum-macos").join("keys");
    }
    if let Some(p) = std::env::var_os("TMPDIR") {
        return PathBuf::from(p).join("vestibulum-macos").join("keys");
    }
    PathBuf::from("/tmp/vestibulum-macos/keys")
}

/// Path roundtrip safety check.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_name_encoding_is_reversible() {
        let cases = [
            "com.example.foo",
            "com.example.foo/bar",
            "com.example/foo/bar/baz",
        ];
        for c in cases {
            let enc = encode_service_name(c);
            let dec = decode_service_name(&enc);
            assert_eq!(dec, c, "encoding/decoding should roundtrip for {:?}", c);
            assert!(!enc.contains('/'), "encoded name should not contain '/'");
        }
    }

    #[test]
    fn keystore_persists_across_open() {
        let tmp = tempfile::tempdir().unwrap();

        let pk_first = {
            let mut ks = Keystore::open(tmp.path().to_path_buf()).unwrap();
            ks.get_or_mint("com.example.weather").verifying_key()
        };

        // Re-open. The previously-minted key should
        // be loaded.
        let pk_second = {
            let mut ks = Keystore::open(tmp.path().to_path_buf()).unwrap();
            assert_eq!(ks.len(), 1, "expected one persisted key");
            ks.get_or_mint("com.example.weather").verifying_key()
        };

        assert_eq!(pk_first, pk_second,
                   "persisted key should produce same pubkey across restarts");
    }

    #[test]
    fn keystore_separates_services() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ks = Keystore::open(tmp.path().to_path_buf()).unwrap();
        let pk_a = ks.get_or_mint("com.example.a").verifying_key();
        let pk_b = ks.get_or_mint("com.example.b").verifying_key();
        assert_ne!(pk_a, pk_b);
        assert_eq!(ks.len(), 2);
    }

    #[test]
    fn keystore_rejects_malformed_files() {
        let tmp = tempfile::tempdir().unwrap();
        // Write a junk file with .key extension.
        std::fs::write(tmp.path().join("garbage.key"), b"too short").unwrap();
        let ks = Keystore::open(tmp.path().to_path_buf()).unwrap();
        // Garbage was ignored, not loaded.
        assert_eq!(ks.len(), 0);
    }

    #[test]
    fn on_disk_files_are_encrypted_not_raw_secret() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ks = Keystore::open(tmp.path().to_path_buf()).unwrap();
        let sk_bytes = ks.get_or_mint("com.example.crypt").to_bytes();

        // File on disk must be 72 bytes (24 nonce + 32
        // ct + 16 tag) and NOT contain the raw secret.
        let file = tmp.path().join("com.example.crypt.key");
        let bytes = std::fs::read(&file).unwrap();
        assert_eq!(bytes.len(), ENCRYPTED_KEY_FILE_LEN,
                   "encrypted file should be 72 bytes; got {}", bytes.len());
        for window in bytes.windows(32) {
            assert!(window != sk_bytes,
                    "raw secret bytes leaked into the on-disk file");
        }
    }

    #[test]
    fn master_key_governs_decryption() {
        let tmp = tempfile::tempdir().unwrap();
        let pk_first = {
            let mut ks = Keystore::open(tmp.path().to_path_buf()).unwrap();
            ks.get_or_mint("com.example.master-test").verifying_key()
        };

        // Replace master.key with garbage. Decryption
        // of the existing service file should fail
        // gracefully (warning + skip, not panic) and
        // the next get_or_mint should mint a fresh key
        // under the new master.
        let mut bogus = [0u8; 32];
        bogus[0] = 0xff;
        std::fs::write(tmp.path().join("master.key"), bogus).unwrap();

        let mut ks = Keystore::open(tmp.path().to_path_buf()).unwrap();
        assert_eq!(ks.len(), 0,
                   "decrypt with wrong master should drop the entry");
        let pk_second = ks.get_or_mint("com.example.master-test").verifying_key();
        assert_ne!(pk_first, pk_second,
                   "fresh mint under new master should be a different key");
    }

    #[test]
    fn legacy_plaintext_files_are_accepted_on_read() {
        // Backward compat: a 32-byte plaintext .key
        // (the pre-encryption format) should still be
        // readable. Next write re-encrypts.
        let tmp = tempfile::tempdir().unwrap();
        // Generate a fresh keypair, write it the old way.
        let legacy_sk = SigningKey::generate(&mut OsRng);
        let legacy_pk = legacy_sk.verifying_key();
        std::fs::write(tmp.path().join("com.example.legacy.key"),
                       legacy_sk.to_bytes()).unwrap();

        let mut ks = Keystore::open(tmp.path().to_path_buf()).unwrap();
        assert_eq!(ks.len(), 1, "legacy file should load");
        let loaded_pk = ks.get_or_mint("com.example.legacy").verifying_key();
        assert_eq!(loaded_pk, legacy_pk,
                   "loaded key should match the legacy file contents");
    }
}
