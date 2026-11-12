//! ★ NUNTIUS (docs/spec/nuntius.md): the system's jailed fetcher, as a
//! client sees it. One instance per client (portcullisd starts it in the
//! fetcher's jail); one Aqueduct channel to it; many concurrent requests,
//! each with an id the client chooses; events in order per request.
//!
//! - [`Request`]: `Open`, `Cancel`, `Budget` (§3.1);
//! - [`Event`]: `Head`, `Data`, `Fd`, `Progress`, `Retrying`, `Done`,
//!   `Error` (§3.2), `Head` always first;
//! - [`wire`]: their encoding on the channel (`CLASS_FETCH`, postcard),
//!   checked on the way in by both ends;
//! - [`Client`]: the client's end; [`server::channel`]: the fetcher's;
//! - [`launch::open`]: a fetcher of the app's own, from portcullisd
//!   (`OpenFetcher`, for an app whose manifest holds `net-fetch`).
//!
//! This crate has no network code, so an app links it without TLS or a
//! resolver. The fetcher (`nuntiusd`, in `navigator-fetch`) uses the same
//! types in-process (`navigator_fetch::nuntius`), so moving a user from the
//! in-process instance to the channel changes the transport, not the
//! interface.

use serde::{Deserialize, Serialize};

pub mod client;
#[cfg(feature = "launch")]
pub mod launch;
pub mod server;
pub mod sites;
pub use sites::Sites;
pub mod wire;

pub use client::{Client, Events, Requests};

/// Most bytes in one `Data` event.
pub const DATA_MAX: usize = 60 * 1024;
/// Caps per instance (§3.5); asked values past them are clamped.
pub const MAX_REQUESTS: usize = 64;
pub const FD_MAX_BYTES: u64 = 1 << 30;
pub const STREAM_TOTAL_MS_MAX: u64 = 10 * 60 * 1000;
pub const CONNECT_MS_MAX: u64 = 30_000;
pub const IDLE_MS_MAX: u64 = 60_000;
pub const ATTEMPTS_MAX: u32 = 8;
pub const BACKOFF_MS_MAX: u64 = 30_000;
/// Longest URL, `Accept` value, header-derived string and reason on the wire.
pub const URL_MAX: usize = 8192;
pub const TEXT_MAX: usize = 512;
/// Largest request body (`Post`): licence and provisioning requests are a few KiB.
pub const BODY_MAX: usize = 32 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Method {
    Get,
    Head,
    /// With [`Open::body`]; never retried, never redirected (spec §3.1).
    Post,
}

/// What a `Post` sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Body {
    pub content_type: String,
    pub bytes: Vec<u8>,
}

/// A resource's validator, from `Head`: an `ETag` (preferred) or a
/// `Last-Modified`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Validator { ETag(String), LastModified(String) }

impl Validator {
    /// The header value (`If-Range` takes either kind as is).
    pub fn value(&self) -> &str { match self { Validator::ETag(v) | Validator::LastModified(v) => v } }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Deliver { Stream, Fd }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits { pub max_bytes: u64, pub connect_ms: u64, pub idle_ms: u64, pub total_ms: u64 }
impl Default for Limits { fn default() -> Self { Limits { max_bytes: u64::MAX, connect_ms: 10_000, idle_ms: 15_000, total_ms: 60_000 } } }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retry { pub max_attempts: u32, pub backoff_ms: u64 }
impl Default for Retry { fn default() -> Self { Retry { max_attempts: 4, backoff_ms: 250 } } }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// 0 is highest: a media playhead before a prefetch.
    pub priority: u8,
    pub accept: Option<String>,
    /// `Post` only: what to send.
    pub body: Option<Body>,
}

impl Open {
    pub fn get(id: u32, url: &str) -> Open {
        Open { id, url: url.to_string(), method: Method::Get, range: None, if_range: None, deliver: Deliver::Stream,
               limits: Limits::default(), retry: Retry::default(), priority: 1, accept: None, body: None }
    }

    /// A `POST` of `bytes` (at most [`BODY_MAX`]), the response streamed back. No retries:
    /// a POST is not idempotent.
    pub fn post(id: u32, url: &str, content_type: &str, bytes: Vec<u8>) -> Open {
        Open { method: Method::Post, retry: Retry { max_attempts: 0, backoff_ms: 0 },
               body: Some(Body { content_type: content_type.to_string(), bytes }), ..Open::get(id, url) }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    Open(Open),
    Cancel { id: u32 },
    /// More byte credit for `Stream` delivery (added to what is left).
    Budget { bytes: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    /// The request's last event.
    pub fn is_last(&self) -> bool { matches!(self, Event::Done { .. } | Event::Error { .. }) }
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
