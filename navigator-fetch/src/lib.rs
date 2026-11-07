//! M1 — the Navigator's fetcher (navigator-backend §2, §8).
//!
//! The only component that holds network, and one that cannot name a file:
//! on FreeBSD, `sandbox::enter` puts the process in Capsicum capability mode
//! with a casper `cap_net` channel as its only way out (see `sandbox`). What
//! it can do then is exactly `fetch`: resolve, connect, TLS, one bounded GET,
//! a bounded number of redirects.

pub mod addr;
pub mod http;
pub mod nuntius;
pub mod prefetch;
#[cfg(target_os = "freebsd")]
pub mod sandbox;

#[cfg(test)]
use std::io::{Read, Write};
use std::collections::{BTreeSet, HashMap};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

pub const MAX_REDIRECTS: usize = 5;
pub const TIMEOUT: Duration = Duration::from_secs(20);

/// How a fetch reaches the network. The sandboxed daemon uses casper; tests
/// and the unsandboxed host use `Direct`.
///
/// ★ TWO STEPS, SO THE CHECK CAN SIT BETWEEN THEM. Resolution and connection
/// used to be one call (`connect(host, port)`), which left nowhere to refuse
/// a private answer: whatever the resolver said, the socket went there. Now a
/// caller resolves, `Conversation` checks every answer against the `Policy`
/// and pins it, and only then connects — to an address it has already judged.
pub trait Connect {
    fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>>;
    fn connect_addr(&self, addr: SocketAddr) -> std::io::Result<TcpStream>;
}

pub struct Direct;
impl Connect for Direct {
    fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
        use std::net::ToSocketAddrs;
        Ok((host, port).to_socket_addrs()?.collect())
    }
    fn connect_addr(&self, addr: SocketAddr) -> std::io::Result<TcpStream> { TcpStream::connect_timeout(&addr, TIMEOUT) }
}

#[cfg(target_os = "freebsd")]
impl Connect for sandbox::Net {
    fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> { sandbox::Net::resolve(self, host, port) }
    fn connect_addr(&self, addr: SocketAddr) -> std::io::Result<TcpStream> { sandbox::Net::connect_addr(self, addr) }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Fetched {
    /// The URL the body came from, after redirects.
    pub url: String,
    pub response: http::Response,
    pub redirects: usize,
}

#[derive(Debug, PartialEq)]
pub enum FetchError {
    BadUrl(String),
    /// Only http and https, only ports 80 and 443 (what casper will resolve),
    /// no credentials in the URL.
    Refused(String),
    Connect(String),
    Tls(String),
    Http(http::HttpError),
    TooManyRedirects,
    /// The host resolved (or is written as) an address the policy does not
    /// admit: the LAN, loopback, metadata, … (threat model §2.1).
    NonPublicAddress { host: String, addr: IpAddr, range: addr::Range },
    /// A second resolution of a host already pinned in this conversation
    /// answered with none of the addresses it answered the first time — DNS
    /// rebinding, or a host that changed address mid-conversion. Either way
    /// the first answer is the one this conversion keeps.
    Rebinding { host: String, pinned: Vec<IpAddr>, answered: Vec<IpAddr> },
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::NonPublicAddress { host, addr, range } =>
                write!(f, "Refused(\"{host} is {addr}, a {range} address; the private network is not granted\")"),
            FetchError::Rebinding { host, pinned, answered } =>
                write!(f, "Refused(\"{host} answered {answered:?} after pinning {pinned:?} (rebinding)\")"),
            other => write!(f, "{other:?}"),
        }
    }
}

/// Load the trust store. Must happen BEFORE capability mode: afterwards the
/// file cannot be named.
pub fn tls_config(pem: &[u8]) -> Result<Arc<rustls::ClientConfig>, String> {
    use rustls_pki_types::{pem::PemObject, CertificateDer};
    let mut roots = rustls::RootCertStore::empty();
    let mut n = 0;
    for c in CertificateDer::pem_slice_iter(pem) {
        let c = c.map_err(|e| format!("trust store: {e:?}"))?;
        if roots.add(c).is_ok() { n += 1 }
    }
    if n == 0 { return Err("trust store holds no usable certificates".into()) }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions().map_err(|e| e.to_string())?
        .with_root_certificates(roots).with_no_client_auth();
    Ok(Arc::new(cfg))
}

/// One-shot: a fresh conversation for a single URL. Pins nothing beyond it.
pub fn fetch(net: &(dyn Connect + Sync), tls: &Arc<rustls::ClientConfig>, url: &str) -> Result<Fetched, FetchError> {
    Conversation::new(net, tls.clone(), addr::Policy::default()).fetch(url)
}

/// Every fetch of one conversion (threat model §2.1).
///
/// ★ THE PIN IS THE REBINDING DEFENCE. The first resolution of a host decides
/// the addresses that host has for the rest of this conversion. A second
/// lookup of the same host and port is not made at all; a lookup on the other
/// port (80 after 443) must answer with at least one already-pinned address,
/// and only those are used. So `evil.example` answering 93.184.x.x first and
/// 127.0.0.1 next gets the second answer refused twice over: by the range
/// check, and — were the private network granted — by the pin.
pub struct Conversation<'a> {
    net: &'a (dyn Connect + Sync),
    tls: Arc<rustls::ClientConfig>,
    policy: addr::Policy,
    /// ★ Shared by every SIBLING of this conversation (`sibling`): the pins
    /// (§2.1 — one conversation, one answer per name, whichever thread asked
    /// first) and the idle connections. Concurrency must not become a way
    /// to get two answers for one name.
    shared: Arc<std::sync::Mutex<Shared>>,
    /// Keep connections open and reuse them (legacy-web §5.4.5.7, step 4).
    reuse: bool,
    /// ★ The app's site list (nuntius.md §4), checked on EVERY hop before
    /// any lookup; `None` is unrestricted.
    sites: Option<Arc<::nuntius::Sites>>,
}

