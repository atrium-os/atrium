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

// --------------------------------------------------------------
// Typed body layouts (D.1).
//
// Each frame-op body is a packed little-endian C-style struct.
// Layouts mirror the Vulkan command shape so the ICD can encode
// in one memcpy. Bodies grow only at the end; new fields land
// behind a protocol-version bump negotiated in OP_GPU_HANDSHAKE.
// --------------------------------------------------------------

/// Body of [`FrameOp::Draw`] — mirrors `vkCmdDraw`. 16 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrawCmd {
    /// Number of vertices to draw.
    pub vertex_count: u32,
    /// Number of instances to draw (1 for non-instanced).
    pub instance_count: u32,
    /// Index of the first vertex within the bound vertex buffer.
    pub first_vertex: u32,
    /// Instance ID of the first instance.
    pub first_instance: u32,
}

impl DrawCmd {
    /// Serialised body length in bytes.
    pub const SIZE: usize = 16;

    /// Encode into a fixed-size little-endian byte array.
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut b = [0u8; Self::SIZE];
        b[0..4].copy_from_slice(&self.vertex_count.to_le_bytes());
        b[4..8].copy_from_slice(&self.instance_count.to_le_bytes());
        b[8..12].copy_from_slice(&self.first_vertex.to_le_bytes());
        b[12..16].copy_from_slice(&self.first_instance.to_le_bytes());
        b
    }

    /// Decode from a body slice. Errors on length mismatch.
    pub fn from_bytes(body: &[u8]) -> Result<Self, FrameBodyError> {
        if body.len() != Self::SIZE {
            return Err(FrameBodyError::WrongLength {
                op: "Draw", expected: Self::SIZE, got: body.len(),
            });
        }
        Ok(Self {
            vertex_count:   u32::from_le_bytes(body[0..4].try_into().unwrap()),
            instance_count: u32::from_le_bytes(body[4..8].try_into().unwrap()),
            first_vertex:   u32::from_le_bytes(body[8..12].try_into().unwrap()),
            first_instance: u32::from_le_bytes(body[12..16].try_into().unwrap()),
        })
    }
}

/// Body of [`FrameOp::DrawIndexed`] — mirrors `vkCmdDrawIndexed`.
/// 20 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrawIndexedCmd {
    /// Number of indices to consume from the bound index buffer.
    pub index_count: u32,
    /// Number of instances to draw.
    pub instance_count: u32,
    /// Offset (in indices) into the bound index buffer.
    pub first_index: u32,
    /// Signed value added to each index before vertex fetch.
    pub vertex_offset: i32,
    /// Instance ID of the first instance.
    pub first_instance: u32,
}

impl DrawIndexedCmd {
    /// Serialised body length in bytes.
    pub const SIZE: usize = 20;

    /// Encode into a fixed-size little-endian byte array.
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut b = [0u8; Self::SIZE];
        b[0..4].copy_from_slice(&self.index_count.to_le_bytes());
        b[4..8].copy_from_slice(&self.instance_count.to_le_bytes());
        b[8..12].copy_from_slice(&self.first_index.to_le_bytes());
        b[12..16].copy_from_slice(&self.vertex_offset.to_le_bytes());
        b[16..20].copy_from_slice(&self.first_instance.to_le_bytes());
        b
    }

    /// Decode from a body slice. Errors on length mismatch.
    pub fn from_bytes(body: &[u8]) -> Result<Self, FrameBodyError> {
        if body.len() != Self::SIZE {
            return Err(FrameBodyError::WrongLength {
                op: "DrawIndexed", expected: Self::SIZE, got: body.len(),
            });
        }
        Ok(Self {
            index_count:    u32::from_le_bytes(body[0..4].try_into().unwrap()),
            instance_count: u32::from_le_bytes(body[4..8].try_into().unwrap()),
            first_index:    u32::from_le_bytes(body[8..12].try_into().unwrap()),
            vertex_offset:  i32::from_le_bytes(body[12..16].try_into().unwrap()),
            first_instance: u32::from_le_bytes(body[16..20].try_into().unwrap()),
        })
    }
}

/// Body of [`FrameOp::BindVertexBuf`] — mirrors
/// `vkCmdBindVertexBuffers` for a single binding slot. 16 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindVertexBufCmd {
    /// Vertex input binding slot to update.
    pub binding: u32,
    /// Resource ID of the vertex buffer.
    pub buffer_id: u32,
    /// Byte offset into the buffer for the start of vertex data.
    pub offset: u64,
}

impl BindVertexBufCmd {
    /// Serialised body length in bytes.
    pub const SIZE: usize = 16;

