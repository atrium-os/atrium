//! Encrypted media through Hospes (docs/spec/hospes.md §8): the supervisor's side.
//!
//! The supervisor starts `hospes-cdm` beside each worker with two sockets: the control
//! channel (ours: sessions, licence messages) and the data channel (the worker's: samples to
//! decrypt). The worker reports the media's init data; we open a session with it, answer the
//! CDM's licence request, and tell the worker when new keys are usable.
//!
//! In a browser the page's EME code fetches the licence. The standalone player has no page:
//! [`Licences`] stands in for it: Clear Key with keys the user supplied
//! (`ATRIUM_PLAYER_CLEARKEY`), or a licence server (`ATRIUM_PLAYER_LICENCE_URL`) that each
//! request is POSTed to through the system fetcher (Nuntius, hospes.md §8.5): neither the
//! CDM nor the supervisor ever holds a socket to the network.
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use hospes_proto::io::{recv, send};
use hospes_proto::Msg;

use crate::sys;

/// Most sessions one playback opens (one per distinct init data).
const SESSIONS_MAX: usize = 16;

/// Where `hospes-cdm` and the CDM library are, and where licences come from.
#[derive(Clone, Debug)]
pub struct CdmOptions {
    pub host: PathBuf,
    pub library: PathBuf,
    /// The origin's CDM store (§8.4). `None`: no persistent state.
    pub store: Option<PathBuf>,
    pub licences: Licences,
    pub mem_mib: u64,
}

/// How licence requests are answered.
#[derive(Clone, Debug)]
pub enum Licences {
    /// Clear Key: these (key id, key) pairs.
    ClearKey(Vec<([u8; 16], [u8; 16])>),
    /// POST each licence (and renewal) request, as is, to `url`; the answer goes back to the
    /// CDM as is. For Widevine that is the service's licence proxy.
    Server { url: String, fetcher: FetcherSource },
}

/// Where a licence request's fetcher comes from: the system's (portcullisd `OpenFetcher`),
/// or a stand-in in tests.
#[derive(Clone)]
pub struct FetcherSource(Arc<dyn Fn() -> io::Result<nuntius::launch::Fetcher> + Send + Sync>);

impl FetcherSource {
    pub fn system() -> FetcherSource {
        FetcherSource(Arc::new(nuntius::launch::open))
    }
    pub fn new(f: impl Fn() -> io::Result<nuntius::launch::Fetcher> + Send + Sync + 'static) -> FetcherSource {
        FetcherSource(Arc::new(f))
    }
}

impl std::fmt::Debug for FetcherSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FetcherSource")
    }
}

/// Largest licence answer taken from a server.
pub const LICENCE_MAX: u64 = 1 << 20;

/// ★ POST `request` to `url` over `client` and return the answer's body: a 2xx with at most
/// [`LICENCE_MAX`] bytes, within 30 s, or why not.
pub fn post_licence(mut client: nuntius::Client, url: &str, request: Vec<u8>) -> Result<Vec<u8>, String> {
    use nuntius::{Event, Limits, Open};
    let mut o = Open::post(1, url, "application/octet-stream", request);
    o.limits = Limits { max_bytes: LICENCE_MAX, connect_ms: 10_000, idle_ms: 20_000, total_ms: 30_000 };
    client.open(o).map_err(|_| "the fetcher's channel".to_string())?;
    client.budget(LICENCE_MAX).map_err(|_| "the fetcher's channel".to_string())?;
    let mut body = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(40);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        let ev = match client.recv_timeout(left) {
            Ok(Some(ev)) => ev,
            Ok(None) => return Err("the licence server did not answer in time".into()),
            Err(_) => return Err("the fetcher stopped".into()),
        };
        match ev {
            Event::Head { status, .. } if !(200..300).contains(&status) => return Err(format!("licence server: HTTP {status}")),
            Event::Data { bytes, .. } => {
                if body.len() as u64 + bytes.len() as u64 > LICENCE_MAX {
                    return Err("licence answer too big".into());
                }
                body.extend_from_slice(&bytes);
            }
            Event::Done { .. } => return Ok(body),
            Event::Error { code, .. } => return Err(format!("licence request: {code:?}")),
            Event::Head { .. } | Event::Fd { .. } | Event::Progress { .. } | Event::Retrying { .. } => {}
        }
    }
}

