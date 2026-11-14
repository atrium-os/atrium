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
    /// ★ The converter is about to ask for `url` (legacy-web §5.4.5.7, step
    /// 4): a fetcher that can fetch in parallel starts it now. Nothing is
    /// answered, nothing is charged here; the request, when it comes, is the
    /// same request as before. Ignored by default.
    fn hint(&mut self, _url: &str) {}
    /// ★ The converter will want `url`'s HEADER (legacy-web §5.4.5.7, cut
    /// A): a fetcher that can fetch ahead fetches its first bytes now,
    /// ahead of every body it was hinted. Ignored by default.
    fn hint_header(&mut self, _url: &str) {}
    /// Bytes `start .. start + len` of `url`'s body: exactly `len` of them,
    /// or fewer only where the body ends. `start + len` is at most
    /// `navigator_dom::natural::MAX_HEADER_BYTES`. The default fetches the
    /// whole body and takes the slice; a fetcher that can ask for less does.
    fn peek(&mut self, url: &str, start: usize, len: usize) -> Result<Vec<u8>, String> {
        let b = self.get_bytes(url)?;
        let a = start.min(b.len());
        Ok(b[a..(a + len).min(b.len())].to_vec())
    }
}

/// ★★ THE CONVERTER'S SCRIPT FETCHES ARE SAME-ORIGIN (legacy-web §5.4.1c,
/// applied to code as well as data; §5.4.1f). Every external script the
/// document declares, every module it imports, and every hint for one goes
/// through this, and only a URL with the DOCUMENT'S OWN ORIGIN reaches the
/// fetcher. Anything else is refused before a byte leaves, and counted by
/// host like every page-network refusal.
///
/// Measured 2026-11-13 (navigator-ui §6.6): with no rule here, 48 ad or
/// tracker scripts on 29 corpus pages were downloaded as code at
/// conversion. The page-network rule then refused what they asked for, but
/// the download itself had already told each host that a conversion
/// happened. Refusing at the source closes that. No blocklist: the rule is
/// computable from the two URLs alone, like §5.4.1c's.
pub struct SameOriginScripts<'a> {
    inner: &'a mut dyn Fetcher,
    origin: Option<url::Origin>,
    /// "cross-origin script <host>" → refusals, in first-refused order.
    pub refused: Vec<(String, u32)>,
}

impl<'a> SameOriginScripts<'a> {
    /// `base` is the document's URL. With none, no script URL has the
    /// document's origin, so every one is refused.
    pub fn new(inner: &'a mut dyn Fetcher, base: Option<&str>) -> Self {
        let origin = base.and_then(|b| url::Url::parse(b).ok()).map(|u| u.origin())
            .filter(|o| o.is_tuple());
        SameOriginScripts { inner, origin, refused: vec![] }
    }

    /// Whether `url` has the document's origin.
    pub fn allows(&self, url: &str) -> bool {
        script_allowed(self.origin.as_ref(), url)
    }

    fn refuse(&mut self, url: &str) -> String {
        // A `data:` or `blob:` script is not a request to anyone: no
        // network refusal is counted for it (it was never run before either).
        if url.starts_with("data:") || url.starts_with("blob:") {
            return format!("not run: {} script", &url[..url.find(':').unwrap_or(0)]);
        }
        let host = url::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_else(|| "?".into());
        let key = format!("cross-origin script {host}");
        match self.refused.iter_mut().find(|(k, _)| *k == key) {
            Some((_, n)) => *n += 1,
            None => self.refused.push((key, 1)),
        }
        format!("refused: cross-origin script {url}")
    }
}

/// `url` is a script the converter may fetch for a document of `origin`:
/// the same scheme, host and port. Opaque origins match nothing.
pub fn script_allowed(origin: Option<&url::Origin>, url: &str) -> bool {
    match (origin, url::Url::parse(url)) {
        (Some(o), Ok(u)) => o.is_tuple() && u.origin() == *o,
        _ => false,
    }
}