    /// Encode into a fixed-size little-endian byte array.
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut b = [0u8; Self::SIZE];
        b[0..4].copy_from_slice(&self.binding.to_le_bytes());
        b[4..8].copy_from_slice(&self.buffer_id.to_le_bytes());
        b[8..16].copy_from_slice(&self.offset.to_le_bytes());
        b
    }

    /// Decode from a body slice. Errors on length mismatch.
    pub fn from_bytes(body: &[u8]) -> Result<Self, FrameBodyError> {
        if body.len() != Self::SIZE {
            return Err(FrameBodyError::WrongLength {
                op: "BindVertexBuf", expected: Self::SIZE, got: body.len(),
            });
        }
        Ok(Self {
            binding:   u32::from_le_bytes(body[0..4].try_into().unwrap()),
            buffer_id: u32::from_le_bytes(body[4..8].try_into().unwrap()),
            offset:    u64::from_le_bytes(body[8..16].try_into().unwrap()),
        })
    }
}

/// Index element width for [`BindIndexBufCmd::index_type`].
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexType {
    /// 16-bit unsigned indices (`VK_INDEX_TYPE_UINT16`).
    Uint16 = 0,
    /// 32-bit unsigned indices (`VK_INDEX_TYPE_UINT32`).
    Uint32 = 1,
}

impl IndexType {
    /// Decode the wire tag.
    pub fn from_u32(v: u32) -> Option<Self> {
        match v {
            0 => Some(IndexType::Uint16),
            1 => Some(IndexType::Uint32),
            _ => None,
        }
    }
}

/// Body of [`FrameOp::BindIndexBuf`] — mirrors `vkCmdBindIndexBuffer`.
/// 16 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindIndexBufCmd {
    /// Resource ID of the index buffer.
    pub buffer_id: u32,
    /// Index element type.
    pub index_type: IndexType,
    /// Byte offset into the buffer for the start of index data.
    pub offset: u64,
}

impl BindIndexBufCmd {
    /// Serialised body length in bytes.
    pub const SIZE: usize = 16;

    /// Encode into a fixed-size little-endian byte array.
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut b = [0u8; Self::SIZE];
        b[0..4].copy_from_slice(&self.buffer_id.to_le_bytes());
        b[4..8].copy_from_slice(&(self.index_type as u32).to_le_bytes());
        b[8..16].copy_from_slice(&self.offset.to_le_bytes());
        b
    }

    /// Decode from a body slice.
    pub fn from_bytes(body: &[u8]) -> Result<Self, FrameBodyError> {
        if body.len() != Self::SIZE {
            return Err(FrameBodyError::WrongLength {
                op: "BindIndexBuf", expected: Self::SIZE, got: body.len(),
            });
        }
        let buffer_id = u32::from_le_bytes(body[0..4].try_into().unwrap());
        let it_raw    = u32::from_le_bytes(body[4..8].try_into().unwrap());
        let offset    = u64::from_le_bytes(body[8..16].try_into().unwrap());
        let index_type = IndexType::from_u32(it_raw)
            .ok_or(FrameBodyError::BadIndexType(it_raw))?;
        Ok(Self { buffer_id, index_type, offset })
    }
}

/// Body of [`FrameOp::Dispatch`] — mirrors `vkCmdDispatch`.
/// 12 bytes (`groupCountX/Y/Z` as three little-endian u32s).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispatchCmd {
    /// Number of compute workgroups in X.
    pub group_count_x: u32,
    /// Number of compute workgroups in Y.
    pub group_count_y: u32,
    /// Number of compute workgroups in Z.
    pub group_count_z: u32,
}

impl DispatchCmd {
    /// Serialised body length in bytes.
    pub const SIZE: usize = 12;

    /// Encode into a fixed-size little-endian byte array.
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut b = [0u8; Self::SIZE];
        b[0..4].copy_from_slice(&self.group_count_x.to_le_bytes());
        b[4..8].copy_from_slice(&self.group_count_y.to_le_bytes());
        b[8..12].copy_from_slice(&self.group_count_z.to_le_bytes());
        b
    }

    /// Decode from a body slice. Errors on length mismatch.
    pub fn from_bytes(body: &[u8]) -> Result<Self, FrameBodyError> {
        if body.len() != Self::SIZE {
            return Err(FrameBodyError::WrongLength {
                op: "Dispatch", expected: Self::SIZE, got: body.len(),
            });
        }
        Ok(Self {
            group_count_x: u32::from_le_bytes(body[0..4].try_into().unwrap()),
            group_count_y: u32::from_le_bytes(body[4..8].try_into().unwrap()),
            group_count_z: u32::from_le_bytes(body[8..12].try_into().unwrap()),
        })
    }
}

/// Body of [`FrameOp::SetViewport`] — mirrors a single
/// `VkViewport` entry. 24 bytes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SetViewportCmd {
    /// Viewport upper-left x in framebuffer pixels.
    pub x: f32,
    /// Viewport upper-left y in framebuffer pixels.
    pub y: f32,
    /// Viewport width in pixels.
    pub width: f32,
    /// Viewport height in pixels.
    pub height: f32,
    /// Minimum depth value (typically 0.0).
    pub min_depth: f32,
    /// Maximum depth value (typically 1.0).
    pub max_depth: f32,
}

