//! Opcode constants for `CLASS_GPU` (envelope-level) and frame-ops
//! (in-stream framing inside `OP_GPU_SUBMIT_FRAME`).
//!
//! Source of truth for the wire numbers. Any change here must land
//! in lockstep with `docs/spec/aqueduct-gpu.md` §4.3 (envelope ops)
//! and §5.1 (frame ops).
//!
//! All envelope opcodes carry `CLASS_GPU = 9` in the envelope header;
//! the `op` u16 selects within this dictionary.
//!
//! All frame-op records inside a frame command stream carry their
//! own u16 opcode + u32 length header (see `FrameOp` + the
//! [`frame`](crate::frame) module).

// ───── Envelope-level opcodes (CLASS_GPU = 9) ─────────────────────

/// Initial capability + format-rules exchange. First op on every
/// connection. Reply carries the host's supported feature flags,
/// format-rules table, and backend identification used by
/// `OP_GPU_SHADER_RESOLVE`. See `docs/spec/aqueduct-gpu.md` §4.3.
pub const OP_GPU_HANDSHAKE:       u16 = 0x0001;

// ─── Resource lifecycle ──
//
// All `_CREATE` ops use pre-assigned IDs (see `ids` module):
// the client allocates a fresh ID within its connection namespace
// and sends fire-and-forget; the host validates as it processes.
// Validation failures propagate via `OP_GPU_VALIDATION_ERR` async
// events into the next fence wait.

/// Allocate a named shared-memory region on the host. The only
/// resource-create op that returns a response (carrying the import
/// token the kmod uses for `IOC_GPU_IMPORT_REGION`). All others
/// are fire-and-forget.
pub const OP_GPU_MEMORY_CREATE:   u16 = 0x0100;
/// Destroy a memory region. Implicit on connection close for all
/// of that connection's regions.
pub const OP_GPU_MEMORY_DESTROY:  u16 = 0x0101;

/// Allocate an image with the given format, extent, usage. ID
/// pre-assigned by the client.
pub const OP_GPU_IMAGE_CREATE:    u16 = 0x0110;
/// Destroy an image.
pub const OP_GPU_IMAGE_DESTROY:   u16 = 0x0111;
/// Write pixel data directly into an image. The full payload
/// carries the bytes inline — there's no staging buffer.
///
/// **Tier-1 / debug path only.** Real GPU backends prefer
/// `OP_GPU_BUFFER_CREATE` + `FOP_COPY_BUF_TO_IMG` because the
/// staging buffer can be GPU-resident and the upload can pipeline
/// with rendering. This op carries the pixels in the aqueduct
/// envelope itself, which:
///
/// - works without Phase 1.5 kmod shared-memory support
/// - is bandwidth-inefficient for large textures
/// - is sufficient for tier-1 SW rendering and for small atlases
///   (icons, glyph atlases ≤ 16 KiB) where the buffer-staging
///   overhead would dominate
///
/// Fire-and-forget; failures surface as `OP_GPU_VALIDATION_ERR`
/// (image not found, byte count mismatch, etc.).
pub const OP_GPU_IMAGE_WRITE:     u16 = 0x0112;

/// Inline write of a sub-region of an existing image's pixels.
/// Used by atrium-text's server-side glyph rasteriser to patch
/// newly-rasterised glyphs into an atlas without re-uploading the
/// whole texture. Image must already exist via a prior
/// `OP_GPU_IMAGE_CREATE` (+ `OP_GPU_IMAGE_WRITE` for the initial
/// fill); this op only modifies the bytes inside the declared
/// `dst_rect`.
///
/// Bounds checks: `dst_x + width ≤ image.width`,
/// `dst_y + height ≤ image.height`, `pixels.len() = row_pitch × height`.
/// Failure surfaces as `OP_GPU_VALIDATION_ERR`.
pub const OP_GPU_IMAGE_WRITE_REGION: u16 = 0x0113;

