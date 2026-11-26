//! Fenestra's records between frescod's broker and `fenestrad`, and between
//! `fenestrad` and a session jail (fenestra.md §3.2).
//!
//! Each record is a fixed 40-byte header (kind, window, ticket, flag, two
//! 64-bit words, payload length, zero) and a payload bounded by its kind.
//! The scene stream bytes in `Data` are carried, never parsed: only the
//! renderer writes them and only the viewer's renderer reads them.
//!
//! A `Want` asks the broker for one window's difference from what a viewer
//! holds; the answer is any number of `Data` records then one `End`, all
//! with the want's ticket. `Changed` says a window's stream moved on, and
//! `Layout` where the windows are, bottom to top.

use std::io::{self, Read, Write};

pub const HEADER: usize = 40;
/// The largest `Data` payload (the broker splits larger differences).
pub const MAX_DATA: usize = 4 << 20;
/// The most windows a `Layout` lists.
pub const MAX_LAYOUT: usize = 256;
const PLACED: usize = 20;
/// The largest screen side a `Layout` may give.
pub const MAX_SCREEN: u32 = 65536;

pub type Version = (u64, u64);

/// One window on the host's screen, in its pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placed {
    pub window: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rec {
    /// fenestrad → frescod: a remote session started (stream every
    /// window) or the last one ended.
    Session { on: bool },
    /// fenestrad → frescod: `window`'s difference from `base`.
    Want { window: u32, ticket: u32, base: Option<Version> },
    /// frescod → fenestrad.
    Changed { window: u32 },
    Data { window: u32, ticket: u32, bytes: Vec<u8> },
    /// The difference for `ticket` is complete and brings a viewer to `now`
    /// (`None`: the window is not there).
    End { window: u32, ticket: u32, now: Option<Version> },
    Layout { screen: (u32, u32), windows: Vec<Placed> },
}

mod kind {
    pub const SESSION: u32 = 1;
    pub const WANT: u32 = 2;
    pub const CHANGED: u32 = 0x10;
    pub const DATA: u32 = 0x11;
    pub const END: u32 = 0x12;
    pub const LAYOUT: u32 = 0x13;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused(pub &'static str);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "fenestra link: {}", self.0)
    }
}

impl std::error::Error for Refused {}

impl From<Refused> for io::Error {
    fn from(r: Refused) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, r)
    }
}

struct Head {
    kind: u32,
    window: u32,
    ticket: u32,
    flag: u32,
    a: u64,
    b: u64,
    len: u32,
}

impl Head {
    fn bytes(&self) -> [u8; HEADER] {
        let mut h = [0u8; HEADER];
        h[0..4].copy_from_slice(&self.kind.to_le_bytes());
        h[4..8].copy_from_slice(&self.window.to_le_bytes());
        h[8..12].copy_from_slice(&self.ticket.to_le_bytes());
        h[12..16].copy_from_slice(&self.flag.to_le_bytes());
        h[16..24].copy_from_slice(&self.a.to_le_bytes());
        h[24..32].copy_from_slice(&self.b.to_le_bytes());
        h[32..36].copy_from_slice(&self.len.to_le_bytes());
        h
    }

    fn parse(h: &[u8; HEADER]) -> Head {
        let w = |i: usize| u32::from_le_bytes([h[i], h[i + 1], h[i + 2], h[i + 3]]);
        let d = |i: usize| u64::from(w(i)) | u64::from(w(i + 4)) << 32;
        Head { kind: w(0), window: w(4), ticket: w(8), flag: w(12), a: d(16), b: d(24), len: w(32) }
    }
}

fn version(v: Option<Version>) -> (u32, u64, u64) {
    match v {
        Some((e, ver)) => (1, e, ver),
        None => (0, 0, 0),
    }
}

impl Rec {
    /// The record's bytes: header, then payload.
    pub fn encode(&self) -> Vec<u8> {
        use kind::*;
        let mut payload = Vec::new();
        let h = match self {
            Rec::Session { on } => Head { kind: SESSION, window: 0, ticket: 0, flag: *on as u32, a: 0, b: 0, len: 0 },
            Rec::Want { window, ticket, base } => {
                let (flag, a, b) = version(*base);
                Head { kind: WANT, window: *window, ticket: *ticket, flag, a, b, len: 0 }
            }
            Rec::Changed { window } => Head { kind: CHANGED, window: *window, ticket: 0, flag: 0, a: 0, b: 0, len: 0 },
            Rec::Data { window, ticket, bytes } => {
                payload.extend_from_slice(bytes);
                Head { kind: DATA, window: *window, ticket: *ticket, flag: 0, a: 0, b: 0, len: bytes.len() as u32 }
            }
            Rec::End { window, ticket, now } => {
                let (flag, a, b) = version(*now);
                Head { kind: END, window: *window, ticket: *ticket, flag, a, b, len: 0 }
            }
            Rec::Layout { screen, windows } => {
                for p in windows {
                    payload.extend_from_slice(&p.window.to_le_bytes());
                    payload.extend_from_slice(&p.x.to_le_bytes());
                    payload.extend_from_slice(&p.y.to_le_bytes());
                    payload.extend_from_slice(&p.width.to_le_bytes());
                    payload.extend_from_slice(&p.height.to_le_bytes());
                }
                Head { kind: LAYOUT, window: 0, ticket: 0, flag: 0, a: u64::from(screen.0), b: u64::from(screen.1), len: payload.len() as u32 }
            }
        };
        let mut out = h.bytes().to_vec();
        out.extend_from_slice(&payload);
        out
    }