impl Fetcher for SameOriginScripts<'_> {
    fn get(&mut self, url: &str) -> Result<String, String> {
        if !self.allows(url) { return Err(self.refuse(url)) }
        self.inner.get(url)
    }
    fn get_bytes(&mut self, url: &str) -> Result<Vec<u8>, String> {
        if !self.allows(url) { return Err(self.refuse(url)) }
        self.inner.get_bytes(url)
    }
    fn bytes_fetched(&self) -> u64 { self.inner.bytes_fetched() }
    fn hint(&mut self, url: &str) { if self.allows(url) { self.inner.hint(url) } }
    fn hint_header(&mut self, _url: &str) {}
    fn peek(&mut self, url: &str, start: usize, len: usize) -> Result<Vec<u8>, String> {
        if !self.allows(url) { return Err(self.refuse(url)) }
        self.inner.peek(url, start, len)
    }
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
///
/// ★★ THE MEASUREMENT INSTRUMENT, NOT A CONVERSION PATH. This fetcher does
/// NOT apply the threat model's §2.1 address rule — curl will connect to
/// 127.0.0.1 or 192.168.1.1 if a page names it, and follows redirects with
/// `-L` to wherever they lead. It is not patched to, on purpose:
/// - curl has no "refuse private addresses" option, and the only way to bolt
///   one on (resolve here, `--resolve` to pin, follow redirects by hand) is a
///   second fetch implementation of the rule, the kind that drifts from the
///   real one. The rule has ONE home: `navigator_fetch::Conversation`.
/// - It cannot run in a real conversion. A confined converter
///   (`PRERENDER_REQUIRE_CONFINEMENT=1`, which the jail entry sets) refuses
///   to start without `PRERENDER_FETCHD`, and inside its jail there is no
///   filesystem for the cache and no network for curl anyway. So this path
///   exists only for unconfined corpus measurement on a developer's machine,
///   fetching URLs the developer chose.
pub struct HttpFetcher {
    cache: PathBuf,
    max_bytes: u64,
    timeout_s: u64,
    pub fetched: u64,
    pub from_cache: u64,
    bytes: u64,
    /// `PRERENDER_OFFLINE=1`: answer from the cache only, a miss is a failed
    /// fetch. For MEASURING conversions from fixed inputs (threat model 2.6):
    /// with the network open, every miss — and failures are never cached —
    /// goes back out to the live site, so two runs of one page can be fed
    /// different bytes and the difference looks like the converter's.
    cache_only: bool,
    /// One entry per request, for the time-to-first-display measurement.
    pub log: Vec<CurlTiming>,
}

impl HttpFetcher {
    pub fn new(cache: PathBuf) -> Self {
        let _ = fs::create_dir_all(&cache);
        let cache_only = std::env::var("PRERENDER_OFFLINE").ok().as_deref() == Some("1");
        Self { cache, max_bytes: 8_000_000, timeout_s: 20, fetched: 0, from_cache: 0, bytes: 0, cache_only, log: vec![] }
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
        crate::timing::fetch(|| self.get_text_inner(url), |s| s.len() as u64)
    }
    fn get_bytes(&mut self, url: &str) -> Result<Vec<u8>, String> {
        crate::timing::fetch(|| self.get_bytes_inner(url), |b| b.len() as u64)
    }
    fn bytes_fetched(&self) -> u64 { self.bytes }
}

