//! GPU backend abstraction.
//!
//! The host endpoint can dispatch frame commands against multiple
//! backends:
//!
//! - **`StubBackend`** (Phase 1.3a, this file) — protocol-correct
//!   but does no GPU work. Records frame submissions in memory,
//!   signals fences as soon as they're submitted. Used to validate
//!   the wire path + guest renderer migration without GPU coupling.
//! - **`MoltenVkBackend`** (Phase 1.3b, follow-up commit) — wraps
//!   `ash::Entry::load()` for the MoltenVK ICD, instantiates a
//!   single shared `MTLDevice`-equivalent via `VkDevice`, dispatches
//!   each frame's command stream as one `VkCommandBuffer`.
//! - **`NativeVulkanBackend`** (future) — same shape as MoltenVK,
//!   for Linux-host dev workflows.
//! - **`AtriumGpuBackend`** (future, D5+) — the daemon binary still
//!   exists but talks to the atrium-gpu kmod via ioctls. On D5+ the
//!   daemon is bypassable entirely, but kept as a dev/CI mode.

use std::sync::atomic::{AtomicU64, Ordering};

use aqueduct_gpu::backends::{BackendId, GpuVendor};
use aqueduct_gpu::ids::ResourceId;

/// The GPU backend trait. One implementation per supported
/// host environment.
///
/// All methods are infallible at this layer — backends that fail
/// to set up should fail at construction; runtime errors surface
/// as `ValidationErr` async events emitted by the session loop
/// (not via this trait).
pub trait Backend: Send + Sync {
    /// What this backend reports in `OP_GPU_HANDSHAKE`. Static for
    /// the lifetime of the backend.
    fn identity(&self) -> BackendId;

    /// Capability bitset reported at handshake. See
    /// `aqueduct_gpu::payloads::HandshakeResponse::CAPS_*`.
    fn caps(&self) -> u64;

    /// Maximum frame command stream size in bytes. Determines the
    /// `FrameBuilder` cap clients negotiate. Backends with tight
    /// memory limits report a smaller number here.
    fn max_frame_bytes(&self) -> u32;

    /// Maximum number of concurrent in-flight fences per connection.
    fn max_fences_inflight(&self) -> u32;

    /// Allocate a memory region. Returns the `atrium_gpu_token`
    /// that the guest kmod redeems via `IOC_GPU_IMPORT_REGION`.
    fn allocate_memory(&self, size: u64, usage: u8) -> [u8; 32];

    /// Submit a frame command stream. The default impl records the
    /// submission count and immediately signals the fence; real
    /// backends translate `frame_buf` and dispatch.
    ///
    /// Returns `true` if the fence should be signalled now,
    /// `false` if signaling is deferred (real backend: deferred
    /// until GPU completion).
    fn submit_frame(
        &self,
        fence_id: ResourceId,
        timeline: u64,
        frame_buf: &[u8],
    ) -> bool;
}

/// Protocol-correct backend that does no GPU work.
///
/// Useful for:
/// - Validating the wire protocol end-to-end before any MoltenVK
///   code lands.
/// - Driving Phase 1.4 (frescod renderer migration) — frescod's
///   renderer can complete its wire-side migration before the real
///   backend exists, since the stub correctly responds to every op.
/// - CI / regression tests where you want frame-throughput numbers
///   from the wire path alone.
///
/// Signals fences immediately on submit (no GPU coupling).
pub struct StubBackend {
    submissions: AtomicU64,
}

impl Default for StubBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl StubBackend {
    /// Construct a fresh stub backend.
    pub fn new() -> Self {
        Self { submissions: AtomicU64::new(0) }
    }

    /// How many frames have been submitted to this backend across
    /// all connections. Diagnostic.
    pub fn submission_count(&self) -> u64 {
        self.submissions.load(Ordering::Relaxed)
    }
}

impl Backend for StubBackend {
    fn identity(&self) -> BackendId {
        // Software backend, generation 0 — matches the convention
        // for vendor-agnostic CI/test paths.
        BackendId::new(GpuVendor::Software, 0)
    }

    fn caps(&self) -> u64 {
        // Stub doesn't materialise pipelines or bundles, but it
        // reports compute + share-surface so frame-routing tests
        // exercise those paths even with no real GPU.
        use aqueduct_gpu::payloads::HandshakeResponse as H;
        H::CAPS_COMPUTE | H::CAPS_SHARE_SURFACE | H::CAPS_COMPOSITION
    }

    fn max_frame_bytes(&self) -> u32 {
        1 << 20 // 1 MiB — plenty for the stub
    }

    fn max_fences_inflight(&self) -> u32 {
        64
    }

    fn allocate_memory(&self, _size: u64, _usage: u8) -> [u8; 32] {
        // Deterministic-looking token; real backend embeds the
        // SHM fd + offset, but stub clients never redeem this.
        let n = self.submissions.fetch_add(0, Ordering::Relaxed);
        let mut tok = [0u8; 32];
        tok[..8].copy_from_slice(&n.to_le_bytes());
        tok[31] = 0xAB; // sentinel
        tok
    }

    fn submit_frame(&self, _fence_id: ResourceId, _timeline: u64, _frame_buf: &[u8]) -> bool {
        self.submissions.fetch_add(1, Ordering::Relaxed);
        true // signal fence immediately
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_signals_fence_immediately() {
        let b = StubBackend::new();
        let fid = ResourceId::new(aqueduct_gpu::ids::IdNamespace::IcdRuntime, 0x1);
        assert!(b.submit_frame(fid, 1, &[]));
        assert_eq!(b.submission_count(), 1);
        assert!(b.submit_frame(fid, 2, &[]));
        assert_eq!(b.submission_count(), 2);
    }

    #[test]
    fn stub_reports_software_backend() {
        let b = StubBackend::new();
        assert_eq!(b.identity().vendor, GpuVendor::Software);
        assert_eq!(b.identity().generation, 0);
    }
}