#[derive(Default)]
struct Shared {
    pins: HashMap<(String, u16), Vec<SocketAddr>>,
    hosts: HashMap<String, BTreeSet<IpAddr>>,
    resolutions: usize,
    idle: HashMap<(String, u16, bool), Vec<Conn>>,
    connects: usize,
    reused: usize,
}

/// Idle connections kept per origin, at most.
pub const MAX_IDLE_PER_ORIGIN: usize = 6;

enum Conn {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl std::io::Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self { Conn::Plain(s) => s.read(buf), Conn::Tls(s) => s.read(buf) }
    }
}

impl Conn {
    fn tcp(&self) -> &TcpStream { match self { Conn::Plain(s) => s, Conn::Tls(s) => s.get_ref() } }

    /// One request and its response; with `range`, the request asks for
    /// `(start, len)` and at most `cap` body bytes are read.
    fn exchange(&mut self, host_header: &str, path: &str, keep: bool, range: Option<(u64, u64)>, cap: Option<usize>) -> Result<(http::Response, bool), FetchError> {
        match self {
            Conn::Plain(s) => {
                http::write_request_range(s, host_header, path, keep, range).map_err(|e| FetchError::Connect(e.to_string()))?;
                http::read_response_capped(s, cap).map_err(FetchError::Http)
            }
            Conn::Tls(s) => {
                http::write_request_range(s, host_header, path, keep, range).map_err(|e| FetchError::Tls(e.to_string()))?;
                http::read_response_capped(s, cap).map_err(FetchError::Http)
            }
        }
    }
}

impl<'a> Conversation<'a> {
    pub fn new(net: &'a (dyn Connect + Sync), tls: Arc<rustls::ClientConfig>, policy: addr::Policy) -> Self {
        Conversation { net, tls, policy, shared: Default::default(), reuse: false, sites: None }
    }

    /// Keep connections open between requests to the same origin, and
    /// reuse them. Off by default: one connection per request, as before.
    pub fn with_reuse(mut self, on: bool) -> Self { self.reuse = on; self }

    /// ★ Narrow this conversation to `sites` (nuntius.md §4): a URL or a
    /// redirect to any other host is refused before it is looked up.
    pub fn with_sites(mut self, sites: Option<::nuntius::Sites>) -> Self { self.sites = sites.map(Arc::new); self }

    fn site_check(&self, host: &str) -> Result<(), FetchError> {
        match &self.sites {
            Some(s) if !s.admits(host) => Err(FetchError::Refused(format!("{host} is not one of this app's sites"))),
            _ => Ok(()),
        }
    }

