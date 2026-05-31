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

    /// Notify the backend a colour image was created, with the
    /// full array-layer count (`6` for cubemaps, `N` for 2D
    /// arrays, `1` for plain 2D).  The default forwards to
    /// [`Backend::image_created`] so backends that don't model
    /// layered images stay unchanged.  Tier-2 overrides this
    /// to allocate `width * height * 4 * layers` so array /
    /// cube sampling can address each slice.
    fn image_created_layered(
        &self,
        image_id: ResourceId,
        width: u32,
        height: u32,
        _array_layers: u32,
    ) {
        self.image_created(image_id, width, height);
    }

    /// Record a colour image's `VkFormat` (numeric) so the
    /// backend can sample its texels with the right
    /// interpretation (e.g. BGRA channel order, sRGB EOTF).
    /// Called right after `image_created` / `image_created_
    /// layered`.  Default no-op (backends that always treat
    /// colour images as RGBA8 ignore it).
    fn set_image_format(&self, _image_id: ResourceId, _vk_format: u32) {}

    /// Notify the backend an image was destroyed. Default no-op.
    fn image_destroyed(&self, _image_id: ResourceId) {}

    /// Notify the backend a depth-format image was created.
    /// Parallel to `image_created` but for `D32_SFLOAT`-style
    /// images that need per-pixel `f32` storage instead of
    /// RGBA8.  ICDs call this in vkBindImageMemory when the
    /// bound image's format is a depth format.  Default no-op.
    fn depth_image_created(
        &self, _image_id: ResourceId, _width: u32, _height: u32,
    ) {}

    /// Notify the backend a depth image was destroyed.
    fn depth_image_destroyed(&self, _image_id: ResourceId) {}

    /// Inline pixel write into an image. Called by the session when
    /// it processes `OP_GPU_IMAGE_WRITE`. SW backends copy into their
    /// per-image Pixmap; GPU backends stage through a transient
    /// upload buffer + vkCmdCopyBufferToImage equivalent. Returns
    /// `Err(diagnostic)` on validation failure (size mismatch,
    /// unknown image) — the session surfaces it as
    /// `OP_GPU_VALIDATION_ERR`. Default rejects (backends without
    /// inline-write support).
    fn image_write_pixels(
        &self,
        _image_id: ResourceId,
        _row_pitch: u32,
        _pixels: &[u8],
    ) -> Result<(), String> {
        Err("backend does not support inline image write".into())
    }

    /// Inline pixel write to a sub-region of an image. Called by the
    /// session when it processes `OP_GPU_IMAGE_WRITE_REGION`. Same
    /// contract as `image_write_pixels` for error reporting.
    /// Default rejects.
    #[allow(clippy::too_many_arguments)]
    fn image_write_region_pixels(
        &self,
        _image_id: ResourceId,
        _dst_x: u32, _dst_y: u32,
        _width: u32, _height: u32,
        _row_pitch: u32,
        _pixels: &[u8],
    ) -> Result<(), String> {
        Err("backend does not support inline image write_region".into())
    }

    /// Notify the backend a buffer was created. Default no-op.
    /// SW backends override this to allocate per-buffer byte
    /// storage so vertex / index data can be sourced at draw
    /// time without going through guest memory-region import.
    fn buffer_created(&self, _buffer_id: ResourceId, _size: u64) {}

    /// Notify the backend a buffer was destroyed. Default no-op.
    fn buffer_destroyed(&self, _buffer_id: ResourceId) {}

    /// Notify the backend a sampler was created with the given
    /// state.  Tier-2 stores the `SamplerDesc`-equivalent so the
    /// per-dispatch uniforms-table builder can hand the runtime
    /// helper (`atrium_tex_sample_2d` etc) a pointer to it.
    /// Other backends override to map this onto their native
    /// sampler primitive.  Default no-op.
    ///
    /// All fields are the raw `VkSamplerCreateInfo` u8/f32
    /// encoding (filter modes are VkFilter, address modes are
    /// VkSamplerAddressMode, mip_filter is VkSamplerMipmapMode).
    #[allow(clippy::too_many_arguments)]
    fn sampler_created(
        &self,
        _sampler_id:    ResourceId,
        _min_filter:    u8,
        _mag_filter:    u8,
        _mip_filter:    u8,
        _address_modes: [u8; 3],
        _max_anisotropy: f32,
        _min_lod:       f32,
        _max_lod:       f32,
        _compare_enable: u8,
        _compare_op:    u32,
    ) {}

    /// Notify the backend a sampler was destroyed.  Default
    /// no-op; Tier-2 evicts the stored descriptor.
    fn sampler_destroyed(&self, _sampler_id: ResourceId) {}

    /// Inline write into a previously-created buffer. Called by
    /// the session when it processes `OP_GPU_BUFFER_WRITE`.
    /// Default rejects (backends without inline-write support).
    fn buffer_write_bytes(
        &self,
        _buffer_id: ResourceId,
        _offset: u64,
        _bytes: &[u8],
    ) -> Result<(), String> {
        Err("backend does not support inline buffer write".into())
    }

    /// Inline read from a previously-created buffer.  Called by
    /// the session when it processes `OP_GPU_BUFFER_READ`.  Default
    /// rejects (backends without inline-read support: stub /
    /// software / moltenvk today; Tier2Backend overrides).
    ///
    /// Returns `bytes.len() == size` on success.
    fn buffer_read_bytes(
        &self,
        _buffer_id: ResourceId,
        _offset: u64,
        _size: u64,
    ) -> Result<Vec<u8>, String> {
        Err("backend does not support inline buffer read".into())
    }

    /// Submit a frame command stream. Returns `true` if the fence
    /// should be signalled now, `false` if signaling is deferred.
    fn submit_frame(
        &self,
        fence_id: ResourceId,
        timeline: u64,
        frame_buf: &[u8],
    ) -> bool;

    /// Present a previously-rendered image to a Fresco surface.
    /// Default impl: count the present + return; backends that
    /// route to a compositor override this. The `surface_id` is
    /// a Fresco window-id (atrium-vk-icd's VkSurfaceKHR handle
    /// is the same value).
    fn present(
        &self,
        _image_id:   ResourceId,
        _surface_id: u64,
        _frame_id:   u64,
    ) {}

    /// Tell the backend that pipeline `pipeline_id` should run
    /// `tier2_shader_id`'s compiled fragment shader during
    /// subsequent draws against it. Called by `Session::handle_
    /// pipeline_create` when the bound fragment shader has a
    /// `tier2_id` recorded.
    ///
    /// Default no-op for backends that don't support Tier-2
    /// shader execution; [`Tier2Backend`](crate::Tier2Backend)
    /// overrides this to populate its pipeline → shader map.
    fn bind_pipeline_tier2(
        &self,
        _pipeline_id: ResourceId,
        _tier2_shader_id: crate::Tier2ShaderId,
    ) {}

    /// Associate a pipeline with its Tier-2 *vertex* shader.
    /// Mirrors [`Backend::bind_pipeline_tier2`] (which carries
    /// the fragment shader). The session calls this when
    /// `PipelineCreatePayload::shaders[0]` resolves to a
    /// Tier-2-compiled vertex shader. Default no-op.
    fn bind_pipeline_tier2_vs(
        &self,
        _pipeline_id: ResourceId,
        _tier2_shader_id: crate::Tier2ShaderId,
    ) {}

    /// Associate a vertex-input layout with a pipeline. Called
    /// by the session when `PipelineCreatePayload::state_blob`
    /// decodes as a [`aqueduct_gpu::Tier2PipelineStateBlob`].
    /// The tier-2 backend uses this to slice bound vertex
    /// buffers into per-vertex attribute bytes at Draw time.
    /// Default no-op.
    fn bind_pipeline_layout(
        &self,
        _pipeline_id: ResourceId,
        _layout: aqueduct_gpu::VertexInputState,
    ) {}

    /// Associate raster state (depth + blend) with a pipeline.
    /// Called alongside [`Backend::bind_pipeline_layout`] when
    /// the state_blob decodes; either field may be `None` to
    /// keep that aspect at its default (no depth attachment /
    /// source-replace blending). Default no-op.
    fn bind_pipeline_raster_state(
        &self,
        _pipeline_id: ResourceId,
        _depth: Option<aqueduct_gpu::Tier2DepthState>,
        _blend: Option<aqueduct_gpu::Tier2BlendState>,
        _blend_extra: &[aqueduct_gpu::Tier2BlendState],
        _raster: Option<aqueduct_gpu::Tier2RasterState>,
        _topology: aqueduct_gpu::Tier2PrimitiveTopology,
        _stencil: Option<aqueduct_gpu::Tier2StencilState>,
        _primitive_restart_enable: bool,
    ) {}

    /// Record how many bytes the VS writes through
    /// Location-decorated `Output`-storage variables.  Tier-2
    /// uses this at Draw time to allocate per-vertex
    /// `vary_scratch` capture buffers + tell
    /// `fill_image_triangle` how many varyings to interpolate.
    /// 0 = the VS only emits `BuiltIn` outputs (e.g.
    /// `gl_Position`-only); rasterizer takes the
    /// null-varying path.  Default no-op.
    fn bind_pipeline_vs_varying_bytes(
        &self,
        _pipeline_id: ResourceId,
        _bytes: u32,
    ) {}

    /// Record whether the FS samples with implicit LOD.
    /// Default no-op.
    fn bind_pipeline_fs_implicit_lod(
        &self,
        _pipeline_id: ResourceId,
        _uses_implicit_lod: bool,
    ) {}

    /// Record the pipeline's MSAA sample count.  Default no-op.
    fn bind_pipeline_sample_count(
        &self,
        _pipeline_id: ResourceId,
        _sample_count: u32,
    ) {}

    /// Record whether the FS uses screen-space derivatives
    /// (`dFdx`/`dFdy`/`fwidth`).  Default no-op.
    fn bind_pipeline_fs_derivatives(
        &self,
        _pipeline_id: ResourceId,
        _uses_derivatives: bool,
    ) {}

    /// Associate a compute pipeline with its Tier-2 shader +
    /// workgroup-size state. Mirror of
    /// [`Backend::bind_pipeline_tier2_vs`] /
    /// [`Backend::bind_pipeline_layout`] for the compute kind.
    /// Default no-op.
    fn bind_pipeline_tier2_compute(
        &self,
        _pipeline_id: ResourceId,
        _tier2_shader_id: crate::Tier2ShaderId,
        _compute_state: aqueduct_gpu::Tier2ComputeStateBlob,
    ) {}
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
    presents:    AtomicU64,
}