/// Allocate a buffer (vertex / index / uniform / storage).
pub const OP_GPU_BUFFER_CREATE:   u16 = 0x0120;
/// Destroy a buffer.
pub const OP_GPU_BUFFER_DESTROY:  u16 = 0x0121;
/// Inline write into a buffer. Mirrors `OP_GPU_IMAGE_WRITE`'s
/// shape: a `BufferWritePayload` carries the target buffer id,
/// the byte offset, and the bytes to copy. Used during bring-up
/// before guest memory-region import is wired; ICDs will move
/// to mapped backing regions in D5+.
pub const OP_GPU_BUFFER_WRITE:    u16 = 0x0122;
/// Inline read from a buffer.  Symmetric to `OP_GPU_BUFFER_WRITE`:
/// a `BufferReadPayload` carries the source buffer id, byte offset,
/// and size; the daemon responds with a `BufferReadResponse`
/// containing the bytes.  Used by ICDs to implement
/// `vkInvalidateMappedMemoryRanges` (pull daemon-side compute
/// output back into the client's mapped pointer).  Will move to
/// shared mapped backing regions in D5+.
pub const OP_GPU_BUFFER_READ:     u16 = 0x0123;

/// Allocate a sampler.
pub const OP_GPU_SAMPLER_CREATE:  u16 = 0x0130;
/// Destroy a sampler.
pub const OP_GPU_SAMPLER_DESTROY: u16 = 0x0131;

// ─── Shaders ──
//
// Two-phase: RESOLVE (cheap, by hash) is the warm path; UPLOAD is
// the cold fallback when atrium-pkg pre-warming missed.

/// Resolve a shader by content hash + target backend. Hit returns
/// a `shader_id` immediately; miss returns a "please upload" error
/// that prompts a follow-up `OP_GPU_SHADER_UPLOAD`. See
/// `aqueduct-gpu.md` §4.1.
pub const OP_GPU_SHADER_RESOLVE:  u16 = 0x0140;
/// Upload shader bytecode (SPIR-V or NIR) and compile to the
/// negotiated backend. Cold path — should be rare on shipped apps
/// since atrium-pkg's install hook pre-compiles everything (§4.2).
pub const OP_GPU_SHADER_UPLOAD:   u16 = 0x0141;

// ─── Pipelines ──

/// Create a pipeline from a state hash + resolved shader IDs.
/// Materialised host-side; the client can immediately reference
/// the pre-assigned `pipeline_id` in subsequent frame ops.
pub const OP_GPU_PIPELINE_CREATE: u16 = 0x0150;
/// Destroy a pipeline.
pub const OP_GPU_PIPELINE_DESTROY:u16 = 0x0151;

// ─── Fences ──

/// Create a frame-granular fence. The client passes its ID to
/// `OP_GPU_SUBMIT_FRAME`; the host signals it when the frame's
/// commands complete.
pub const OP_GPU_FENCE_CREATE:    u16 = 0x0160;
/// Destroy a fence.
pub const OP_GPU_FENCE_DESTROY:   u16 = 0x0161;

// ─── Frame submission + sync ──

/// Submit a complete frame command stream. The payload carries the
/// fence ID to signal on completion and the packed frame-op stream
/// (see [`FrameOp`] and the [`frame`](crate::frame) module).
/// Fire-and-forget; completion is signalled via the fence.
pub const OP_GPU_SUBMIT_FRAME:    u16 = 0x0200;
/// Wait (with timeout) for a fence to signal. Returns whether the
/// fence is currently signalled. Apps use this to map
/// `vkWaitForFences` and equivalent native-API constructs.
pub const OP_GPU_WAIT_FENCE:      u16 = 0x0201;

// ─── Composition / surface sharing ──

/// Hand frescod a reference to an image so it can be composed as
/// a textured rect in its scene. Returns an opaque share token
/// that the app passes to frescod via fresco-protocol's
/// `slot_set_texture` mechanism. The image stays GPU-resident;
/// only the handle traverses the wire.
pub const OP_GPU_SHARE_SURFACE:   u16 = 0x0210;

/// Client → server: present a rendered image to a Fresco surface.
/// Fired by `vkQueuePresentKHR` after the app's last frame for
/// that swapchain image. The host endpoint (today
/// frescod-aqueduct; native HW at D5+) routes the image's
/// pixels to the surface's per-window `WindowSurface` per
/// `aqueduct-gpu.md` §7.1.1. Fire-and-forget; ordering is
/// preserved by aqueduct-gpu's per-connection timeline.
pub const OP_GPU_PRESENT:         u16 = 0x0211;

