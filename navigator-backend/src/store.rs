//! Where recordings and saved sessions live: a content-addressed store.
//!
//! ★ Spec §5: the store is Tessera, and Tessera is content-addressed. This is
//! the seam: `DirStore` is the on-disk stand-in (one file per object, named
//! by its BLAKE3 address, the same shape as the converter's
//! `PRERENDER_BLOB_DIR`), and pointing it at a directory on a Tessera volume
//! is the deployment. Nothing above this trait knows which it has.
//!
//! ★★ THE STORE DOES NOT VOUCH FOR WHAT IT RETURNS. A `get` re-hashes the
//! bytes and refuses a mismatch: a file can be replaced under its name, and a
//! name is only a claim about content until it is checked.

use crate::session::digest;
use std::collections::HashMap;
use std::path::PathBuf;

pub trait Store {
    /// Store bytes, returning their address. Idempotent: the same bytes are
    /// the same address and are stored once.
    fn put(&mut self, bytes: &[u8]) -> Result<String, String>;
    /// The bytes at an address, checked against it.
    fn get(&self, address: &str) -> Result<Vec<u8>, String>;
    /// Take an object out (clearing history, navigator-ui.md §5.5). The
    /// store does not know what still names it: the caller does.
    fn remove(&mut self, address: &str) -> Result<(), String>;
    /// Every object's address and size, in no order.
    fn list(&self) -> Result<Vec<(String, u64)>, String>;
}

fn valid(address: &str) -> bool {
    address.len() == 64 && address.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn checked(address: &str, bytes: Vec<u8>) -> Result<Vec<u8>, String> {
    if digest(&bytes) == address { Ok(bytes) } else { Err(format!("object {address} does not hash to its address")) }
}

/// In memory, for tests and for a host with nowhere to write.
#[derive(Default)]
pub struct MemoryStore { objects: HashMap<String, Vec<u8>> }

impl Store for MemoryStore {
    fn put(&mut self, bytes: &[u8]) -> Result<String, String> {
        let a = digest(bytes);
        self.objects.entry(a.clone()).or_insert_with(|| bytes.to_vec());
        Ok(a)
    }
    fn get(&self, address: &str) -> Result<Vec<u8>, String> {
        if !valid(address) { return Err(format!("{address:?} is not an address")) }
        let b = self.objects.get(address).cloned().ok_or_else(|| format!("no object {address}"))?;
        checked(address, b)
    }
    fn remove(&mut self, address: &str) -> Result<(), String> {
        self.objects.remove(address);
        Ok(())
    }
    fn list(&self) -> Result<Vec<(String, u64)>, String> {
        Ok(self.objects.iter().map(|(a, b)| (a.clone(), b.len() as u64)).collect())
    }
}

/// One file per object in a directory, named by address.
pub struct DirStore { dir: PathBuf }

impl DirStore {
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self, String> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        Ok(DirStore { dir })
    }
}

impl Store for DirStore {
    fn put(&mut self, bytes: &[u8]) -> Result<String, String> {
        let a = digest(bytes);
        let path = self.dir.join(&a);
        if path.exists() { return Ok(a) }
        // Written beside and renamed into place, so a crash never leaves a
        // truncated object under a valid name.
        let tmp = self.dir.join(format!(".{a}.tmp"));
        std::fs::write(&tmp, bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(a)
    }
    fn get(&self, address: &str) -> Result<Vec<u8>, String> {
        // ★ Checked BEFORE it becomes a path: an "address" of `../x` must not
        // name a file outside the store.
        if !valid(address) { return Err(format!("{address:?} is not an address")) }
        let b = std::fs::read(self.dir.join(address)).map_err(|e| format!("no object {address}: {e}"))?;
        checked(address, b)
    }
    fn remove(&mut self, address: &str) -> Result<(), String> {
        if !valid(address) { return Err(format!("{address:?} is not an address")) }
        match std::fs::remove_file(self.dir.join(address)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("{address}: {e}")),
        }
    }
    fn list(&self) -> Result<Vec<(String, u64)>, String> {
        let mut v = vec![];
        for e in std::fs::read_dir(&self.dir).map_err(|e| format!("{}: {e}", self.dir.display()))? {
            let e = e.map_err(|e| format!("{}: {e}", self.dir.display()))?;
            let name = e.file_name().to_string_lossy().into_owned();
            // Only objects: a `.x.tmp` mid-write, or anything else, is not one.
            if valid(&name) {
                v.push((name, e.metadata().map(|m| m.len()).unwrap_or(0)));
            }
        }
        Ok(v)
    }
}
