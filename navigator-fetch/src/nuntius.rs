//! ★ NUNTIUS v1 (nuntius.md §3, build step 1; with step 3's `Fd`): the
//! fetcher's request/event interface, inside `navigator-fetch` until it is
//! split into the `nuntius` crate. One instance, many concurrent requests,
//! each with the client's id; events on one channel, in order per request.
//!
//! Everything goes through the instance's [`Conversation`] — so the address
//! rule between resolve and connect, the pins, the redirect cap and the
//! refusal of downgrades are exactly `fetch`'s — and adds:
//! - [`Deliver::Stream`]: `Data` events of at most [`DATA_MAX`] bytes, under
//!   FLOW CONTROL: a request reads the socket only while the instance has
//!   byte credit, which the client grants with `Budget`;
//! - [`Deliver::Fd`]: a progressively filled anonymous file, its read-only
//!   descriptor sent with `Fd` as soon as `Head` is known (a resource of
//!   unknown length is streamed instead);
//! - timeouts: `connect_ms` (DNS, connect, TLS and the server's first
//!   answer), `idle_ms` (no byte for that long, mid-body too), `total_ms`;
//! - retries on a timeout, a reset, a short body, a 5xx, 408 or 429, with a
//!   backoff that doubles (and a little jitter), each announced with
//!   `Retrying`; a retry RESUMES with `Range` from the last byte received and
//!   `If-Range` with the validator from `Head`, and is refused (`Malformed`)
//!   rather than spliced if the resource changed: a 200, or another
//!   validator. Policy, DNS, TLS and other 4xx errors are never retried;
//! - `Cancel`: the socket is shut down at once, freeing the connection.

use crate::{Conversation, FetchError};
use std::collections::HashMap;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Most bytes in one `Data` event.
pub const DATA_MAX: usize = 60 * 1024;
/// Caps per instance (§3.5).
pub const MAX_REQUESTS: usize = 64;
pub const FD_MAX_BYTES: u64 = 1 << 30;
pub const STREAM_TOTAL_MS_MAX: u64 = 10 * 60 * 1000;
pub const CONNECT_MS_MAX: u64 = 30_000;
pub const IDLE_MS_MAX: u64 = 60_000;
pub const ATTEMPTS_MAX: u32 = 8;
pub const BACKOFF_MS_MAX: u64 = 30_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method { Get, Head }

/// A resource's validator, from `Head`: an `ETag` (preferred) or a
/// `Last-Modified`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Validator { ETag(String), LastModified(String) }

