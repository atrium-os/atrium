//! The frame protocol between the broker's host and a document worker.
//!
//! ★★ ONE PROTOCOL, ONE MODULE, BOTH ENDS. The same argument that produced
//! `navigator-dom`: two implementations of a format eventually disagree about
//! the same bytes, and a disagreement across a jail boundary is the worst
//! place to find one.
//!
//! ★★★ AND THE WORKER'S SIDE OF THIS PIPE IS UNTRUSTED. The worker is jailed
//! *because we assume it can be compromised* (spec §2), so everything it
//! writes back is attacker-controlled: the length prefix included. A host
//! that allocated whatever a worker's header asked for would have handed the
//! attacker the broker's memory — the one process holding capabilities. Every
//! read here is bounded before a single byte is allocated.
//!
//! Frames are `TAG LEN\n` followed by exactly LEN bytes. Text header so a
//! failure is legible in a log; byte payload so a document passes through
//! unmangled.

use std::io::{BufRead, BufReader, Read, Write};

/// ★ The largest frame either side will accept. A recording is bounded by
/// `Limits::max_total_bytes` (16 MiB); this is that plus room for a header
/// and a serialized document coming back, and NOT a round number chosen to
/// look generous — a limit far above anything real is not protection.
pub const MAX_FRAME: usize = 24 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub tag: String,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(tag: &str, payload: impl Into<Vec<u8>>) -> Self {
        Frame { tag: tag.to_string(), payload: payload.into() }
    }
    pub fn text(&self) -> String { String::from_utf8_lossy(&self.payload).into_owned() }
}

#[derive(Debug)]
pub enum WireError {
    /// The peer closed. Ordinary at shutdown, and a crash otherwise — the
    /// caller knows which it expected.
    Closed,
    Io(std::io::Error),
    /// ★ Malformed framing from the peer. Named separately from `Io` because
    /// a worker that sends nonsense is a different event from a broken pipe:
    /// one is a bug or an attack, the other is a process that went away.
    Malformed(String),
    /// The peer asked to send more than `MAX_FRAME`.
    TooLarge { announced: usize },
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::Closed => write!(f, "worker closed the connection"),
            WireError::Io(e) => write!(f, "io: {e}"),
            WireError::Malformed(w) => write!(f, "malformed frame: {w}"),
            WireError::TooLarge { announced } =>
                write!(f, "frame of {announced} bytes exceeds the limit"),
        }
    }
}

impl From<std::io::Error> for WireError {
    fn from(e: std::io::Error) -> Self { WireError::Io(e) }
}

pub fn write_frame(w: &mut impl Write, f: &Frame) -> Result<(), WireError> {
    write_frame_limited(w, f, MAX_FRAME)
}

/// `write_frame` for the one frame that may be larger: a worker's pixel reply.
pub fn write_frame_limited(w: &mut impl Write, f: &Frame, max: usize) -> Result<(), WireError> {
    if f.payload.len() > max.min(MAX_PIXEL_FRAME) {
        return Err(WireError::TooLarge { announced: f.payload.len() });
    }
    write!(w, "{} {}\n", f.tag, f.payload.len())?;
    w.write_all(&f.payload)?;
    w.flush()?;
    Ok(())
}

pub fn read_frame(r: &mut BufReader<impl Read>) -> Result<Frame, WireError> {
    read_frame_limited(r, MAX_FRAME)
}

/// The largest reply to an image request: the document's whole decoded-image
/// budget plus a header. ★ Not the default for every frame: a pixel reply
/// is the one frame allowed to be this large, and only while the broker is
/// waiting for one (`read_frame_limited`, with the bound the broker set for
/// that request).
pub const MAX_PIXEL_FRAME: usize = navigator_dom::profile::MAX_DECODED_IMAGE_BYTES + 64;

/// `read_frame` with a caller-chosen ceiling, which may not exceed
/// `MAX_PIXEL_FRAME`.
pub fn read_frame_limited(r: &mut BufReader<impl Read>, max: usize) -> Result<Frame, WireError> {
    let max = max.min(MAX_PIXEL_FRAME);
    let mut header = String::new();
    // ★ Bounded even here. `read_line` on a hostile peer that never sends a
    // newline grows this String until the broker dies, which is a denial of
    // service that costs the attacker one byte a second.
    let n = r.take(256).read_line(&mut header)?;
    if n == 0 { return Err(WireError::Closed) }
    let header = header.trim_end();
    let (tag, len) = header.split_once(' ')
        .ok_or_else(|| WireError::Malformed(format!("no length in {header:?}")))?;
    let len: usize = len.parse()
        .map_err(|_| WireError::Malformed(format!("bad length in {header:?}")))?;
    // ★ CHECKED BEFORE THE ALLOCATION, not after the read.
    if len > max { return Err(WireError::TooLarge { announced: len }) }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok(Frame { tag: tag.to_string(), payload })
}

// The verbs. Kept as constants so a typo is a compile error on both sides
// rather than a mysterious `Malformed` at runtime.
pub const REQ_OPEN: &str = "OPEN";
pub const REQ_NAVIGATE: &str = "NAV";
pub const REQ_BACK: &str = "BACK";
pub const REQ_FORWARD: &str = "FWD";
pub const REQ_TRIGGERS: &str = "TRIG";
pub const REQ_BYTES: &str = "SIZE";
/// The document's URL, which the broker resolves a link against.
pub const REQ_URL: &str = "URL";
/// The worker's half of a saved session: answers a `session::Trail` as JSON.
pub const REQ_SAVE: &str = "SAVE";
/// Replay a `session::Trail` onto the document this worker just opened.
pub const REQ_RESTORE: &str = "RSTR";
/// Decode one image (src/image.rs). Payload: `<address> <budget> <tw> <th>\n`
/// and the encoded bytes, where `tw th` is the painted size in device pixels
/// (`0 0`: the natural size); the answer is `<width> <height>\n` and RGBA8
/// pixels.
pub const REQ_IMAGE: &str = "IMG";
/// Whether the current document has an `<a>`/`<area>` with this href:
/// answers `yes` or `no`.
pub const REQ_HREF: &str = "HREF";
/// Progressive display (legacy-web §5.4.5.7; `progressive`). The scripted
/// conversion's readable recording: answers `ready <why>` or `kept <why>`.
pub const REQ_UPGRADE: &str = "UPGR";
/// Swap the held document in. Payload `<offset>\n<anchor path>`; answers
/// `<same|nearest|top> <offset>\n<path>` and then one offered trigger per line.
pub const REQ_SWAP: &str = "SWAP";
/// A transitions artifact. Answers `now` and one offered trigger per line,
/// `with-swap`, or `needs-swap`.
pub const REQ_ATTACH: &str = "ATCH";
/// The document's first screen (`display`). Payload: `<initial 0|1>
/// <limit_px> <cached_len>\n` and `cached_len` bytes of a normalized document
/// the broker stored (0 for none); the answer is `<normalized_len>\n`, that
/// many bytes of the normalized document this worker produced (0 for none),
/// then the NSG text.
pub const REQ_SCENE: &str = "SCN";
/// ★ The session's content source (`display::Bands`; pergola.md §10.4).
/// Payload: `<initial 0|1> <generation> <cached_len> want <y0> <y1> <zoom_64>
/// <prefetch>\n` or `… idle <bands> 0 0 0\n`, then `cached_len` bytes of the
/// stored normalized document (sent only when the worker builds its source
/// for a new generation). The answer is `display::encode_answer`'s.
pub const REQ_BANDS: &str = "BND";
pub const RSP_OK: &str = "OK";
pub const RSP_ERR: &str = "ERR";
