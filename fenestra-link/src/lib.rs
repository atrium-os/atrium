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
//! `Layout` where the windows are, bottom to top, and whether the host
//! shows a whole desktop (a WM runs) or each app on its own (§3.4). `Chrome` is frescod's
//! own layer and the background ([`chrome`]), checked in full on decode.

use std::io::{self, Read, Write};

pub mod chrome;

pub const HEADER: usize = 40;
/// The largest `Data` payload (the broker splits larger differences).
pub const MAX_DATA: usize = 4 << 20;
/// The most windows a `Layout` lists.
pub const MAX_LAYOUT: usize = 256;
const PLACED: usize = 28;
/// The largest screen side a `Layout` may give.
pub const MAX_SCREEN: u32 = 65536;
/// A viewer's output (`Output`): each side in device pixels, and its scale
/// in thousandths (fenestra.md §4.1).
pub const OUTPUT_SIDE: std::ops::RangeInclusive<u32> = 64..=8192;
pub const OUTPUT_SCALE: std::ops::RangeInclusive<u32> = 500..=4000;
/// The longest `Viewer` label.
pub const MAX_VIEWER: usize = 64;

/// Whether `s` may be a `Viewer` label: printable ASCII, not empty, at
/// most [`MAX_VIEWER`] bytes.
pub fn viewer_label_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_VIEWER && s.bytes().all(|b| (0x20..0x7f).contains(&b))
}

pub type Version = (u64, u64);

/// One window on the host's screen, in its pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placed {
    pub window: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// For a slot or a pop-up, the top-level window it belongs to, placed
    /// before it in the same layout; 0 for a top-level window.
    pub parent: u32,
    /// [`placed_kind`].
    pub kind: u8,
}

/// What a placed window is to its parent (fenestra.md §3.4): a per-app
/// viewer draws a slot inside its parent's window and a pop-up as a
/// window of its own against the parent's.
pub mod placed_kind {
    pub const TOP: u8 = 0;
    pub const SLOT: u8 = 1;
    pub const POPUP: u8 = 2;
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
    /// `desktop`: a WM runs on the host, so a viewer shows the whole
    /// desktop; without one each app window is a window of its own on the
    /// viewer (fenestra.md §3.4).
    Layout { screen: (u32, u32), desktop: bool, windows: Vec<Placed> },
    /// frescod → fenestrad: its trusted chrome and the background.
    Chrome(chrome::Chrome),
    /// fenestrad → frescod: the viewer's output, the size and scale its
    /// remote output should have (§4.1).
    Output { width: u32, height: u32, scale_milli: u32 },
    /// fenestrad → frescod: the viewer's input, in the host screen's
    /// pixels, routed as frescod routes its own input devices'.
    Input(Input),
    /// fenestrad → frescod: who is viewing, as the host's own records name
    /// them (the SSH key's comment in the person's authorized_keys, else
    /// the address they came from), for the seat's viewed screen (§11.4).
    /// Printable ASCII, 1 to [`MAX_VIEWER`] bytes.
    Viewer { label: String },
    /// frescod → fenestrad: the person at the host's seat pressed
    /// Disconnect on the screen it shows while viewed (§11.4): end every
    /// session now.
    Disconnect,
}

/// A viewer's input event (fenestra.md §9).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Input {
    /// HID usage, pressed, HID modifier byte.
    Key { usage: u16, pressed: bool, modifiers: u8 },
    /// The pointer to `(x, y)` on the host's screen.
    Motion { x: f32, y: f32 },
    /// The pointer to `(x, y)` inside `window` (its pixels from its top
    /// left): per-app sessions, where windows the host stacks may overlap
    /// but are apart on the viewer. Buttons and scrolls then go to it.
    MotionIn { window: u32, x: f32, y: f32 },
    /// Button 1 (primary) to 5, at the pointer.
    Button { button: u8, pressed: bool },
    /// Scroll at the pointer, in pixels.
    Scroll { dx: f32, dy: f32 },
    /// Per-app sessions: the person is in `window` on the viewer; keys
    /// go to it (focus and raise on the host).
    Focus { window: u32 },
    /// Per-app sessions: the person resized `window` on the viewer to
    /// `width` x `height` device pixels (fenestra.md §6.4).
    Resize { window: u32, width: u32, height: u32 },
}

impl Eq for Input {}

/// The most a scroll may move in one event, pixels.
pub const MAX_SCROLL: f32 = 10_000.0;