impl HttpFetcher {
    fn get_text_inner(&mut self, url: &str) -> Result<String, String> {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(format!("unsupported scheme: {url}"));
        }
        let path = self.cache.join(Self::key(url));
        if let Ok(s) = fs::read_to_string(&path) {
            self.from_cache += 1;
            self.log.push(CurlTiming { url: url.to_string(), cached: true, ..CurlTiming::default() });
            return Ok(s);
        }
        if self.cache_only {
            self.log.push(CurlTiming { url: url.to_string(), cached: true, ..CurlTiming::default() });
            return Err(format!("offline, not cached: {url}"))
        }
        let out = self.curl(url)?;
        let body = String::from_utf8_lossy(&out).to_string();
        let _ = fs::write(&path, &body);
        Ok(body)
    }
    fn get_bytes_inner(&mut self, url: &str) -> Result<Vec<u8>, String> {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(format!("unsupported scheme: {url}"));
        }
        let path = self.cache.join(Self::key_ext(url, "bin"));
        if let Ok(b) = fs::read(&path) {
            self.from_cache += 1;
            self.log.push(CurlTiming { url: url.to_string(), cached: true, bytes: b.len() as u64, ..CurlTiming::default() });
            return Ok(b)
        }
        if self.cache_only {
            self.log.push(CurlTiming { url: url.to_string(), cached: true, ..CurlTiming::default() });
            return Err(format!("offline, not cached: {url}"))
        }
        let out = self.curl(url)?;
        let _ = fs::write(&path, &out);
        Ok(out)
    }

    /// One curl, with its own timing (§5.4.5.7): DNS, TCP, TLS, first byte
    /// and total, as curl measures them, written to stderr so the body on
    /// stdout is untouched. ★ One process per URL, so one connection per
    /// URL: nothing is reused, which is also what fetchd does today
    /// (`Connection: close`).
    fn curl(&mut self, url: &str) -> Result<Vec<u8>, String> {
        let out = Command::new("curl")
            .args(["-fsSL", "--max-time", &self.timeout_s.to_string(),
                   "--max-filesize", &self.max_bytes.to_string(),
                   "-w", "%{stderr}\nCURLTIME %{time_namelookup} %{time_connect} %{time_appconnect} %{time_starttransfer} %{time_total} %{num_connects}\n",
                   "-A", "atrium-navigator-corpus/0.1 (measurement)", url])
            .output()
            .map_err(|e| format!("curl: {e}"))?;
        let err = String::from_utf8_lossy(&out.stderr);
        let mut t = CurlTiming { url: url.to_string(), ok: out.status.success(), bytes: out.stdout.len() as u64, ..CurlTiming::default() };
        if let Some(l) = err.lines().rev().find(|l| l.starts_with("CURLTIME ")) {
            let v: Vec<f64> = l[9..].split_whitespace().filter_map(|x| x.parse().ok()).collect();
            if v.len() == 6 {
                (t.dns, t.connect, t.tls, t.ttfb, t.total) = (v[0], v[1], v[2], v[3], v[4]);
                t.connects = v[5] as u32;
            }
        }
        self.log.push(t);
        if !out.status.success() { return Err(format!("fetch failed: {url}")); }
        self.fetched += 1;
        self.bytes += out.stdout.len() as u64;
        Ok(out.stdout)
    }
}

/// What curl said one fetch cost, in seconds from the start of that fetch
/// (curl's own cumulative times). All zero for a cache hit.
#[derive(Debug, Clone, Default)]
pub struct CurlTiming {
    pub url: String,
    pub cached: bool,
    pub ok: bool,
    pub bytes: u64,
    pub dns: f64,
    pub connect: f64,
    pub tls: f64,
    pub ttfb: f64,
    pub total: f64,
    pub connects: u32,
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
    /// The fetcher process, when this side spawned one. `None` for the
    /// jailed converter's relay (`over`), whose peer is its own parent.
    child: Option<std::process::Child>,
    stdin: Option<Box<dyn std::io::Write>>,
    stdout: std::io::BufReader<Box<dyn std::io::Read>>,
    /// Written before each URL. Empty for fetchd itself; `"F "` for the
    /// converter's relay, whose stdout also carries its report.
    prefix: &'static str,
    bytes: u64,
    pub requests: u64,
    /// Hints passed on (`send_hint`).
    pub hints: u64,
    /// Set once the stream is out of step; every later request fails fast
    /// instead of reading another request's bytes as its own.
    broken: Option<String>,
}

/// One reply as fetchd frames it, read and bounded.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Ok { status: u16, rest: String, body: Vec<u8> },
    Err(String),
}

impl Reply {
    /// The frame, byte for byte as fetchd would write it.
    pub fn to_frame(&self) -> Vec<u8> {
        match self {
            Reply::Ok { status, rest, body } => {
                let mut v = format!("ok {status} {rest}\n").into_bytes();
                v.extend_from_slice(body);
                v
            }
            Reply::Err(why) => format!("err {}\n", why.replace(['\n', '\r'], " ")).into_bytes(),
        }
    }
}