impl SetViewportCmd {
    /// Serialised body length in bytes.
    pub const SIZE: usize = 24;

    /// Encode into a fixed-size little-endian byte array.
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut b = [0u8; Self::SIZE];
        b[0..4].copy_from_slice(&self.x.to_le_bytes());
        b[4..8].copy_from_slice(&self.y.to_le_bytes());
        b[8..12].copy_from_slice(&self.width.to_le_bytes());
        b[12..16].copy_from_slice(&self.height.to_le_bytes());
        b[16..20].copy_from_slice(&self.min_depth.to_le_bytes());
        b[20..24].copy_from_slice(&self.max_depth.to_le_bytes());
        b
    }

    /// Decode from a body slice.
    pub fn from_bytes(body: &[u8]) -> Result<Self, FrameBodyError> {
        if body.len() != Self::SIZE {
            return Err(FrameBodyError::WrongLength {
                op: "SetViewport", expected: Self::SIZE, got: body.len(),
            });
        }
        Ok(Self {
            x:         f32::from_le_bytes(body[0..4].try_into().unwrap()),
            y:         f32::from_le_bytes(body[4..8].try_into().unwrap()),
            width:     f32::from_le_bytes(body[8..12].try_into().unwrap()),
            height:    f32::from_le_bytes(body[12..16].try_into().unwrap()),
            min_depth: f32::from_le_bytes(body[16..20].try_into().unwrap()),
            max_depth: f32::from_le_bytes(body[20..24].try_into().unwrap()),
        })
    }
}

/// Errors raised by typed body decoders.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum FrameBodyError {
    /// The body slice was not the size the decoder expects.
    #[error("body for {op} has wrong length: expected {expected}, got {got}")]
    WrongLength {
        /// Op name (e.g. "Draw").
        op: &'static str,
        /// Required size in bytes.
        expected: usize,
        /// Actual size in bytes.
        got: usize,
    },
    /// `BindIndexBufCmd::index_type` was not a recognised tag.
    #[error("unknown index type {0}")]
    BadIndexType(u32),
}

// Convenience push_* methods for FrameBuilder. Each just encodes
// the body and forwards to push().
impl FrameBuilder {
    /// Encode and append a [`FrameOp::Draw`] record.
    pub fn push_draw(&mut self, cmd: DrawCmd) -> Result<(), FrameDecodeError> {
        self.push(FrameOp::Draw, &cmd.to_bytes())
    }

    /// Encode and append a [`FrameOp::DrawIndexed`] record.
    pub fn push_draw_indexed(&mut self, cmd: DrawIndexedCmd)
        -> Result<(), FrameDecodeError>
    {
        self.push(FrameOp::DrawIndexed, &cmd.to_bytes())
    }

    /// Encode and append a [`FrameOp::BindVertexBuf`] record.
    pub fn push_bind_vertex_buf(&mut self, cmd: BindVertexBufCmd)
        -> Result<(), FrameDecodeError>
    {
        self.push(FrameOp::BindVertexBuf, &cmd.to_bytes())
    }

    /// Encode and append a [`FrameOp::BindIndexBuf`] record.
    pub fn push_bind_index_buf(&mut self, cmd: BindIndexBufCmd)
        -> Result<(), FrameDecodeError>
    {
        self.push(FrameOp::BindIndexBuf, &cmd.to_bytes())
    }

    /// Encode and append a [`FrameOp::SetViewport`] record.
    pub fn push_set_viewport(&mut self, cmd: SetViewportCmd)
        -> Result<(), FrameDecodeError>
    {
        self.push(FrameOp::SetViewport, &cmd.to_bytes())
    }