impl Validator {
    fn header(&self) -> &str { match self { Validator::ETag(v) | Validator::LastModified(v) => v } }
    fn of(h: &crate::http::Head) -> Option<Validator> {
        // A weak ETag cannot validate a range (RFC 9110 §13.1.5).
        if let Some(e) = h.header("etag").filter(|e| !e.starts_with("W/")) { return Some(Validator::ETag(e.to_string())) }
        h.header("last-modified").map(|v| Validator::LastModified(v.to_string()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deliver { Stream, Fd }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits { pub max_bytes: u64, pub connect_ms: u64, pub idle_ms: u64, pub total_ms: u64 }
impl Default for Limits { fn default() -> Self { Limits { max_bytes: u64::MAX, connect_ms: 10_000, idle_ms: 15_000, total_ms: 60_000 } } }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retry { pub max_attempts: u32, pub backoff_ms: u64 }
impl Default for Retry { fn default() -> Self { Retry { max_attempts: 4, backoff_ms: 250 } } }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Open {
    pub id: u32,
    pub url: String,
    pub method: Method,
    /// `(first byte, length)`; `None` for the whole resource.
    pub range: Option<(u64, Option<u64>)>,
    pub if_range: Option<Validator>,
    pub deliver: Deliver,
    pub limits: Limits,
    pub retry: Retry,
    pub priority: u8,
    pub accept: Option<String>,
}

impl Open {
    pub fn get(id: u32, url: &str) -> Open {
        Open { id, url: url.to_string(), method: Method::Get, range: None, if_range: None, deliver: Deliver::Stream,
               limits: Limits::default(), retry: Retry::default(), priority: 1, accept: None }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request { Open(Open), Cancel { id: u32 }, Budget { bytes: u64 } }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode { Policy, Dns, Connect, Tls, Http(u16), Timeout, TooBig, Cancelled, Malformed }

#[derive(Debug)]
pub enum Event {
    Head { id: u32, status: u16, final_url: String, redirects: usize, content_type: Option<String>, length: Option<u64>,
           accept_ranges: bool, validator: Option<Validator> },
    /// `offset` is absolute in the resource, so a resumed request continues
    /// the same numbering.
    Data { id: u32, offset: u64, bytes: Vec<u8> },
    /// The read-only descriptor of the file being filled.
    Fd { id: u32, fd: std::os::fd::OwnedFd },
    Progress { id: u32, received: u64 },
    Retrying { id: u32, attempt: u32, reason: String },
    Done { id: u32, bytes: u64 },
    Error { id: u32, code: ErrorCode },
}

impl Event {
    pub fn id(&self) -> u32 {
        match self { Event::Head { id, .. } | Event::Data { id, .. } | Event::Fd { id, .. } | Event::Progress { id, .. }
                   | Event::Retrying { id, .. } | Event::Done { id, .. } | Event::Error { id, .. } => *id }
    }
}

struct Ctl { cancelled: AtomicBool, sock: Mutex<Option<TcpStream>> }

struct Inner {
    /// Byte credit for `Stream` delivery, and its condition.
    budget: Mutex<u64>,
    credit: Condvar,
    live: Mutex<HashMap<u32, Arc<Ctl>>>,
}

/// One fetcher instance (one client).
pub struct Nuntius {
    conv: Conversation<'static>,
    inner: Arc<Inner>,
    events: Sender<Event>,
}

impl Nuntius {
    /// An instance over `conv` (its policy and pins), sending events to
    /// `events`, with `budget` bytes of initial credit.
    pub fn new(conv: Conversation<'static>, events: Sender<Event>, budget: u64) -> Nuntius {
        Nuntius { conv, events, inner: Arc::new(Inner { budget: Mutex::new(budget), credit: Condvar::new(), live: Mutex::new(HashMap::new()) }) }
    }

    /// Requests in flight.
    pub fn live(&self) -> usize { self.inner.live.lock().unwrap().len() }
    pub fn budget(&self) -> u64 { *self.inner.budget.lock().unwrap() }

    pub fn request(&self, r: Request) {
        match r {
            Request::Budget { bytes } => {
                let mut b = self.inner.budget.lock().unwrap();
                *b = b.saturating_add(bytes);
                drop(b);
                self.inner.credit.notify_all();
            }
            Request::Cancel { id } => {
                if let Some(c) = self.inner.live.lock().unwrap().get(&id) {
                    c.cancelled.store(true, Ordering::SeqCst);
                    if let Some(s) = c.sock.lock().unwrap().as_ref() { let _ = s.shutdown(std::net::Shutdown::Both); }
                    self.inner.credit.notify_all();
                }
            }
            Request::Open(o) => {
                let mut live = self.inner.live.lock().unwrap();
                if live.len() >= MAX_REQUESTS || live.contains_key(&o.id) {
                    let _ = self.events.send(Event::Error { id: o.id, code: ErrorCode::Malformed });
                    return;
                }
                let ctl = Arc::new(Ctl { cancelled: AtomicBool::new(false), sock: Mutex::new(None) });
                live.insert(o.id, ctl.clone());
                drop(live);
                let (conv, inner, events) = (self.conv.sibling(), self.inner.clone(), self.events.clone());
                std::thread::spawn(move || {
                    let id = o.id;
                    // The slot is free before the client hears the end.
                    let last = run(conv, o, &ctl, &inner, &events);
                    inner.live.lock().unwrap().remove(&id);
                    let _ = events.send(last);
                });
            }
        }
    }
}

/// Clamp what the client asked to the instance's caps.
fn clamp(mut o: Open) -> Open {
    o.limits.connect_ms = o.limits.connect_ms.clamp(1, CONNECT_MS_MAX);
    o.limits.idle_ms = o.limits.idle_ms.clamp(1, IDLE_MS_MAX);
    o.limits.total_ms = o.limits.total_ms.clamp(1, STREAM_TOTAL_MS_MAX);
    if o.deliver == Deliver::Fd { o.limits.max_bytes = o.limits.max_bytes.min(FD_MAX_BYTES) }
    o.retry.max_attempts = o.retry.max_attempts.min(ATTEMPTS_MAX);
    o.retry.backoff_ms = o.retry.backoff_ms.min(BACKOFF_MS_MAX);
    o
}

enum Fail { Retry(&'static str, ErrorCode), Final(ErrorCode) }

fn classify(e: &FetchError) -> Fail {
    use crate::http::HttpError;
    match e {
        FetchError::BadUrl(_) => Fail::Final(ErrorCode::Malformed),
        FetchError::Refused(_) | FetchError::NonPublicAddress { .. } | FetchError::Rebinding { .. } => Fail::Final(ErrorCode::Policy),
        FetchError::Connect(m) if m.starts_with("no address") || m.contains("lookup") || m.contains("resolve") => Fail::Final(ErrorCode::Dns),
        FetchError::Connect(m) if m.contains("timed out") || m.contains("WouldBlock") || m.contains("Resource temporarily") => Fail::Retry("timeout", ErrorCode::Timeout),
        FetchError::Connect(_) => Fail::Retry("connect", ErrorCode::Connect),
        FetchError::Tls(_) => Fail::Final(ErrorCode::Tls),
        FetchError::TooManyRedirects => Fail::Final(ErrorCode::Malformed),
        FetchError::Http(HttpError::Io(m)) if m.contains("timed out") || m.contains("Resource temporarily") || m.contains("would block") => Fail::Retry("timeout", ErrorCode::Timeout),
        FetchError::Http(HttpError::Io(_)) => Fail::Retry("reset", ErrorCode::Connect),
        FetchError::Http(HttpError::Malformed("connection closed inside body")) | FetchError::Http(HttpError::Malformed("connection closed inside headers")) => Fail::Retry("reset", ErrorCode::Connect),
        FetchError::Http(_) => Fail::Final(ErrorCode::Malformed),
    }
}

/// A 206's `Content-Range`: (first, last, total).
fn content_range(h: &crate::http::Head) -> Option<(u64, u64, Option<u64>)> {
    let cr = h.header("content-range")?;
    let spec = cr.strip_prefix("bytes ")?;
    let (span, total) = spec.split_once('/')?;
    let (a, b) = span.split_once('-')?;
    let (a, b) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
    (b >= a).then_some((a, b, total.trim().parse().ok()))
}

fn run(mut conv: Conversation<'static>, o: Open, ctl: &Ctl, inner: &Inner, ev: &Sender<Event>) -> Event {
    let o = clamp(o);
    let id = o.id;
    let send = |e: Event| { let _ = ev.send(e); };
    let start = Instant::now();
    let deadline = start + Duration::from_millis(o.limits.total_ms);
    let first = o.range.map(|r| r.0).unwrap_or(0);
    let want_len = o.range.and_then(|r| r.1);
    // Bytes of the resource delivered (from `first`), all attempts' reads,
    // the validator, whether Head went out, and the file for Fd delivery.
    let (mut got, mut read_total, mut attempt) = (0u64, 0u64, 0u32);
    let mut validator: Option<Validator> = o.if_range.clone();
    let mut head_sent = false;
    let mut file: Option<std::fs::File> = None;
    let mut deliver = o.deliver;
    let mut last_progress = Instant::now() - Duration::from_secs(1);
    let cancelled = || ctl.cancelled.load(Ordering::SeqCst);
    loop {
        if cancelled() { return Event::Error { id, code: ErrorCode::Cancelled } }
        if Instant::now() >= deadline { return Event::Error { id, code: ErrorCode::Timeout } }
        let from = first + got;
        let len = want_len.map(|n| n.saturating_sub(got));
        if want_len.is_some() && len == Some(0) { return Event::Done { id, bytes: got } }
        let ranged = o.range.is_some() || got > 0;
        let range = ranged.then_some((from, len));
        let if_range = if ranged { validator.as_ref().map(|v| v.header().to_string()) } else { None };
        let head_wait = Duration::from_millis(o.limits.connect_ms).min(deadline.saturating_duration_since(Instant::now()));
        let failure: Fail = 'attempt: {
            let mut s = match conv.open_stream(&o.url, o.method == Method::Head, range, if_range.as_deref(), o.accept.as_deref(), head_wait) {
                Ok(s) => s,
                Err(e) => break 'attempt classify(&e),
            };
            *ctl.sock.lock().unwrap() = s.sock.try_clone().ok();
            if cancelled() { let _ = s.sock.shutdown(std::net::Shutdown::Both); return Event::Error { id, code: ErrorCode::Cancelled } }
            let st = s.head.status;
            if matches!(st, 408 | 429 | 500..=599) { break 'attempt Fail::Retry("server", ErrorCode::Http(st)) }
            if !(200..300).contains(&st) { return Event::Error { id, code: ErrorCode::Http(st) } }
            // Where the body starts in the resource, and what is to skip.
            let mut skip = 0u64;
            if st == 206 {
                match content_range(&s.head) {
                    Some((a, ..)) if ranged && a == from => {}
                    _ => { return Event::Error { id, code: ErrorCode::Malformed } }
                }
            } else if ranged {
                // The whole resource to a ranged request: a resume (or an
                // If-Range that failed) means the resource changed — never
                // splice two versions. A first request that asked for a range
                // the server ignored gets its prefix skipped.
                if got > 0 || o.if_range.is_some() { return Event::Error { id, code: ErrorCode::Malformed } }
                skip = from;
            }
            let v_now = Validator::of(&s.head);
            if got > 0 && v_now.is_some() && validator.is_some() && v_now != validator {
                return Event::Error { id, code: ErrorCode::Malformed };
            }
            if !head_sent {
                head_sent = true;
                if validator.is_none() { validator = v_now.clone() }
                let length = match (st, content_range(&s.head)) {
                    (206, Some((_, _, Some(t)))) => Some(t),
                    _ => s.head.header("content-length").and_then(|v| v.parse().ok()),
                };
                send(Event::Head { id, status: st, final_url: s.url.clone(), redirects: s.redirects,
                                   content_type: s.head.header("content-type").map(str::to_string), length,
                                   accept_ranges: st == 206 || s.head.header("accept-ranges").is_some_and(|v| v.contains("bytes")),
                                   validator: v_now });
                if o.method == Method::Head { return Event::Done { id, bytes: 0 } }
                if deliver == Deliver::Fd {
                    if length.is_none() { deliver = Deliver::Stream }
                    else if length.unwrap_or(0) > o.limits.max_bytes { return Event::Error { id, code: ErrorCode::TooBig } }
                    else {
                        match anon_file() {
                            Ok((w, ro)) => { file = Some(w); send(Event::Fd { id, fd: ro }) }
                            Err(_) => deliver = Deliver::Stream,
                        }
                    }
                }
            }
            s.set_idle(Duration::from_millis(o.limits.idle_ms));
            loop {
                if cancelled() { return Event::Error { id, code: ErrorCode::Cancelled } }
                if Instant::now() >= deadline { return Event::Error { id, code: ErrorCode::Timeout } }
                // ★ A ranged request is complete when its length is in hand:
                // never wait for credit (or the server's close) it will not
                // use — with the budget spent exactly, that wait was until
                // the deadline.
                if want_len.is_some_and(|l| got >= l) {
                    send(Event::Progress { id, received: got });
                    return Event::Done { id, bytes: got };
                }
                // Flow control: a Stream request reads only with credit.
                let mut max = DATA_MAX;
                if deliver == Deliver::Stream {
                    let mut b = inner.budget.lock().unwrap();
                    while *b == 0 && !cancelled() {
                        let wait = deadline.saturating_duration_since(Instant::now());
                        if wait.is_zero() { break }
                        b = inner.credit.wait_timeout(b, wait.min(Duration::from_millis(200))).unwrap().0;
                    }
                    if cancelled() { continue }
                    if *b == 0 { continue }
                    max = max.min(*b as usize);
                }
                if let Some(l) = want_len { max = max.min(l.saturating_sub(got) as usize).max(1) }
                let piece = match s.read(max) {
                    Ok(Some(p)) => p,
                    Ok(None) => {
                        // The body's end: complete only if the length is.
                        if want_len.is_some_and(|l| got < l) { break 'attempt Fail::Retry("short", ErrorCode::Connect) }
                        send(Event::Progress { id, received: got });
                        return Event::Done { id, bytes: got };
                    }
                    Err(e) => { if cancelled() { return Event::Error { id, code: ErrorCode::Cancelled } } break 'attempt classify(&e) }
                };
                read_total += piece.len() as u64;
                if read_total > o.limits.max_bytes.saturating_add(first) { return Event::Error { id, code: ErrorCode::TooBig } }
                let mut p = &piece[..];
                if skip > 0 { let n = (skip as usize).min(p.len()); skip -= n as u64; p = &p[n..] }
                if let Some(l) = want_len { p = &p[..p.len().min(l.saturating_sub(got) as usize)] }
                if p.is_empty() { continue }
                if got + p.len() as u64 > o.limits.max_bytes { return Event::Error { id, code: ErrorCode::TooBig } }
                match (&mut file, deliver) {
                    (Some(f), Deliver::Fd) => {
                        use std::io::Write;
                        if f.write_all(p).is_err() { return Event::Error { id, code: ErrorCode::TooBig } }
                    }
                    _ => {
                        let mut b = inner.budget.lock().unwrap();
                        *b = b.saturating_sub(p.len() as u64);
                        drop(b);
                        send(Event::Data { id, offset: first + got, bytes: p.to_vec() });
                    }
                }
                got += p.len() as u64;
                if last_progress.elapsed() >= Duration::from_millis(250) {
                    last_progress = Instant::now();
                    send(Event::Progress { id, received: got });
                }
            }
        };
        match failure {
            Fail::Final(code) => { return Event::Error { id, code } }
            Fail::Retry(why, code) => {
                attempt += 1;
                if attempt >= o.retry.max_attempts.max(1) { return Event::Error { id, code } }
                send(Event::Retrying { id, attempt, reason: why.to_string() });
                // Backoff: doubling, a little jitter from the id and attempt.
                let base = o.retry.backoff_ms.saturating_mul(1 << (attempt - 1).min(10)).min(BACKOFF_MS_MAX);
                let jitter = (id as u64 * 7919 + attempt as u64 * 104_729) % (base / 4 + 1);
                let until = (Instant::now() + Duration::from_millis(base + jitter)).min(deadline);
                while Instant::now() < until {
                    if cancelled() { return Event::Error { id, code: ErrorCode::Cancelled } }
                    std::thread::sleep(Duration::from_millis(10).min(until.saturating_duration_since(Instant::now())));
                }
            }
        }
    }
}

/// ★ An anonymous file to fill, and a READ-ONLY descriptor of it for the
/// client (nuntius.md §3.3): `shm_open(SHM_ANON)` with the descriptor
/// narrowed to read, seek and fstat on FreeBSD (allowed in capability
/// mode); `memfd_create` and a read-only reopen on Linux; elsewhere (the
/// macOS dev host) an unlinked temporary file.
pub fn anon_file() -> std::io::Result<(std::fs::File, std::os::fd::OwnedFd)> {
    #[allow(unused_imports)]
    use std::os::fd::{FromRawFd, OwnedFd};
    #[cfg(target_os = "freebsd")]
    unsafe {
        let fd = libc::shm_open(libc::SHM_ANON, libc::O_RDWR, 0o600);
        if fd < 0 { return Err(std::io::Error::last_os_error()) }
        let ro = libc::dup(fd);
        if ro < 0 { libc::close(fd); return Err(std::io::Error::last_os_error()) }
        let mut rights: libc::cap_rights_t = std::mem::zeroed();
        libc::__cap_rights_init(libc::CAP_RIGHTS_VERSION, &mut rights, libc::CAP_READ, libc::CAP_SEEK, libc::CAP_FSTAT, 0u64);
        if libc::cap_rights_limit(ro, &rights) != 0 { libc::close(fd); libc::close(ro); return Err(std::io::Error::last_os_error()) }
        return Ok((std::fs::File::from_raw_fd(fd), OwnedFd::from_raw_fd(ro)));
    }
    #[cfg(target_os = "linux")]
    unsafe {
        let fd = libc::memfd_create(c"nuntius".as_ptr(), libc::MFD_CLOEXEC);
        if fd < 0 { return Err(std::io::Error::last_os_error()) }
        let path = std::ffi::CString::new(format!("/proc/self/fd/{fd}")).unwrap();
        let ro = libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
        if ro < 0 { libc::close(fd); return Err(std::io::Error::last_os_error()) }
        return Ok((std::fs::File::from_raw_fd(fd), OwnedFd::from_raw_fd(ro)));
    }
    #[allow(unreachable_code)]
    {
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let p = std::env::temp_dir().join(format!("nuntius-{}-{n}", std::process::id()));
        let w = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&p)?;
        let ro = std::fs::File::open(&p)?;
        let _ = std::fs::remove_file(&p);
        Ok((w, OwnedFd::from(ro)))
    }
}
