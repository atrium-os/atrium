//! forum-ctl — the intent wire between Forum's chrome apps and the WM core
//! (`docs/spec/forum.md` §3-4).
//!
//! The decomposed-shell contract: only `forum-wm` holds `window-management`. The
//! visible chrome — `forum-dock`, `forum-bar`, `forum-shelf`, `forum-overview` — are
//! ordinary `graphics`-only Insula apps. They never touch another app's surface
//! directly; they ask the core for *intents* over this wire, and the core (which
//! holds the cap) authorizes and carries them out against Fresco. So a bug in the
//! dock/shelf/overview can't manipulate windows — it never held the power.
//!
//! Wire: postcard-encoded `Intent` (chrome → core) and `Reply` (core → chrome).

use serde::{Deserialize, Serialize};

pub use fresco_protocol::{WmDisplayMode, WmOutput, WmSurfaceInfo};

/// Where one monitor goes relative to another ([`Intent::PlaceOutput`]).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Side { RightOf, LeftOf, Above, Below }

impl std::str::FromStr for Side {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "right-of" => Ok(Side::RightOf),
            "left-of" => Ok(Side::LeftOf),
            "above" => Ok(Side::Above),
            "below" => Ok(Side::Below),
            _ => Err(format!("{s}: want right-of, left-of, above or below")),
        }
    }
}

/// The canonical forum-ctl socket inside a Portcullis jail. Portcullis nullfs-mounts
/// the per-service directory `/atrium/sockets/forum-ctl/` (for both `window-management`,
/// which forum-wm serves it through, and `forum-control`, which chrome connects
/// through), so the socket lands here. Mirrors fresco-client's scheme.
pub const JAILED_FORUM_CTL_SOCKET: &str = "/atrium/sockets/forum-ctl/forum-ctl.sock";
/// Dev fallback when running bare (outside a jail).
pub const DEV_FORUM_CTL_SOCKET: &str = "/tmp/forum-ctl.sock";

/// Resolve the forum-ctl socket path: `$FORUM_CTL_SOCKET` → the in-jail directory
/// (if its mount is present) → the dev fallback. The same binary then works jailed
/// and bare with no env wiring (the jail runs `exec.clean`, stripping env). forum-wm
/// binds this; chrome connects to it.
pub fn default_socket_path() -> std::path::PathBuf {
    if let Ok(s) = std::env::var("FORUM_CTL_SOCKET") {
        return std::path::PathBuf::from(s);
    }
    // The per-service dir is the mounted unit; if it exists we're jailed-or-staged.
    if std::path::Path::new("/atrium/sockets/forum-ctl").is_dir() {
        return std::path::PathBuf::from(JAILED_FORUM_CTL_SOCKET);
    }
    std::path::PathBuf::from(DEV_FORUM_CTL_SOCKET)
}

/// A chrome app's request to the WM core. The core authorizes each before acting.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Intent {
    /// "What surfaces are in this session?" — the dock/bar/overview need the list
    /// to draw it. Read-only: the core answers from its Fresco enumerate.
    ListSurfaces,
    /// "Focus this surface" — e.g. the overview switcher on a click, or the dock on
    /// an app-icon activate. The core makes it the focused surface (which also
    /// drives input routing + the deadline lane + GPU power, §2.5) and re-declares
    /// the layout to Fresco.
    Focus { surface_id: u32 },
    /// "Which modes does the monitor offer, and which is lit?" — a display
    /// settings panel needs the list. Read-only.
    ListDisplayModes,
    /// "Switch the display to this mode." `refresh_mhz` 0 takes the first
    /// listed refresh at that size. The core asks frescod, which switches at
    /// its next frame and broadcasts the new geometry; the WM then re-lays out.
    SetDisplayMode { width: u32, height: u32, refresh_mhz: u32 },
    /// "Which monitors are there, and where on the desktop?" Read-only.
    ListOutputs,
    /// "Put output `output` on this side of output `of`." frescod checks the
    /// arrangement (every monitor touching another, no overlap), applies and
    /// remembers it for this set of monitors.
    PlaceOutput { output: u32, side: Side, of: u32 },
    /// "Send the focused window to the next monitor."
    MoveToNextMonitor,
}

/// The core's answer to an `Intent`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Reply {
    /// The session's surfaces + the currently-focused one (answer to `ListSurfaces`).
    Surfaces { surfaces: Vec<WmSurfaceInfo>, focus: u32 },
    /// The intent was carried out (answer to `Focus`).
    Ack,
    /// The intent failed or was refused.
    Err { message: String },
    /// The monitor's modes and the lit one's index (`u32::MAX` if none or no
    /// monitor) — answer to `ListDisplayModes`.
    DisplayModes { modes: Vec<WmDisplayMode>, current: u32 },
    /// The monitors and where they sit — answer to `ListOutputs`.
    Outputs { outputs: Vec<WmOutput> },
}

/// Encode a message for the wire.
pub fn encode<T: Serialize>(v: &T) -> Result<Vec<u8>, postcard::Error> {
    postcard::to_allocvec(v)
}

/// Decode a message from the wire.
pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, postcard::Error> {
    postcard::from_bytes(bytes)
}

// ── framing + the request/serve helpers (the actual socket I/O) ──────────────

use std::io::{self, Read, Write};

/// Write one length-prefixed (u32 LE) frame. postcard isn't self-delimiting over a
/// stream, so each message carries its length.
pub fn write_frame<W: Write>(w: &mut W, bytes: &[u8]) -> io::Result<()> {
    w.write_all(&(bytes.len() as u32).to_le_bytes())?;
    w.write_all(bytes)?;
    w.flush()
}

/// Read one length-prefixed frame.
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let n = u32::from_le_bytes(len) as usize;
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

/// Chrome side: connect to the core's forum-ctl socket, send one `Intent`, read the
/// `Reply`. One intent per connection keeps the wire trivially framed and stateless.
pub fn request<P: AsRef<std::path::Path>>(socket: P, intent: &Intent) -> io::Result<Reply> {
    let mut s = std::os::unix::net::UnixStream::connect(socket)?;
    let bytes = encode(intent).map_err(io::Error::other)?;
    write_frame(&mut s, &bytes)?;
    let reply_bytes = read_frame(&mut s)?;
    decode(&reply_bytes).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intents_round_trip() {
        for i in [Intent::ListSurfaces, Intent::Focus { surface_id: 7 }, Intent::ListDisplayModes,
                  Intent::SetDisplayMode { width: 2560, height: 1440, refresh_mhz: 59951 },
                  Intent::ListOutputs, Intent::MoveToNextMonitor,
                  Intent::PlaceOutput { output: 1, side: "left-of".parse().unwrap(), of: 0 }] {
            let b = encode(&i).unwrap();
            assert_eq!(decode::<Intent>(&b).unwrap(), i);
        }
    }

    #[test]
    fn replies_round_trip() {
        let r = Reply::Surfaces { surfaces: Vec::new(), focus: 3 };
        assert_eq!(decode::<Reply>(&encode(&r).unwrap()).unwrap(), r);
        assert_eq!(decode::<Reply>(&encode(&Reply::Ack).unwrap()).unwrap(), Reply::Ack);
        let m = WmDisplayMode { width: 1920, height: 1080, refresh_mhz: 60000, preferred: true };
        let r = Reply::DisplayModes { modes: vec![m], current: 0 };
        assert_eq!(decode::<Reply>(&encode(&r).unwrap()).unwrap(), r);
    }
}
