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
                write!(f, "frame of {announced} bytes exceeds the {MAX_FRAME} limit"),
        }
    }
}

impl From<std::io::Error> for WireError {
    fn from(e: std::io::Error) -> Self { WireError::Io(e) }
}

pub fn write_frame(w: &mut impl Write, f: &Frame) -> Result<(), WireError> {
    if f.payload.len() > MAX_FRAME {
        return Err(WireError::TooLarge { announced: f.payload.len() });
    }
    write!(w, "{} {}\n", f.tag, f.payload.len())?;
    w.write_all(&f.payload)?;
    w.flush()?;
    Ok(())
}

pub fn read_frame(r: &mut BufReader<impl Read>) -> Result<Frame, WireError> {
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
    if len > MAX_FRAME { return Err(WireError::TooLarge { announced: len }) }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok(Frame { tag: tag.to_string(), payload })
}

// The verbs. Kept as constants so a typo is a compile error on both sides
// rather than a mysterious `Malformed` at runtime.
pub const REQ_OPEN: &str = "OPEN";
pub const REQ_NAVIGATE: &str = "NAV";
pub const REQ_BACK: &str = "BACK";
pub const REQ_TRIGGERS: &str = "TRIG";
pub const REQ_BYTES: &str = "SIZE";
pub const RSP_OK: &str = "OK";
pub const RSP_ERR: &str = "ERR";