    /// ★ Another handle on THIS conversation, for another thread: the same
    /// pins, the same idle connections, the same policy.
    pub fn sibling(&self) -> Conversation<'a> {
        Conversation { net: self.net, tls: self.tls.clone(), policy: self.policy, shared: self.shared.clone(), reuse: self.reuse, sites: self.sites.clone() }
    }

    pub fn policy(&self) -> addr::Policy { self.policy }

    /// Names resolved so far (a pinned name is resolved once).
    pub fn resolutions(&self) -> usize { self.shared.lock().unwrap().resolutions }
    /// Connections opened, and requests that reused one.
    pub fn connections(&self) -> (usize, usize) { let s = self.shared.lock().unwrap(); (s.connects, s.reused) }

    pub fn fetch(&mut self, url: &str) -> Result<Fetched, FetchError> {
        self.fetch_inner(url, None)
    }

    /// ★ Bytes `start .. start + len` of the resource and no more
    /// (legacy-web §5.4.5.7, cut A: an image's header): asked for with
    /// `Range`, and read to at most `len` bytes whatever the server sends.
    /// The same request otherwise — the address rule, the pins, the
    /// redirects. A server that ignores `Range` answers 200 with the body
    /// from its start, so for `start > 0` the first `start + len` bytes are
    /// read and the caller takes its slice (`Ranged::bytes`).
    pub fn fetch_range(&mut self, url: &str, start: u64, len: u64) -> Result<Ranged, FetchError> {
        let f = self.fetch_inner(url, Some((start, len)))?;
        Ranged::from(f, start, len)
    }

    fn fetch_inner(&mut self, url: &str, range: Option<(u64, u64)>) -> Result<Fetched, FetchError> {
        let mut current = url::Url::parse(url).map_err(|e| FetchError::BadUrl(e.to_string()))?;
        for redirects in 0..=MAX_REDIRECTS {
            let response = self.get_once(&current, range)?;
            if matches!(response.status, 301 | 302 | 303 | 307 | 308) {
                let loc = response.header("location").ok_or(FetchError::Http(http::HttpError::Malformed("redirect without Location")))?;
                let next = current.join(loc).map_err(|e| FetchError::BadUrl(e.to_string()))?;
                if current.scheme() == "https" && next.scheme() != "https" {
                    return Err(FetchError::Refused(format!("redirect downgrades to {next}")));
                }
                current = next;
                continue;
            }
            return Ok(Fetched { url: current.to_string(), response, redirects });
        }
        Err(FetchError::TooManyRedirects)
    }

    /// ★★ THE ADDRESS RULE (threat model §2.1), in its one home. Every
    /// answer is checked against the policy; the first answer for a name is
    /// PINNED for the rest of the conversation, and a later answer that
    /// shares none of the pinned addresses is rebinding. Held under the
    /// conversation's lock, so two siblings asking at once get one answer.
    pub fn resolve_checked(&mut self, host: &str, port: u16) -> Result<Vec<SocketAddr>, FetchError> {
        let key = (host.to_ascii_lowercase(), port);
        let mut sh = self.shared.lock().unwrap();
        if let Some(p) = sh.pins.get(&key) { return Ok(p.clone()) }
        sh.resolutions += 1;
        let answers = self.net.resolve(host, port).map_err(|e| FetchError::Connect(e.to_string()))?;
        if answers.is_empty() { return Err(FetchError::Connect(format!("no address for {host}"))) }
        for a in &answers {
            if let Err(range) = self.policy.admit(a.ip()) {
                return Err(FetchError::NonPublicAddress { host: host.to_string(), addr: a.ip(), range });
            }
        }
        let chosen: Vec<SocketAddr> = match sh.hosts.get(&key.0) {
            None => answers,
            Some(pinned) => {
                let keep: Vec<SocketAddr> = answers.iter().copied().filter(|a| pinned.contains(&a.ip())).collect();
                if keep.is_empty() {
                    return Err(FetchError::Rebinding { host: host.to_string(),
                        pinned: pinned.iter().copied().collect(), answered: answers.iter().map(|a| a.ip()).collect() });
                }
                keep
            }
        };
        sh.hosts.entry(key.0.clone()).or_default().extend(chosen.iter().map(|a| a.ip()));
        sh.pins.insert(key, chosen.clone());
        Ok(chosen)
    }

    fn connect(&mut self, lookup: &str, port: u16, https: bool, host: &str) -> Result<Conn, FetchError> {
        let addrs = self.resolve_checked(lookup, port)?;
        let mut last = None;
        let mut tcp = None;
        for a in &addrs {
            match self.net.connect_addr(*a) {
                Ok(s) => { tcp = Some(s); break }
                Err(e) => last = Some(format!("connect {a}: {e}")),
            }
        }
        let tcp = tcp.ok_or_else(|| FetchError::Connect(last.unwrap_or_else(|| format!("no address for {host}"))))?;
        tcp.set_read_timeout(Some(TIMEOUT)).ok();
        tcp.set_write_timeout(Some(TIMEOUT)).ok();
        self.shared.lock().unwrap().connects += 1;
        if https {
            let name = rustls_pki_types::ServerName::try_from(lookup.to_string()).map_err(|e| FetchError::Tls(e.to_string()))?;
            let conn = rustls::ClientConnection::new(self.tls.clone(), name).map_err(|e| FetchError::Tls(e.to_string()))?;
            Ok(Conn::Tls(Box::new(rustls::StreamOwned::new(conn, tcp))))
        } else {
            Ok(Conn::Plain(tcp))
        }
    }

    /// One request. With `range` `(start, len)`, a `Range` request whose
    /// body is read to at most `start + len` bytes: a 206 sends at most
    /// `len`, and a 200 (the server ignored the range) is the body from its
    /// start, of which the range is the slice.
    fn get_once(&mut self, u: &url::Url, range: Option<(u64, u64)>) -> Result<http::Response, FetchError> {
        let cap = range.map(|(s, n)| (s + n) as usize);
        let https = match u.scheme() { "https" => true, "http" => false, s => return Err(FetchError::Refused(format!("scheme {s}"))) };
        if !u.username().is_empty() || u.password().is_some() {
            return Err(FetchError::Refused("credentials in URL".into()));
        }
        let host = u.host_str().ok_or_else(|| FetchError::BadUrl("no host".into()))?.to_string();
        let port = u.port_or_known_default().unwrap_or(0);
        if port != 80 && port != 443 { return Err(FetchError::Refused(format!("port {port}"))) }
        self.site_check(&host)?;
        let literal = match u.host() {
            Some(url::Host::Ipv4(a)) => Some(IpAddr::V4(a)),
            Some(url::Host::Ipv6(a)) => Some(IpAddr::V6(a)),
            _ => None,
        };
        if let Some(ip) = literal {
            self.policy.admit(ip).map_err(|range| FetchError::NonPublicAddress { host: host.clone(), addr: ip, range })?;
        }
        let host_header = match u.port() { Some(p) => format!("{host}:{p}"), None => host.clone() };
        let mut path = u.path().to_string();
        if let Some(q) = u.query() { path.push('?'); path.push_str(q) }

        let lookup = host.trim_start_matches('[').trim_end_matches(']').to_string();
        let key = (lookup.to_ascii_lowercase(), port, https);
        // ★ A reused connection was opened to an address this conversation
        // pinned and checked; it is reused only for the same origin. If the
        // server closed it while idle, the request fails before any response
        // byte, and a GET is retried ONCE on a fresh connection.
        if self.reuse {
            let idle = self.shared.lock().unwrap().idle.get_mut(&key).and_then(Vec::pop);
            if let Some(mut c) = idle {
                match c.exchange(&host_header, &path, true, range, cap) {
                    Ok((resp, again)) => {
                        let mut sh = self.shared.lock().unwrap();
                        sh.reused += 1;
                        if again { let v = sh.idle.entry(key).or_default(); if v.len() < MAX_IDLE_PER_ORIGIN { v.push(c) } }
                        return Ok(resp);
                    }
                    Err(FetchError::Http(http::HttpError::Malformed("connection closed inside headers")))
                    | Err(FetchError::Http(http::HttpError::Io(_)))
                    | Err(FetchError::Connect(_)) | Err(FetchError::Tls(_)) => {}
                    Err(e) => return Err(e),
                }
            }
        }
        let mut c = self.connect(&lookup, port, https, &host)?;
        let (resp, again) = c.exchange(&host_header, &path, self.reuse, range, cap)?;
        if self.reuse && again {
            let mut sh = self.shared.lock().unwrap();
            let v = sh.idle.entry(key).or_default();
            if v.len() < MAX_IDLE_PER_ORIGIN { v.push(c) }
        }
        Ok(resp)
    }
}

