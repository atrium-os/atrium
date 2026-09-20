//! Fetching external script, behind a seam.
//!
//! In the shipped design the converter has no network of its own: it goes
//! through the brokered fetcher (backend spec §2), which is the only component
//! holding a network capability. This trait is that seam. The instrument uses
//! an HTTP implementation; a jailed converter would implement the same trait
//! against the broker, and nothing else in the crate changes.

use std::{collections::HashMap, fs, path::PathBuf, process::Command};

pub trait Fetcher {
    fn get(&mut self, url: &str) -> Result<String, String>;
    /// Bytes actually pulled over the wire, for the report.
    fn bytes_fetched(&self) -> u64 { 0 }
}

/// A fetcher that fetches nothing — the default for tests, so no test can
/// silently depend on the network.
pub struct NoNetwork;
impl Fetcher for NoNetwork {
    fn get(&mut self, _url: &str) -> Result<String, String> { Err("no network".into()) }
}

/// Serves from an in-memory table. Lets a test exercise the external-script
/// path deterministically.
#[derive(Default)]
pub struct MapFetcher(pub HashMap<String, String>);
impl Fetcher for MapFetcher {
    fn get(&mut self, url: &str) -> Result<String, String> {
        self.0.get(url).cloned().ok_or_else(|| format!("404 {url}"))
    }
}

/// HTTP via curl, with an on-disk cache keyed by the URL.
///
/// Cached by content address so a re-run costs nothing and third-party servers
/// are hit once — the same amortisation the real conversion pipeline gets from
/// Tessera, and simple politeness besides.
pub struct HttpFetcher {
    cache: PathBuf,
    max_bytes: u64,
    timeout_s: u64,
    pub fetched: u64,
    pub from_cache: u64,
    bytes: u64,
}

impl HttpFetcher {
    pub fn new(cache: PathBuf) -> Self {
        let _ = fs::create_dir_all(&cache);
        Self { cache, max_bytes: 8_000_000, timeout_s: 20, fetched: 0, from_cache: 0, bytes: 0 }
    }
    fn key(url: &str) -> String {
        // FNV-1a: a stable name, no dependency, and collisions are harmless
        // here because a miss just re-fetches.
        let mut h: u64 = 0xcbf29ce484222325;
        for b in url.as_bytes() { h ^= *b as u64; h = h.wrapping_mul(0x100000001b3); }
        format!("{h:016x}.js")
    }
}

impl Fetcher for HttpFetcher {
    fn get(&mut self, url: &str) -> Result<String, String> {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(format!("unsupported scheme: {url}"));
        }
        let path = self.cache.join(Self::key(url));
        if let Ok(s) = fs::read_to_string(&path) {
            self.from_cache += 1;
            return Ok(s);
        }
        let out = Command::new("curl")
            .args(["-fsSL", "--max-time", &self.timeout_s.to_string(),
                   "--max-filesize", &self.max_bytes.to_string(),
                   "-A", "atrium-navigator-corpus/0.1 (measurement)", url])
            .output()
            .map_err(|e| format!("curl: {e}"))?;
        if !out.status.success() { return Err(format!("fetch failed: {url}")); }
        let body = String::from_utf8_lossy(&out.stdout).to_string();
        self.fetched += 1;
        self.bytes += body.len() as u64;
        let _ = fs::write(&path, &body);
        Ok(body)
    }
    fn bytes_fetched(&self) -> u64 { self.bytes }
}