impl FetchdFetcher {
    /// Kept in step with navigator-fetch's body limit.
    pub const MAX_BODY: usize = 8 * 1024 * 1024;
    const MAX_HEADER: usize = 16 * 1024;
    /// A chain is at most navigator-fetch's redirects, the last response
    /// and a loop's repeated target.
    const MAX_HOPS: usize = 16;
    const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

    pub fn spawn(program: &str, args: &[String]) -> std::io::Result<Self> {
        use std::process::{Command, Stdio};
        let mut child = Command::new(program).args(args)
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit())
            .spawn()?;
        let stdin: Box<dyn std::io::Write> = Box::new(child.stdin.take().expect("piped"));
        let stdout: Box<dyn std::io::Read> = Box::new(child.stdout.take().expect("piped"));
        Ok(FetchdFetcher { child: Some(child), stdin: Some(stdin), stdout: std::io::BufReader::new(stdout),
                           prefix: "", bytes: 0, requests: 0, hints: 0, broken: None })
    }

    /// The same protocol over descriptors this process already holds — the
    /// jailed converter's stdout (requests, prefixed `F `) and stdin
    /// (replies). Its parent relays each request to a real fetchd.
    pub fn over(w: Box<dyn std::io::Write>, r: Box<dyn std::io::Read>, prefix: &'static str) -> Self {
        FetchdFetcher { child: None, stdin: Some(w), stdout: std::io::BufReader::new(r),
                        prefix, bytes: 0, requests: 0, hints: 0, broken: None }
    }

    fn fail<T>(&mut self, why: String) -> Result<T, String> {
        self.broken = Some(why.clone());
        Err(why)
    }

    /// Send one URL and read one reply, bounded. `Err` means the STREAM is
    /// unusable (and stays so); an `err` frame is `Ok(Reply::Err)`.
    pub fn exchange(&mut self, url: &str) -> Result<Reply, String> {
        let prefix = self.prefix;
        self.exchange_line(url, prefix, "")
    }

    /// `P <start> <len> <url>`: a peek, answered with one frame as a fetch
    /// is (`navigator_fetch::serve`). The same line whether this side talks
    /// to fetchd or to the relay, which passes it on.
    pub fn exchange_peek(&mut self, url: &str, start: usize, len: usize) -> Result<Reply, String> {
        self.exchange_line(url, "", &format!("P {start} {len} "))
    }

    fn exchange_line(&mut self, url: &str, prefix: &str, verb: &str) -> Result<Reply, String> {
        use std::io::{Read, Write};
        if let Some(b) = &self.broken { return Err(format!("fetcher unusable: {b}")) }
        // ★ One request per line, so a URL that carries a line break would
        // smuggle a second request of the page's choosing. Refused here.
        if url.contains(['\n', '\r']) { return Ok(Reply::Err(format!("refused: line break in URL {url:?}"))) }
        self.requests += 1;
        let Some(stdin) = self.stdin.as_mut() else { return Err("fetcher input closed".into()) };
        if let Err(e) = write!(stdin, "{prefix}{verb}{url}\n").and_then(|_| stdin.flush()) {
            return self.fail(format!("fetcher went away: {e}"));
        }
        let line = match read_line_bounded(&mut self.stdout, Self::MAX_HEADER) {
            Ok(Some(l)) => l,
            Ok(None) => return self.fail("fetcher exited".into()),
            Err(e) => return self.fail(e),
        };
        if let Some(reason) = line.strip_prefix("err ") { return Ok(Reply::Err(reason.to_string())) }
        let f: Vec<&str> = line.splitn(3, ' ').collect();
        let rest = f.get(2).copied().unwrap_or("");
        let len = rest.split(' ').nth(1).and_then(|s| s.parse::<usize>().ok());
        let (Some(&"ok"), Some(status), Some(len)) = (f.first(), f.get(1).and_then(|s| s.parse::<u16>().ok()), len) else {
            return self.fail(format!("unrecognised fetcher reply {line:?}"));
        };
        if len > Self::MAX_BODY { return self.fail(format!("fetcher claims a {len}-byte body")) }
        let mut body = vec![0u8; len];
        if let Err(e) = self.stdout.read_exact(&mut body) { return self.fail(format!("short body: {e}")) }
        self.bytes += len as u64;
        Ok(Reply::Ok { status, rest: rest.to_string(), body })
    }

    /// `N <url>`: the page itself with its chain (status, set a cookie,
    /// URL), bounded like the rest of the reply. Only fetchd itself
    /// answers it (not the converter's relay).
    pub fn exchange_navigation(&mut self, url: &str) -> Result<(Reply, Vec<(u16, bool, String)>), String> {
        let r = self.exchange_line(url, "", "N ")?;
        let line = match read_line_bounded(&mut self.stdout, Self::MAX_HEADER) {
            Ok(Some(l)) => l,
            Ok(None) => return self.fail("fetcher exited".into()),
            Err(e) => return self.fail(e),
        };
        let Some(n) = line.strip_prefix("hops ").and_then(|n| n.parse::<usize>().ok()).filter(|&n| n <= Self::MAX_HOPS) else {
            return self.fail(format!("unrecognised chain {line:?}"));
        };
        let mut hops = Vec::with_capacity(n);
        for _ in 0..n {
            let l = match read_line_bounded(&mut self.stdout, Self::MAX_HEADER) {
                Ok(Some(l)) => l,
                Ok(None) => return self.fail("fetcher exited".into()),
                Err(e) => return self.fail(e),
            };
            let f: Vec<&str> = l.splitn(3, ' ').collect();
            match (f.first().and_then(|v| v.parse::<u16>().ok()), f.get(1), f.get(2)) {
                (Some(st), Some(&c @ ("0" | "1")), Some(u)) => hops.push((st, c == "1", u.to_string())),
                _ => return self.fail(format!("unrecognised hop {l:?}")),
            }
        }
        Ok((r, hops))
    }

    /// The body as it came off the wire. `get` is this, decoded.
    fn get_raw(&mut self, url: &str) -> Result<Vec<u8>, String> {
        let r = self.exchange(url)?;
        Self::body_of(url, r)
    }

    fn body_of(url: &str, r: Reply) -> Result<Vec<u8>, String> {
        match r {
            Reply::Err(reason) => Err(format!("fetch {url}: {reason}")),
            // Same contract as HttpFetcher's `curl -f`: an HTTP error is a failure.
            Reply::Ok { status, .. } if !(200..300).contains(&status) => Err(format!("fetch {url}: HTTP {status}")),
            Reply::Ok { body, .. } => Ok(body),
        }
    }
}