/// ★ A RESPONSE BEING STREAMED (Nuntius, nuntius.md §3): its head, read;
/// its body, read in pieces on demand.
pub struct Stream {
    /// The URL the response came from, after redirects.
    pub url: String,
    pub redirects: usize,
    pub head: http::Head,
    body: http::Body,
    conn: Conn,
    /// The socket, to shut down from another thread (a cancel is at once).
    pub sock: TcpStream,
}

impl Stream {
    /// Up to `max` more body bytes; `Ok(None)` at its delimited end.
    pub fn read(&mut self, max: usize) -> Result<Option<Vec<u8>>, FetchError> {
        self.body.next(&mut self.conn, max).map_err(FetchError::Http)
    }
    /// How long a read may wait for a byte (the idle timeout).
    pub fn set_idle(&self, d: Duration) { self.sock.set_read_timeout(Some(d.max(Duration::from_millis(1)))).ok(); }
}

impl<'a> Conversation<'a> {
    /// ★ A request whose body is STREAMED (Nuntius): `head_only` for a
    /// `HEAD`, `range` `(first byte, length or to the end)`, `If-Range` with
    /// a validator, an `Accept`. Everything else is `fetch`'s: the URL
    /// checks, the address rule and the pins, at most `MAX_REDIRECTS`
    /// redirects and none that downgrade. `head_wait` bounds the wait for
    /// the response's head (connect, TLS and the server's first answer);
    /// the connection is never reused.
    pub fn open_stream(&mut self, url: &str, head_only: bool, range: Option<(u64, Option<u64>)>, if_range: Option<&str>,
                       accept: Option<&str>, head_wait: Duration) -> Result<Stream, FetchError> {
        let mut current = url::Url::parse(url).map_err(|e| FetchError::BadUrl(e.to_string()))?;
        for redirects in 0..=MAX_REDIRECTS {
            let https = match current.scheme() { "https" => true, "http" => false, s => return Err(FetchError::Refused(format!("scheme {s}"))) };
            if !current.username().is_empty() || current.password().is_some() { return Err(FetchError::Refused("credentials in URL".into())) }
            let host = current.host_str().ok_or_else(|| FetchError::BadUrl("no host".into()))?.to_string();
            let port = current.port_or_known_default().unwrap_or(0);
            if port != 80 && port != 443 { return Err(FetchError::Refused(format!("port {port}"))) }
            self.site_check(&host)?;
            let literal = match current.host() {
                Some(url::Host::Ipv4(a)) => Some(IpAddr::V4(a)),
                Some(url::Host::Ipv6(a)) => Some(IpAddr::V6(a)),
                _ => None,
            };
            if let Some(ip) = literal {
                self.policy.admit(ip).map_err(|range| FetchError::NonPublicAddress { host: host.clone(), addr: ip, range })?;
            }
            let host_header = match current.port() { Some(p) => format!("{host}:{p}"), None => host.clone() };
            let mut path = current.path().to_string();
            if let Some(q) = current.query() { path.push('?'); path.push_str(q) }
            let lookup = host.trim_start_matches('[').trim_end_matches(']').to_string();
            let mut c = self.connect(&lookup, port, https, &host)?;
            let sock = c.tcp().try_clone().map_err(|e| FetchError::Connect(e.to_string()))?;
            sock.set_read_timeout(Some(head_wait.max(Duration::from_millis(1)))).ok();
            let w = match &mut c {
                Conn::Plain(s) => http::write_request_full(s, &host_header, &path, head_only, range, if_range, accept).map_err(|e| FetchError::Connect(e.to_string())),
                Conn::Tls(s) => http::write_request_full(s, &host_header, &path, head_only, range, if_range, accept).map_err(|e| FetchError::Tls(e.to_string())),
            };
            w?;
            let (head, rest) = http::read_head(&mut c, head_only).map_err(FetchError::Http)?;
            if matches!(head.status, 301 | 302 | 303 | 307 | 308) {
                let loc = head.header("location").ok_or(FetchError::Http(http::HttpError::Malformed("redirect without Location")))?;
                let next = current.join(loc).map_err(|e| FetchError::BadUrl(e.to_string()))?;
                if current.scheme() == "https" && next.scheme() != "https" { return Err(FetchError::Refused(format!("redirect downgrades to {next}"))) }
                let _ = sock.shutdown(std::net::Shutdown::Both);
                current = next;
                continue;
            }
            let body = http::Body::new(&head, rest);
            return Ok(Stream { url: current.to_string(), redirects, head, body, conn: c, sock });
        }
        Err(FetchError::TooManyRedirects)
    }
}

/// What a ranged fetch got: bytes `start ..` of the body, at most the
/// length asked for, and whether the body ends within them.
#[derive(Debug, Clone, PartialEq)]
pub struct Ranged {
    pub url: String,
    pub status: u16,
    pub redirects: usize,
    pub bytes: Vec<u8>,
    /// The resource ends at the last of `bytes`.
    pub end: bool,
}