impl Default for StubBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl StubBackend {
    /// Construct a fresh stub backend.
    pub fn new() -> Self {
        Self {
            submissions: AtomicU64::new(0),
            presents:    AtomicU64::new(0),
        }
    }

    /// How many frames have been submitted to this backend across
    /// all connections. Diagnostic.
    pub fn submission_count(&self) -> u64 {
        self.submissions.load(Ordering::Relaxed)
    }

    /// How many OP_GPU_PRESENT ops have been received. Diagnostic.
    pub fn present_count(&self) -> u64 {
        self.presents.load(Ordering::Relaxed)
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
    /// OP_GPU_PRESENT counter. Observable for tests.
    presents: AtomicU64,
    /// Optional hook called from `present()`. Lets a higher-layer
    /// daemon (frescod-aqueduct) install a routing callback that
    /// reads the rendered image's pixmap and writes it into a
    /// per-window WindowSurface. The hook receives the backend
    /// (so it can call `read_image_pixels`), the image_id, the
    /// surface_id (== Fresco window-id by atrium-vk-icd's
    /// VK_EXT_atrium_surface convention), and the monotonic
    /// frame_id. Default: None (just bumps the present counter).
    #[allow(clippy::type_complexity)]
    present_hook: Mutex<
        Option<Box<dyn Fn(&SoftwareBackend, ResourceId, u64, u64) + Send + Sync>>,
    >,
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
            presents: AtomicU64::new(0),
            present_hook: Mutex::new(None),
        }
    }

    /// How many OP_GPU_PRESENT ops have been received. Diagnostic /
    /// test observation.
    pub fn present_count(&self) -> u64 {
        self.presents.load(Ordering::Relaxed)
    }

    /// Install a present routing callback. The hook fires from
    /// `present()` after the counter bump; it receives a reference
    /// to this backend (call `read_image_pixels` to fetch the
    /// rendered bytes) and the present payload fields. Replaces
    /// any previously-installed hook.
    ///
    /// Used by daemons that own a compositor (frescod-aqueduct):
    /// install a hook that copies the rendered pixmap into the
    /// surface's per-window WindowSurface; the existing
    /// skip-hierarchy + queued page-flip path then handles the
    /// actual scanout.
    #[allow(clippy::type_complexity)]
    pub fn set_present_hook<F>(&self, hook: F)
    where
        F: Fn(&SoftwareBackend, ResourceId, u64, u64) + Send + Sync + 'static,
    {
        if let Ok(mut h) = self.present_hook.lock() {
            *h = Some(Box::new(hook));
        }
    }

    /// Remove any installed present hook.
    pub fn clear_present_hook(&self) {
        if let Ok(mut h) = self.present_hook.lock() {
            *h = None;
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

    fn image_write_pixels(
        &self,
        image_id: ResourceId,
        row_pitch: u32,
        pixels: &[u8],
    ) -> Result<(), String> {
        let mut images = self.images.lock()
            .map_err(|_| "image table poisoned".to_string())?;
        let pixmap = images.get_mut(&(image_id.raw() as u64))
            .ok_or_else(|| format!("image {image_id} not registered"))?;

        let width  = pixmap.width()  as usize;
        let height = pixmap.height() as usize;
        let dst_pitch = width * 4;
        let src_pitch = row_pitch as usize;

        if src_pitch < dst_pitch {
            return Err(format!(
                "row_pitch {} too small for {}×{} image (need ≥{})",
                src_pitch, width, height, dst_pitch));
        }
        if pixels.len() != src_pitch * height {
            return Err(format!(
                "pixel buffer length {} != row_pitch {} × height {}",
                pixels.len(), src_pitch, height));
        }

        // Copy row-by-row. tiny-skia Pixmaps are tightly packed
        // RGBA8 (no destination padding); source may pad to a wider
        // pitch. Note tiny-skia stores PREMULTIPLIED RGBA — callers
        // uploading non-premultiplied pixels must premultiply first.
        // (Tier-1 atlas-upload callers like glyph_cache already do.)
        let dst = pixmap.data_mut();
        for row in 0..height {
            let src_off = row * src_pitch;
            let dst_off = row * dst_pitch;
            dst[dst_off..dst_off + dst_pitch]
                .copy_from_slice(&pixels[src_off..src_off + dst_pitch]);
        }
        Ok(())
    }

    fn image_write_region_pixels(
        &self,
        image_id: ResourceId,
        dst_x: u32, dst_y: u32,
        width: u32, height: u32,
        row_pitch: u32,
        pixels: &[u8],
    ) -> Result<(), String> {
        let mut images = self.images.lock()
            .map_err(|_| "image table poisoned".to_string())?;
        let pixmap = images.get_mut(&(image_id.raw() as u64))
            .ok_or_else(|| format!("image {image_id} not registered"))?;

        let img_w = pixmap.width()  as u64;
        let img_h = pixmap.height() as u64;
        let dst_x = dst_x as u64;
        let dst_y = dst_y as u64;
        let w = width as u64;
        let h = height as u64;
        if dst_x.saturating_add(w) > img_w || dst_y.saturating_add(h) > img_h {
            return Err(format!(
                "region ({dst_x},{dst_y} {w}×{h}) extends past image bounds {img_w}×{img_h}"
            ));
        }
        let region_pitch = (w as usize) * 4;
        let src_pitch    = row_pitch as usize;
        if src_pitch < region_pitch {
            return Err(format!(
                "row_pitch {src_pitch} too small for region width {w} (need ≥{region_pitch})"
            ));
        }
        if pixels.len() != src_pitch * (h as usize) {
            return Err(format!(
                "pixel buffer length {} != row_pitch {} × height {}",
                pixels.len(), src_pitch, h
            ));
        }

        // Copy row-by-row into the sub-rect. The Pixmap's internal
        // row stride is `pixmap.width() * 4` (tightly packed).
        let pixmap_pitch = pixmap.width() as usize * 4;
        let dst = pixmap.data_mut();
        for row in 0..(h as usize) {
            let src_off = row * src_pitch;
            let dst_off = ((dst_y as usize) + row) * pixmap_pitch
                        + (dst_x as usize) * 4;
            dst[dst_off..dst_off + region_pitch]
                .copy_from_slice(&pixels[src_off..src_off + region_pitch]);
        }
        Ok(())
    }

    fn submit_frame(&self, _fence_id: ResourceId, _timeline: u64, frame_buf: &[u8]) -> bool {
        self.submissions.fetch_add(1, Ordering::Relaxed);

        // Partition the frame into renderpasses. Each pass renders
        // into its own target image; supporting multiple per frame
        // lets compositors do offscreen-then-sample patterns (e.g.
        // render glyph atlas, then sample it in the main pass).
        let passes = match partition_renderpasses(frame_buf) {
            Ok(p) if p.is_empty() => {
                log::warn!("SoftwareBackend::submit_frame: frame contains no BEGIN_RENDERPASS");
                self.dispatch_failures.fetch_add(1, Ordering::Relaxed);
                return true;
            }
            Ok(p) => p,
            Err(e) => {
                log::warn!("SoftwareBackend::submit_frame: {e}");
                self.dispatch_failures.fetch_add(1, Ordering::Relaxed);
                return true;
            }
        };

        let mut images = match self.images.lock() {
            Ok(g) => g,
            Err(_) => {
                self.dispatch_failures.fetch_add(1, Ordering::Relaxed);
                return true;
            }
        };

        for pass in &passes {
            // Take this pass's target Pixmap out of the map so we can
            // split borrows: the renderer needs `&mut PixmapMut` on
            // the target plus `&HashMap<u64, Pixmap>` for source
            // images (atlases for textured-rect / glyph_run).
            //
            // Critical for multi-pass: each pass's target may serve
            // as the SOURCE for a subsequent pass (the render-to-
            // texture pattern). Re-insertion happens between passes,
            // so subsequent passes see the previous pass's output as
            // a source image.
            let target_key = pass.target_id.raw() as u64;
            let Some(mut target_pixmap) = images.remove(&target_key) else {
                log::warn!(
                    "SoftwareBackend::submit_frame: pass target image {} not registered",
                    pass.target_id,
                );
                self.dispatch_failures.fetch_add(1, Ordering::Relaxed);
                continue;
            };

            // The pass's byte range is [Begin .. End], inclusive of
            // both bookend records — the renderer walks the full
            // range and runs its Begin/End handlers.
            let pass_bytes = &frame_buf[pass.byte_range.clone()];
            {
                let mut renderer = TinySkiaRenderer::new(target_pixmap.as_mut(), &images);
                match renderer.dispatch_frame(pass_bytes) {
                    Ok(draws) => log::debug!(
                        "SoftwareBackend: pass into {}: {draws} draws",
                        pass.target_id,
                    ),
                    Err(e) => {
                        log::warn!(
                            "SoftwareBackend::submit_frame: pass into {} failed: {e}",
                            pass.target_id,
                        );
                        self.dispatch_failures.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            images.insert(target_key, target_pixmap);
        }
        // Stub fence-signal semantics: tier-1 work completes inline,
        // so we signal immediately. Real GPU backends defer.
        true
    }

    fn present(&self, image_id: ResourceId, surface_id: u64, frame_id: u64) {
        self.presents.fetch_add(1, Ordering::Relaxed);
        log::debug!(
            "SoftwareBackend::present image={image_id} surface={surface_id} frame={frame_id}"
        );
        // Fire the installed routing hook, if any. The hook can
        // call `read_image_pixels` on us to materialize the
        // rendered bytes for its compositor.
        if let Ok(h) = self.present_hook.lock() {
            if let Some(hook) = h.as_ref() {
                hook(self, image_id, surface_id, frame_id);
            }
        }
    }
}

/// One contiguous BeginRenderPass..EndRenderPass slice of a frame
/// command stream. The byte range covers both bookends inclusively.
#[derive(Debug, Clone)]
pub(crate) struct RenderPassSlice {
    pub(crate) target_id: ResourceId,
    pub(crate) byte_range: std::ops::Range<usize>,
}

/// Walk the frame stream and pull out one [`RenderPassSlice`] per
/// BeginRenderPass..EndRenderPass pair. Records outside any
/// renderpass are ignored (they'd be a validation error at the
/// renderer level anyway).
pub(crate) fn partition_renderpasses(frame_buf: &[u8]) -> Result<Vec<RenderPassSlice>, &'static str> {
    use aqueduct_gpu::frame::FrameDecoder;
    use aqueduct_gpu::opcodes::FrameOp;

    let mut decoder = FrameDecoder::new(frame_buf);
    let mut out = Vec::new();
    // Offset of the in-progress BeginRenderPass record's first byte.
    let mut open: Option<(ResourceId, usize)> = None;
    let header_sz = 8usize; // FrameOp record header (per spec §5.1)

    let mut cursor = 0usize;
    while let Ok(Some((op, body))) = decoder.next() {
        let rec_start = cursor;
        let rec_end = cursor + header_sz + body.len();
        match op {
            FrameOp::BeginRenderPass => {
                if open.is_some() {
                    return Err("nested BEGIN_RENDERPASS");
                }
                let p = BeginRenderPassBody::from_bytes(body)
                    .map_err(|_| "BEGIN_RENDERPASS body too short")?;
                open = Some((ResourceId(p.target_image_id), rec_start));
            }
            FrameOp::EndRenderPass => {
                let (target_id, start) = open.take()
                    .ok_or("END_RENDERPASS without matching BEGIN")?;
                out.push(RenderPassSlice {
                    target_id,
                    byte_range: start..rec_end,
                });
            }
            _ => {}
        }
        cursor = rec_end;
    }
    if open.is_some() {
        return Err("BEGIN_RENDERPASS without matching END_RENDERPASS");
    }
    Ok(out)
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

    fn present(&self, _image_id: ResourceId, _surface_id: u64, _frame_id: u64) {
        self.presents.fetch_add(1, Ordering::Relaxed);
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

    #[test]
    fn software_backend_present_counter_ticks() {
        use aqueduct_gpu::ids::IdNamespace;
        let b = SoftwareBackend::new();
        assert_eq!(b.present_count(), 0);
        b.present(ResourceId::new(IdNamespace::IcdRuntime, 5), 42, 1);
        b.present(ResourceId::new(IdNamespace::IcdRuntime, 6), 42, 2);
        b.present(ResourceId::new(IdNamespace::IcdRuntime, 7), 42, 3);
        assert_eq!(b.present_count(), 3);
    }

    #[test]
    fn software_backend_present_hook_runs_with_atomic_counter() {
        use aqueduct_gpu::ids::IdNamespace;
        use std::sync::Arc;
        let hook_runs = Arc::new(AtomicU64::new(0));
        let hook_runs_inner = hook_runs.clone();
        let b = SoftwareBackend::new();
        b.set_present_hook(move |_back, _image_id, _surface_id, _frame_id| {
            hook_runs_inner.fetch_add(1, Ordering::Relaxed);
        });
        b.present(ResourceId::new(IdNamespace::IcdRuntime, 5), 42, 1);
        b.present(ResourceId::new(IdNamespace::IcdRuntime, 6), 42, 2);
        assert_eq!(hook_runs.load(Ordering::Relaxed), 2);
        b.clear_present_hook();
        b.present(ResourceId::new(IdNamespace::IcdRuntime, 7), 42, 3);
        assert_eq!(hook_runs.load(Ordering::Relaxed), 2);
        assert_eq!(b.present_count(), 3);
    }
}
