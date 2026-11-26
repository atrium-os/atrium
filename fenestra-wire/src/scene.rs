//! What travels over a session once it is proven (fenestra.md §5, §8.1):
//!
//! - one unidirectional stream per window, opened by the host, starting
//!   with a [`WindowHeader`], then the window's scene stream
//!   (`fresco_render_core::stream` records);
//! - acks from the viewer as datagrams ([`Ack`]): the latest wins, a lost
//!   one is covered by the next;
//! - on the control stream, `Msg::App` bodies of [`Control`].

use crate::WireError;

/// `magic u32 | window u32 | epoch u64` at the start of a window stream.
pub const WINDOW_HEADER_LEN: usize = 16;
const WINDOW_MAGIC: u32 = 0x4645_4e31; // "FEN1"

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowHeader {
    /// The host's id for the window, stable for the session.
    pub window: u32,
    /// The window's writer epoch.
    pub epoch: u64,
}

impl WindowHeader {
    pub fn encode(&self) -> [u8; WINDOW_HEADER_LEN] {
        let mut b = [0u8; WINDOW_HEADER_LEN];
        b[0..4].copy_from_slice(&WINDOW_MAGIC.to_le_bytes());
        b[4..8].copy_from_slice(&self.window.to_le_bytes());
        b[8..16].copy_from_slice(&self.epoch.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8; WINDOW_HEADER_LEN]) -> Result<WindowHeader, WireError> {
        let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        if u32_at(0) != WINDOW_MAGIC {
            return Err(WireError::Protocol("not a window stream"));
        }
        let mut e = [0u8; 8];
        e.copy_from_slice(&b[8..16]);
        Ok(WindowHeader { window: u32_at(4), epoch: u64::from_le_bytes(e) })
    }
}

/// An ack datagram: `tag u8 | window u32 | epoch u64 | version u64`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ack {
    /// The viewer holds `(epoch, version)` of `window`.
    Holds { window: u32, epoch: u64, version: u64 },
    /// The viewer's copy of `window` is not the host's: send it whole.
    Whole { window: u32 },
}

pub const ACK_LEN: usize = 21;

impl Ack {
    pub fn encode(&self) -> [u8; ACK_LEN] {
        let mut b = [0u8; ACK_LEN];
        match *self {
            Ack::Holds { window, epoch, version } => {
                b[0] = 1;
                b[1..5].copy_from_slice(&window.to_le_bytes());
                b[5..13].copy_from_slice(&epoch.to_le_bytes());
                b[13..21].copy_from_slice(&version.to_le_bytes());
            }
            Ack::Whole { window } => {
                b[0] = 2;
                b[1..5].copy_from_slice(&window.to_le_bytes());
            }
        }
        b
    }

    pub fn decode(b: &[u8]) -> Result<Ack, WireError> {
        if b.len() != ACK_LEN {
            return Err(WireError::Protocol("ack of the wrong length"));
        }
        let window = u32::from_le_bytes([b[1], b[2], b[3], b[4]]);
        let mut e = [0u8; 8];
        let mut v = [0u8; 8];
        e.copy_from_slice(&b[5..13]);
        v.copy_from_slice(&b[13..21]);
        match b[0] {
            1 => Ok(Ack::Holds { window, epoch: u64::from_le_bytes(e), version: u64::from_le_bytes(v) }),
            2 => Ok(Ack::Whole { window }),
            _ => Err(WireError::Protocol("unknown ack")),
        }
    }
}

/// Most windows a resume may name.
pub const MAX_RESUME: usize = 64;

/// Control bodies (inside `Msg::App`): `tag u8`, then fixed fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    /// Viewer, right after the welcome: what it still holds from an
    /// earlier connection of this session, so the host sends only what
    /// changed since (§12). Empty for a fresh viewer.
    Resume(Vec<(u32, u64, u64)>),
    /// Host: `window` is gone.
    Closed(u32),
}

impl Control {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::new();
        match self {
            Control::Resume(w) => {
                b.push(1);
                b.extend_from_slice(&(w.len().min(MAX_RESUME) as u32).to_le_bytes());
                for (win, e, v) in w.iter().take(MAX_RESUME) {
                    b.extend_from_slice(&win.to_le_bytes());
                    b.extend_from_slice(&e.to_le_bytes());
                    b.extend_from_slice(&v.to_le_bytes());
                }
            }
            Control::Closed(w) => {
                b.push(2);
                b.extend_from_slice(&w.to_le_bytes());
            }
        }
        b
    }

    pub fn decode(b: &[u8]) -> Result<Control, WireError> {
        let bad = || WireError::Protocol("bad control body");
        let u32_at = |i: usize| -> Result<u32, WireError> {
            let s = b.get(i..i + 4).ok_or_else(bad)?;
            Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
        };
        let u64_at = |i: usize| -> Result<u64, WireError> {
            let s = b.get(i..i + 8).ok_or_else(bad)?;
            let mut a = [0u8; 8];
            a.copy_from_slice(s);
            Ok(u64::from_le_bytes(a))
        };
        match b.first() {
            Some(1) => {
                let n = u32_at(1)? as usize;
                if n > MAX_RESUME || b.len() != 5 + n * 20 {
                    return Err(bad());
                }
                let mut w = Vec::with_capacity(n);
                for i in 0..n {
                    let at = 5 + i * 20;
                    w.push((u32_at(at)?, u64_at(at + 4)?, u64_at(at + 12)?));
                }
                Ok(Control::Resume(w))
            }
            Some(2) if b.len() == 5 => Ok(Control::Closed(u32_at(1)?)),
            _ => Err(bad()),
        }
    }
}
