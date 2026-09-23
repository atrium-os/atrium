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
    /// ★ RAW bytes. An image's intrinsic size lives in its header, and a
    /// header read through a lossy UTF-8 conversion is not the header. The
    /// default exists so a text-only fetcher still compiles; every fetcher
    /// that can serve an image overrides it.
    fn get_bytes(&mut self, url: &str) -> Result<Vec<u8>, String> {
        self.get(url).map(String::into_bytes)
    }
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
    fn key_ext(url: &str, ext: &str) -> String {
        let mut h: u64 = 0xcbf29ce484222325;
        for b in url.as_bytes() { h ^= *b as u64; h = h.wrapping_mul(0x100000001b3); }
        format!("{h:016x}.{ext}")
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
    fn get_bytes(&mut self, url: &str) -> Result<Vec<u8>, String> {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(format!("unsupported scheme: {url}"));
        }
        let path = self.cache.join(Self::key_ext(url, "bin"));
        if let Ok(b) = fs::read(&path) { self.from_cache += 1; return Ok(b) }
        let out = Command::new("curl")
            .args(["-fsSL", "--max-time", &self.timeout_s.to_string(),
                   "--max-filesize", &self.max_bytes.to_string(),
                   "-A", "atrium-navigator-corpus/0.1 (measurement)", url])
            .output()
            .map_err(|e| format!("curl: {e}"))?;
        if !out.status.success() { return Err(format!("fetch failed: {url}")); }
        self.fetched += 1;
        self.bytes += out.stdout.len() as u64;
        let _ = fs::write(&path, &out.stdout);
        Ok(out.stdout)
    }
    fn bytes_fetched(&self) -> u64 { self.bytes }
}

/// The brokered fetcher (backend spec §8.1, M1): `navigator-fetchd`, a
/// process in Capsicum capability mode — usually inside its own jail,
/// launched through `portcullis exec --daemon`. One line per URL out; one
/// frame back: `ok <status> <redirects> <len> <final-url>\n` + `len` bytes,
/// or `err <reason>\n`.
///
/// ★ THE REPLY IS UNTRUSTED INPUT. The fetcher is sandboxed because it parses
/// what the network sends, so what it writes back is bounded before it is
/// believed: the header line has a length limit, and a stated body length
/// over `MAX_BODY` is a failure, not an allocation.
pub struct FetchdFetcher {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    stdout: std::io::BufReader<std::process::ChildStdout>,
    bytes: u64,
    pub requests: u64,
    /// Set once the stream is out of step; every later request fails fast
    /// instead of reading another request's bytes as its own.
    broken: Option<String>,
}

impl FetchdFetcher {
    /// Kept in step with navigator-fetch's body limit.
    pub const MAX_BODY: usize = 8 * 1024 * 1024;
    const MAX_HEADER: usize = 16 * 1024;
    const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

    pub fn spawn(program: &str, args: &[String]) -> std::io::Result<Self> {
        use std::process::{Command, Stdio};
        let mut child = Command::new(program).args(args)
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit())
            .spawn()?;
        let stdin = child.stdin.take();
        let stdout = std::io::BufReader::new(child.stdout.take().expect("piped"));
        Ok(FetchdFetcher { child, stdin, stdout, bytes: 0, requests: 0, broken: None })
    }

    fn fail<T>(&mut self, why: String) -> Result<T, String> {
        self.broken = Some(why.clone());
        Err(why)
    }

    /// The body as it came off the wire. `get` is this, decoded.
    fn get_raw(&mut self, url: &str) -> Result<Vec<u8>, String> {
        use std::io::{BufRead, Read, Write};
        if let Some(b) = &self.broken { return Err(format!("fetcher unusable: {b}")) }
        // ★ One request per line, so a URL that carries a line break would
        // smuggle a second request of the page's choosing. Refused here.
        if url.contains(['\n', '\r']) { return Err(format!("refused: line break in URL {url:?}")) }
        self.requests += 1;
        let Some(stdin) = self.stdin.as_mut() else { return Err("fetcher input closed".into()) };
        if let Err(e) = writeln!(stdin, "{url}").and_then(|_| stdin.flush()) {
            return self.fail(format!("fetcher went away: {e}"));
        }
        let mut line = Vec::new();
        match (&mut self.stdout).take(Self::MAX_HEADER as u64).read_until(b'\n', &mut line) {
            Ok(0) => return self.fail("fetcher exited".into()),
            Ok(_) if line.last() != Some(&b'\n') => return self.fail("fetcher reply header too long".into()),
            Ok(_) => {}
            Err(e) => return self.fail(format!("fetcher read: {e}")),
        }
        let line = String::from_utf8_lossy(&line).trim_end().to_string();
        if let Some(reason) = line.strip_prefix("err ") { return Err(format!("fetch {url}: {reason}")) }
        let f: Vec<&str> = line.splitn(5, ' ').collect();
        let (Some(&"ok"), Some(status), Some(len)) = (f.first(), f.get(1).and_then(|s| s.parse::<u16>().ok()),
                                                     f.get(3).and_then(|s| s.parse::<usize>().ok())) else {
            return self.fail(format!("unrecognised fetcher reply {line:?}"));
        };
        if len > Self::MAX_BODY { return self.fail(format!("fetcher claims a {len}-byte body")) }
        let mut body = vec![0u8; len];
        if let Err(e) = self.stdout.read_exact(&mut body) { return self.fail(format!("short body: {e}")) }
        self.bytes += len as u64;
        // Same contract as HttpFetcher's `curl -f`: an HTTP error is a failure.
        if !(200..300).contains(&status) { return Err(format!("fetch {url}: HTTP {status}")) }
        Ok(body)
    }
}

