//! # aqueduct-gpu-host — macOS bring-up host endpoint daemon
//!
//! The privileged macOS-side process that mediates aqueduct-gpu
//! traffic between FreeBSD guest VMs (frescod's renderer, the Vulkan
//! ICD) and the host's MoltenVK / Metal backend.
//!
//! ## Why this exists
//!
//! Atrium's long-term graphics path (D5+) talks directly from a guest
//! Atrium app to the atrium-gpu kmod, with no host-side mediator. But
//! during macOS-HVF bring-up, we have:
//!
//! - A FreeBSD guest VM running our compositor stack
//! - A macOS host providing the actual GPU via MoltenVK + Metal
//! - No native FreeBSD GPU driver yet
//!
//! `aqueduct-gpu-host` bridges the two. It listens on a Unix socket
//! (or, in production bring-up, the QEMU ivshmem channel), accepts
//! incoming aqueduct-gpu connections from the guest, decodes the
//! envelope stream, and dispatches each frame's commands against
//! MoltenVK on the host.
//!
//! ## Goes away in D5+
//!
//! When native GPU kernel drivers land in D5+, this daemon is no
//! longer in the data path. Guest apps talk directly to the
//! atrium-gpu kmod via `IOC_GPU_*` ioctls. The wire protocol
//! (`aqueduct-gpu`) is the same — only the dispatch target changes.
//! The host endpoint stays around as a CI / dev configuration.
//!
//! ## Trust posture
//!
//! `aqueduct-gpu-host` is a privileged mediator (analogous to
//! portcullisd's role for capability grants). It has full visibility
//! into every connected app's GPU memory. Per `aqueduct-gpu.md`
//! §12.1, this is explicitly outside the wire-encryption threat
//! model: if the host endpoint is compromised, the platform is
//! compromised regardless of wire crypto. Mitigation: keep this
//! daemon small, audit it, hold it to the same scrutiny as any
//! kernel-level component.
//!
//! ## Phase 1.3 scope
//!
//! - Accept loop on `/tmp/aqueduct-gpu.sock` (configurable)
//! - Per-connection state (resource tables, per-connection MoltenVK
//!   sub-contexts)
//! - Handshake responder (reports backend identity + caps)
//! - Stub dispatchers for each `OP_GPU_*` — protocol-correct but no
//!   actual GPU work yet (lands in 1.3b alongside frescod's renderer
//!   migration)
//! - Async event emission scaffolding (fence-signaled, validation-err)
//!
//! ## Phase 1.3b scope (next)
//!
//! - Real MoltenVK device creation
//! - Frame command stream → `MTLCommandBuffer` translation
//! - Shader cache (compile SPIR-V → `MTLLibrary` on upload, key by
//!   `(spirv_hash, backend_id, compiler_version)`)
//! - Memory region allocation + atrium-gpu kmod handshake (the
//!   `atrium_gpu_token` import path)

#![deny(missing_docs)]
// P3a: portable SIMD (std::simd) for the Tier-2 rasterizer's
// fixed-function inner loops (coverage / interp / blend), lowering
// to NEON on aarch64 and SSE on x86 with no external dependency.
#![feature(portable_simd)]

pub mod backend;
/// Carillon doorbell-driven GPU VM transport core, re-exported from the
/// pure `carillon-transport` crate (shared with the FreeBSD guest pump).
#[cfg(unix)]
pub use carillon_transport as carillon;
pub mod cost_model;
pub mod listener;
pub mod moltenvk;
pub mod resources;
pub mod session;
pub mod shader_annotate;
pub mod shader_cache;
pub mod shader_inspect;
pub mod shader_ssa;
pub mod shader_validator;
pub mod software;
pub mod tier2_backend;
pub mod tier2_registry;

pub use backend::{Backend, SoftwareBackend, StubBackend};
pub use cost_model::{
    CostMode, CostModelBackend, DeviceProfile, FrameLedger, OpKind, Topology,
};
pub use tier2_backend::{
    AssembledVertices, PresentCallback, PresentedFrame, Tier2Backend,
};
pub use tier2_registry::{
    BlendFactor, BlendFactorPair, BlendOp, BlendState, ColorWriteMask,
    DrawTriangle, Tier2Registry, Tier2ShaderId,
};
pub use moltenvk::{MoltenVkBackend, MoltenVkError};
#[cfg(unix)]
pub use carillon_transport::{
    serve_ivshmem, CompDesc, Doorbell, GuestRing, Host as CarillonHost,
    IvshmemServer, Region as CarillonRegion, ShutdownHandle, SubDesc,
};
pub use listener::Listener;
pub use resources::ResourceTable;
pub use session::Session;
