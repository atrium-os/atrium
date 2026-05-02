//! Wire envelope codec.
//!
//! Every atrium-rpc message rides in a 10-byte fixed header followed
//! by `length` payload bytes. Header layout (little-endian for u16/u32):
//!
//! ```text
//! offset  size  field
//!   0      1    version
//!   1      1    opcode_class
//!   2      2    op
//!   4      2    flags
//!   6      4    length
//! ```
//!
//! The codec is intentionally tiny — no allocation, no surprises.
//! Service dictionaries layer their own marshalling on top of the
//! payload bytes (postcard, hand-rolled, whatever fits).

use std::io::{self, Read, Write};

/// Current envelope version. A future version 2 must keep `version`
/// at byte 0 with a different value so unrecognised versions are
/// cleanly rejected.
pub const ENVELOPE_VERSION: u8 = 1;

/// Header byte count. Sized for a single read/write syscall on the
/// hot path (with vectored I/O grouping it together with the payload
/// when both fit in the kernel buffer).
pub const HEADER_LEN: usize = 10;

/// Per-deployment safety cap. Refuse messages larger than this on
/// receive; senders should chunk large data into UPLOAD_DATA frames
/// or use shm rendezvous instead. 16 MiB is plenty for any single
/// non-shm message in practice.
pub const MAX_PAYLOAD: u32 = 16 * 1024 * 1024;

/// Envelope flag bits (low 4 bits — class-specific bits live in the
/// upper 12).
pub mod flag {
    /// Payload contains hash references the receiver may want to
    /// resolve from cache before processing the message body.
    pub const HAS_HASH_REFS:    u16 = 1 << 0;
    /// Sender expects a response (with `IS_RESPONSE` set + matching
    /// op).
    pub const RESPONSE_EXPECTED: u16 = 1 << 1;
    /// This message IS a response to a previous `RESPONSE_EXPECTED`
    /// request.
    pub const IS_RESPONSE:      u16 = 1 << 2;
    /// Async event — never expects a response.
    pub const ASYNC_EVENT:      u16 = 1 << 3;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub version:      u8,
    pub opcode_class: u8,
    pub op:           u16,
    pub flags:        u16,
    pub length:       u32,
}

impl Header {
    pub fn new(opcode_class: u8, op: u16, flags: u16, length: u32) -> Self {
        Self { version: ENVELOPE_VERSION, opcode_class, op, flags, length }
    }

    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[0] = self.version;
        b[1] = self.opcode_class;
        b[2..4].copy_from_slice(&self.op.to_le_bytes());
        b[4..6].copy_from_slice(&self.flags.to_le_bytes());
        b[6..10].copy_from_slice(&self.length.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> io::Result<Self> {
        if b.len() < HEADER_LEN {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "header truncated",
            ));
        }
        let version      = b[0];
        let opcode_class = b[1];
        let op           = u16::from_le_bytes([b[2], b[3]]);
        let flags        = u16::from_le_bytes([b[4], b[5]]);
        let length       = u32::from_le_bytes([b[6], b[7], b[8], b[9]]);
        if version != ENVELOPE_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported envelope version {version}"),
            ));
        }
        if length > MAX_PAYLOAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("payload {length} exceeds MAX_PAYLOAD"),
            ));
        }
        Ok(Self { version, opcode_class, op, flags, length })
    }
}

/// Write a complete message (header + payload) to the writer. Single
/// `write_all` call for the header followed by the payload — leaves
/// vectored-write batching to the OS / caller's BufWriter.
pub fn write_message<W: Write>(
    w: &mut W,
    h: Header,
    payload: &[u8],
) -> io::Result<()> {
    debug_assert_eq!(h.length as usize, payload.len(),
        "header length must match payload");
    w.write_all(&h.encode())?;
    if !payload.is_empty() {
        w.write_all(payload)?;
    }
    Ok(())
}

/// Read one complete message into `payload_buf`. Returns the header
/// and the slice of `payload_buf` that holds the payload. The buffer
/// must be large enough for the announced payload; if it isn't,
/// returns `InvalidData` so callers can either grow and retry or
/// reject the message.
pub fn read_message<'a, R: Read>(
    r: &mut R,
    payload_buf: &'a mut [u8],
) -> io::Result<(Header, &'a [u8])> {
    let mut hbuf = [0u8; HEADER_LEN];
    r.read_exact(&mut hbuf)?;
    let h = Header::decode(&hbuf)?;
    let n = h.length as usize;
    if n > payload_buf.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("payload {} exceeds caller buffer {}", n, payload_buf.len()),
        ));
    }
    if n > 0 {
        r.read_exact(&mut payload_buf[..n])?;
    }
    Ok((h, &payload_buf[..n]))
}

/// Read one complete message, allocating a Vec for the payload.
/// Convenience for tools that don't want to manage a buffer pool.
pub fn read_message_alloc<R: Read>(
    r: &mut R,
) -> io::Result<(Header, Vec<u8>)> {
    let mut hbuf = [0u8; HEADER_LEN];
    r.read_exact(&mut hbuf)?;
    let h = Header::decode(&hbuf)?;
    let n = h.length as usize;
    let mut buf = vec![0u8; n];
    if n > 0 {
        r.read_exact(&mut buf)?;
    }
    Ok((h, buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn header_roundtrip() {
        let h = Header::new(2, 0x0042, flag::RESPONSE_EXPECTED, 1234);
        let bytes = h.encode();
        let h2 = Header::decode(&bytes).unwrap();
        assert_eq!(h, h2);
    }

    #[test]
    fn message_roundtrip() {
        let payload = vec![1, 2, 3, 4, 5];
        let mut sink = Vec::new();
        write_message(
            &mut sink,
            Header::new(0, 0x01, 0, payload.len() as u32),
            &payload,
        ).unwrap();
        let mut r = Cursor::new(sink);
        let (h, p) = read_message_alloc(&mut r).unwrap();
        assert_eq!(h.opcode_class, 0);
        assert_eq!(h.op, 0x01);
        assert_eq!(p, payload);
    }

    #[test]
    fn rejects_oversize() {
        let mut bad = [0u8; HEADER_LEN];
        bad[0] = ENVELOPE_VERSION;
        bad[6..10].copy_from_slice(&(MAX_PAYLOAD + 1).to_le_bytes());
        assert!(Header::decode(&bad).is_err());
    }

    #[test]
    fn rejects_unknown_version() {
        let mut bad = [0u8; HEADER_LEN];
        bad[0] = 99;
        assert!(Header::decode(&bad).is_err());
    }
}
