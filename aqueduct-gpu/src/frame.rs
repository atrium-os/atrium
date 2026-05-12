//! Frame command stream framing — packed records of `FrameOp` +
//! body bytes, used inside `SubmitFramePayload::command_buf`.
//!
//! Each record carries a fixed 8-byte header:
//!
//! ```text
//!   offset  size  field
//!   0       2     op       (u16, FrameOp::as_u16)
//!   2       1     flags    (currently reserved; must be 0)
//!   3       1     _pad     (must be 0)
//!   4       4     length   (u32, total record bytes including header)
//! ```
//!
//! Followed by `length - 8` bytes of op-specific body. Bodies are
//! laid out as plain little-endian C structs (no postcard / serde
//! framing — at this layer we are on the hot path and want
//! memcpy-friendly encoding).
//!
//! The bodies are not yet typed in this crate; Phase 1 ships the
//! framing machinery and lets clients write raw bytes per op.
//! Phase 2 lands the typed body builders alongside the Vulkan ICD
//! work, where the same Vulkan command-buffer recording surface
//! benefits from strongly-typed encoders.
//!
//! See `docs/spec/aqueduct-gpu.md` §5 for the frame-op table.

use thiserror::Error;

use crate::opcodes::FrameOp;

/// Header size in bytes (op:u16 + flags:u8 + _pad:u8 + length:u32).
pub const RECORD_HEADER_LEN: usize = 8;

/// Build a frame command stream incrementally, then take the
/// underlying byte buffer to hand to `SubmitFramePayload::command_buf`.
///
/// The builder owns a growable `Vec<u8>`; each `push_*` call appends
/// a record header + the caller-supplied body. The builder enforces
/// only that:
///
/// - The body length fits in u32 (i.e. ≤ ~4 GiB; well-bounded for
///   any realistic frame).
/// - The total command-stream size stays under the per-connection
///   `max_frame_bytes` cap negotiated at handshake (caller-supplied
///   here; the client wires it up).
///
/// Semantic validity (e.g., "every `BeginRenderPass` is paired with
/// an `EndRenderPass`", "draws only between Begin/End") is not
/// enforced at this layer. The host endpoint's parser catches
/// violations during dispatch and surfaces them as
/// `OP_GPU_VALIDATION_ERR`.
#[derive(Debug, Clone)]
pub struct FrameBuilder {
    buf: Vec<u8>,
    cap: u32,
}

/// Errors that can occur while building or decoding a frame stream.
#[derive(Debug, Error)]
pub enum FrameDecodeError {
    /// The buffer ran out of bytes mid-record.
    #[error("truncated record at offset {0}")]
    Truncated(usize),
    /// A record's `length` field is smaller than the minimum header.
    #[error("invalid record length {length} at offset {offset}")]
    InvalidLength {
        /// Position in the buffer.
        offset: usize,
        /// The bad length field.
        length: u32,
    },
    /// Recognised but malformed: opcode unknown to this protocol version.
    #[error("unknown frame op {0:#06x} at offset {1}")]
    UnknownOp(u16, usize),
    /// The reserved flags byte was non-zero.
    #[error("non-zero reserved flags byte at offset {0}")]
    InvalidFlags(usize),
    /// Buffer would grow past the configured cap.
    #[error("frame command stream would exceed cap of {cap} bytes")]
    OverCap {
        /// The cap the builder was configured with.
        cap: u32,
    },
}

impl FrameBuilder {
    /// Create a new builder with the given maximum buffer size in
    /// bytes (typically `HandshakeResponse::max_frame_bytes`).
    pub fn new(max_bytes: u32) -> Self {
        Self { buf: Vec::new(), cap: max_bytes }
    }