    /// Encode and append a [`FrameOp::Dispatch`] record.
    pub fn push_dispatch(&mut self, cmd: DispatchCmd)
        -> Result<(), FrameDecodeError>
    {
        self.push(FrameOp::Dispatch, &cmd.to_bytes())
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
    fn draw_cmd_roundtrip() {
        let cmd = DrawCmd {
            vertex_count: 3, instance_count: 1,
            first_vertex: 0, first_instance: 0,
        };
        let bytes = cmd.to_bytes();
        assert_eq!(bytes.len(), DrawCmd::SIZE);
        assert_eq!(DrawCmd::from_bytes(&bytes).unwrap(), cmd);

        let cmd2 = DrawCmd {
            vertex_count: 0xDEAD_BEEF, instance_count: 7,
            first_vertex: 42, first_instance: 0xCAFE,
        };
        assert_eq!(DrawCmd::from_bytes(&cmd2.to_bytes()).unwrap(), cmd2);
    }

    #[test]
    fn draw_indexed_cmd_roundtrip() {
        let cmd = DrawIndexedCmd {
            index_count: 36, instance_count: 1,
            first_index: 12, vertex_offset: -8, first_instance: 0,
        };
        let bytes = cmd.to_bytes();
        assert_eq!(bytes.len(), DrawIndexedCmd::SIZE);
        assert_eq!(DrawIndexedCmd::from_bytes(&bytes).unwrap(), cmd);
    }

    #[test]
    fn bind_vertex_buf_cmd_roundtrip() {
        let cmd = BindVertexBufCmd {
            binding: 2, buffer_id: 0x1000_0042,
            offset: 0x0000_0001_2345_6789,
        };
        let bytes = cmd.to_bytes();
        assert_eq!(bytes.len(), BindVertexBufCmd::SIZE);
        assert_eq!(BindVertexBufCmd::from_bytes(&bytes).unwrap(), cmd);
    }

    #[test]
    fn bind_index_buf_cmd_roundtrip() {
        for it in [IndexType::Uint16, IndexType::Uint32] {
            let cmd = BindIndexBufCmd {
                buffer_id: 0x2000_0007, index_type: it, offset: 4096,
            };
            let bytes = cmd.to_bytes();
            assert_eq!(bytes.len(), BindIndexBufCmd::SIZE);
            assert_eq!(BindIndexBufCmd::from_bytes(&bytes).unwrap(), cmd);
        }
    }

    #[test]
    fn bind_index_buf_cmd_bad_index_type() {
        let mut bytes = BindIndexBufCmd {
            buffer_id: 1, index_type: IndexType::Uint16, offset: 0,
        }.to_bytes();
        bytes[4..8].copy_from_slice(&99u32.to_le_bytes());
        let r = BindIndexBufCmd::from_bytes(&bytes);
        assert_eq!(r, Err(FrameBodyError::BadIndexType(99)));
    }

    #[test]
    fn dispatch_cmd_roundtrip() {
        let cmd = DispatchCmd { group_count_x: 8, group_count_y: 4, group_count_z: 1 };
        let b = cmd.to_bytes();
        assert_eq!(b.len(), DispatchCmd::SIZE);
        assert_eq!(DispatchCmd::from_bytes(&b).unwrap(), cmd);
    }

    #[test]
    fn set_viewport_cmd_roundtrip() {
        let cmd = SetViewportCmd {
            x: 0.0, y: 0.0, width: 1920.0, height: 1080.0,
            min_depth: 0.0, max_depth: 1.0,
        };
        let bytes = cmd.to_bytes();
        assert_eq!(bytes.len(), SetViewportCmd::SIZE);
        assert_eq!(SetViewportCmd::from_bytes(&bytes).unwrap(), cmd);
    }

    #[test]
    fn body_decoder_rejects_wrong_length() {
        assert!(matches!(
            DrawCmd::from_bytes(&[0u8; 12]),
            Err(FrameBodyError::WrongLength { op: "Draw", expected: 16, got: 12 }),
        ));
        assert!(matches!(
            DrawIndexedCmd::from_bytes(&[0u8; 16]),
            Err(FrameBodyError::WrongLength { op: "DrawIndexed", .. }),
        ));
    }

    #[test]
    fn builder_push_typed_helpers() {
        let mut b = FrameBuilder::new(1024);
        b.push_bind_vertex_buf(BindVertexBufCmd {
            binding: 0, buffer_id: 7, offset: 0,
        }).unwrap();
        b.push_set_viewport(SetViewportCmd {
            x: 0.0, y: 0.0, width: 800.0, height: 600.0,
            min_depth: 0.0, max_depth: 1.0,
        }).unwrap();
        b.push_draw(DrawCmd {
            vertex_count: 3, instance_count: 1,
            first_vertex: 0, first_instance: 0,
        }).unwrap();
        let buf = b.into_buf();

        let mut d = FrameDecoder::new(&buf);
        let (op, body) = d.next().unwrap().unwrap();
        assert_eq!(op, FrameOp::BindVertexBuf);
        let bv = BindVertexBufCmd::from_bytes(body).unwrap();
        assert_eq!(bv.binding, 0);
        assert_eq!(bv.buffer_id, 7);

        let (op, body) = d.next().unwrap().unwrap();
        assert_eq!(op, FrameOp::SetViewport);
        let vp = SetViewportCmd::from_bytes(body).unwrap();
        assert_eq!(vp.width, 800.0);

        let (op, body) = d.next().unwrap().unwrap();
        assert_eq!(op, FrameOp::Draw);
        let dr = DrawCmd::from_bytes(body).unwrap();
        assert_eq!(dr.vertex_count, 3);

        assert!(d.next().unwrap().is_none());
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
