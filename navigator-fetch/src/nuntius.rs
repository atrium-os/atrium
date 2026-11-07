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

// ★ The interface is the `nuntius` crate's, so a client moving from this
// in-process instance to the channel (`nuntiusd`) keeps its types.
pub use nuntius::{Deliver, ErrorCode, Event, Limits, Method, Open, Request, Retry, Validator,
                  ATTEMPTS_MAX, BACKOFF_MS_MAX, CONNECT_MS_MAX, DATA_MAX, FD_MAX_BYTES, IDLE_MS_MAX, MAX_REQUESTS, STREAM_TOTAL_MS_MAX};

/// A response's validator: an `ETag` (preferred) or a `Last-Modified`.
fn validator_of(h: &crate::http::Head) -> Option<Validator> {
    // A weak ETag cannot validate a range (RFC 9110 §13.1.5).
    if let Some(e) = h.header("etag").filter(|e| !e.starts_with("W/")) { return Some(Validator::ETag(e.to_string())) }
    h.header("last-modified").map(|v| Validator::LastModified(v.to_string()))
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

    /// Cancel every request in flight (the client has gone).
    pub fn cancel_all(&self) {
        let ids: Vec<u32> = self.inner.live.lock().unwrap().keys().copied().collect();
        for id in ids { self.request(Request::Cancel { id }) }
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

/// Byte credit an instance on a channel starts with, before the client's
/// first `Budget`.
pub const CHANNEL_CREDIT: u64 = 1 << 20;

/// ★ SERVE ONE CLIENT ON ITS CHANNEL (nuntius.md §6 step 2): `nuntiusd`'s
/// loop, and the same in-process for tests. Requests from `s` go to one
/// instance over `conv`; its events go back in order, `Fd` with its
/// descriptor. Returns when the client hangs up (every request is then
/// cancelled), or with `InvalidData` when it sends something out of shape —
/// the channel ends rather than guess.
pub fn serve_channel(s: std::os::unix::net::UnixStream, conv: Conversation<'static>, credit: u64) -> std::io::Result<()> {
    let (mut reqs, mut out) = nuntius::server::channel(s)?;
    let (tx, rx) = std::sync::mpsc::channel();
    let n = Nuntius::new(conv, tx, credit);
    std::thread::spawn(move || { for e in rx { if out.send(e).is_err() { break } } });
    let r = loop {
        match reqs.recv() {
            Ok(Some(r)) => n.request(r),
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        }
    };
    n.cancel_all();
    r
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
    // Where the request is complete, in bytes from `first`: the range's
    // length, else (once `Head` gives it) the rest of the resource.
    let mut end = want_len;
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
        let if_range = if ranged { validator.as_ref().map(|v| v.value().to_string()) } else { None };
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
            let v_now = validator_of(&s.head);
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
                // ★ A whole resource of known length is complete when that
                // length is in hand, as a range is: with the credit spent
                // exactly, waiting for credit before reading the server's
                // end of body waited until the deadline.
                if end.is_none() { end = length.map(|t| t.saturating_sub(first)) }
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
                if end.is_some_and(|l| got >= l) {
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
                if let Some(l) = end { max = max.min(l.saturating_sub(got) as usize).max(1) }
                let piece = match s.read(max) {
                    Ok(Some(p)) => p,
                    Ok(None) => {
                        // The body's end: complete only if the length is.
                        if end.is_some_and(|l| got < l) { break 'attempt Fail::Retry("short", ErrorCode::Connect) }
                        send(Event::Progress { id, received: got });
                        return Event::Done { id, bytes: got };
                    }
                    Err(e) => { if cancelled() { return Event::Error { id, code: ErrorCode::Cancelled } } break 'attempt classify(&e) }
                };
                read_total += piece.len() as u64;
                if read_total > o.limits.max_bytes.saturating_add(first) { return Event::Error { id, code: ErrorCode::TooBig } }
                let mut p = &piece[..];
                if skip > 0 { let n = (skip as usize).min(p.len()); skip -= n as u64; p = &p[n..] }
                if let Some(l) = end { p = &p[..p.len().min(l.saturating_sub(got) as usize)] }
                if p.is_empty() { continue }
                if got + p.len() as u64 > o.limits.max_bytes { return Event::Error { id, code: ErrorCode::TooBig } }
                match (&mut file, deliver) {
                    (Some(f), Deliver::Fd) => {
                        // ★ At an explicit offset: on FreeBSD the client's
                        // read-only descriptor is a dup, sharing the file
                        // offset with this one, so a plain write would land
                        // wherever the client's last read left it.
                        use std::os::unix::fs::FileExt;
                        if f.write_all_at(p, got).is_err() { return Event::Error { id, code: ErrorCode::TooBig } }
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
/// client (nuntius.md §3.3): `memfd_create` (an `SHM_ANON` object that grows on write) with the descriptor
/// narrowed to read, seek and fstat on FreeBSD (allowed in capability
/// mode); `memfd_create` and a read-only reopen on Linux; elsewhere (the
/// macOS dev host) an unlinked temporary file.
pub fn anon_file() -> std::io::Result<(std::fs::File, std::os::fd::OwnedFd)> {
    #[allow(unused_imports)]
    use std::os::fd::{FromRawFd, OwnedFd};
    #[cfg(target_os = "freebsd")]
    unsafe {
        // memfd_create is shm_open2(SHM_ANON) with SHM_GROW_ON_WRITE: a
        // plain SHM_ANON object refuses writes past its size, so filling it
        // with write(2) failed at the first byte. Allowed in capability mode.
        let fd = libc::memfd_create(c"nuntius".as_ptr(), libc::MFD_CLOEXEC);
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
        // ★ A counter, not the clock alone: two threads asking within the
        // clock's resolution got the same name, and `create_new` failed
        // (seen as an intermittent test failure). A name taken by anything
        // else is passed over.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let mut tries = 0;
        let (w, p) = loop {
            let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let p = std::env::temp_dir().join(format!("nuntius-{}-{n}-{seq}", std::process::id()));
            match std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&p) {
                Ok(w) => break (w, p),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && tries < 16 => tries += 1,
                Err(e) => return Err(e),
            }
        };
        let ro = std::fs::File::open(&p)?;
        let _ = std::fs::remove_file(&p);
        Ok((w, OwnedFd::from(ro)))
    }
}
