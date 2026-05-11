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

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use aqueduct_gpu::backends::{BackendId, GpuVendor};
use aqueduct_gpu::ids::ResourceId;

use crate::software::{BeginRenderPassBody, TinySkiaRenderer};

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

    /// Notify the backend an image was created. Called by the
    /// session when it processes `OP_GPU_IMAGE_CREATE`. SW backends
    /// use this to allocate per-image pixel storage; GPU backends
    /// typically ignore it (the GPU-side allocation happens through
    /// `allocate_memory` + a later `vkBindImageMemory` equivalent).
    ///
    /// Default no-op for backends that don't need per-image state.
    fn image_created(&self, _image_id: ResourceId, _width: u32, _height: u32) {}

    /// Notify the backend an image was destroyed. Default no-op.
    fn image_destroyed(&self, _image_id: ResourceId) {}

    /// Submit a frame command stream. Returns `true` if the fence
    /// should be signalled now, `false` if signaling is deferred.
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
/// **Per-image pixel storage** lives in the
/// `images: Mutex<HashMap<u64, tiny_skia::Pixmap>>` field. The
/// session calls `image_created` / `image_destroyed` to keep this
/// map in sync with the wire-level OP_GPU_IMAGE_{CREATE,DESTROY}.
/// `submit_frame` decodes the frame stream, finds the target image
/// in BEGIN_RENDERPASS, takes a `PixmapMut` from the map entry,
/// and dispatches through `TinySkiaRenderer`.
///
/// The Pixmap map is keyed by `ResourceId.raw() as u64`. Because
/// IDs are partitioned by namespace (top 4 bits — see
/// `aqueduct_gpu::ids`), `IcdRuntime` IDs from different sessions
/// can collide on the low bits but never on the full u32. For
/// Phase 1.3c this single shared map is sufficient since tier-1's
/// canonical client is frescod (one client); multi-session
/// isolation lives at the session layer (each session validates
/// that the image IDs it references belong to its own connection),
/// not at the backend layer.
pub struct SoftwareBackend {
    submissions: AtomicU64,
    images: Mutex<HashMap<u64, tiny_skia::Pixmap>>,
    /// Telemetry counter for "submit_frame called but no target
    /// image" or other dispatch failures. Diagnostic only — actual
    /// errors propagate to the session as `OP_GPU_VALIDATION_ERR`
    /// in a follow-on commit; today they're logged + counted.
    dispatch_failures: AtomicU64,
}

impl Default for SoftwareBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl SoftwareBackend {
    /// Construct a fresh tier-1 software backend.
    pub fn new() -> Self {
        Self {
            submissions: AtomicU64::new(0),
            images: Mutex::new(HashMap::new()),
            dispatch_failures: AtomicU64::new(0),
        }
    }

    /// How many frames have been submitted. Diagnostic.
    pub fn submission_count(&self) -> u64 {
        self.submissions.load(Ordering::Relaxed)
    }

    /// How many submit_frame calls failed dispatch. Diagnostic.
    pub fn dispatch_failure_count(&self) -> u64 {
        self.dispatch_failures.load(Ordering::Relaxed)
    }

    /// Read back the rendered pixels of `image_id`. Returns RGBA8
    /// bytes (row-major, no padding). Used by tests today and,
    /// in a follow-on commit, by the session when handling
    /// `OP_GPU_SHARE_SURFACE` for compositor handoff.
    pub fn read_image_pixels(&self, image_id: ResourceId) -> Option<Vec<u8>> {
        let images = self.images.lock().ok()?;
        let pixmap = images.get(&(image_id.raw() as u64))?;
        Some(pixmap.data().to_vec())
    }

    /// Whether the backend currently has a pixel buffer for `image_id`.
    /// Diagnostic / test helper.
    pub fn has_image(&self, image_id: ResourceId) -> bool {
        self.images
            .lock()
            .map(|m| m.contains_key(&(image_id.raw() as u64)))
            .unwrap_or(false)
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
        // Tier-1 doesn't actually share memory with the guest yet
        // (that arrives in Phase 1.5 with IOC_GPU_IMPORT_REGION).
        // Returns a sentinel token; the per-image Pixmap allocated
        // in `image_created` is what actually backs rendering.
        let n = self.submissions.load(Ordering::Relaxed);
        let mut tok = [0u8; 32];
        tok[..8].copy_from_slice(&n.to_le_bytes());
        tok[31] = 0xC0;
        tok
    }

