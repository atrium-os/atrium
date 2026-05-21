//! Disk-backed keystore for the Vestibulum daemon.
//!
//! v0 storage model: one file per (service, persona)
//! pair, raw 32-byte ed25519 secret key. Files live
//! under `$INSULA_VESTIBULUMD_KEYSTORE/<service>.key`.
//!
//! # Security caveat
//!
//! v0 writes **plaintext** secret keys. That's
//! demonstrably insecure outside a developer-loopback
//! scenario; a real deployment must wrap the keys via
//! one of:
//!   - macOS Keychain Services (the production path
//!     per `vestibulum.md` §3.1 — wraps under the
//!     user's login credential, hardware-backed where
//!     available).
//!   - File-level encryption with a key derived from
//!     a passphrase / biometric prompt at daemon start.
//!
//! v0's loose-files-of-secrets approach validates the
//! lifecycle (mint -> persist -> restart -> reload)
//! without buying into a specific wrapping strategy.
//! Marked with a TODO above each unsafe-ish access.

use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use std::collections::HashMap;
use std::path::PathBuf;

/// In-memory cache + disk-backing for ed25519 keypairs.
pub struct Keystore {
    root: PathBuf,
    cache: HashMap<String, SigningKey>,
}

impl Keystore {
    /// Open the keystore at `root`, loading any
    /// existing key files into memory. Missing
    /// directory is created.
    pub fn open(root: PathBuf) -> Result<Self, std::io::Error> {
        std::fs::create_dir_all(&root)?;

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
            let service = decode_service_name(stem);
            let bytes = std::fs::read(&path)?;
            if bytes.len() != 32 {
                eprintln!(
                    "vestibulum: ignoring malformed key file {}: \
                     expected 32 bytes, got {}",
                    path.display(),
                    bytes.len()
                );
                continue;
            }
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&bytes);
            cache.insert(service, SigningKey::from_bytes(&arr));
        }

        Ok(Keystore { root, cache })
    }

    /// Get or mint the keypair for `service`. Newly-
    /// minted keys are persisted to disk synchronously
    /// (the next process restart sees them).
    pub fn get_or_mint(&mut self, service: &str) -> &SigningKey {
        if !self.cache.contains_key(service) {
            let sk = SigningKey::generate(&mut OsRng);
            // TODO(vestibulum-secure-storage): wrap or
            // hardware-back this write.
            let path = self.path_for(service);
            if let Err(e) = std::fs::write(&path, sk.to_bytes()) {
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
}