impl Ranged {
    /// ★ The answer is checked against the question, not believed: a 206
    /// must say (`Content-Range`) that it starts where it was asked to; a
    /// 200 is the body from its start, and is cut to the range here.
    fn from(f: Fetched, start: u64, len: u64) -> Result<Ranged, FetchError> {
        let bad = |why| Err(FetchError::Http(http::HttpError::Malformed(why)));
        let r = f.response;
        let (bytes, end) = match r.status {
            206 => {
                let cr = r.header("content-range").unwrap_or("").to_string();
                // `bytes <first>-<last>/<total or *>`
                let spec = cr.strip_prefix("bytes ").unwrap_or("");
                let (span, total) = spec.split_once('/').unwrap_or(("", ""));
                let (first, last) = span.split_once('-').unwrap_or(("", ""));
                let (Ok(first), Ok(last)) = (first.trim().parse::<u64>(), last.trim().parse::<u64>()) else { return bad("206 without a readable Content-Range") };
                if first != start || last < first { return bad("206 for another range") }
                let mut b = r.body;
                b.truncate(len as usize);
                // The resource ends here when the total says so, or, with
                // no total, when fewer bytes came than were asked for.
                let end = match total.trim().parse::<u64>() { Ok(t) => last + 1 >= t, Err(_) => (b.len() as u64) < len };
                (b, end)
            }
            200..=299 => {
                let whole = r.body;
                let from = (start as usize).min(whole.len());
                let to = (from + len as usize).min(whole.len());
                // Read to at most start + len: fewer means the body ended.
                let end = (whole.len() as u64) < start + len;
                (whole[from..to].to_vec(), end)
            }
            _ => return Ok(Ranged { url: f.url, status: r.status, redirects: f.redirects, bytes: vec![], end: true }),
        };
        Ok(Ranged { url: f.url, status: r.status, redirects: f.redirects, bytes, end })
    }
}

/// fetchd's options for the serve loop: `--workers N` (default
/// `prefetch::DEFAULT_WORKERS`; 0 ignores hints) and `--no-reuse`.
pub fn serve_options(args: &[String]) -> (usize, bool) {
    let workers = args.iter().position(|a| a == "--workers").and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok()).unwrap_or(prefetch::DEFAULT_WORKERS);
    (workers, !args.iter().any(|a| a == "--no-reuse"))
}

/// `--peek-window-ms N`: the header window (`prefetch::PEEK_WINDOW` by default).
pub fn peek_window_option(args: &[String]) -> Duration {
    args.iter().position(|a| a == "--peek-window-ms").and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok()).map(Duration::from_millis).unwrap_or(prefetch::PEEK_WINDOW)
}

/// ★ fetchd's serve loop, portable so it is tested where it is written
/// (the FreeBSD binary wraps it in capability mode). One line in per
/// request, one frame out per request, in order:
/// - `<url>`: fetched (from a prefetch if hinted), answered
///   `ok <status> <redirects> <len> <final-url>\n` + body, or `err <why>\n`;
/// - `H <url>`: a hint, no answer (`prefetch`);
/// - `HH <url>`: a header hint, no answer: the first `prefetch::HEAD_BYTES`
///   are fetched ahead of every body (legacy-web §5.4.5.7, cut A);
/// - `P <start> <len> <url>`: bytes `start .. start + len` of the body
///   (`prefetch::Prefetcher::peek`), framed as a fetch is: exactly `len`
///   bytes, or fewer only where the body ends; `err <why>` past
///   `prefetch::MAX_PEEK`, or once the HEADER WINDOW has closed with the
///   bytes not in hand. ★ The window (`peek_window`, from the first peek
///   of the conversion) is how long a first display may wait for its
///   images' headers. It is kept HERE, with the network's other timeouts,
///   so the converter's decisions read no clock (threat model §2.6): to the
///   converter a header not in time is a fetch that failed;
/// - `B <bytes>`: the most hints may pull, no answer;
/// - `--gate`: `gate` is called (fetchd prints its M1 gate).
pub fn serve(conv: &Conversation<'_>, workers: usize, input: impl std::io::BufRead, out: impl std::io::Write,
             gate: &mut dyn FnMut(&mut dyn std::io::Write)) {
    serve_with(conv, workers, prefetch::PEEK_WINDOW, input, out, gate)
}