impl CdmOptions {
    /// From `ATRIUM_PLAYER_CDM` (the library: the test CDM until Widevine),
    /// `ATRIUM_PLAYER_HOSPES_CDM` (default: `hospes-cdm` next to this executable),
    /// `ATRIUM_PLAYER_CLEARKEY` (`KID:KEY[,KID:KEY...]` in hex) or `ATRIUM_PLAYER_LICENCE_URL`
    /// (a licence server, which wins), and optionally
    /// `ATRIUM_PLAYER_CDM_STORE` (a directory).
    pub fn from_env() -> Option<CdmOptions> {
        let library: PathBuf = std::env::var_os("ATRIUM_PLAYER_CDM")?.into();
        if !library.is_file() {
            return None;
        }
        let licences = match std::env::var("ATRIUM_PLAYER_LICENCE_URL") {
            Ok(url) if url.starts_with("https://") || url.starts_with("http://") => Licences::Server { url, fetcher: FetcherSource::system() },
            _ => Licences::ClearKey(parse_keys(&std::env::var("ATRIUM_PLAYER_CLEARKEY").unwrap_or_default())?),
        };
        let host = std::env::var_os("ATRIUM_PLAYER_HOSPES_CDM")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_exe().unwrap_or_default().with_file_name("hospes-cdm"));
        let store = std::env::var_os("ATRIUM_PLAYER_CDM_STORE").map(PathBuf::from);
        Some(CdmOptions { host, library, store, licences, mem_mib: 512 })
    }
}

/// `KID:KEY,KID:KEY`, 32 hex digits each.
pub fn parse_keys(s: &str) -> Option<Vec<([u8; 16], [u8; 16])>> {
    let hex16 = |h: &str| -> Option<[u8; 16]> {
        let h = h.trim();
        if h.len() != 32 {
            return None;
        }
        let mut o = [0u8; 16];
        for (i, b) in o.iter_mut().enumerate() {
            *b = u8::from_str_radix(h.get(2 * i..2 * i + 2)?, 16).ok()?;
        }
        Some(o)
    };
    let v: Option<Vec<_>> = s.split(',').filter(|p| !p.trim().is_empty()).map(|p| {
        let (k, v) = p.split_once(':')?;
        Some((hex16(k)?, hex16(v)?))
    }).collect();
    v.filter(|v| !v.is_empty())
}

const B64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn b64url(d: &[u8]) -> String {
    let mut s = String::new();
    for c in d.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..=c.len() {
            s.push(B64URL[(n >> (18 - 6 * i)) as usize & 63] as char);
        }
    }
    s
}

fn unb64url(s: &str) -> Option<Vec<u8>> {
    let mut o = Vec::new();
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes() {
        let v = B64URL.iter().position(|&x| x == c)? as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            o.push((acc >> bits) as u8);
        }
    }
    Some(o)
}

/// The key ids a Clear Key licence request (`{"kids":["...",...],"type":...}`) asks for.
/// The CDM is not trusted to write good JSON: anything odd is `None`.
pub fn request_kids(msg: &[u8]) -> Option<Vec<[u8; 16]>> {
    let s = std::str::from_utf8(msg).ok()?;
    let at = s.find("\"kids\"")? + 6;
    let rest = s[at..].trim_start().strip_prefix(':')?.trim_start().strip_prefix('[')?;
    let list = &rest[..rest.find(']')?];
    let mut out = Vec::new();
    for item in list.split(',') {
        let item = item.trim();
        let id = item.strip_prefix('"')?.strip_suffix('"')?;
        out.push(unb64url(id)?.try_into().ok()?);
        if out.len() > 64 {
            return None;
        }
    }
    Some(out)
}

/// A Clear Key licence with the keys we have for `kids`; `None` if we have none of them.
pub fn clear_key_licence(keys: &[([u8; 16], [u8; 16])], kids: &[[u8; 16]]) -> Option<Vec<u8>> {
    let have: Vec<String> = kids
        .iter()
        .filter_map(|kid| keys.iter().find(|(k, _)| k == kid))
        .map(|(kid, key)| format!("{{\"kty\":\"oct\",\"kid\":\"{}\",\"k\":\"{}\"}}", b64url(kid), b64url(key)))
        .collect();
    if have.is_empty() {
        return None;
    }
    Some(format!("{{\"keys\":[{}],\"type\":\"temporary\"}}", have.join(",")).into_bytes())
}

/// What the supervisor learns from the CDM.
#[derive(Debug, PartialEq)]
pub enum CdmEvent {
    /// New keys are usable: the worker should try its waiting samples again.
    KeysUsable,
    /// The CDM can't play this (no licence, refused): the reason, for the log.
    Failed(String),
    /// The host exited or broke the protocol: treated like a crashed worker.
    Lost,
}