// ─── Bundle lifecycle (third-party scene-graph extensions) ──

/// Load a third-party bundle into the host endpoint. Payload
/// references the manifest's CAS hash; transitively the manifest
/// references shader hashes pre-warmed in Tessera by atrium-pkg's
/// install pass. Host materialises all declared pipelines, render
/// passes, samplers; returns a fresh `bundle_namespace_id` (tag
/// 0x1..0xE) under which the bundle's resources are addressable.
pub const OP_GPU_BUNDLE_LOAD:     u16 = 0x0220;
/// Unload a previously-loaded bundle; the host releases all
/// resources in its namespace. See `aqueduct-gpu.md` §7.3.
pub const OP_GPU_BUNDLE_UNLOAD:   u16 = 0x0221;

// ─── Async events (server → client) ──
//
// Delivered to the client out-of-band of any specific request,
// surfaced into the next fence wait or poll.

/// A fence the client is waiting on has been signalled.
pub const OP_GPU_FENCE_SIGNALED:  u16 = 0x0301;
/// Device-lost event (analogue of VK_ERROR_DEVICE_LOST). All
/// resources on this connection are invalid; client must reconnect.
pub const OP_GPU_DEVICE_LOST:     u16 = 0x0302;
/// Validation failure for a previously-submitted fire-and-forget
/// op. Carries the offending op + resource ID + diagnostic message.
pub const OP_GPU_VALIDATION_ERR:  u16 = 0x0303;
/// Per-resource validation failure during `OP_GPU_BUNDLE_LOAD`.
/// Bundle load fails atomically only if any of these arrive before
/// the load response.
pub const OP_GPU_BUNDLE_LOAD_ERR: u16 = 0x0304;

// ───── In-stream frame ops ────────────────────────────────────────
//
// Records inside the byte buffer of `OP_GPU_SUBMIT_FRAME`. Each
// record has a fixed-size header (op:u16 + flags:u8 + _pad:u8 +
// length:u32 — 8 bytes total) followed by the op-specific body.
// See `aqueduct-gpu.md` §5.1.

/// In-frame operation kinds. Encoded as u16 in the frame command
/// stream's record header.
///
/// Closed wire vocabulary: third-party bundles do **not** extend
/// this enum. Bundle expressiveness flows through `FOP_BIND_PIPELINE`
/// referencing a bundle-shipped `pipeline_id`. See
/// `aqueduct-gpu.md` §3 (closed wire vocabulary principle).
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameOp {
    /// Begin a render pass. Body: color_target_ids, depth_target_id,
    /// clear_values, viewport.
    BeginRenderPass = 0x0010,
    /// End the current render pass.
    EndRenderPass   = 0x0011,
    /// Bind a depth attachment for the current render pass.
    /// Optional follow-up to `BeginRenderPass` -- the guest's
    /// framebuffer carries a depth attachment as
    /// `attachments[1]` and the ICD emits this op after the
    /// color BeginRenderPass to wire it in. Body is
    /// `{ image_id: u32, clear_value: f32 }` (8 bytes).
    /// Tier-1 ignores unknown ops; tier-2 consumes this to
    /// persist depth across draws within and across passes.
    BindDepthAttachment = 0x0012,

    /// Bind a pipeline (graphics or compute) by its resolved ID.
    BindPipeline    = 0x0020,
    /// Bind a descriptor set for the current pipeline.
    BindDescriptors = 0x0021,
    /// Bind a vertex buffer.
    BindVertexBuf   = 0x0022,
    /// Bind an index buffer.
    BindIndexBuf    = 0x0023,

    /// Set the viewport (dynamic state).
    SetViewport     = 0x0030,
    /// Set the scissor rectangle.
    SetScissor      = 0x0031,
    /// Inline push-constant data (≤ 128 bytes).
    PushConstants   = 0x0032,
    /// Set the dynamic cull mode (`vkCmdSetCullMode`).
    SetCullMode     = 0x0033,
    /// Set the dynamic front-face winding (`vkCmdSetFrontFace`).
    SetFrontFace    = 0x0034,
    /// Toggle the depth test (`vkCmdSetDepthTestEnable`).
    SetDepthTestEnable  = 0x0035,
    /// Toggle depth writes (`vkCmdSetDepthWriteEnable`).
    SetDepthWriteEnable = 0x0036,

    /// Non-indexed draw.
    Draw            = 0x0040,
    /// Indexed draw.
    DrawIndexed     = 0x0041,
    /// Indirect-args draw.
    DrawIndirect    = 0x0042,

    /// Compute dispatch with explicit grid dimensions.
    Dispatch        = 0x0050,
    /// Compute dispatch with grid dims read from a buffer.
    DispatchIndirect= 0x0051,

    /// Copy from a buffer to an image.
    CopyBufToImg    = 0x0060,
    /// Copy from an image to a buffer.
    CopyImgToBuf    = 0x0061,
    /// Blit between images (with optional filter).
    Blit            = 0x0062,

    /// Pipeline-barrier between stages and resources.
    PipelineBarrier = 0x0070,
}

