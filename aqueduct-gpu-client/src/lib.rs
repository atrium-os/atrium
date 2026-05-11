//! # aqueduct-gpu-client — guest-side aqueduct-gpu client
//!
//! Wraps an [`aqueduct::Connection`] with the aqueduct-gpu protocol:
//! handshake, ID allocation, typed envelope helpers, frame submit/wait,
//! async event reception.
//!
//! ## Architecture
//!
//! ```text
//!   App / frescod / atrium-vk-icd
//!     │  ergonomic calls (allocate_memory, create_image, submit_frame...)
//!     ▼
//!   GpuClient (this crate)
//!     │  postcard encoding + aqueduct::Connection::send_message
//!     ▼
//!   aqueduct::Connection (envelope transport)
//!     │  Unix socket / ivshmem
//!     ▼
//!   aqueduct-gpu-host daemon (macOS bring-up) or atrium-gpu kmod (D5+)
//! ```
//!
//! ## Phase 1 scope
//!
//! - [`GpuClient::handshake`] performs `OP_GPU_HANDSHAKE` and caches
//!   the host's reported capabilities.
//! - `allocate_*` methods for memory regions, images, buffers,
//!   samplers, fences, shaders, pipelines.
//! - [`GpuClient::submit_frame`] flushes a [`FrameBuilder`] as a
//!   single `OP_GPU_SUBMIT_FRAME` envelope.
//! - [`GpuClient::wait_fence`] is the request-response sync point.
//! - [`GpuClient::recv_event`] surfaces async events
//!   (fence-signaled, validation-error, device-lost, bundle-load-err).
//!
//! Resource IDs are monotonically allocated in the
//! [`IdNamespace::IcdRuntime`] partition; the client owns its own
//! ID counter per connection.
//!
//! ## What this crate is NOT
//!
//! - Not a Vulkan ICD. That's `atrium-vk-icd`, which sits on top of
//!   this crate (Phase 2).
//! - Not a frame-op encoder. Callers build frame command streams via
//!   [`aqueduct_gpu::FrameBuilder`] directly; this crate just submits
//!   them.
//! - Not a host endpoint. That's `aqueduct-gpu-host` (Phase 1.3 on
//!   macOS bring-up) or the atrium-gpu kmod's direct submission path
//!   (D5+).

#![deny(missing_docs)]

mod client;
mod error;
mod ids;

pub use client::{GpuClient, GpuEvent, PROTOCOL_VERSION};
pub use error::{GpuClientError, GpuClientResult};
pub use ids::IdAllocator;

// Re-export the protocol types so callers can `use aqueduct_gpu_client::*`
// without also adding aqueduct-gpu to their dependencies.
pub use aqueduct_gpu::{
    backends::{BackendId, GpuVendor},
    frame::{FrameBuilder, FrameDecoder, FrameDecodeError},
    ids::{IdNamespace, ResourceId},
    opcodes::FrameOp,
    payloads,
    CLASS_GPU,
};
