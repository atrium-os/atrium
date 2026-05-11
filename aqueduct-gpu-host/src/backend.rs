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

/// Tier-1 software renderer. Hand-coded CPU rasterisation of
/// Atrium-native bundle operations via tiny-skia. **Does not
/// interpret SPIR-V or NIR.**
///
/// Per `docs/spec/aqueduct-gpu.md` §6.5, this is the
/// power-policy-friendly default for static / idle desktop UI on
/// battery, and the only backend on GPU-less systems for
/// Atrium-native workloads. Third-party bundles with custom
/// shaders, and Vulkan games, are refused at handshake-cap
/// negotiation.
///
/// Phase 1.3c-stub: the struct exists and the daemon's `--backend
/// software` CLI flag selects it, but `submit_frame` panics rather
/// than rasterise. The real tiny-skia integration lands in a
/// follow-up commit. This stub lets the CLI surface and the
/// capability advertisement stabilise before the rasterisation
/// code lands.
pub struct SoftwareBackend {
    submissions: AtomicU64,
}

impl Default for SoftwareBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl SoftwareBackend {
    /// Construct a fresh tier-1 software backend.
    pub fn new() -> Self {
        Self { submissions: AtomicU64::new(0) }
    }

    /// How many frames have been submitted. Diagnostic.
    pub fn submission_count(&self) -> u64 {
        self.submissions.load(Ordering::Relaxed)
    }
}

impl Backend for SoftwareBackend {
    fn identity(&self) -> BackendId {
        // Software vendor; generation 0 == tier-1 (tiny-skia).
        // Reserve generation 1+ for future tier-2 (general SW Vulkan).
        BackendId::new(GpuVendor::Software, 0)
    }

    fn caps(&self) -> u64 {
        // Tier-1 advertises ONLY the per-bundle-op caps it has
        // hand-coded equivalents for. Composition-related caps
        // (CAPS_COMPOSITION, CAPS_SHARE_SURFACE) reflect whether
        // tier-1 can hand a rendered framebuffer back to frescod;
        // initially yes — tiny-skia produces raw pixels into a
        // shared region.
        //
        // CAPS_COMPUTE / CAPS_SPIRV_UPLOAD / CAPS_BUNDLE_LOAD
        // stay unset: tier-1 does not run arbitrary shaders, does
        // not accept SPIR-V upload, does not materialise third-
        // party bundles.
        use aqueduct_gpu::payloads::HandshakeResponse as H;
        H::CAPS_COMPOSITION
            | H::CAPS_SHARE_SURFACE
            | H::CAPS_TIER1_RECT
            | H::CAPS_TIER1_TEXT
            | H::CAPS_TIER1_TEXTURE
            | H::CAPS_TIER1_PATH
    }

    fn max_frame_bytes(&self) -> u32 {
        // Tier-1 rendering is CPU-bound; frames stay small. The
        // tiny-skia integration parses one record at a time, so
        // memory pressure scales with frame complexity, not size
        // upfront. Cap at 1 MiB to keep guest allocations bounded.
        1 << 20
    }

    fn max_fences_inflight(&self) -> u32 {
        64
    }

    fn allocate_memory(&self, _size: u64, _usage: u8) -> [u8; 32] {
        // Stub: returns a deterministic sentinel token. The
        // tiny-skia path doesn't yet allocate real backing memory
        // — that lands when frame dispatch lands.
        let n = self.submissions.load(Ordering::Relaxed);
        let mut tok = [0u8; 32];
        tok[..8].copy_from_slice(&n.to_le_bytes());
        tok[31] = 0xC0; // sentinel distinct from StubBackend's 0xAB
        tok
    }

    fn submit_frame(&self, _fence_id: ResourceId, _timeline: u64, _frame_buf: &[u8]) -> bool {
        self.submissions.fetch_add(1, Ordering::Relaxed);
        // Phase 1.3c-stub: panic to make it obvious that the
        // real rasterisation hasn't landed yet. Tests configured
        // for `software` backend will fail here, which is the
        // intended behaviour until the tiny-skia integration ships.
        //
        // Once frame dispatch lands, this becomes:
        //   self.dispatch_frame(frame_buf, ...) -> Result<()>
        //   self.signal_fence(fence_id, timeline);
        //   true
        unimplemented!(
            "SoftwareBackend frame dispatch not yet implemented; \
             use --backend stub for protocol tests or --backend \
             moltenvk for real rendering"
        );
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

    #[test]
    fn software_backend_advertises_tier1_caps_not_compute() {
        use aqueduct_gpu::payloads::HandshakeResponse as H;
        let b = SoftwareBackend::new();
        let caps = b.caps();

        // Tier-1 ops are advertised…
        assert!(caps & H::CAPS_TIER1_RECT != 0);
        assert!(caps & H::CAPS_TIER1_TEXT != 0);
        assert!(caps & H::CAPS_TIER1_TEXTURE != 0);
        assert!(caps & H::CAPS_TIER1_PATH != 0);

        // …and the composition path bits (so frescod knows it can
        // receive rendered pixels via share-surface).
        assert!(caps & H::CAPS_COMPOSITION != 0);
        assert!(caps & H::CAPS_SHARE_SURFACE != 0);

        // …but the third-party-shader caps stay clear.
        assert_eq!(caps & H::CAPS_COMPUTE, 0,
            "tier-1 does not run compute shaders");
        assert_eq!(caps & H::CAPS_SPIRV_UPLOAD, 0,
            "tier-1 does not accept SPIR-V upload");
        assert_eq!(caps & H::CAPS_BUNDLE_LOAD, 0,
            "tier-1 does not materialise third-party bundles");
    }

    #[test]
    fn software_backend_identity_is_tier1() {
        let b = SoftwareBackend::new();
        assert_eq!(b.identity().vendor, GpuVendor::Software);
        // generation 0 == tier-1 by convention (see spec §6.5).
        // Tier-2 (general SW Vulkan) would use generation 1+ if it
        // ever ships.
        assert_eq!(b.identity().generation, 0);
    }
}