/// `serve`, with the header window given (`--peek-window-ms`).
pub fn serve_with(conv: &Conversation<'_>, workers: usize, peek_window: Duration, input: impl std::io::BufRead,
                  mut out: impl std::io::Write, gate: &mut dyn FnMut(&mut dyn std::io::Write)) {
    let mut window_end: Option<std::time::Instant> = None;
    std::thread::scope(|scope| {
        let mut pf = prefetch::Prefetcher::start(scope, conv, workers.max(1));
        for line in input.lines() {
            let Ok(url) = line else { break };
            let url = url.trim();
            if url.is_empty() { continue }
            if let Some(h) = url.strip_prefix("H ") { if workers > 0 { pf.hint(h.trim()) } continue }
            if let Some(h) = url.strip_prefix("HH ") { if workers > 0 { pf.hint_header(h.trim()) } continue }
            if let Some(p) = url.strip_prefix("P ") {
                let now = std::time::Instant::now();
                let end = *window_end.get_or_insert(now + peek_window);
                let f: Vec<&str> = p.splitn(3, ' ').collect();
                let num = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok());
                let r = match (num(0), num(1), f.get(2)) {
                    (Some(start), Some(len), Some(u)) => pf.peek(u.trim(), start, len, end.saturating_duration_since(now)),
                    _ => Err("malformed peek".into()),
                };
                match r {
                    Ok(pk) => {
                        let _ = writeln!(out, "ok {} {} {} {}", pk.status, pk.redirects, pk.bytes.len(), pk.url);
                        let _ = out.write_all(&pk.bytes);
                    }
                    Err(e) => { let _ = writeln!(out, "err {}", e.replace(['\n', '\r'], " ")); }
                }
                let _ = out.flush();
                continue;
            }
            // `B <bytes>`: the caller's byte budget, which bounds what hints
            // may pull (prefetch::DEFAULT_BUDGET until told).
            if let Some(b) = url.strip_prefix("B ") { if let Ok(n) = b.trim().parse() { pf.set_budget(n) } continue }
            if url == "--gate" { let _ = out.flush(); gate(&mut out); continue }
            match pf.fetch(url) {
                Ok(f) => {
                    let _ = writeln!(out, "ok {} {} {} {}", f.response.status, f.redirects, f.response.body.len(), f.url);
                    let _ = out.write_all(&f.response.body);
                }
                Err(e) => { let _ = writeln!(out, "err {e}"); }
            }
            let _ = out.flush();
        }
        pf.close();
    });
}

/// A `Connect` that is used from one thread at a time, made shareable by a
/// lock: casper's channel (`sandbox::Net`) is a descriptor that must not be
/// used by two threads at once. Lookups and connects are serialized; the
/// transfers, which are the time, are not.
pub struct Locked<C: Connect + Send>(pub std::sync::Mutex<C>);
impl<C: Connect + Send> Connect for Locked<C> {
    fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> { self.0.lock().unwrap().resolve(host, port) }
    fn connect_addr(&self, addr: SocketAddr) -> std::io::Result<TcpStream> { self.0.lock().unwrap().connect_addr(addr) }
}


#[cfg(test)]
mod policy_tests {
    //! Threat model §2.1: the address check, the pin and the redirect hop.
    //!
    //! The resolver is scripted, so a test says exactly what DNS answered;
    //! `connect_addr` records which address it was asked for and connects to
    //! a loopback server that plays the remote host. That split is the point:
    //! the policy judges the ANSWERED address, and the test can watch it do so
    //! without the machine's real DNS or a real LAN.
    use super::*;
    use std::sync::Mutex;
    use std::collections::VecDeque;
    use std::net::TcpListener;

    struct Scripted {
        /// One answer per resolver call, in order.
        answers: Mutex<VecDeque<Vec<IpAddr>>>,
        resolved: Mutex<Vec<(String, u16)>>,
        connected: Mutex<Vec<SocketAddr>>,
        /// Where every connect really goes: a server speaking `responses`.
        server: SocketAddr,
    }

