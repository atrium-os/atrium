//! # aqueduct-gpu — GPU dispatch protocol for Atrium
//!
//! Wire-format types and a guest-side frame builder for the GPU
//! opcode class (`CLASS_GPU = 9`) on the aqueduct envelope transport.
//! Shared by both ends:
//!
//! - **Guest-side**: linked into `aqueduct-gpu-client` (used by
//!   frescod's renderer) and `atrium-vk-icd` (used by Vulkan apps).
//!   Records frames into a [`FrameBuilder`] and produces wire bytes
//!   for `OP_GPU_SUBMIT_FRAME`.
//! - **Host-side**: linked into `aqueduct-gpu-host` (the macOS
//!   daemon during bring-up) and the atrium-gpu kmod path (in D5+
//!   native HW). Decodes incoming envelopes and dispatches the frame
//!   command stream to the underlying backend.
//!
//! ## Design references
//!
//! - Protocol design + opcode tables: `docs/spec/aqueduct-gpu.md`
//! - Aqueduct envelope mechanics: `docs/spec/aqueduct.md`
//! - Trust boundaries + sandbox primitives:
//!   `docs/spec/aqueduct-gpu.md` §11 (universal sandbox) and §12
//!   (trust boundaries and threat model).
//!
//! ## Crate layout
//!
//! - [`opcodes`] — `OP_GPU_*` opcode constants for the envelope layer
//!   and `FOP_*` frame-op constants for the in-stream command framing.
//! - [`ids`] — partitioned u32 resource ID namespace
//!   (built-in / bundle / ICD-runtime tags).
//! - [`payloads`] — typed payload structs for each envelope-level op,
//!   serialised via postcard (matching `fresco-protocol`).
//! - [`frame`] — [`FrameBuilder`] for accumulating in-frame `FOP_*`
//!   records into a single byte buffer, ready to be wrapped in
//!   `OP_GPU_SUBMIT_FRAME`.
//! - [`backends`] — backend identification (RDNA3, Gen12LP,
//!   Ampere-SM86, M-series-Apple7, atrium-gpu-v1, …) used by
//!   `OP_GPU_HANDSHAKE` and `OP_GPU_SHADER_RESOLVE`.
//!
//! ## Stability
//!
//! Phase 1 of the aqueduct-gpu implementation. Wire layout is the
//! design committed in `docs/spec/aqueduct-gpu.md`; expect minor
//! schema evolution as the host endpoint + frescod migration land.
//! Versioning per `aqueduct.md` §5 — the `OP_GPU_HANDSHAKE` exchange
//! is the negotiation point.

#![deny(missing_docs)]

pub mod backends;
pub mod frame;
pub mod ids;
pub mod opcodes;
pub mod payloads;

pub use backends::{BackendId, GpuVendor};
pub use frame::{
    FrameBuilder, FrameDecoder, FrameDecodeError,
    FrameBodyError,
    DrawCmd, DrawIndexedCmd, BindVertexBufCmd, BindIndexBufCmd,
    IndexType, SetViewportCmd,
};
pub use ids::{IdNamespace, ResourceId, BUNDLE_NAMESPACE_RANGE,
               BUILTIN_NAMESPACE, ICD_RUNTIME_NAMESPACE};
pub use opcodes::{
    OP_GPU_HANDSHAKE,
    OP_GPU_MEMORY_CREATE, OP_GPU_MEMORY_DESTROY,
    OP_GPU_IMAGE_CREATE, OP_GPU_IMAGE_DESTROY,
    OP_GPU_BUFFER_CREATE, OP_GPU_BUFFER_DESTROY, OP_GPU_BUFFER_WRITE,
    OP_GPU_SAMPLER_CREATE, OP_GPU_SAMPLER_DESTROY,
    OP_GPU_SHADER_RESOLVE, OP_GPU_SHADER_UPLOAD,
    OP_GPU_PIPELINE_CREATE, OP_GPU_PIPELINE_DESTROY,
    OP_GPU_FENCE_CREATE, OP_GPU_FENCE_DESTROY,
    OP_GPU_SUBMIT_FRAME, OP_GPU_WAIT_FENCE,
    OP_GPU_SHARE_SURFACE,
    OP_GPU_BUNDLE_LOAD, OP_GPU_BUNDLE_UNLOAD,
    OP_GPU_FENCE_SIGNALED, OP_GPU_DEVICE_LOST,
    OP_GPU_VALIDATION_ERR, OP_GPU_BUNDLE_LOAD_ERR,
    FrameOp,
};
pub use payloads::*;

/// Re-exported from `aqueduct` for convenience: GPU dispatch class.
pub const CLASS_GPU: u8 = aqueduct::classes::CLASS_GPU;