    fn image_created(&self, image_id: ResourceId, width: u32, height: u32) {
        if width == 0 || height == 0 {
            log::warn!("SoftwareBackend::image_created: zero-extent image {image_id} ({width}x{height})");
            return;
        }
        // tiny_skia::Pixmap caps at i32::MAX width/height; we cap
        // tighter to keep allocations sane (16K × 16K = 1 GiB on
        // BGRA8 — same as Vulkan's typical maxImageDimension2D).
        const MAX_DIM: u32 = 16 * 1024;
        if width > MAX_DIM || height > MAX_DIM {
            log::warn!("SoftwareBackend::image_created: image {image_id} {width}x{height} exceeds {MAX_DIM} cap");
            return;
        }
        let Some(pixmap) = tiny_skia::Pixmap::new(width, height) else {
            log::warn!("SoftwareBackend::image_created: pixmap alloc failed for {width}x{height}");
            return;
        };
        if let Ok(mut images) = self.images.lock() {
            images.insert(image_id.raw() as u64, pixmap);
            log::debug!("SoftwareBackend: registered image {image_id} ({width}x{height})");
        }
    }

    fn image_destroyed(&self, image_id: ResourceId) {
        if let Ok(mut images) = self.images.lock() {
            images.remove(&(image_id.raw() as u64));
        }
    }

    fn submit_frame(&self, _fence_id: ResourceId, _timeline: u64, frame_buf: &[u8]) -> bool {
        self.submissions.fetch_add(1, Ordering::Relaxed);

        // Find the BEGIN_RENDERPASS body to learn the target image.
        // We don't permit zero-renderpass frames here — tier-1 is
        // strictly compositor-shaped. Empty frames count as
        // dispatch failures so they show up in telemetry.
        let target_image_id = match find_target_image_id(frame_buf) {
            Ok(id) => id,
            Err(e) => {
                log::warn!("SoftwareBackend::submit_frame: {e}");
                self.dispatch_failures.fetch_add(1, Ordering::Relaxed);
                return true; // still signal the fence; client gets
                             // OP_GPU_VALIDATION_ERR via the session
            }
        };

        let mut images = match self.images.lock() {
            Ok(g) => g,
            Err(_) => {
                self.dispatch_failures.fetch_add(1, Ordering::Relaxed);
                return true;
            }
        };

        let Some(pixmap) = images.get_mut(&(target_image_id.raw() as u64)) else {
            log::warn!("SoftwareBackend::submit_frame: target image {target_image_id} not registered");
            self.dispatch_failures.fetch_add(1, Ordering::Relaxed);
            return true;
        };

        let mut renderer = TinySkiaRenderer::new(pixmap.as_mut());
        match renderer.dispatch_frame(frame_buf) {
            Ok(draws) => log::debug!("SoftwareBackend: dispatched {draws} draws into {target_image_id}"),
            Err(e) => {
                log::warn!("SoftwareBackend::submit_frame: render error: {e}");
                self.dispatch_failures.fetch_add(1, Ordering::Relaxed);
            }
        }
        // Stub fence-signal semantics: tier-1 work completes inline,
        // so we signal immediately. Real GPU backends defer.
        true
    }
}

/// Peek into a frame command stream and pull out the
/// `target_image_id` from its BEGIN_RENDERPASS record. Used by
/// `SoftwareBackend::submit_frame` to choose the Pixmap *before*
/// constructing the renderer (since the renderer needs the target
/// at construction time).
fn find_target_image_id(frame_buf: &[u8]) -> Result<ResourceId, &'static str> {
    use aqueduct_gpu::frame::FrameDecoder;
    use aqueduct_gpu::opcodes::FrameOp;

    let mut decoder = FrameDecoder::new(frame_buf);
    while let Ok(Some((op, body))) = decoder.next() {
        if op == FrameOp::BeginRenderPass {
            let p = BeginRenderPassBody::from_bytes(body)
                .map_err(|_| "BEGIN_RENDERPASS body too short")?;
            return Ok(ResourceId(p.target_image_id));
        }
    }
    Err("frame contains no BEGIN_RENDERPASS")
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