    fn server(responses: Vec<&'static str>) -> SocketAddr {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let a = l.local_addr().unwrap();
        std::thread::spawn(move || {
            for r in responses {
                let Ok((mut s, _)) = l.accept() else { return };
                // ★ Read the WHOLE request head before answering. `write!`
                // on a bare TcpStream is several write(2)s, so one read()
                // could see only the first; replying and closing then made
                // the client's next write fail with EPIPE — a flake that
                // looked like a policy failure (~1 run in 2 on Linux).
                let mut head = Vec::new();
                let mut buf = [0u8; 4096];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut buf) { Ok(0) | Err(_) => break, Ok(n) => head.extend_from_slice(&buf[..n]) }
                }
                let _ = s.write_all(r.as_bytes());
            }
        });
        a
    }

    impl Scripted {
        fn new(answers: Vec<Vec<&str>>, responses: Vec<&'static str>) -> Self {
            Scripted {
                answers: std::sync::Mutex::new(answers.into_iter().map(|v| v.into_iter().map(|s| s.parse().unwrap()).collect()).collect()),
                resolved: Default::default(), connected: Default::default(), server: server(responses),
            }
        }
    }

    impl Connect for Scripted {
        fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
            self.resolved.lock().unwrap().push((host.to_string(), port));
            let ips = self.answers.lock().unwrap().pop_front().ok_or_else(|| std::io::Error::other("unscripted lookup"))?;
            Ok(ips.into_iter().map(|ip| SocketAddr::new(ip, port)).collect())
        }
        fn connect_addr(&self, addr: SocketAddr) -> std::io::Result<TcpStream> {
            self.connected.lock().unwrap().push(addr);
            TcpStream::connect(self.server)
        }
    }

    fn tls() -> Arc<rustls::ClientConfig> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        Arc::new(rustls::ClientConfig::builder_with_provider(provider).with_safe_default_protocol_versions().unwrap()
            .with_root_certificates(rustls::RootCertStore::empty()).with_no_client_auth())
    }

    const OK: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
    const PUBLIC: &str = "93.184.215.14";

    #[test]
    fn a_public_name_is_fetched_from_the_address_it_resolved_to() {
        let net = Scripted::new(vec![vec![PUBLIC]], vec![OK]);
        let mut c = Conversation::new(&net, tls(), addr::Policy::default());
        let f = c.fetch("http://example.com/").unwrap();
        assert_eq!(f.response.body, b"ok");
        assert_eq!(*net.connected.lock().unwrap(), vec![SocketAddr::new(PUBLIC.parse().unwrap(), 80)]);
    }

    #[test]
    fn every_non_public_literal_is_refused_before_any_lookup_or_connect() {
        for url in [
            "http://127.0.0.1/", "http://127.1.2.3/", "http://0.0.0.0/", "http://10.0.0.5/", "http://172.16.0.1/",
            "http://172.31.255.255/", "http://192.168.1.1/", "http://169.254.169.254/latest/meta-data/",
            "http://169.254.1.1/", "http://100.64.0.1/", "http://224.0.0.1/", "http://255.255.255.255/",
            "http://192.0.2.1/", "http://198.51.100.1/", "http://203.0.113.1/", "http://198.18.0.1/", "http://240.0.0.1/",
            // the URL parser's normalisation of obfuscated IPv4 spellings
            "http://2130706433/", "http://0x7f.0.0.1/", "http://0177.0.0.1/", "http://127.1/", "http://3232235777/",
            "http://[::1]/", "http://[::]/", "http://[fe80::1]/", "http://[fd00::1]/", "http://[fc00::1]/",
            "http://[fd00:ec2::254]/", "http://[ff02::1]/", "http://[2001:db8::1]/",
            "http://[::ffff:127.0.0.1]/", "http://[::ffff:7f00:1]/", "http://[::ffff:10.0.0.1]/",
            "http://[::ffff:169.254.169.254]/", "http://[64:ff9b::7f00:1]/", "http://[2002:c0a8:101::1]/",
            "https://127.0.0.1/", "https://[::1]/",
        ] {
            let net = Scripted::new(vec![], vec![]);
            let mut c = Conversation::new(&net, tls(), addr::Policy::default());
            match c.fetch(url) {
                Err(FetchError::NonPublicAddress { .. }) => {}
                other => panic!("{url}: {other:?}"),
            }
            assert!(net.resolved.lock().unwrap().is_empty(), "{url} reached the resolver");
            assert!(net.connected.lock().unwrap().is_empty(), "{url} reached connect");
        }
    }

    #[test]
    fn a_name_that_resolves_inward_is_refused_for_every_range_and_family() {
        for ip in ["127.0.0.1", "10.1.2.3", "172.20.0.1", "192.168.0.10", "169.254.169.254", "169.254.7.7",
                   "100.100.100.100", "0.0.0.0", "239.1.1.1", "255.255.255.255",
                   "::1", "fe80::1", "fd12:3456::1", "ff05::1", "::ffff:192.168.1.1", "::ffff:127.0.0.1",
                   "64:ff9b::a00:1", "2002:a00:1::"] {
            let net = Scripted::new(vec![vec![ip]], vec![]);
            let mut c = Conversation::new(&net, tls(), addr::Policy::default());
            match c.fetch("http://intranet.example/") {
                Err(FetchError::NonPublicAddress { addr, .. }) => assert_eq!(addr, ip.parse::<IpAddr>().unwrap()),
                other => panic!("{ip}: {other:?}"),
            }
            assert!(net.connected.lock().unwrap().is_empty(), "{ip} reached connect");
        }
    }

    #[test]
    fn a_mixed_answer_is_refused_whole() {
        // A rebinding attacker's race: one public and one private answer.
        let net = Scripted::new(vec![vec![PUBLIC, "127.0.0.1"]], vec![]);
        let mut c = Conversation::new(&net, tls(), addr::Policy::default());
        assert!(matches!(c.fetch("http://evil.example/"), Err(FetchError::NonPublicAddress { .. })));
        assert!(net.connected.lock().unwrap().is_empty());
    }

    #[test]
    fn a_redirect_to_a_private_literal_is_refused_at_the_hop() {
        let net = Scripted::new(vec![vec![PUBLIC]],
            vec!["HTTP/1.1 302 Found\r\nLocation: http://192.168.1.1/admin\r\nContent-Length: 0\r\n\r\n"]);
        let mut c = Conversation::new(&net, tls(), addr::Policy::default());
        match c.fetch("http://example.com/") {
            Err(FetchError::NonPublicAddress { addr, .. }) => assert_eq!(addr, "192.168.1.1".parse::<IpAddr>().unwrap()),
            other => panic!("{other:?}"),
        }
        assert_eq!(net.connected.lock().unwrap().len(), 1, "only the public hop connected");
    }

    #[test]
    fn a_site_list_refuses_other_hosts_and_redirects_before_any_lookup() {
        let sites = || Some(::nuntius::Sites::parse(&["example.com", "*.cdn.example.net"]).unwrap());
        // A host that is not listed: refused before resolve or connect.
        let net = Scripted::new(vec![], vec![]);
        let mut c = Conversation::new(&net, tls(), addr::Policy::default()).with_sites(sites());
        assert!(matches!(c.fetch("http://elsewhere.example/"), Err(FetchError::Refused(_))));
        assert!(matches!(c.open_stream("http://elsewhere.example/", false, None, None, None, Duration::from_secs(5)), Err(FetchError::Refused(_))));
        assert!(net.resolved.lock().unwrap().is_empty() && net.connected.lock().unwrap().is_empty());
        // A listed host redirecting off the list: refused at the hop, in both
        // the whole-body path and the stream path.
        let redirect = "HTTP/1.1 302 Found\r\nLocation: http://tracker.example/x\r\nContent-Length: 0\r\n\r\n";
        let net = Scripted::new(vec![vec![PUBLIC]], vec![redirect, redirect]);
        let mut c = Conversation::new(&net, tls(), addr::Policy::default()).with_sites(sites());
        assert!(matches!(c.fetch("http://example.com/"), Err(FetchError::Refused(m)) if m.contains("tracker.example")));
        assert!(matches!(c.open_stream("http://example.com/", false, None, None, None, Duration::from_secs(5)), Err(FetchError::Refused(_))));
        assert_eq!(net.resolved.lock().unwrap().len(), 1, "only the listed name was looked up");
        // A listed redirect target is followed; a sibling keeps the list.
        let net = Scripted::new(vec![vec![PUBLIC], vec![PUBLIC]],
            vec!["HTTP/1.1 302 Found\r\nLocation: http://a.cdn.example.net/v\r\nContent-Length: 0\r\n\r\n",
                 "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"]);
        let mut c = Conversation::new(&net, tls(), addr::Policy::default()).with_sites(sites());
        assert_eq!(c.fetch("http://example.com/").unwrap().response.body, b"ok");
        assert!(matches!(c.sibling().fetch("http://elsewhere.example/"), Err(FetchError::Refused(_))));
    }

    #[test]
    fn a_redirect_to_a_name_that_resolves_privately_is_refused() {
        let net = Scripted::new(vec![vec![PUBLIC], vec!["10.0.0.5"]],
            vec!["HTTP/1.1 301 Moved\r\nLocation: http://router.example/\r\nContent-Length: 0\r\n\r\n"]);
        let mut c = Conversation::new(&net, tls(), addr::Policy::default());
        assert!(matches!(c.fetch("http://example.com/"), Err(FetchError::NonPublicAddress { .. })));
        assert_eq!(net.connected.lock().unwrap().len(), 1);
    }

    #[test]
    fn rebinding_on_the_same_port_never_reaches_the_resolver_again() {
        // The second lookup WOULD answer loopback; it is never made.
        let net = Scripted::new(vec![vec![PUBLIC], vec!["127.0.0.1"]], vec![OK, OK]);
        let mut c = Conversation::new(&net, tls(), addr::Policy::default());
        c.fetch("http://evil.example/a").unwrap();
        c.fetch("http://evil.example/b").unwrap();
        assert_eq!(c.resolutions(), 1);
        let want = SocketAddr::new(PUBLIC.parse().unwrap(), 80);
        assert_eq!(*net.connected.lock().unwrap(), vec![want, want]);
    }

    #[test]
    fn rebinding_on_the_other_port_is_refused_by_range_and_by_pin() {
        // Default policy: the inward answer is refused by its range.
        let net = Scripted::new(vec![vec![PUBLIC], vec!["127.0.0.1"]], vec![]);
        let mut c = Conversation::new(&net, tls(), addr::Policy::default());
        c.resolve_checked("evil.example", 80).unwrap();
        assert!(matches!(c.resolve_checked("evil.example", 443), Err(FetchError::NonPublicAddress { .. })));
        // ★ Under the grant the range check admits loopback — and the PIN
        // still refuses it. That is the case the pin exists for.
        let net = Scripted::new(vec![vec![PUBLIC], vec!["127.0.0.1"]], vec![]);
        let mut c = Conversation::new(&net, tls(), addr::Policy { grant_private_network: true });
        c.resolve_checked("evil.example", 80).unwrap();
        assert!(matches!(c.resolve_checked("evil.example", 443), Err(FetchError::Rebinding { .. })));
        // A different PUBLIC answer is refused too: the first answer is kept.
        let net = Scripted::new(vec![vec![PUBLIC], vec!["8.8.8.8"]], vec![]);
        let mut c = Conversation::new(&net, tls(), addr::Policy::default());
        c.resolve_checked("Evil.Example", 80).unwrap();
        assert!(matches!(c.resolve_checked("evil.example", 443), Err(FetchError::Rebinding { .. })));
        // An overlapping answer keeps only the pinned addresses.
        let net = Scripted::new(vec![vec![PUBLIC], vec!["8.8.8.8", PUBLIC]], vec![]);
        let mut c = Conversation::new(&net, tls(), addr::Policy::default());
        c.resolve_checked("evil.example", 80).unwrap();
        assert_eq!(c.resolve_checked("evil.example", 443).unwrap(), vec![SocketAddr::new(PUBLIC.parse().unwrap(), 443)]);
    }

    #[test]
    fn the_grant_admits_the_lan_but_never_metadata() {
        let net = Scripted::new(vec![vec!["192.168.1.1"]], vec![OK]);
        let mut c = Conversation::new(&net, tls(), addr::Policy { grant_private_network: true });
        assert_eq!(c.fetch("http://router.lan/").unwrap().response.body, b"ok");
        let net = Scripted::new(vec![], vec![]);
        let mut c = Conversation::new(&net, tls(), addr::Policy { grant_private_network: true });
        assert!(matches!(c.fetch("http://169.254.169.254/"), Err(FetchError::NonPublicAddress { range: addr::Range::Metadata, .. })));
        let net = Scripted::new(vec![vec!["169.254.169.254"]], vec![]);
        let mut c = Conversation::new(&net, tls(), addr::Policy { grant_private_network: true });
        assert!(matches!(c.fetch("http://metadata.google.internal/"), Err(FetchError::NonPublicAddress { range: addr::Range::Metadata, .. })));
    }

    #[test]
    fn the_refusal_names_the_host_the_address_and_the_range() {
        let net = Scripted::new(vec![vec!["10.0.0.5"]], vec![]);
        let mut c = Conversation::new(&net, tls(), addr::Policy::default());
        let e = c.fetch("http://router.example/").unwrap_err().to_string();
        assert!(e.contains("router.example") && e.contains("10.0.0.5") && e.contains("RFC 1918"), "{e}");
    }
}