mod kind {
    pub const SESSION: u32 = 1;
    pub const WANT: u32 = 2;
    pub const INPUT: u32 = 3;
    pub const OUTPUT: u32 = 4;
    pub const CHANGED: u32 = 0x10;
    pub const DATA: u32 = 0x11;
    pub const END: u32 = 0x12;
    pub const LAYOUT: u32 = 0x13;
    pub const CHROME: u32 = 0x14;
    pub const DISCONNECT: u32 = 0x15;
    pub const VIEWER: u32 = 5;
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
            Rec::Input(i) => {
                let (sub, flag, a, b) = match *i {
                    Input::Key { usage, pressed, modifiers } => (1, pressed as u32, u64::from(usage) | u64::from(modifiers) << 16, 0),
                    Input::Motion { x, y } => (2, 0, u64::from(x.to_bits()), u64::from(y.to_bits())),
                    Input::Button { button, pressed } => (3, pressed as u32, u64::from(button), 0),
                    Input::Scroll { dx, dy } => (4, 0, u64::from(dx.to_bits()), u64::from(dy.to_bits())),
                    Input::MotionIn { x, y, .. } => (5, 0, u64::from(x.to_bits()), u64::from(y.to_bits())),
                    Input::Focus { .. } => (6, 0, 0, 0),
                    Input::Resize { width, height, .. } => (7, 0, u64::from(width) | u64::from(height) << 32, 0),
                };
                let window = match *i {
                    Input::MotionIn { window, .. } | Input::Focus { window } | Input::Resize { window, .. } => window,
                    _ => 0,
                };
                Head { kind: INPUT, window, ticket: sub, flag, a, b, len: 0 }
            }
            Rec::Output { width, height, scale_milli } => {
                Head { kind: OUTPUT, window: 0, ticket: 0, flag: 0, a: u64::from(*width) | u64::from(*height) << 32, b: u64::from(*scale_milli), len: 0 }
            }
            Rec::Layout { screen, desktop, windows } => {
                for p in windows {
                    payload.extend_from_slice(&p.window.to_le_bytes());
                    payload.extend_from_slice(&p.x.to_le_bytes());
                    payload.extend_from_slice(&p.y.to_le_bytes());
                    payload.extend_from_slice(&p.width.to_le_bytes());
                    payload.extend_from_slice(&p.height.to_le_bytes());
                    payload.extend_from_slice(&p.parent.to_le_bytes());
                    payload.extend_from_slice(&u32::from(p.kind).to_le_bytes());
                }
                Head { kind: LAYOUT, window: 0, ticket: 0, flag: *desktop as u32, a: u64::from(screen.0), b: u64::from(screen.1), len: payload.len() as u32 }
            }
            Rec::Chrome(c) => {
                payload = c.encode();
                Head { kind: CHROME, window: 0, ticket: 0, flag: 0, a: 0, b: 0, len: payload.len() as u32 }
            }
            Rec::Disconnect => Head { kind: DISCONNECT, window: 0, ticket: 0, flag: 0, a: 0, b: 0, len: 0 },
            Rec::Viewer { label } => {
                payload.extend_from_slice(label.as_bytes());
                Head { kind: VIEWER, window: 0, ticket: 0, flag: 0, a: 0, b: 0, len: payload.len() as u32 }
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
            kind::CHROME => chrome::MAX_BODY,
            kind::VIEWER => MAX_VIEWER,
            kind::SESSION | kind::WANT | kind::CHANGED | kind::END | kind::INPUT | kind::OUTPUT | kind::DISCONNECT => 0,
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
            INPUT => {
                unused(h.window == 0 || (5..=7).contains(&h.ticket))?;
                let f32_of = |v: u64| -> Result<f32, Refused> {
                    let v = u32::try_from(v).map_err(|_| Refused("unused field not zero"))?;
                    let f = f32::from_bits(v);
                    if f.is_finite() { Ok(f) } else { Err(Refused("non-finite input")) }
                };
                Rec::Input(match h.ticket {
                    1 => {
                        unused(h.a >> 24 == 0 && h.b == 0)?;
                        Input::Key { usage: h.a as u16, pressed: flag(h.flag)?, modifiers: (h.a >> 16) as u8 }
                    }
                    2 => {
                        unused(h.flag == 0)?;
                        let (x, y) = (f32_of(h.a)?, f32_of(h.b)?);
                        let range = 0.0..MAX_SCREEN as f32;
                        if !range.contains(&x) || !range.contains(&y) {
                            return Err(Refused("pointer off any screen"));
                        }
                        Input::Motion { x, y }
                    }
                    3 => {
                        unused(h.b == 0)?;
                        if !(1..=5).contains(&h.a) {
                            return Err(Refused("button out of range"));
                        }
                        Input::Button { button: h.a as u8, pressed: flag(h.flag)? }
                    }
                    4 => {
                        unused(h.flag == 0)?;
                        let (dx, dy) = (f32_of(h.a)?, f32_of(h.b)?);
                        if dx.abs() > MAX_SCROLL || dy.abs() > MAX_SCROLL {
                            return Err(Refused("scroll too large"));
                        }
                        Input::Scroll { dx, dy }
                    }
                    5 => {
                        unused(h.flag == 0)?;
                        let (x, y) = (f32_of(h.a)?, f32_of(h.b)?);
                        let range = 0.0..MAX_SCREEN as f32;
                        if !range.contains(&x) || !range.contains(&y) {
                            return Err(Refused("pointer off the window"));
                        }
                        Input::MotionIn { window: window(h.window)?, x, y }
                    }
                    6 => {
                        unused(h.flag == 0 && h.a == 0 && h.b == 0)?;
                        Input::Focus { window: window(h.window)? }
                    }
                    7 => {
                        unused(h.flag == 0 && h.b == 0)?;
                        let (width, height) = (h.a as u32, (h.a >> 32) as u32);
                        if !OUTPUT_SIDE.contains(&width) || !OUTPUT_SIDE.contains(&height) {
                            return Err(Refused("window size"));
                        }
                        Input::Resize { window: window(h.window)?, width, height }
                    }
                    _ => return Err(Refused("unknown input")),
                })
            }
            LAYOUT => {
                unused(h.window == 0 && h.ticket == 0)?;
                let desktop = flag(h.flag)?;
                let side = |v: u64| if (1..=u64::from(MAX_SCREEN)).contains(&v) { Ok(v as u32) } else { Err(Refused("screen size")) };
                let screen = (side(h.a)?, side(h.b)?);
                if payload.len() % PLACED != 0 {
                    return Err(Refused("layout length"));
                }
                let mut windows = Vec::with_capacity(payload.len() / PLACED);
                for c in payload.chunks_exact(PLACED) {
                    let w = |i: usize| u32::from_le_bytes([c[i], c[i + 1], c[i + 2], c[i + 3]]);
                    let kind = u8::try_from(w(24)).ok().filter(|k| *k <= placed_kind::POPUP).ok_or(Refused("placed kind"))?;
                    let p = Placed { window: window(w(0))?, x: w(4) as i32, y: w(8) as i32, width: w(12), height: w(16), parent: w(20), kind };
                    if p.width == 0 || p.height == 0 || p.width > MAX_SCREEN || p.height > MAX_SCREEN {
                        return Err(Refused("window size"));
                    }
                    // Within a screen's reach either way, so offsets between
                    // two placed windows never overflow an i32.
                    if p.x.unsigned_abs() > MAX_SCREEN || p.y.unsigned_abs() > MAX_SCREEN {
                        return Err(Refused("window position"));
                    }
                    // A top-level window has no parent; a slot's or pop-up's is
                    // a top-level window placed before it.
                    let parent_ok = match kind {
                        placed_kind::TOP => p.parent == 0,
                        _ => windows.iter().any(|q: &Placed| q.window == p.parent && q.kind == placed_kind::TOP),
                    };
                    if !parent_ok {
                        return Err(Refused("a placed window's parent"));
                    }
                    if windows.iter().any(|q: &Placed| q.window == p.window) {
                        return Err(Refused("a window placed twice"));
                    }
                    windows.push(p);
                }
                Rec::Layout { screen, desktop, windows }
            }
            OUTPUT => {
                unused(h.window == 0 && h.ticket == 0 && h.flag == 0)?;
                let (width, height) = (h.a as u32, (h.a >> 32) as u32);
                let scale_milli = u32::try_from(h.b).map_err(|_| Refused("output scale"))?;
                if !OUTPUT_SIDE.contains(&width) || !OUTPUT_SIDE.contains(&height) || !OUTPUT_SCALE.contains(&scale_milli) {
                    return Err(Refused("output size or scale"));
                }
                Rec::Output { width, height, scale_milli }
            }
            CHROME => {
                unused(h.window == 0 && h.ticket == 0 && h.flag == 0 && h.a == 0 && h.b == 0)?;
                Rec::Chrome(chrome::Chrome::decode(payload).map_err(|e| Refused(e.0))?)
            }
            VIEWER => {
                unused(h.window == 0 && h.ticket == 0 && h.flag == 0 && h.a == 0 && h.b == 0)?;
                let label = std::str::from_utf8(payload).ok().filter(|l| viewer_label_ok(l)).ok_or(Refused("viewer label"))?;
                Rec::Viewer { label: label.to_string() }
            }
            DISCONNECT => {
                unused(h.window == 0 && h.ticket == 0 && h.flag == 0 && h.a == 0 && h.b == 0)?;
                Rec::Disconnect
            }
            _ => return Err(Refused("unknown kind")),
        })
    }

    /// Records the broker sends; it takes only the others.
    pub fn from_broker(&self) -> bool {
        matches!(self, Rec::Changed { .. } | Rec::Data { .. } | Rec::End { .. } | Rec::Layout { .. } | Rec::Chrome(_) | Rec::Disconnect)
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
