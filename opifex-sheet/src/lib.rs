//! The install sheet (docs/spec/opifex.md §7.1).
//!
//! Anything may ask for an app from the catalog to be installed: one
//! connection to [`SOCKET`], one [`Ask`] line, one [`Answer`] line. The
//! asker learns only `Installed`, `Declined` or `Failed`; the person decides
//! on the sheet, opifex-sheetd's own window, and their press counts only once
//! frescod has counted it (`PROMPT`). opifexd then installs the release the
//! sheet showed, for that person, and nothing else.

pub mod sheet;
pub mod view;

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Where the sheet listens.
pub const SOCKET: &str = "/atrium/sockets/opifex-sheet/opifex-sheet.sock";
/// Overrides [`SOCKET`].
pub const SOCKET_ENV: &str = "OPIFEX_SHEET_SOCKET";
pub const MAX_LINE: usize = 4 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Ask {
    /// Install the app `id` from the catalog, if the person agrees.
    /// `for_uid`: the app it is asked for, when the asker asks on its
    /// behalf: Monstrator, for an `atrium-app:` link to an app that isn't
    /// installed (opening-links.md §4.3, L5). Taken only from the uids in
    /// `OPIFEX_SHEET_FOR_OTHERS` (default `_monstrator`, 65038).
    Install {
        id: String,
        #[serde(default)]
        for_uid: Option<u32>,
    },
    /// Follow the catalog served at `url` (https), for the asking person,
    /// if they agree (opifex.md §6.5; C7).
    AddCatalog { url: String },
    /// Install the app in the `.insula` file sent with this line (fdpass:
    /// the asker hands over the file the person opened, never a path), if
    /// the person agrees and a publisher this machine trusts signed it
    /// (opifex.md §7.4). Scrinium asks it for a double-clicked bundle.
    SideLoad,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Answer {
    /// Installed now, or it already was at the newest version.
    Installed,
    /// The person said not now (or closed the sheet, or let it time out).
    Declined,
    Failed { why: String },
    /// The catalog is followed now, for that person.
    Added { name: String },
}

pub fn socket() -> PathBuf {
    std::env::var_os(SOCKET_ENV).map(PathBuf::from).unwrap_or_else(|| SOCKET.into())
}

/// Ask for `id` to be installed; waits for the person (up to two minutes).
pub fn install(id: &str) -> io::Result<Answer> {
    let mut s = UnixStream::connect(socket())?;
    s.set_read_timeout(Some(Duration::from_secs(150)))?;
    let mut line = serde_json::to_vec(&Ask::Install { id: id.into(), for_uid: None })?;
    line.push(b'\n');
    s.write_all(&line)?;
    let mut back = String::new();
    BufReader::new(s).read_line(&mut back)?;
    serde_json::from_str(back.trim_end()).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Ask for the catalog at `url` to be added for this person; waits for them.
pub fn add_catalog(url: &str) -> io::Result<Answer> {
    let mut s = UnixStream::connect(socket())?;
    s.set_read_timeout(Some(Duration::from_secs(150)))?;
    let mut line = serde_json::to_vec(&Ask::AddCatalog { url: url.into() })?;
    line.push(b'\n');
    s.write_all(&line)?;
    let mut back = String::new();
    BufReader::new(s).read_line(&mut back)?;
    serde_json::from_str(back.trim_end()).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Ask for the app in `file` (an `.insula` the person opened) to be installed
/// for this person; waits for them.
pub fn side_load(file: &std::fs::File) -> io::Result<Answer> {
    use std::os::fd::AsRawFd;
    let s = UnixStream::connect(socket())?;
    s.set_read_timeout(Some(Duration::from_secs(150)))?;
    let mut line = serde_json::to_vec(&Ask::SideLoad)?;
    line.push(b'\n');
    opifex::fdpass::write_line(&s, &line, Some(file.as_raw_fd()))?;
    let mut back = String::new();
    BufReader::new(s).read_line(&mut back)?;
    serde_json::from_str(back.trim_end()).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// A capability in plain words, and whether the sheet marks it.
pub fn words(cap: &str) -> (String, bool) {
    let (w, risky) = match cap {
        "clipboard" => ("read and write the clipboard", false),
        "notify" => ("show notifications", false),
        "open-uri" | "open_uri" => ("open links in other apps", false),
        "audio" => ("play sound", false),
        "fonts" => ("use extra fonts", false),
        "network" => ("use the network", true),
        "filesystem" => ("read and write folders of yours", true),
        "camera" => ("use the camera", true),
        "microphone" => ("use the microphone", true),
        "usb-hid" | "usb_hid" => ("talk to USB input devices directly", true),
        "audio-monitor" | "audio_monitor" => ("hear what other apps play", true),
        "window-management" | "window_management" => ("manage other apps' windows", true),
        "arca" => ("keep secrets in your keychain (Arca)", false),
        "embeds" => ("show parts of other apps inside its window", false),
        _ => return (cap.replace(['-', '_'], " "), true),
    };
    (w.into(), risky)
}

/// Bytes in words ("3.4 MB").
pub fn size(n: u64) -> String {
    match n {
        0..=999 => format!("{n} bytes"),
        1_000..=999_999 => format!("{:.0} kB", n as f64 / 1e3),
        1_000_000..=999_999_999 => format!("{:.1} MB", n as f64 / 1e6),
        _ => format!("{:.1} GB", n as f64 / 1e9),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_words() {
        assert_eq!(words("camera"), ("use the camera".into(), true));
        assert_eq!(words("clipboard").1, false);
        assert_eq!(words("tessera-cas-read"), ("tessera cas read".into(), true), "unknown ones are marked");
        assert_eq!(size(512), "512 bytes");
        assert_eq!(size(3_400_000), "3.4 MB");
    }

    #[test]
    fn answers_round_trip() {
        for a in [Answer::Installed, Answer::Declined, Answer::Failed { why: "x".into() }] {
            assert_eq!(serde_json::from_str::<Answer>(&serde_json::to_string(&a).unwrap()).unwrap(), a);
        }
    }
}
