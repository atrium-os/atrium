//! Opening a URL in the standalone player (atrium-player.md §6). The
//! player has no network: it starts `atrium-player-fetch`, a separate
//! process holding the Navigator's sandboxed fetcher (Nuntius, nuntius.md
//! §5 client 2), which hands back a read-only descriptor of the file it is
//! filling. The player plays that as a growing `Load` and is told when the
//! download is complete. Neither the supervisor nor the decode worker ever
//! gets a socket to the network.
//!
//! Until nuntiusd and portcullisd's `net.fetch` launch exist (nuntius.md
//! §6 steps 2 and 4), this helper is the fetcher's process.
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Duration;

use limen::media_player::{code, ToChild, ToParent};
use serde::{Deserialize, Serialize};

use crate::app::{Host, HostEvent};
use crate::sys;

/// Longest URL the player asks for.
pub const URL_MAX: usize = 8192;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToFetcher {
    Get { url: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FromFetcher {
    /// The file being filled, as descriptor 0 on this frame (read-only).
    /// `length` when the server said.
    File { length: Option<u64>, content_type: Option<String> },
    /// The download is complete.
    Done { bytes: u64 },
    Failed { reason: String },
}

impl ToFetcher {
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("encodes")
    }
    pub fn decode(b: &[u8]) -> Option<Self> {
        let m: Self = postcard::from_bytes(b).ok()?;
        let ToFetcher::Get { url } = &m;
        (url.len() <= URL_MAX && !url.chars().any(char::is_control)).then_some(m)
    }
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

/// `atrium-player-fetch` next to this executable, unless
/// `ATRIUM_PLAYER_FETCH` says otherwise.
pub fn default_fetcher() -> PathBuf {
    if let Some(p) = std::env::var_os("ATRIUM_PLAYER_FETCH") {
        return p.into();
    }
    std::env::current_exe().unwrap_or_default().with_file_name("atrium-player-fetch")
}

/// The fetcher process, asked for one URL.
pub struct Fetcher {
    child: Child,
    sock: UnixStream,
}

impl Fetcher {
    /// `extra`: arguments for the fetcher (`--unconfined-for-testing` off
    /// FreeBSD, `--trust PEM`).
    pub fn spawn(exe: &std::path::Path, url: &str, extra: &[String]) -> io::Result<Fetcher> {
        let (mine, theirs) = UnixStream::pair()?;
        let mut cmd = Command::new(exe);
        cmd.arg("--fd").arg(theirs.as_raw_fd().to_string()).args(extra).stdin(std::process::Stdio::null());
        sys::pass_fd(&mut cmd, theirs.as_raw_fd());
        let child = cmd.spawn()?;
        drop(theirs);
        // The URL goes after the fetcher is sandboxed: it reads its request
        // only once it can't open a file.
        sys::send_frame(&mine, &ToFetcher::Get { url: url.to_string() }.encode(), &[])?;
        Ok(Fetcher { child, sock: mine })
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

impl Drop for Fetcher {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
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
        assert_eq!(ToFetcher::decode(&ToFetcher::Get { url: "https://a/\n".into() }.encode()), None);
        assert!(is_url("https://example.org/v.webm") && !is_url("/tmp/v.webm"));
    }
}
