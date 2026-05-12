//! Host-side shader cache — the warm path for `OP_GPU_SHADER_RESOLVE`.
//!
//! Per `docs/spec/aqueduct-gpu.md` §4.1–4.2, shader resolution is a
//! two-phase wire: clients first send a content-hash + backend
//! identity query (`RESOLVE`); the host returns Hit if it already has
//! the bytecode keyed by `(hash, backend, compiler_version)`. On
//! Miss, the client follows up with the actual bytecode via
//! `UPLOAD`. The host validates (see [`shader_validator`]) and
//! caches.
//!
//! ## On-disk layout
//!
//! ```text
//!   cache_dir/
//!     <hash_hex>_<backend_vendor>_<backend_gen>_<compiler_ver>.bin
//! ```
//!
//! - `hash_hex`: 64-char lowercase hex of the 32-byte `bytecode_hash`.
//! - `backend_vendor`, `backend_gen`: lowercase decimal.
//! - `compiler_ver`: caller-supplied tag; currently `0` (bump when
//!   we change SPIR-V → MTLLibrary translation).
//! - `.bin`: post-validation, post-translation bytecode. For tier-1
//!   SW the payload is the original SPIR-V; for tier-3 it'd be
//!   `MTLLibrary` blobs.
//!
//! ## Eventual home: Tessera CAS
//!
//! This module is the **scaffolding** for the actual warm path. On
//! D5+ the storage backend swaps to Tessera CAS (atrium-pkg's
//! shader-precompile hook populates it at install time). The
//! API surface here matches what the Tessera-backed implementation
//! will expose, so the swap is mechanical.
//!
//! ## Threat model
//!
//! - Bytes returned from `lookup` are forwarded to the GPU as-is.
//!   Cache is treated as **trusted** — once a hash:vendor:gen:ver
//!   tuple lands in the cache, the bytes are accepted without
//!   re-validation. Anything writing to the cache (this module,
//!   atrium-pkg) is responsible for validating first.
//! - Cache directory must be on a trusted filesystem; the host
//!   endpoint is privileged (§12.1 of the spec).
//! - Filename construction sanitises hash bytes to hex; no
//!   path-traversal vectors via untrusted hash content.

#![warn(missing_docs)]

use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use aqueduct_gpu::backends::BackendId;
use aqueduct_gpu::payloads::ShaderKind;

/// A shader cache rooted at a single directory. Cheap to construct;
/// holds an in-memory hit/miss LRU on top of the disk store.
pub struct ShaderCache {
    root: PathBuf,
    /// Last-N-lookups in-memory index. Bounded so a misbehaving
    /// client can't OOM the host via cache thrashing.
    in_mem: Mutex<InMemoryIndex>,
}

/// Bounded in-memory hash → bytes cache. Acts as a hot-path
/// shortcut over the disk fetch. Bytes are heap-allocated copies;
/// expect ≤ a few KB per shader for tier-1 / built-in pipelines.
struct InMemoryIndex {
    entries: Vec<(CacheKey, Vec<u8>)>,
    capacity: usize,
}

/// Cache lookup key — the tuple of inputs that uniquely identifies
/// a compiled shader artifact.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CacheKey {
    /// 32-byte content hash of the shader source bytecode.
    pub bytecode_hash: [u8; 32],
    /// Target backend identity (`vendor`, `generation`).
    pub backend: BackendId,
    /// Compiler revision tag. Lockstep with the backend's
    /// SPIR-V → backend-bytecode translator version. Bump when
    /// retranslating an old shader would produce different output.
    pub compiler_version: u32,
    /// Shader source language (currently `SpirV` or `Nir`).
    pub kind: ShaderKind,
}

/// What `lookup` returns.
#[derive(Debug)]
pub enum LookupResult {
    /// Cache hit — bytes ready to hand to the backend.
    Hit(Vec<u8>),
    /// Cache miss — caller should follow up with an UPLOAD.
    Miss,
    /// Disk error (e.g. permission denied). Treat as miss but log.
    Error(io::Error),
}

const DEFAULT_IN_MEM_CAPACITY: usize = 64;

impl ShaderCache {
    /// Open (or create) a cache directory. Creates the directory if
    /// missing; permission errors surface to the caller.
    pub fn open<P: AsRef<Path>>(root: P) -> io::Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            in_mem: Mutex::new(InMemoryIndex {
                entries: Vec::with_capacity(DEFAULT_IN_MEM_CAPACITY),
                capacity: DEFAULT_IN_MEM_CAPACITY,
            }),
        })
    }

    /// Look up a compiled shader. Checks the in-memory index first,
    /// then the disk store. Updates the in-mem index on disk hits.
    pub fn lookup(&self, key: &CacheKey) -> LookupResult {
        // ── In-memory hit ────────────────────────────────────────
        if let Ok(mem) = self.in_mem.lock() {
            if let Some(bytes) = mem.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone()) {
                return LookupResult::Hit(bytes);
            }
        }
        // ── Disk lookup ──────────────────────────────────────────
        let path = self.path_for(key);
        match fs::read(&path) {
            Ok(bytes) => {
                if let Ok(mut mem) = self.in_mem.lock() {
                    mem.insert(key.clone(), bytes.clone());
                }
                LookupResult::Hit(bytes)
            }
            Err(e) if e.kind() == ErrorKind::NotFound => LookupResult::Miss,
            Err(e) => LookupResult::Error(e),
        }
    }

    /// Insert post-validation bytes for `key`. Caller MUST have
    /// validated the bytes (see [`crate::shader_validator`]) before
    /// inserting; the cache trusts the contents.
    ///
    /// Atomic via write-to-temp + rename.
    pub fn insert(&self, key: &CacheKey, bytes: &[u8]) -> io::Result<()> {
        let path = self.path_for(key);
        let tmp = path.with_extension("bin.tmp");
        fs::write(&tmp, bytes)?;
        fs::rename(&tmp, &path)?;
        if let Ok(mut mem) = self.in_mem.lock() {
            mem.insert(key.clone(), bytes.to_vec());
        }
        Ok(())
    }

    /// Drop everything (both in-mem and disk). Test-only / dev tool.
    pub fn clear(&self) -> io::Result<()> {
        if let Ok(mut mem) = self.in_mem.lock() {
            mem.entries.clear();
        }
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                let _ = fs::remove_file(entry.path());
            }
        }
        Ok(())
    }

    fn path_for(&self, key: &CacheKey) -> PathBuf {
        let hash_hex = hex_of(&key.bytecode_hash);
        let kind_tag = match key.kind {
            ShaderKind::SpirV => "spv",
            ShaderKind::Nir   => "nir",
        };
        let name = format!(
            "{hash_hex}_{vendor:02}_{generation:04}_{ver:08}_{kind_tag}.bin",
            vendor = key.backend.vendor.as_u8(),
            generation = key.backend.generation,
            ver = key.compiler_version,
        );
        self.root.join(name)
    }
}

