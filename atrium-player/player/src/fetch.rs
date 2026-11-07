//! Opening a URL in the standalone player (atrium-player.md §6). The
//! player has no network: portcullisd starts a jailed Nuntius of its own
//! (nuntius.md §6.3, `OpenFetcher`) when the player's manifest holds
//! `net-fetch`, running as `_nuntius` in its own jail. A thread here turns
//! its events into [`FromFetcher`] messages: a read-only descriptor of the
//! file being filled, which the player plays as a growing `Load`, then
//! `Done` or `Failed`. Neither the supervisor nor the decode worker ever
//! gets a socket to the network.
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use limen::media_player::{code, ToChild, ToParent};
use serde::{Deserialize, Serialize};

use crate::app::{Host, HostEvent};
use crate::sys;

/// Longest URL the player asks for.
pub const URL_MAX: usize = 8192;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FromFetcher {
    /// The file being filled, as descriptor 0 on this frame (read-only).
    /// `length` when the server said.
    File { length: Option<u64>, content_type: Option<String> },
    /// The download is complete.
    Done { bytes: u64 },
    Failed { reason: String },
}

impl FromFetcher {
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("encodes")
    }
    pub fn decode(b: &[u8]) -> Option<Self> {
        let m: Self = postcard::from_bytes(b).ok()?;
        let ok = match &m {
            FromFetcher::File { content_type, .. } => content_type.as_ref().map_or(true, |c| c.len() <= 256 && !c.chars().any(char::is_control)),
            FromFetcher::Done { .. } => true,
            FromFetcher::Failed { reason } => reason.len() <= 256 && !reason.chars().any(char::is_control),
        };
        ok.then_some(m)
    }
}

/// Whether `s` is something to fetch rather than a path.
pub fn is_url(s: &str) -> bool {
    s.starts_with("https://") || s.starts_with("http://")
}

/// The fetcher, asked for one URL.
pub struct Fetcher {
    /// The guard keeps the fetcher's jail; dropping it ends it.
    _guard: nuntius::launch::FetcherGuard,
    sock: UnixStream,
}

impl Fetcher {
    /// ★ The system fetcher (portcullisd's `OpenFetcher`), asked for `url`.
    /// `PermissionDenied` when the player's manifest does not hold
    /// `net-fetch`.
    pub fn system(url: &str) -> io::Result<Fetcher> {
        Self::over(nuntius::launch::open()?, url)
    }

    /// The same, over a fetcher already opened (tests open one by hand).
    pub fn over(f: nuntius::launch::Fetcher, url: &str) -> io::Result<Fetcher> {
        if url.len() > URL_MAX || url.chars().any(char::is_control) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "a URL the fetcher won't take"));
        }
        let (client, guard) = f.into_parts();
        let (mine, theirs) = UnixStream::pair()?;
        let url = url.to_string();
        std::thread::Builder::new().name("player-fetch".into()).spawn(move || translate(client, &url, &theirs))?;
        Ok(Fetcher { _guard: guard, sock: mine })
    }

    /// The next message, waiting at most `wait`. `Err` when the fetcher is
    /// gone or broke the protocol.
    pub fn poll(&mut self, wait: Duration) -> io::Result<Option<(FromFetcher, Option<OwnedFd>)>> {
        if !sys::readable(&self.sock, wait.as_millis().min(i32::MAX as u128) as i32) {
            return Ok(None);
        }
        let bad = || io::Error::new(io::ErrorKind::InvalidData, "fetcher protocol");
        let Some((payload, mut fds)) = sys::recv_frame(&self.sock)? else {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "fetcher gone"));
        };
        let m = FromFetcher::decode(&payload).ok_or_else(bad)?;
        let fd = fds.pop();
        match (&m, &fd, fds.is_empty()) {
            (FromFetcher::File { .. }, Some(_), true) | (FromFetcher::Done { .. } | FromFetcher::Failed { .. }, None, true) => Ok(Some((m, fd))),
            _ => Err(bad()),
        }
    }
}