impl Fetcher for FetchdFetcher {
    fn get(&mut self, url: &str) -> Result<String, String> {
        self.get_raw(url).map(|b| String::from_utf8_lossy(&b).into_owned())
    }
    fn get_bytes(&mut self, url: &str) -> Result<Vec<u8>, String> { self.get_raw(url) }
    fn bytes_fetched(&self) -> u64 { self.bytes }
}

impl Drop for FetchdFetcher {
    /// End of input is the fetcher's signal to exit, and — through a
    /// launcher — for its jail to be torn down. Waited for, so a conversion
    /// never leaves its fetcher behind.
    ///
    /// ★ BOUNDED. A fetcher that ignores end-of-input — a broken or
    /// compromised one — would otherwise hang the conversion in `wait`
    /// forever. The grace lets a launcher tear its jail down (a killed
    /// launcher leaks the jail: backend §4.7a); the kill is the backstop.
    fn drop(&mut self) {
        drop(self.stdin.take());
        let start = std::time::Instant::now();
        let mut nap = std::time::Duration::from_millis(1);
        while start.elapsed() < Self::SHUTDOWN_GRACE {
            if let Ok(Some(_)) = self.child.try_wait() { return }
            std::thread::sleep(nap);
            nap = (nap * 2).min(std::time::Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One fetcher process shared by the script loader and the page network.
#[derive(Clone)]
pub struct SharedFetcher(pub std::rc::Rc<std::cell::RefCell<FetchdFetcher>>);
impl Fetcher for SharedFetcher {
    fn get(&mut self, url: &str) -> Result<String, String> { self.0.borrow_mut().get(url) }
    fn get_bytes(&mut self, url: &str) -> Result<Vec<u8>, String> { self.0.borrow_mut().get_bytes(url) }
    fn bytes_fetched(&self) -> u64 { self.0.borrow().bytes_fetched() }
}

#[cfg(test)]
mod fetchd_tests {
    use super::*;

    /// A stand-in fetcher: a shell loop speaking the protocol.
    fn fake(script: &str) -> FetchdFetcher {
        FetchdFetcher::spawn("sh", &["-c".into(), script.into()]).expect("sh")
    }

    #[test]
    fn a_body_comes_back_and_the_stream_stays_in_step() {
        let mut f = fake(r#"while read u; do printf 'ok 200 0 5 %s\nhello' "$u"; done"#);
        assert_eq!(f.get("https://a.example/x"), Ok("hello".into()));
        assert_eq!(f.get("https://a.example/y"), Ok("hello".into()));
        assert_eq!(f.requests, 2);
    }

    #[test]
    fn an_http_error_status_is_a_failure() {
        let mut f = fake(r#"while read u; do printf 'ok 404 0 3 %s\nnah' "$u"; done"#);
        assert!(f.get("https://a.example/").unwrap_err().contains("HTTP 404"));
        // …and the body was consumed, so the next request is still in step.
        assert!(f.get("https://a.example/").unwrap_err().contains("HTTP 404"));
    }

    #[test]
    fn an_err_frame_is_a_failure_that_keeps_the_stream() {
        let mut f = fake(r#"while read u; do echo 'err Refused("scheme file")'; done"#);
        assert!(f.get("file:///etc/passwd").unwrap_err().contains("Refused"));
        assert!(f.get("file:///x").is_err());
    }

    #[test]
    fn a_line_break_in_a_url_is_refused_before_it_is_sent() {
        // The fake would answer a smuggled second request; it must never see one.
        let mut f = fake(r#"while read u; do printf 'ok 200 0 4 %s\nSEEN' "$u"; done"#);
        assert!(f.get("https://a.example/\nhttps://evil.example/").unwrap_err().contains("line break"));
        assert_eq!(f.requests, 0);
        assert_eq!(f.get("https://a.example/"), Ok("SEEN".into()));
    }

    #[test]
    fn a_lying_length_breaks_the_fetcher_instead_of_desyncing_it() {
        // Claims 100 bytes, sends 5, then exits: a short read, then fail-fast.
        let mut f = fake(r#"read u; printf 'ok 200 0 100 %s\nhello' "$u""#);
        assert!(f.get("https://a.example/").unwrap_err().contains("short body"));
        assert!(f.get("https://a.example/").unwrap_err().contains("unusable"));
    }

    #[test]
    fn an_oversized_claim_is_refused_without_allocating_it() {
        let mut f = fake(r#"read u; printf 'ok 200 0 99999999999 %s\n' "$u""#);
        assert!(f.get("https://a.example/").unwrap_err().contains("claims"));
    }

    #[test]
    fn a_fetcher_that_ignores_end_of_input_is_killed_after_the_grace() {
        let t = std::time::Instant::now();
        drop(fake("trap '' HUP; sleep 60"));
        let took = t.elapsed();
        assert!(took < std::time::Duration::from_secs(10), "drop waited {took:?}");
        assert!(took >= std::time::Duration::from_secs(4), "it must wait the grace first: {took:?}");
    }

    #[test]
    fn a_fetcher_that_dies_is_reported() {
        let mut f = fake("exit 0");
        assert!(f.get("https://a.example/").is_err());
    }
}