/// One `hospes-cdm` and its sessions.
pub struct CdmProc {
    child: Child,
    ctrl: UnixStream,
    rx: mpsc::Receiver<Option<Msg>>,
    licences: Licences,
    /// Answers from licence servers: (session, answer or why not).
    answers: (mpsc::Sender<(Vec<u8>, Result<Vec<u8>, String>)>, mpsc::Receiver<(Vec<u8>, Result<Vec<u8>, String>)>),
    initialized: bool,
    /// Init data waiting for `Initialized`.
    queued: Vec<(u8, Vec<u8>)>,
    promise: u32,
    sessions: usize,
    /// Promises we are waiting on (create, update).
    outstanding: usize,
    failed: bool,
}

impl CdmProc {
    /// Start the host; returns it and the worker's end of the data channel.
    pub fn spawn(o: &CdmOptions) -> io::Result<(CdmProc, UnixStream)> {
        let (ctrl, ctrl_c) = UnixStream::pair()?;
        let (data_w, data_c) = UnixStream::pair()?;
        let lib = File::open(&o.library)?;
        let store = o.store.as_ref().map(File::open).transpose()?;
        let mut c = Command::new(&o.host);
        let mut fds = vec![("--ctrl", ctrl_c.as_raw_fd()), ("--data", data_c.as_raw_fd()), ("--lib", lib.as_raw_fd())];
        if let Some(s) = &store {
            fds.push(("--store", s.as_raw_fd()));
        }
        for (flag, fd) in fds {
            c.arg(flag).arg(fd.to_string());
            sys::pass_fd(&mut c, fd);
        }
        c.arg("--mem").arg(o.mem_mib.to_string());
        c.stdin(std::process::Stdio::null());
        let child = c.spawn()?;
        drop((ctrl_c, data_c, lib, store));
        let mut r = ctrl.try_clone()?;
        let (tx, rx) = mpsc::channel();
        // The control channel is read on its own thread so a slow CDM never stalls frames.
        std::thread::Builder::new().name("hospes-cdm-ctrl".into()).spawn(move || loop {
            match recv(&mut r) {
                Ok(Some(m)) => {
                    if tx.send(Some(m)).is_err() {
                        return;
                    }
                }
                _ => {
                    let _ = tx.send(None);
                    return;
                }
            }
        })?;
        let mut p = CdmProc { child, ctrl, rx, licences: o.licences.clone(), answers: mpsc::channel(), initialized: false, queued: Vec::new(), promise: 0, sessions: 0, outstanding: 0, failed: false };
        p.send(Msg::Init { allow_persistent: false, link_mask: 0 });
        Ok((p, data_w))
    }

    fn send(&mut self, m: Msg) {
        if send(&mut self.ctrl, &m).is_err() {
            self.failed = true;
        }
    }

    fn next_promise(&mut self) -> u32 {
        self.promise += 1;
        self.promise
    }

    /// The worker found init data (`kind`: the CDM's `InitDataType`).
    pub fn init_data(&mut self, kind: u8, data: Vec<u8>) {
        if !self.initialized {
            self.queued.push((kind, data));
            return;
        }
        if self.sessions >= SESSIONS_MAX {
            return;
        }
        self.sessions += 1;
        self.outstanding += 1;
        let promise = self.next_promise();
        self.send(Msg::CreateSession { promise, session_type: 0, init_type: kind as u32, init: data });
    }

    /// A licence exchange is under way: the caller should look again soon.
    pub fn busy(&self) -> bool {
        !self.failed && (!self.initialized || self.outstanding > 0)
    }