/// ★ The system fetcher's events as the player's messages, on `out` (the
/// same frames [`Fetcher::poll`] reads): `Fd` delivery, with a file of our
/// own filled from `Data` when the response has no length. Ends with `Done`
/// or `Failed`, or when the player hangs up.
fn translate(mut c: nuntius::Client, url: &str, out: &UnixStream) {
    use nuntius::{Deliver, Event, Limits, Open};
    use std::io::Write;
    let send = |m: FromFetcher, fd: Option<&OwnedFd>| {
        let raw: Vec<_> = fd.map(|f| f.as_raw_fd()).into_iter().collect();
        sys::send_frame(out, &m.encode(), &raw).is_ok()
    };
    let mut o = Open::get(1, url);
    o.deliver = Deliver::Fd;
    o.limits = Limits { max_bytes: nuntius::FD_MAX_BYTES, connect_ms: 10_000, idle_ms: 20_000, total_ms: nuntius::STREAM_TOTAL_MS_MAX };
    o.accept = Some("video/*, audio/*;q=0.9, */*;q=0.5".into());
    if c.open(o).is_err() {
        send(FromFetcher::Failed { reason: "the fetcher's channel".into() }, None);
        return;
    }
    let mut head: (Option<u64>, Option<String>) = (None, None);
    let mut own: Option<std::fs::File> = None;
    let mut sent_file = false;
    loop {
        let ev = match c.recv() {
            Ok(ev) => ev,
            Err(_) => { send(FromFetcher::Failed { reason: "the fetcher stopped".into() }, None); return }
        };
        let ok = match ev {
            Event::Head { status, .. } if !(200..300).contains(&status) => {
                send(FromFetcher::Failed { reason: format!("HTTP {status}") }, None);
                return;
            }
            Event::Head { length, content_type, .. } => {
                head = (length, content_type.map(|c| c.chars().filter(|c| !c.is_control()).take(256).collect()));
                true
            }
            Event::Fd { fd, .. } if !sent_file => { sent_file = true; send(FromFetcher::File { length: head.0, content_type: head.1.clone() }, Some(&fd)) }
            Event::Data { bytes, .. } => {
                if own.is_none() {
                    let Ok((w, ro)) = nuntius::anon_file() else { send(FromFetcher::Failed { reason: "a file".into() }, None); return };
                    own = Some(w);
                    sent_file = true;
                    if !send(FromFetcher::File { length: None, content_type: head.1.clone() }, Some(&ro)) { return }
                }
                if own.as_mut().unwrap().write_all(&bytes).is_err() {
                    send(FromFetcher::Failed { reason: "the file is full".into() }, None);
                    return;
                }
                c.budget(bytes.len() as u64).is_ok()
            }
            Event::Done { bytes, .. } => {
                if !sent_file {
                    // An empty body.
                    if let Ok((_, ro)) = nuntius::anon_file() { send(FromFetcher::File { length: Some(0), content_type: None }, Some(&ro)); }
                }
                send(FromFetcher::Done { bytes }, None);
                return;
            }
            Event::Error { code, .. } => { send(FromFetcher::Failed { reason: format!("{code:?}") }, None); return }
            Event::Fd { .. } | Event::Progress { .. } | Event::Retrying { .. } => true,
        };
        if !ok { return }
    }
}

/// The standalone player's host for a URL: the fetcher's file arrives as a
/// growing `Load`, then `Play`, and `LoadDone` when it is complete.
pub struct UrlHost {
    fetcher: Option<Fetcher>,
    loaded: bool,
    /// What went wrong, for the caller to print.
    pub failed: Option<String>,
}

impl UrlHost {
    pub fn new(f: Fetcher) -> UrlHost {
        UrlHost { fetcher: Some(f), loaded: false, failed: None }
    }
}

impl Host for UrlHost {
    fn poll(&mut self, wait: Duration) -> Vec<HostEvent> {
        let mut out = Vec::new();
        let Some(f) = self.fetcher.as_mut() else {
            std::thread::sleep(wait);
            return out;
        };
        match f.poll(wait) {
            Ok(None) => {}
            Ok(Some((FromFetcher::File { .. }, Some(fd)))) if !self.loaded => {
                self.loaded = true;
                out.push(HostEvent::Load(fd, true));
                out.push(HostEvent::Msg(ToChild::Play));
            }
            Ok(Some((FromFetcher::Done { .. }, _))) if self.loaded => {
                out.push(HostEvent::Msg(ToChild::LoadDone));
                self.fetcher = None;
            }
            // Cut off part way: play what arrived, to its end.
            Ok(Some((FromFetcher::Failed { reason }, _))) if self.loaded => {
                eprintln!("atrium-player: the download stopped: {reason}");
                out.push(HostEvent::Msg(ToChild::LoadDone));
                self.fetcher = None;
            }
            Err(_) if self.loaded => {
                eprintln!("atrium-player: the fetcher broke off");
                out.push(HostEvent::Msg(ToChild::LoadDone));
                self.fetcher = None;
            }
            Ok(Some((FromFetcher::Failed { reason }, _))) => {
                self.failed = Some(reason);
                self.fetcher = None;
                out.push(HostEvent::Gone);
            }
            Ok(Some(_)) | Err(_) => {
                self.failed = Some("the fetcher broke off".into());
                self.fetcher = None;
                out.push(HostEvent::Gone);
            }
        }
        out
    }

    fn emit(&mut self, m: ToParent) {
        if let ToParent::Error { code: c, track: None } = m {
            let what = match c {
                code::DECODE => "can't decode it",
                code::UNSUPPORTED => "unsupported format",
                code::TOO_BIG => "too big",
                _ => "failed",
            };
            self.failed.get_or_insert_with(|| what.into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire() {
        let ok = [
            FromFetcher::File { length: Some(5), content_type: Some("video/webm".into()) },
            FromFetcher::Done { bytes: 5 },
            FromFetcher::Failed { reason: "Http(404)".into() },
        ];
        for m in ok {
            assert_eq!(FromFetcher::decode(&m.encode()), Some(m));
        }
        assert_eq!(FromFetcher::decode(&FromFetcher::Failed { reason: "a\u{1b}".into() }.encode()), None);
        assert!(is_url("https://example.org/v.webm") && !is_url("/tmp/v.webm"));
    }
}
