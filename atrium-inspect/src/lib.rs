//! The inspector's wire (dev-tools.md §3.6, D23–D25): an IDE that started
//! a dev build of an app reads its UI, read-only. It asks for the node
//! list (each node's kind, box, colours and fonts, and the app's source
//! line that made it), outlines a node or every box, and picks the node
//! under the next press. Pergola (`pergola::inspect`) and libatrium answer;
//! Artifex asks.
//!
//! The app finds the IDE's socket at the descriptor `ATRIUM_INSPECT_FD`
//! names, which portcullisd puts there for a Run from Artifex and nothing
//! else does. Frames are a little-endian `u32` length then postcard, each
//! way, bounded by [`MAX_FRAME`]; the IDE is a peer like any other.

use serde::{Deserialize, Serialize};

/// The largest frame either side takes.
pub const MAX_FRAME: usize = 8 << 20;
/// The most nodes a tree answer lists.
pub const MAX_ITEMS: usize = 20_000;
/// The most boxes "every box" outlines.
pub const MAX_BOUNDS: usize = 1_500;

/// What the IDE asks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Ask {
    /// The node tree as last drawn: answered with [`Tell::Tree`].
    Tree,
    /// Take the next press for the inspector (not the app), and answer
    /// [`Tell::Picked`]; `false` stops waiting for one.
    Pick(bool),
    /// Outline this node (`None`: none).
    Highlight(Option<u32>),
    /// Outline every box.
    Bounds(bool),
}

/// What the app tells.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Tell {
    /// First on the socket: which process this is.
    Hello { pid: u32 },
    Tree(Vec<Item>),
    /// The node under the press a [`Ask::Pick`] waited for.
    Picked(u32),
    /// What is drawn changed since the last [`Tell::Tree`].
    Changed,
}

/// One node of the tree, parents before children.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub id: u32,
    pub parent: Option<u32>,
    pub depth: u16,
    /// `Rect`, `Text`, `Stack`, ...
    pub kind: String,
    /// Its box in the window, in points (x, y, w, h); `None` when it is
    /// scrolled out of view or has no size.
    pub rect: Option<[f32; 4]>,
    /// Its colours, fonts and text, in a line.
    pub detail: String,
    /// The app's source line that made it.
    pub site: Option<Site>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Site {
    pub file: String,
    pub line: u32,
    pub col: u32,
}

/// A frame: length, then the message.
pub fn encode<T: Serialize>(msg: &T) -> Option<Vec<u8>> {
    let body = postcard::to_allocvec(msg).ok()?;
    if body.len() > MAX_FRAME {
        return None;
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    Some(out)
}

/// Frames out of a byte stream. A frame over [`MAX_FRAME`] or one that
/// does not decode is an error: drop the peer.
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
}

impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Bytes waiting for the rest of their frame.
    pub fn pending(&self) -> usize { self.buf.len() }

    pub fn next<T: for<'de> Deserialize<'de>>(&mut self) -> Result<Option<T>, String> {
        let Some(head) = self.buf.get(..4) else { return Ok(None) };
        let n = u32::from_le_bytes([head[0], head[1], head[2], head[3]]) as usize;
        if n > MAX_FRAME {
            return Err(format!("a frame of {n} bytes (at most {MAX_FRAME})"));
        }
        let Some(body) = self.buf.get(4..4 + n) else { return Ok(None) };
        let msg = postcard::from_bytes(body).map_err(|e| format!("a frame that does not decode: {e}"))?;
        self.buf.drain(..4 + n);
        Ok(Some(msg))
    }
}

/// The app's end of the IDE's socket.
pub struct Link {
    stream: std::os::unix::net::UnixStream,
    rx: Decoder,
    tx: Vec<u8>,
    /// A press goes to the inspector, not the app.
    pub picking: bool,
    /// The IDE has a tree and wants to hear when it changes.
    pub following: bool,
    /// The node to outline ([`Ask::Highlight`]).
    pub highlight: Option<u32>,
    /// Outline every box ([`Ask::Bounds`]).
    pub bounds: bool,
}