    /// The payload length a header announces, refused past its kind's
    /// bound before anything is buffered.
    pub fn payload_len(header: &[u8; HEADER]) -> Result<usize, Refused> {
        let h = Head::parse(header);
        let len = h.len as usize;
        let max = match h.kind {
            kind::DATA => MAX_DATA,
            kind::LAYOUT => MAX_LAYOUT * PLACED,
            kind::SESSION | kind::WANT | kind::CHANGED | kind::END => 0,
            _ => return Err(Refused("unknown kind")),
        };
        if len > max {
            return Err(Refused("payload too long"));
        }
        Ok(len)
    }

    /// Parse one record. Every field a kind does not use must be zero.
    pub fn decode(header: &[u8; HEADER], payload: &[u8]) -> Result<Rec, Refused> {
        use kind::*;
        let len = Rec::payload_len(header)?;
        if payload.len() != len {
            return Err(Refused("payload length"));
        }
        if header[36..40] != [0; 4] {
            return Err(Refused("reserved bytes not zero"));
        }
        let h = Head::parse(header);
        let flag = |f: u32| match f {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Refused("flag not 0 or 1")),
        };
        let ver = |h: &Head| match flag(h.flag)? {
            true => Ok(Some((h.a, h.b))),
            false if h.a == 0 && h.b == 0 => Ok(None),
            false => Err(Refused("version without its flag")),
        };
        let window = |w: u32| if (1..=u32::from(u16::MAX)).contains(&w) { Ok(w) } else { Err(Refused("window id")) };
        let unused = |ok: bool| if ok { Ok(()) } else { Err(Refused("unused field not zero")) };
        Ok(match h.kind {
            SESSION => {
                unused(h.window == 0 && h.ticket == 0 && h.a == 0 && h.b == 0)?;
                Rec::Session { on: flag(h.flag)? }
            }
            WANT => Rec::Want { window: window(h.window)?, ticket: h.ticket, base: ver(&h)? },
            CHANGED => {
                unused(h.ticket == 0 && h.flag == 0 && h.a == 0 && h.b == 0)?;
                Rec::Changed { window: window(h.window)? }
            }
            DATA => {
                unused(h.flag == 0 && h.a == 0 && h.b == 0)?;
                if payload.is_empty() {
                    return Err(Refused("empty data"));
                }
                Rec::Data { window: window(h.window)?, ticket: h.ticket, bytes: payload.to_vec() }
            }
            END => Rec::End { window: window(h.window)?, ticket: h.ticket, now: ver(&h)? },
            LAYOUT => {
                unused(h.window == 0 && h.ticket == 0 && h.flag == 0)?;
                let side = |v: u64| if (1..=u64::from(MAX_SCREEN)).contains(&v) { Ok(v as u32) } else { Err(Refused("screen size")) };
                let screen = (side(h.a)?, side(h.b)?);
                if payload.len() % PLACED != 0 {
                    return Err(Refused("layout length"));
                }
                let mut windows = Vec::with_capacity(payload.len() / PLACED);
                for c in payload.chunks_exact(PLACED) {
                    let w = |i: usize| u32::from_le_bytes([c[i], c[i + 1], c[i + 2], c[i + 3]]);
                    let p = Placed { window: window(w(0))?, x: w(4) as i32, y: w(8) as i32, width: w(12), height: w(16) };
                    if p.width == 0 || p.height == 0 || p.width > MAX_SCREEN || p.height > MAX_SCREEN {
                        return Err(Refused("window size"));
                    }
                    if windows.iter().any(|q: &Placed| q.window == p.window) {
                        return Err(Refused("a window placed twice"));
                    }
                    windows.push(p);
                }
                Rec::Layout { screen, windows }
            }
            _ => return Err(Refused("unknown kind")),
        })
    }

    /// Records the broker sends; it takes only the others.
    pub fn from_broker(&self) -> bool {
        matches!(self, Rec::Changed { .. } | Rec::Data { .. } | Rec::End { .. } | Rec::Layout { .. })
    }
}

/// Read one record (blocking). `Ok(None)` at a clean end of stream.
pub fn read<R: Read>(r: &mut R) -> io::Result<Option<Rec>> {
    let mut h = [0u8; HEADER];
    let mut got = 0;
    while got < HEADER {
        match r.read(&mut h[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let len = Rec::payload_len(&h)?;
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok(Some(Rec::decode(&h, &payload)?))
}

/// Write one record (blocking).
pub fn write<W: Write>(w: &mut W, rec: &Rec) -> io::Result<()> {
    w.write_all(&rec.encode())
}
