//! The Navigator's control plane (navigator-ui.md §2; backend §3).
//!
//! Two sockets, both navigatord's:
//!
//! - the **control socket**, where the UI asks for pages by address and
//!   for link decisions, and is told what was opened. The UI never gets a
//!   page's bytes: an opened page comes back as a one-shot ticket the UI
//!   passes to its doc-viewer child (Limen L6), which redeems it here;
//! - the **page socket**, where a doc-viewer child redeems a ticket for the
//!   recording and the address it was converted from.
//!
//! Frames are a little-endian `u32` length and a postcard body, at most
//! [`MAX_FRAME`]. A descriptor rides alone right after the frame that
//! announces it (`Chrome`), so a reader never buffers past a frame.

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

use serde::{Deserialize, Serialize};

/// The largest control frame. Addresses are bounded by [`URL_MAX`].
pub const MAX_FRAME: usize = 64 * 1024;
/// The longest address either side sends.
pub const URL_MAX: usize = 16 * 1024;
/// The longest title navigatord reports.
pub const TITLE_MAX: usize = 256;
/// The largest recording a ticket redeems for.
pub const RECORDING_MAX: u64 = 128 << 20;

pub type Ticket = [u8; 16];

/// UI → navigatord.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// The first message. `chrome`: send a frescod connection opened over
    /// the chrome channel (F4d) for the UI's window, so navigatord can name
    /// the page's origin in frescod's strip.
    Hello { chrome: bool },
    /// The UI's window on that connection (its local id), whose origin
    /// navigatord sets.
    Window { window: u32 },
    /// Fetch and convert the page at `url` (or take it from the cache) and
    /// show its origin. Answered by `Opened` or `Failed` with the same `id`.
    Open { id: u32, url: String },
    /// What activating `href` in the page at `page` does. Answered by
    /// `Link` with the same `id`.
    Activate { id: u32, page: String, href: String },
    /// The UI shows no page (Start, a stop page): no origin in the strip.
    Blank,
}

/// What a link does (backend `links::LinkAction`, and refusals).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinkAction {
    Navigate { url: String, insecure: bool },
    InDocument { fragment: String },
    Handoff { url: String, scheme: String },
    Blocked { why: String },
}

/// navigatord → UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    /// The answer to `Hello`. With `fd`, a descriptor follows: the UI's
    /// frescod connection.
    Chrome { fd: bool },
    Opened { id: u32, url: String, title: String, insecure: bool, ticket: Ticket, ms: u32 },
    Failed { id: u32, url: String, why: String },
    Link { id: u32, action: LinkAction },
}

/// What a ticket redeems for on the page socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageHeader {
    /// The address the document was converted from (links resolve against it).
    pub base: String,
    /// The recording's length; its bytes follow the frame.
    pub len: u64,
}

fn bad(m: impl Into<String>) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, m.into()) }

/// Write one frame.
pub fn write_frame<T: Serialize>(w: &mut impl Write, m: &T) -> io::Result<()> {
    let b = postcard::to_stdvec(m).map_err(|e| bad(e.to_string()))?;
    if b.len() > MAX_FRAME {
        return Err(bad("frame over MAX_FRAME"));
    }
    let mut out = (b.len() as u32).to_le_bytes().to_vec();
    out.extend_from_slice(&b);
    w.write_all(&out)
}

/// Read one frame; `Ok(None)` at a clean end of stream.
pub fn read_frame<T: for<'a> Deserialize<'a>>(r: &mut impl Read) -> io::Result<Option<T>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let n = u32::from_le_bytes(len) as usize;
    if n > MAX_FRAME {
        return Err(bad(format!("a {n}-byte frame")));
    }
    let mut b = vec![0u8; n];
    r.read_exact(&mut b)?;
    postcard::from_bytes(&b).map(Some).map_err(|e| bad(e.to_string()))
}

/// Send one descriptor alone (after the frame that announced it).
pub fn send_fd(s: &UnixStream, fd: RawFd) -> io::Result<()> {
    portcullis_ipc::fdpass::send_fds(s, &[fd])
}

/// Receive the descriptor a frame announced.
pub fn recv_fd(s: &UnixStream) -> io::Result<OwnedFd> {
    portcullis_ipc::fdpass::recv_fds(s, 1)?.pop().ok_or_else(|| bad("no descriptor"))
}

/// A string fit to send: no controls, at most `max` bytes.
pub fn clean(s: &str, max: usize) -> bool {
    s.len() <= max && !s.chars().any(char::is_control)
}

/// The child's side of the page socket: redeem `ticket` for the base
/// address and the recording, at most `RECORDING_MAX` bytes.
pub fn redeem(path: &str, ticket: &Ticket) -> io::Result<(String, Vec<u8>)> {
    let mut s = UnixStream::connect(path)?;
    s.write_all(ticket)?;
    let h: PageHeader = read_frame(&mut s)?.ok_or_else(|| bad("the ticket was refused"))?;
    if h.len > RECORDING_MAX || !clean(&h.base, URL_MAX) {
        return Err(bad("a header out of bounds"));
    }
    let mut b = Vec::with_capacity(h.len as usize);
    (&mut s).take(h.len).read_to_end(&mut b)?;
    if b.len() as u64 != h.len {
        return Err(bad("a short recording"));
    }
    Ok((h.base, b))
}

/// The raw descriptor of a stream, for `send_fd`.
pub fn raw(s: &impl AsRawFd) -> RawFd { s.as_raw_fd() }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_bounds_hold() {
        let mut buf = Vec::new();
        let m = Request::Open { id: 7, url: "https://atlas.example/".into() };
        write_frame(&mut buf, &m).unwrap();
        write_frame(&mut buf, &Event::Link { id: 7, action: LinkAction::InDocument { fragment: "a".into() } }).unwrap();
        let mut r = &buf[..];
        assert_eq!(read_frame::<Request>(&mut r).unwrap(), Some(m));
        assert!(matches!(read_frame::<Event>(&mut r).unwrap(), Some(Event::Link { id: 7, .. })));
        assert_eq!(read_frame::<Event>(&mut r).unwrap(), None);
        let huge = (MAX_FRAME as u32 + 1).to_le_bytes();
        assert!(read_frame::<Event>(&mut &huge[..]).is_err());
    }

    #[test]
    fn a_descriptor_follows_its_frame() {
        let (a, b) = UnixStream::pair().unwrap();
        let (x, _y) = UnixStream::pair().unwrap();
        write_frame(&mut &a, &Event::Chrome { fd: true }).unwrap();
        send_fd(&a, raw(&x)).unwrap();
        assert_eq!(read_frame::<Event>(&mut &b).unwrap(), Some(Event::Chrome { fd: true }));
        recv_fd(&b).unwrap();
    }
}