    /// Append a record. Returns an error if appending would exceed
    /// the cap or if `body.len() + HEADER_LEN > u32::MAX`.
    pub fn push(&mut self, op: FrameOp, body: &[u8]) -> Result<(), FrameDecodeError> {
        let total = (body.len() as u64) + RECORD_HEADER_LEN as u64;
        if total > u32::MAX as u64 {
            return Err(FrameDecodeError::InvalidLength {
                offset: self.buf.len(),
                length: u32::MAX,
            });
        }
        if (self.buf.len() as u64) + total > self.cap as u64 {
            return Err(FrameDecodeError::OverCap { cap: self.cap });
        }
        let total = total as u32;

        // 8-byte header, little-endian.
        self.buf.extend_from_slice(&op.as_u16().to_le_bytes());
        self.buf.push(0); // flags
        self.buf.push(0); // _pad
        self.buf.extend_from_slice(&total.to_le_bytes());
        self.buf.extend_from_slice(body);
        Ok(())
    }

    /// Current size of the command stream in bytes.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether the builder has any records yet.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Clear all records; the buffer's capacity is preserved so
    /// subsequent frames don't re-allocate.
    pub fn clear(&mut self) {
        self.buf.clear();
    }

    /// Consume the builder and return the underlying buffer.
    pub fn into_buf(self) -> Vec<u8> {
        self.buf
    }

    /// Borrow the currently-accumulated buffer without consuming
    /// the builder. Useful when a caller needs to peek at the
    /// in-progress record stream (e.g. cmdbuf-recording verification
    /// tests in atrium-vk-icd) while keeping the builder live for
    /// further `push` calls.
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    /// Adopt an already-built byte buffer (e.g. one returned by a
    /// prior `into_buf()` that the caller deduplicated against the
    /// previous frame, or a frame replayed from disk). The buffer
    /// MUST be a valid record stream; the builder does not
    /// re-validate. Future `push` calls append normally.
    pub fn from_bytes(max_bytes: u32, buf: Vec<u8>) -> Self {
        Self { buf, cap: max_bytes }
    }

    /// Borrow the underlying buffer (e.g., for benchmarking, or to
    /// hand a slice to `SubmitFramePayload::command_buf` without
    /// consuming the builder).
    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }
}

/// Walk a frame command stream record-by-record.
///
/// Used host-side to dispatch each frame op against the backend.
/// `next()` returns `Ok(Some((op, body)))` per record, `Ok(None)`
/// at clean end-of-stream, or `Err(...)` on malformed input.
#[derive(Debug)]
pub struct FrameDecoder<'a> {
    buf: &'a [u8],
    cursor: usize,
}