impl FrameOp {
    /// Convert a wire u16 back to a `FrameOp`. Returns `None` for
    /// unknown values (which a host should treat as a fatal frame-
    /// stream parse error and convert into `OP_GPU_VALIDATION_ERR`).
    pub fn from_u16(v: u16) -> Option<Self> {
        Some(match v {
            0x0010 => FrameOp::BeginRenderPass,
            0x0011 => FrameOp::EndRenderPass,
            0x0012 => FrameOp::BindDepthAttachment,
            0x0020 => FrameOp::BindPipeline,
            0x0021 => FrameOp::BindDescriptors,
            0x0022 => FrameOp::BindVertexBuf,
            0x0023 => FrameOp::BindIndexBuf,
            0x0030 => FrameOp::SetViewport,
            0x0031 => FrameOp::SetScissor,
            0x0032 => FrameOp::PushConstants,
            0x0033 => FrameOp::SetCullMode,
            0x0034 => FrameOp::SetFrontFace,
            0x0035 => FrameOp::SetDepthTestEnable,
            0x0036 => FrameOp::SetDepthWriteEnable,
            0x0040 => FrameOp::Draw,
            0x0041 => FrameOp::DrawIndexed,
            0x0042 => FrameOp::DrawIndirect,
            0x0050 => FrameOp::Dispatch,
            0x0051 => FrameOp::DispatchIndirect,
            0x0060 => FrameOp::CopyBufToImg,
            0x0061 => FrameOp::CopyImgToBuf,
            0x0062 => FrameOp::Blit,
            0x0070 => FrameOp::PipelineBarrier,
            _ => return None,
        })
    }

    /// The u16 wire encoding of this op.
    pub const fn as_u16(self) -> u16 {
        self as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_op_roundtrip() {
        for op in [
            FrameOp::BeginRenderPass, FrameOp::EndRenderPass,
            FrameOp::BindDepthAttachment,
            FrameOp::BindPipeline, FrameOp::BindDescriptors,
            FrameOp::BindVertexBuf, FrameOp::BindIndexBuf,
            FrameOp::SetViewport, FrameOp::SetScissor,
            FrameOp::PushConstants,
            FrameOp::SetCullMode, FrameOp::SetFrontFace,
            FrameOp::SetDepthTestEnable, FrameOp::SetDepthWriteEnable,
            FrameOp::Draw, FrameOp::DrawIndexed, FrameOp::DrawIndirect,
            FrameOp::Dispatch, FrameOp::DispatchIndirect,
            FrameOp::CopyBufToImg, FrameOp::CopyImgToBuf, FrameOp::Blit,
            FrameOp::PipelineBarrier,
        ] {
            assert_eq!(FrameOp::from_u16(op.as_u16()), Some(op));
        }
    }

    #[test]
    fn frame_op_rejects_unknown() {
        assert_eq!(FrameOp::from_u16(0x0099), None);
        assert_eq!(FrameOp::from_u16(0xFFFF), None);
    }
}