    /// Handle what the CDM said since last time.
    pub fn poll(&mut self) -> Vec<CdmEvent> {
        let mut out = Vec::new();
        while let Ok(m) = self.rx.try_recv() {
            let Some(m) = m else {
                self.failed = true;
                out.push(CdmEvent::Lost);
                break;
            };
            match m {
                Msg::NewSession { .. } => self.outstanding = self.outstanding.saturating_sub(1),
                Msg::Ready { .. } | Msg::Expiration { .. } | Msg::Closed { .. } | Msg::KeyStatus { .. } => {}
                Msg::Initialized { ok: true } => {
                    self.initialized = true;
                    for (k, d) in std::mem::take(&mut self.queued) {
                        self.init_data(k, d);
                    }
                }
                Msg::Initialized { ok: false } => out.push(CdmEvent::Failed("CDM initialisation failed".into())),
                Msg::Resolved { .. } => self.outstanding = self.outstanding.saturating_sub(1),
                Msg::Rejected { message, .. } => {
                    self.outstanding = self.outstanding.saturating_sub(1);
                    out.push(CdmEvent::Failed(format!("CDM refused: {}", String::from_utf8_lossy(&message))));
                }
                // A licence request (type 0) or renewal (1). Clear Key's temporary sessions
                // never renew; a server gets both.
                Msg::SessionMessage { id, message_type: t @ (0 | 1), message } => match &self.licences {
                    Licences::ClearKey(keys) if t == 0 => match request_kids(&message).and_then(|kids| clear_key_licence(keys, &kids)) {
                        Some(response) => {
                            let promise = self.next_promise();
                            self.outstanding += 1;
                            self.send(Msg::UpdateSession { promise, id, response });
                        }
                        None => out.push(CdmEvent::Failed("no key for this media".into())),
                    },
                    Licences::ClearKey(_) => {}
                    Licences::Server { url, fetcher } => {
                        // Off this thread: a slow server never stalls frames.
                        self.outstanding += 1;
                        let (url, fetcher, tx) = (url.clone(), fetcher.clone(), self.answers.0.clone());
                        let _ = std::thread::Builder::new().name("player-licence".into()).spawn(move || {
                            let r = (fetcher.0)().map_err(|e| format!("no fetcher: {e}")).and_then(|f| {
                                let (client, _guard) = f.into_parts();
                                post_licence(client, &url, message)
                            });
                            let _ = tx.send((id, r));
                        });
                    }
                },
                // Individualisation (3): the provisioning flow (hospes.md §8.5) is not wired
                // yet; say so rather than stall.
                Msg::SessionMessage { message_type: 3, .. } => {
                    out.push(CdmEvent::Failed("the CDM asked for individualisation, which is not supported yet".into()));
                }
                Msg::SessionMessage { .. } => {}
                Msg::KeysChange { has_new_usable, .. } => {
                    if has_new_usable {
                        out.push(CdmEvent::KeysUsable);
                    }
                }
                Msg::Fail { what, .. } => {
                    eprintln!("atrium-player: hospes-cdm: {}", String::from_utf8_lossy(&what));
                    out.push(CdmEvent::Lost);
                }
                // Data-channel messages never come this way.
                _ => out.push(CdmEvent::Lost),
            }
        }
        while let Ok((id, r)) = self.answers.1.try_recv() {
            self.outstanding = self.outstanding.saturating_sub(1);
            match r {
                Ok(response) => {
                    let promise = self.next_promise();
                    self.outstanding += 1;
                    self.send(Msg::UpdateSession { promise, id, response });
                }
                Err(why) => out.push(CdmEvent::Failed(why)),
            }
        }
        if self.failed && !out.contains(&CdmEvent::Lost) {
            out.push(CdmEvent::Lost);
        }
        out
    }

    /// Whether the host has exited, waiting up to `grace` for it: a worker that lost its
    /// data channel reports an error, and that can arrive before the host is reaped.
    pub fn exited(&mut self, grace: std::time::Duration) -> bool {
        let until = std::time::Instant::now() + grace;
        loop {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return true;
            }
            if std::time::Instant::now() >= until {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for CdmProc {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KID: [u8; 16] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];

    #[test]
    fn keys_from_env_syntax() {
        let k = parse_keys("00112233445566778899aabbccddeeff:3a2b1c0d4e5f60718293a4b5c6d7e8f9").unwrap();
        assert_eq!(k[0].0, KID);
        assert_eq!(k[0].1[0], 0x3a);
        assert!(parse_keys("").is_none());
        assert!(parse_keys("0011:22").is_none());
        assert!(parse_keys("00112233445566778899aabbccddeeff:3a2b1c0d4e5f60718293a4b5c6d7e8fg").is_none());
    }

    #[test]
    fn licence_for_request() {
        // The test CDM's request (hospes/test-cdm/vectors/transcript.txt).
        let req = br#"{"kids":["ABEiM0RVZneImaq7zN3u_w","oKGio6SlpqeoqaqrrK2urw"],"type":"temporary","hospes":{"storage_id_len":32}}"#;
        let kids = request_kids(req).unwrap();
        assert_eq!(kids.len(), 2);
        assert_eq!(kids[0], KID);
        let keys = parse_keys("00112233445566778899aabbccddeeff:3a2b1c0d4e5f60718293a4b5c6d7e8f9").unwrap();
        let lic = String::from_utf8(clear_key_licence(&keys, &kids).unwrap()).unwrap();
        // The same encoding as the reference licence (vectors/licence.json).
        assert_eq!(lic, r#"{"keys":[{"kty":"oct","kid":"ABEiM0RVZneImaq7zN3u_w","k":"OiscDU5fYHGCk6S1xtfo-Q"}],"type":"temporary"}"#);
        assert!(clear_key_licence(&keys, &kids[1..]).is_none());
        for bad in [&b"{}"[..], b"{\"kids\":[\"!!\"]}", b"{\"kids\":[\"AAAA\"]}", b"{\"kids\":\"x\"}", b"\xff"] {
            assert!(request_kids(bad).is_none(), "{bad:?}");
        }
    }
}