impl<'a> FrameDecoder<'a> {
    /// Create a decoder over the given command stream.
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, cursor: 0 }
    }

    /// Bytes already consumed.
    pub fn position(&self) -> usize {
        self.cursor
    }

    /// Bytes remaining.
    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.cursor)
    }

    /// Read the next record. Returns the typed [`FrameOp`] and a
    /// reference into the underlying buffer for the body. `Ok(None)`
    /// at clean end-of-stream.
    #[allow(clippy::type_complexity)]
    pub fn next(&mut self) -> Result<Option<(FrameOp, &'a [u8])>, FrameDecodeError> {
        if self.cursor == self.buf.len() {
            return Ok(None);
        }
        if self.buf.len() - self.cursor < RECORD_HEADER_LEN {
            return Err(FrameDecodeError::Truncated(self.cursor));
        }

        let header = &self.buf[self.cursor..self.cursor + RECORD_HEADER_LEN];
        let op_u16 = u16::from_le_bytes([header[0], header[1]]);
        let flags = header[2];
        let pad = header[3];
        let length = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);

        if flags != 0 || pad != 0 {
            return Err(FrameDecodeError::InvalidFlags(self.cursor));
        }
        if (length as usize) < RECORD_HEADER_LEN {
            return Err(FrameDecodeError::InvalidLength {
                offset: self.cursor,
                length,
            });
        }
        if self.buf.len() - self.cursor < length as usize {
            return Err(FrameDecodeError::Truncated(self.cursor));
        }
        let op = FrameOp::from_u16(op_u16)
            .ok_or(FrameDecodeError::UnknownOp(op_u16, self.cursor))?;

        let body_start = self.cursor + RECORD_HEADER_LEN;
        let body_end   = self.cursor + length as usize;
        let body = &self.buf[body_start..body_end];

        self.cursor = body_end;
        Ok(Some((op, body)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_builder() {
        let b = FrameBuilder::new(64);
        assert_eq!(b.len(), 0);
        assert!(b.is_empty());
    }

    #[test]
    fn single_record_roundtrip() {
        let mut b = FrameBuilder::new(64);
        b.push(FrameOp::Draw, &[1, 2, 3, 4]).unwrap();
        let buf = b.into_buf();
        assert_eq!(buf.len(), RECORD_HEADER_LEN + 4);

        let mut d = FrameDecoder::new(&buf);
        let (op, body) = d.next().unwrap().unwrap();
        assert_eq!(op, FrameOp::Draw);
        assert_eq!(body, &[1, 2, 3, 4]);
        assert!(d.next().unwrap().is_none());
    }

    #[test]
    fn multi_record_sequence() {
        let mut b = FrameBuilder::new(256);
        b.push(FrameOp::BeginRenderPass, &[0xAA; 8]).unwrap();
        b.push(FrameOp::BindPipeline,    &[0xBB; 4]).unwrap();
        b.push(FrameOp::Draw,            &[0xCC; 16]).unwrap();
        b.push(FrameOp::EndRenderPass,   &[]).unwrap();
        let buf = b.into_buf();

        let mut d = FrameDecoder::new(&buf);
        let (op, _) = d.next().unwrap().unwrap();
        assert_eq!(op, FrameOp::BeginRenderPass);
        let (op, _) = d.next().unwrap().unwrap();
        assert_eq!(op, FrameOp::BindPipeline);
        let (op, body) = d.next().unwrap().unwrap();
        assert_eq!(op, FrameOp::Draw);
        assert_eq!(body.len(), 16);
        let (op, body) = d.next().unwrap().unwrap();
        assert_eq!(op, FrameOp::EndRenderPass);
        assert_eq!(body.len(), 0);
        assert!(d.next().unwrap().is_none());
    }

    #[test]
    fn cap_enforcement() {
        let mut b = FrameBuilder::new(20);
        b.push(FrameOp::Draw, &[0; 4]).unwrap();
        // header (8) + 4-byte body = 12; another 12 = 24, over cap.
        let r = b.push(FrameOp::Draw, &[0; 4]);
        assert!(matches!(r, Err(FrameDecodeError::OverCap { .. })));
    }

    #[test]
    fn decoder_rejects_truncated() {
        let mut b = FrameBuilder::new(64);
        b.push(FrameOp::Draw, &[1, 2, 3, 4]).unwrap();
        let mut buf = b.into_buf();
        buf.pop(); // chop the last body byte
        let mut d = FrameDecoder::new(&buf);
        let r = d.next();
        assert!(matches!(r, Err(FrameDecodeError::Truncated(_))));
    }

    #[test]
    fn decoder_rejects_unknown_op() {
        // Hand-craft an envelope with an opcode we don't recognise.
        let mut buf = vec![];
        buf.extend_from_slice(&0xFFFFu16.to_le_bytes());
        buf.push(0); // flags
        buf.push(0); // pad
        buf.extend_from_slice(&(RECORD_HEADER_LEN as u32).to_le_bytes());
        let mut d = FrameDecoder::new(&buf);
        let r = d.next();
        assert!(matches!(r, Err(FrameDecodeError::UnknownOp(0xFFFF, 0))));
    }

    #[test]
    fn decoder_rejects_invalid_flags() {
        let mut buf = vec![];
        buf.extend_from_slice(&FrameOp::Draw.as_u16().to_le_bytes());
        buf.push(0x01); // non-zero flags
        buf.push(0);
        buf.extend_from_slice(&(RECORD_HEADER_LEN as u32).to_le_bytes());
        let mut d = FrameDecoder::new(&buf);
        let r = d.next();
        assert!(matches!(r, Err(FrameDecodeError::InvalidFlags(0))));
    }
}
