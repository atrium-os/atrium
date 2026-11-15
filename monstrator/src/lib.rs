//! Monstrator, the link opener (docs/spec/opening-links.md).
//!
//! An app asks to open a link with [`open_link`]: one connection to
//! [`SOCKET`], one [`Ask`] line, one [`Answer`] line. The app must send the
//! frescod activation token of the press that asked for it, at once (the
//! token lives one second); portcullisd spends it, not Monstrator. The
//! answer says only whether the link was opened (D6), never in what.

pub mod sheet;
pub mod view;

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Where Monstrator listens (an app's jail sees it here too: `open-uri`).
pub const SOCKET: &str = "/atrium/sockets/monstrator/monstrator.sock";
/// Overrides [`SOCKET`].
pub const SOCKET_ENV: &str = "MONSTRATOR_SOCKET";
/// The longest request line.
pub const MAX_LINE: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Ask {
    /// Open `url` in the app that takes it, on the press `activation`.
    OpenLink { url: String, activation: [u8; 16] },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Answer {
    Opened,
    NotOpened { why: String },
}

/// Monstrator's socket, as configured.
pub fn socket() -> PathBuf {
    std::env::var_os(SOCKET_ENV).map(PathBuf::from).unwrap_or_else(|| SOCKET.into())
}

/// Ask Monstrator at `sock` to open `url` on the press `activation`.
/// `Ok(Err(why))`: it answered no.
pub fn open_link(sock: &Path, url: &str, activation: [u8; 16], wait: Duration) -> io::Result<Result<(), String>> {
    let mut s = UnixStream::connect(sock)?;
    s.set_read_timeout(Some(wait))?;
    let mut line = serde_json::to_vec(&Ask::OpenLink { url: url.into(), activation }).map_err(io::Error::other)?;
    line.push(b'\n');
    s.write_all(&line)?;
    let mut r = BufReader::new(s);
    let mut back = String::new();
    r.read_line(&mut back)?;
    match serde_json::from_str::<Answer>(back.trim_end()).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))? {
        Answer::Opened => Ok(Ok(())),
        Answer::NotOpened { why } => Ok(Err(why)),
    }
}

/// Which app a link goes to without asking (§5): the default, else the
/// only app. `None`: the person must choose (the sheet, L3).
pub fn pick(apps: &[portcullis_ipc::LinkChoice]) -> Option<&str> {
    apps.iter().find(|a| a.default).or(if apps.len() == 1 { apps.first() } else { None }).map(|a| a.id.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use portcullis_ipc::LinkChoice;

    fn c(id: &str, default: bool) -> LinkChoice {
        LinkChoice { id: id.into(), name: id.into(), default }
    }

    #[test]
    fn the_default_or_the_only_app() {
        assert_eq!(pick(&[c("a", false), c("b", true)]), Some("b"));
        assert_eq!(pick(&[c("a", false)]), Some("a"));
        assert_eq!(pick(&[c("a", false), c("b", false)]), None, "a choice: ask");
        assert_eq!(pick(&[]), None);
    }

    #[test]
    fn asks_and_answers_on_the_wire() {
        let a = Ask::OpenLink { url: "mailto:a@b".into(), activation: [7; 16] };
        let s = serde_json::to_string(&a).unwrap();
        assert_eq!(serde_json::from_str::<Ask>(&s).unwrap(), a);
        assert!(s.len() < MAX_LINE);
    }
}