/// A line of at most `max` bytes, without its newline. `Ok(None)` at a clean
/// end of stream. ★ Bounded before it is believed: a peer that never sends a
/// newline is refused at `max`, not buffered forever.
pub fn read_line_bounded(r: &mut impl std::io::BufRead, max: usize) -> Result<Option<String>, String> {
    use std::io::Read;
    let mut line = Vec::new();
    match std::io::BufRead::read_until(&mut Read::take(&mut *r, max as u64), b'\n', &mut line) {
        Ok(0) => Ok(None),
        Ok(_) if line.last() != Some(&b'\n') => Err("reply header too long or truncated".into()),
        Ok(_) => Ok(Some(String::from_utf8_lossy(&line).trim_end_matches(['\n', '\r']).to_string())),
        Err(e) => Err(format!("read: {e}")),
    }
}

impl FetchdFetcher {
    /// Tell fetchd the most its hints may pull: `B <bytes>`, no answer.
    pub fn send_budget(&mut self, bytes: u64) {
        use std::io::Write;
        if self.broken.is_some() { return }
        if let Some(stdin) = self.stdin.as_mut() {
            if write!(stdin, "B {bytes}\n").and_then(|_| stdin.flush()).is_err() {
                self.broken = Some("fetcher went away".into());
            }
        }
    }

