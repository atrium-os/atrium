//! The security keys each user enrolled for local verification (sigillum.md
//! §12): credential id, P-256 public key and the last sign count, kept by
//! sigillumd in its own state directory, one file per user. Enrolling needs
//! the user's passcode (the broker asks it first); verifying checks the
//! key's signature against what is stored here.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sigillum_proto::client::MAX_LOGIN_KEYS;
use sigillum_proto::driver::cap;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginKey {
    pub cred_id: Vec<u8>,
    /// SEC1 uncompressed P-256 point (65 bytes).
    pub public_key: Vec<u8>,
    pub sign_count: u32,
    pub name: String,
}

impl LoginKey {
    fn valid(&self) -> bool {
        !self.cred_id.is_empty()
            && self.cred_id.len() <= cap::CRED_ID
            && self.public_key.len() == 65
            && self.public_key[0] == 4
            && self.name.len() <= 64
    }
}

pub trait KeyStore: Send {
    /// The user's keys; empty when none or unreadable.
    fn list(&self, user: &str) -> Vec<LoginKey>;
    /// Add one; false if it can't be stored (or the user has the most).
    fn add(&mut self, user: &str, key: LoginKey) -> bool;
    /// Record a key's new sign count after a verification.
    fn set_count(&mut self, user: &str, cred_id: &[u8], count: u32) -> bool;
}

/// No store configured: no user has a key, and none can be enrolled.
pub struct NoKeys;

impl KeyStore for NoKeys {
    fn list(&self, _: &str) -> Vec<LoginKey> {
        Vec::new()
    }
    fn add(&mut self, _: &str, _: LoginKey) -> bool {
        false
    }
    fn set_count(&mut self, _: &str, _: &[u8], _: u32) -> bool {
        false
    }
}

/// `<dir>/<user>.json`, mode 0600, replaced whole by rename.
pub struct FileKeys(pub PathBuf);

/// A login name as a file name: nothing that can leave the directory.
pub fn valid_user(u: &str) -> bool {
    !u.is_empty() && u.len() <= 32 && !u.starts_with('.') && u.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

impl FileKeys {
    fn path(&self, user: &str) -> Option<PathBuf> {
        valid_user(user).then(|| self.0.join(format!("{user}.json")))
    }

    fn write(&self, user: &str, keys: &[LoginKey]) -> bool {
        let Some(p) = self.path(user) else { return false };
        let tmp = p.with_extension("json.new");
        let body = match serde_json::to_vec(keys) {
            Ok(b) => b,
            Err(_) => return false,
        };
        let ok = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .and_then(|mut f| f.write_all(&body).and_then(|_| f.sync_all()))
            .is_ok();
        ok && std::fs::rename(&tmp, &p).is_ok()
    }
}

impl KeyStore for FileKeys {
    fn list(&self, user: &str) -> Vec<LoginKey> {
        let Some(p) = self.path(user) else { return Vec::new() };
        let Ok(b) = std::fs::read(p) else { return Vec::new() };
        if b.len() > 64 * 1024 {
            return Vec::new();
        }
        let keys: Vec<LoginKey> = serde_json::from_slice(&b).unwrap_or_default();
        keys.into_iter().filter(LoginKey::valid).take(MAX_LOGIN_KEYS).collect()
    }

    fn add(&mut self, user: &str, key: LoginKey) -> bool {
        let mut keys = self.list(user);
        if !key.valid() || keys.len() >= MAX_LOGIN_KEYS || keys.iter().any(|k| k.cred_id == key.cred_id) {
            return false;
        }
        keys.push(key);
        self.write(user, &keys)
    }

    fn set_count(&mut self, user: &str, cred_id: &[u8], count: u32) -> bool {
        let mut keys = self.list(user);
        let Some(k) = keys.iter_mut().find(|k| k.cred_id == cred_id) else { return false };
        k.sign_count = count;
        self.write(user, &keys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(id: u8) -> LoginKey {
        let mut pk = vec![4u8];
        pk.extend([id; 64]);
        LoginKey { cred_id: vec![id; 16], public_key: pk, sign_count: 0, name: format!("key {id}") }
    }

    #[test]
    fn file_store_round_trips_and_refuses_bad_names() {
        let d = std::env::temp_dir().join(format!("sigillum-keys-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let mut s = FileKeys(d.clone());
        assert!(s.list("alice").is_empty());
        assert!(s.add("alice", key(1)));
        assert!(!s.add("alice", key(1)), "no duplicate");
        assert!(s.add("alice", key(2)));
        assert!(s.set_count("alice", &[1; 16], 7));
        let l = s.list("alice");
        assert_eq!((l.len(), l[0].sign_count), (2, 7));
        for bad in ["../etc", "", ".hidden", "a/b", "x".repeat(33).as_str()] {
            assert!(!s.add(bad, key(3)), "{bad}");
        }
        for i in 3..=MAX_LOGIN_KEYS as u8 {
            assert!(s.add("alice", key(i)));
        }
        assert!(!s.add("alice", key(99)), "at most MAX_LOGIN_KEYS");
        std::fs::remove_dir_all(d).unwrap();
    }
}