impl InMemoryIndex {
    fn insert(&mut self, key: CacheKey, bytes: Vec<u8>) {
        // Bounded LRU-ish: remove old entries for the same key,
        // append at back, drop front when over capacity.
        self.entries.retain(|(k, _)| *k != key);
        self.entries.push((key, bytes));
        while self.entries.len() > self.capacity {
            self.entries.remove(0);
        }
    }
}

/// Lowercase-hex encoding of a byte slice.
fn hex_of(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(nibble_to_hex(b >> 4));
        s.push(nibble_to_hex(b & 0x0F));
    }
    s
}
fn nibble_to_hex(n: u8) -> char {
    match n {
        0..=9   => (b'0' + n) as char,
        10..=15 => (b'a' + n - 10) as char,
        _       => '?',
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aqueduct_gpu::backends::GpuVendor;

    fn tmp_root(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("aqueduct-gpu-shader-cache-{}-{}", std::process::id(), tag));
        p
    }

    fn fresh(tag: &str) -> (ShaderCache, PathBuf) {
        let dir = tmp_root(tag);
        let _ = fs::remove_dir_all(&dir);
        let c = ShaderCache::open(&dir).unwrap();
        (c, dir)
    }

    fn key(hash_byte: u8) -> CacheKey {
        CacheKey {
            bytecode_hash: [hash_byte; 32],
            backend: BackendId::new(GpuVendor::Software, 0),
            compiler_version: 0,
            kind: ShaderKind::SpirV,
        }
    }

    #[test]
    fn miss_then_insert_then_hit() {
        let (c, dir) = fresh("roundtrip");
        let k = key(0xAA);
        assert!(matches!(c.lookup(&k), LookupResult::Miss));
        c.insert(&k, b"\x03\x02\x23\x07rest").unwrap();
        match c.lookup(&k) {
            LookupResult::Hit(b) => assert_eq!(&b, b"\x03\x02\x23\x07rest"),
            other => panic!("expected Hit, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn different_backends_dont_collide() {
        let (c, dir) = fresh("collision");
        let k1 = CacheKey {
            backend: BackendId::new(GpuVendor::Software, 0),
            ..key(0xBB)
        };
        let k2 = CacheKey {
            backend: BackendId::new(GpuVendor::Apple, 1),
            ..key(0xBB)
        };
        c.insert(&k1, b"sw-bytes").unwrap();
        c.insert(&k2, b"mtl-bytes").unwrap();
        assert!(matches!(c.lookup(&k1), LookupResult::Hit(b) if b == b"sw-bytes"));
        assert!(matches!(c.lookup(&k2), LookupResult::Hit(b) if b == b"mtl-bytes"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn hex_encoding_lowercase_64chars() {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0xFF, 0x12, 0x34]);
        let s = hex_of(&bytes);
        assert_eq!(s.len(), 64);
        assert!(s.starts_with("deadbeef00ff1234"));
        assert!(s.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)));
    }

    #[test]
    fn in_mem_index_evicts_old_entries() {
        let dir = tmp_root("evict");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let c = ShaderCache {
            root: dir.clone(),
            in_mem: Mutex::new(InMemoryIndex {
                entries: Vec::new(),
                capacity: 2,
            }),
        };
        let k1 = key(1);
        let k2 = key(2);
        let k3 = key(3);
        c.insert(&k1, b"one").unwrap();
        c.insert(&k2, b"two").unwrap();
        c.insert(&k3, b"three").unwrap();
        // k1 evicted from in-mem; disk still has it.
        {
            let mem = c.in_mem.lock().unwrap();
            assert_eq!(mem.entries.len(), 2);
            assert!(mem.entries.iter().any(|(k, _)| k == &k2));
            assert!(mem.entries.iter().any(|(k, _)| k == &k3));
            assert!(!mem.entries.iter().any(|(k, _)| k == &k1));
        }
        // Disk lookup still hits and repopulates in-mem.
        match c.lookup(&k1) {
            LookupResult::Hit(b) => assert_eq!(&b, b"one"),
            other => panic!("expected disk Hit, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn clear_removes_disk_files() {
        let (c, dir) = fresh("clear");
        c.insert(&key(0xC1), b"x").unwrap();
        c.insert(&key(0xC2), b"y").unwrap();
        c.clear().unwrap();
        assert!(matches!(c.lookup(&key(0xC1)), LookupResult::Miss));
        assert!(matches!(c.lookup(&key(0xC2)), LookupResult::Miss));
        let _ = fs::remove_dir_all(&dir);
    }
}