    /// Pass a hint on: `H <url>`, no answer (fetchd's prefetcher). A URL
    /// with a line break, or a fetcher already out of step, is not hinted.
    pub fn send_hint(&mut self, url: &str) {
        use std::io::Write;
        if self.broken.is_some() || url.contains(['\n', '\r']) { return }
        if let Some(stdin) = self.stdin.as_mut() {
            if write!(stdin, "H {url}\n").and_then(|_| stdin.flush()).is_err() {
                self.broken = Some("fetcher went away".into());
            }
        }
        self.hints += 1;
    }

    /// Pass a header hint on: `HH <url>`, no answer.
    pub fn send_hint_header(&mut self, url: &str) {
        use std::io::Write;
        if self.broken.is_some() || url.contains(['\n', '\r']) { return }
        if let Some(stdin) = self.stdin.as_mut() {
            if write!(stdin, "HH {url}\n").and_then(|_| stdin.flush()).is_err() {
                self.broken = Some("fetcher went away".into());
            }
        }
    }
}

impl Fetcher for FetchdFetcher {
    fn hint(&mut self, url: &str) { self.send_hint(url) }
    fn hint_header(&mut self, url: &str) { self.send_hint_header(url) }
    fn peek(&mut self, url: &str, start: usize, len: usize) -> Result<Vec<u8>, String> {
        crate::timing::fetch(|| {
            let r = self.exchange_peek(url, start, len)?;
            let b = Self::body_of(url, r)?;
            // ★ Bounded before it is believed: more than was asked for is
            // a fetcher out of step, not a longer header.
            if b.len() > len { return Err(format!("peek {url}: {} bytes for a {len}-byte ask", b.len())) }
            Ok(b)
        }, |b| b.len() as u64)
    }
    fn get(&mut self, url: &str) -> Result<String, String> {
        crate::timing::fetch(|| self.get_raw(url), |b| b.len() as u64).map(|b| String::from_utf8_lossy(&b).into_owned())
    }
    fn get_bytes(&mut self, url: &str) -> Result<Vec<u8>, String> {
        crate::timing::fetch(|| self.get_raw(url), |b| b.len() as u64)
    }
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
        let Some(child) = self.child.as_mut() else { return };
        let start = std::time::Instant::now();
        let mut nap = std::time::Duration::from_millis(1);
        while start.elapsed() < Self::SHUTDOWN_GRACE {
            if let Ok(Some(_)) = child.try_wait() { return }
            std::thread::sleep(nap);
            nap = (nap * 2).min(std::time::Duration::from_millis(20));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// ★ A TOTAL fetched-bytes budget for ONE conversion (threat model §2.2, P2).
///
/// fetchd bounds each response (8 MiB), and nothing bounded their sum: a page
/// naming hundreds of subresources could pull gigabytes through one
/// conversion, a disk-fill through the relay cache and a bandwidth cost the
/// reader never chose. This is enforced in the DRIVER's relay, on the
/// trusted side of the jail: the converter only asks, so it cannot spend
/// past it however it is compromised.
///
/// Every byte pulled from upstream counts, delivered or not. Once the budget
/// is spent, a request is refused WITHOUT reaching upstream; the response
/// that crosses it is pulled (fetchd already bounds it to 8 MiB) but refused
/// rather than delivered. So the converter receives at most `limit` bytes,
/// and upstream is asked for at most `limit` plus one response.
#[derive(Debug, Clone, PartialEq)]
pub struct FetchBudget {
    pub limit: u64,
    /// Bytes pulled from upstream, delivered or not.
    pub pulled: u64,
    /// Bytes handed to the converter.
    pub delivered: u64,
    /// Requests refused by the budget.
    pub refused: u32,
    /// The first URL refused, for the report.
    pub first_refused: Option<String>,
}

impl FetchBudget {
    /// 128 MiB, chosen from what the corpus actually pulls (both corpora,
    /// 123 conversions, relayed bytes from the 2026-11-05 run): p50 1.5 MiB,
    /// p90 8.4 MiB, p99 26.7 MiB, max 75.6 MiB (theverge, 295 requests). It
    /// admits every conversion measured with 1.7x headroom over the largest
    /// (64 MiB would have refused theverge), and bounds the worst case at
    /// 128 MiB delivered, 136 MiB pulled, where nothing bounded it before.
    pub const DEFAULT: u64 = 128 * 1024 * 1024;

    pub fn new(limit: u64) -> Self {
        FetchBudget { limit, pulled: 0, delivered: 0, refused: 0, first_refused: None }
    }

    /// Answer one relayed request, asking `upstream` only while the budget
    /// lasts.
    pub fn admit(&mut self, url: &str, upstream: impl FnOnce(&str) -> Reply) -> Reply {
        if self.pulled >= self.limit { return self.refuse(url) }
        let r = upstream(url);
        if let Reply::Ok { body, .. } = &r {
            self.pulled += body.len() as u64;
            if self.pulled > self.limit { return self.refuse(url) }
            self.delivered += body.len() as u64;
        }
        r
    }

    fn refuse(&mut self, url: &str) -> Reply {
        self.refused += 1;
        if self.first_refused.is_none() { self.first_refused = Some(url.to_string()) }
        Reply::Err(format!("refused: per-conversion fetch budget exhausted ({} bytes pulled, limit {})",
                           self.pulled, self.limit))
    }

    /// The report's refusal line, when the budget refused anything.
    pub fn reason(&self) -> Option<String> {
        (self.refused > 0).then(|| format!(
            "fetch budget exhausted: {} bytes pulled of a {}-byte limit; {} request(s) refused, first {}",
            self.pulled, self.limit, self.refused, self.first_refused.as_deref().unwrap_or("?")))
    }
}

/// One fetcher process shared by the script loader and the page network.
#[derive(Clone)]
pub struct SharedFetcher(pub std::rc::Rc<std::cell::RefCell<FetchdFetcher>>);
impl Fetcher for SharedFetcher {
    fn get(&mut self, url: &str) -> Result<String, String> { self.0.borrow_mut().get(url) }
    fn hint(&mut self, url: &str) { self.0.borrow_mut().hint(url) }
    fn hint_header(&mut self, url: &str) { self.0.borrow_mut().hint_header(url) }
    fn peek(&mut self, url: &str, start: usize, len: usize) -> Result<Vec<u8>, String> { self.0.borrow_mut().peek(url, start, len) }
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

    fn ok(n: usize) -> Reply { Reply::Ok { status: 200, rest: format!("0 {n} u"), body: vec![b'x'; n] } }

    #[test]
    fn the_budget_delivers_up_to_its_limit_then_refuses_without_asking() {
        let mut b = FetchBudget::new(250);
        assert!(matches!(b.admit("a", |_| ok(100)), Reply::Ok { .. }));
        assert!(matches!(b.admit("b", |_| ok(100)), Reply::Ok { .. }));
        // Crosses the limit: pulled, refused, not delivered.
        let r = b.admit("c", |_| ok(100));
        assert!(matches!(&r, Reply::Err(e) if e.contains("fetch budget exhausted")), "{r:?}");
        assert_eq!((b.pulled, b.delivered), (300, 200));
        // Spent: refused without reaching upstream at all.
        let r = b.admit("d", |_| panic!("upstream must not be asked once the budget is spent"));
        assert!(matches!(r, Reply::Err(_)));
        assert_eq!(b.refused, 2);
        assert_eq!(b.first_refused.as_deref(), Some("c"));
        assert!(b.reason().unwrap().contains("2 request(s) refused, first c"));
    }

    #[test]
    fn errors_cost_nothing_and_an_unspent_budget_reports_nothing() {
        let mut b = FetchBudget::new(10);
        assert!(matches!(b.admit("a", |_| Reply::Err("nope".into())), Reply::Err(_)));
        assert!(matches!(b.admit("b", |_| ok(10)), Reply::Ok { .. }), "exactly the limit is within it");
        assert_eq!(b.refused, 0);
        assert!(b.reason().is_none());
    }

    #[test]
    fn a_fetcher_that_dies_is_reported() {
        let mut f = fake("exit 0");
        assert!(f.get("https://a.example/").is_err());
    }
}
