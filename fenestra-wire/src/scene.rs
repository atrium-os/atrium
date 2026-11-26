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

/// Most windows a layout may place.
pub const MAX_LAYOUT: usize = 256;
/// The largest screen or window side a layout may give.
pub const MAX_SIDE: u32 = 65536;

/// A window on the host's screen: `(window, x, y, width, height)`.
pub type Placed = (u32, i32, i32, u32, u32);

/// Control bodies (inside `Msg::App`): `tag u8`, then fixed fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    /// Viewer, right after the welcome: what it still holds from an
    /// earlier connection of this session, so the host sends only what
    /// changed since (§12). Empty for a fresh viewer.
    Resume(Vec<(u32, u64, u64)>),
    /// Host: `window` is gone.
    Closed(u32),
    /// Host, whole-desktop sessions (§3.4): the screen's size and where
    /// its windows are, bottom to top.
    Layout { screen: (u32, u32), windows: Vec<Placed> },
    /// Viewer: input, in the host screen's pixels (whole desktop, §9).
    Input(Input),
}

/// A viewer's input event (§9).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Input {
    Key { usage: u16, pressed: bool, modifiers: u8 },
    Motion { x: f32, y: f32 },
    Button { button: u8, pressed: bool },
    Scroll { dx: f32, dy: f32 },
}

impl Eq for Input {}

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
            Control::Input(i) => match *i {
                Input::Key { usage, pressed, modifiers } => {
                    b.push(4);
                    b.extend_from_slice(&usage.to_le_bytes());
                    b.push(pressed as u8);
                    b.push(modifiers);
                }
                Input::Motion { x, y } => {
                    b.push(5);
                    b.extend_from_slice(&x.to_le_bytes());
                    b.extend_from_slice(&y.to_le_bytes());
                }
                Input::Button { button, pressed } => {
                    b.push(6);
                    b.push(button);
                    b.push(pressed as u8);
                }
                Input::Scroll { dx, dy } => {
                    b.push(7);
                    b.extend_from_slice(&dx.to_le_bytes());
                    b.extend_from_slice(&dy.to_le_bytes());
                }
            },
            Control::Layout { screen, windows } => {
                b.push(3);
                b.extend_from_slice(&screen.0.to_le_bytes());
                b.extend_from_slice(&screen.1.to_le_bytes());
                b.extend_from_slice(&(windows.len().min(MAX_LAYOUT) as u32).to_le_bytes());
                for (w, x, y, wd, ht) in windows.iter().take(MAX_LAYOUT) {
                    b.extend_from_slice(&w.to_le_bytes());
                    b.extend_from_slice(&x.to_le_bytes());
                    b.extend_from_slice(&y.to_le_bytes());
                    b.extend_from_slice(&wd.to_le_bytes());
                    b.extend_from_slice(&ht.to_le_bytes());
                }
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
            Some(4) if b.len() == 5 => {
                let pressed = match b[3] {
                    0 => false,
                    1 => true,
                    _ => return Err(bad()),
                };
                Ok(Control::Input(Input::Key { usage: u16::from_le_bytes([b[1], b[2]]), pressed, modifiers: b[4] }))
            }
            Some(5) | Some(7) if b.len() == 9 => {
                let (x, y) = (f32::from_bits(u32_at(1)?), f32::from_bits(u32_at(5)?));
                if !x.is_finite() || !y.is_finite() {
                    return Err(bad());
                }
                Ok(Control::Input(if b[0] == 5 { Input::Motion { x, y } } else { Input::Scroll { dx: x, dy: y } }))
            }
            Some(6) if b.len() == 3 => {
                let pressed = match b[2] {
                    0 => false,
                    1 => true,
                    _ => return Err(bad()),
                };
                if !(1..=5).contains(&b[1]) {
                    return Err(bad());
                }
                Ok(Control::Input(Input::Button { button: b[1], pressed }))
            }
            Some(3) => {
                let side = |v: u32| if (1..=MAX_SIDE).contains(&v) { Ok(v) } else { Err(bad()) };
                let screen = (side(u32_at(1)?)?, side(u32_at(5)?)?);
                let n = u32_at(9)? as usize;
                if n > MAX_LAYOUT || b.len() != 13 + n * 20 {
                    return Err(bad());
                }
                let mut windows: Vec<Placed> = Vec::with_capacity(n);
                for i in 0..n {
                    let at = 13 + i * 20;
                    let p = (u32_at(at)?, u32_at(at + 4)? as i32, u32_at(at + 8)? as i32, side(u32_at(at + 12)?)?, side(u32_at(at + 16)?)?);
                    if windows.iter().any(|q| q.0 == p.0) {
                        return Err(bad());
                    }
                    windows.push(p);
                }
                Ok(Control::Layout { screen, windows })
            }
            _ => Err(bad()),
        }
    }
}