impl Link {
    /// The socket `ATRIUM_INSPECT_FD` names, once per process.
    pub fn from_env() -> Option<Self> {
        use std::os::fd::FromRawFd;
        static TAKEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let fd: i32 = std::env::var("ATRIUM_INSPECT_FD").ok()?.parse().ok()?;
        if fd < 3 || TAKEN.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return None;
        }
        // SAFETY: fstat on a descriptor number only reads its status.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut st) } != 0 || (st.st_mode & libc::S_IFMT) != libc::S_IFSOCK {
            return None;
        }
        // SAFETY: a socket this process was started with and nothing else
        // owns: `TAKEN` lets it be taken once.
        let stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
        let mut l = Self::new(stream).ok()?;
        l.send(&Tell::Hello { pid: std::process::id() });
        Some(l)
    }

    pub fn new(stream: std::os::unix::net::UnixStream) -> std::io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Self { stream, rx: Decoder::default(), tx: Vec::new(), picking: false, following: false, highlight: None, bounds: false })
    }

    /// Queue `msg` and send what the socket takes now.
    pub fn send(&mut self, msg: &Tell) {
        if let Some(f) = encode(msg) {
            if self.tx.len() + f.len() <= 2 * MAX_FRAME {
                self.tx.extend_from_slice(&f);
            }
        }
        self.flush();
    }

    fn flush(&mut self) {
        use std::io::Write;
        while !self.tx.is_empty() {
            match self.stream.write(&self.tx) {
                Ok(0) => break,
                Ok(n) => { self.tx.drain(..n); }
                Err(_) => break,
            }
        }
    }

    /// What the IDE asked since the last call; `Err` when it went or
    /// broke the protocol (drop the link).
    pub fn poll(&mut self) -> Result<Vec<Ask>, String> {
        use std::io::Read;
        self.flush();
        let mut buf = [0u8; 4096];
        loop {
            match self.stream.read(&mut buf) {
                Ok(0) => return Err("the inspector went".into()),
                Ok(n) => {
                    self.rx.push(&buf[..n]);
                    if self.rx.buf.len() > MAX_FRAME + 4 {
                        return Err("the inspector sent too much".into());
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.to_string()),
            }
        }
        let mut asks = Vec::new();
        while let Some(a) = self.rx.next::<Ask>()? {
            asks.push(a);
        }
        Ok(asks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_a_bad_one_is_an_error() {
        let mut d = Decoder::default();
        let f = encode(&Ask::Highlight(Some(7))).unwrap();
        d.push(&f[..3]);
        assert_eq!(d.next::<Ask>().unwrap(), None, "half a frame waits");
        d.push(&f[3..]);
        assert_eq!(d.next::<Ask>().unwrap(), Some(Ask::Highlight(Some(7))));
        let mut big = Decoder::default();
        big.push(&((MAX_FRAME as u32) + 1).to_le_bytes());
        assert!(big.next::<Ask>().is_err(), "an oversize length drops the peer");
        let mut junk = Decoder::default();
        junk.push(&[1, 0, 0, 0, 0xff]);
        assert!(junk.next::<Ask>().is_err());
    }

    #[test]
    fn the_link_says_hello_hears_asks_and_notices_the_ide_go() {
        use std::io::{Read, Write};
        let (app, mut ide) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut l = Link::new(app).unwrap();
        l.send(&Tell::Hello { pid: 9 });
        let mut buf = [0u8; 64];
        let n = ide.read(&mut buf).unwrap();
        let mut d = Decoder::default();
        d.push(&buf[..n]);
        assert_eq!(d.next::<Tell>().unwrap(), Some(Tell::Hello { pid: 9 }));
        ide.write_all(&encode(&Ask::Tree).unwrap()).unwrap();
        ide.write_all(&encode(&Ask::Bounds(true)).unwrap()).unwrap();
        assert_eq!(l.poll().unwrap(), vec![Ask::Tree, Ask::Bounds(true)]);
        drop(ide);
        assert!(l.poll().is_err());
    }
}
