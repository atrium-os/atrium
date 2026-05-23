//! Tier-2 software-execution Backend.
//!
//! Implements the [`Backend`] trait by composition: an
//! internal Tier-2 shader registry plus a per-image
//! framebuffer map keyed by `ResourceId`. The interesting
//! method is [`Tier2Backend::run_fragment_shader_into`],
//! which routes a registered Tier-2 fragment shader's
//! output into a previously-created image.
//!
//! # Phase status
//!
//! **Phase 2 v5d step 2.** This is the scaffolding tier:
//! the Backend trait surface is implemented but
//! `submit_frame` is a stub. Real wire-protocol routing —
//! where a guest's draw call against a Tier-2-bound
//! pipeline kicks off `run_fragment_shader_into`
//! automatically — lands in v5e once the wire ops for
//! "bind a Tier-2 pipeline" are finalised.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};

use aqueduct_gpu::ids::ResourceId;
use aqueduct_gpu::backends::{BackendId, GpuVendor};

use crate::backend::Backend;
use crate::tier2_registry::{
    BlendFactor, BlendFactorPair, BlendOp, BlendState, ColorWriteMask,
    DrawTriangle, Tier2ExecError, Tier2Registry, Tier2ShaderId,
};

use aqueduct_gpu::frame::{
    BindDepthAttachmentCmd, BindIndexBufCmd, BindVertexBufCmd, DispatchCmd,
    DrawCmd, DrawIndexedCmd, FrameDecoder, IndexType, SetViewportCmd,
};
use aqueduct_gpu::opcodes::FrameOp;
use aqueduct_gpu::{
    Tier2BlendFactor as WireBlendFactor,
    Tier2BlendOp as WireBlendOp,
    Tier2BlendState as WireBlendState,
    Tier2ComputeStateBlob, Tier2DepthState, VertexInputState,
};

/// Backend that routes draws through Tier-2 compiled
/// fragment shaders. Image storage lives in this backend
/// (one RGBA8 buffer per registered image) so calls to
/// [`Tier2Backend::run_fragment_shader_into`] can write
/// pixels without going through `image_write_pixels`.
pub struct Tier2Backend {
    registry: Arc<Tier2Registry>,
    images:   Mutex<HashMap<u64, ImageStorage>>,
    /// Depth-format image storage. Distinct map from `images`
    /// (RGBA8 colour) because depth lives as `Vec<f32>` and
    /// fill_image_triangle's depth_buffer parameter expects
    /// `&mut [f32]`. Populated by `image_created_depth`;
    /// referenced via `BindDepthAttachment` in the wire.
    depth_images: Mutex<HashMap<u32, DepthImageStorage>>,
    /// Per-(image_id) flag: has this depth image been
    /// cleared yet in the current render pass?  The walker
    /// resets it on EndRenderPass; a fresh BindDepthAttachment
    /// inside a pass triggers a one-shot fill_with(clear_value).
    /// Avoids re-clearing on every BindDepthAttachment when an
    /// app re-issues it mid-pass.
    depth_clear_cleared: Mutex<HashMap<u32, bool>>,
    /// Per-buffer byte storage keyed by `ResourceId.raw()`.
    /// Populated on `buffer_created`; filled by
    /// `buffer_write_bytes`. The draw-walker (D.3+) reads
    /// vertex / index data out of here.
    buffers:  Mutex<HashMap<u32, BufferStorage>>,
    /// Pipeline ResourceId.raw() → bound Tier-2 fragment
    /// shader. Populated by [`Tier2Backend::bind_pipeline`]
    /// (called by the Session when it processes
    /// `OP_GPU_PIPELINE_CREATE` for a Tier-2 pipeline).
    /// `submit_frame` consults this map to know which
    /// shader to fire for each `FrameOp::BindPipeline`
    /// record it walks.
    pipeline_shaders: Mutex<HashMap<u32, Tier2ShaderId>>,
    /// Per-pipeline Tier-2 vertex shader (mirror of
    /// `pipeline_shaders` for VS). Populated by
    /// [`Tier2Backend::bind_pipeline_vs`] when the session
    /// processes a Tier-2 graphics pipeline create.
    pipeline_vs_shaders: Mutex<HashMap<u32, Tier2ShaderId>>,
    /// Vertex-input layout keyed by pipeline ResourceId.raw().
    /// Populated by [`Tier2Backend::bind_pipeline_layout`] when
    /// the session decodes a `Tier2PipelineStateBlob`. The
    /// frame-walker consults this map at Draw time to slice
    /// bound vertex buffers into per-vertex attribute bytes.
    pipeline_layouts: Mutex<HashMap<u32, VertexInputState>>,
    /// Per-pipeline raster state (depth + blend). Populated by
    /// [`Tier2Backend::bind_raster_state`] when the session
    /// decodes a `Tier2PipelineStateBlob`.
    pipeline_raster: Mutex<HashMap<u32, PipelineRasterState>>,
    /// Per-pipeline compute-shader binding: Tier-2 shader id
    /// + workgroup local-size. Populated when the session
    /// processes a Compute-kind pipeline create.
    pipeline_compute: Mutex<HashMap<u32, (Tier2ShaderId, Tier2ComputeStateBlob)>>,
    /// Cumulative count of `cs_main` invocations the walker
    /// has driven. Observable for tests + diagnostics.
    cs_invocations: AtomicU64,
    /// Last-dispatch SSBO snapshot (the bytes the shader
    /// wrote into the `out_buffer` parameter through its
    /// `StorageBuffer` storage-class variable).
    last_compute_output: Mutex<Option<Vec<u8>>>,
    /// Last-dispatch per-binding SSBO snapshots.  Index =
    /// binding number.  Populated when the bound compute
    /// pipeline declares >= 2 SSBO bindings (multi-binding
    /// arc).  For the single-binding legacy path, this stays
    /// empty and callers read `last_compute_output` instead.
    last_compute_outputs_by_binding: Mutex<Vec<Vec<u8>>>,
    /// Optional pre-fill bytes per binding, used by the
    /// next dispatch.  The dispatcher copies these into the
    /// freshly-allocated buffer for the matching binding
    /// before invoking cs_main; missing entries (or shorter
    /// ones than the buffer) leave the rest zeroed.  Used
    /// by tests that need to verify the shader's read path
    /// (RMW, parallel reductions, etc.).
    compute_input_by_binding: Mutex<HashMap<u32, Vec<u8>>>,
    /// Storage-image bindings for the next compute dispatch:
    /// descriptor binding -> image_id.raw().  The dispatcher
    /// builds an `ImageDesc` table from these (looking each
    /// image up in `images`) and passes its base in the X0
    /// (`uniforms`) cs_main slot.  Populated by
    /// `bind_compute_storage_image`; consumed (drained) per
    /// dispatch.
    compute_image_by_binding: Mutex<HashMap<u32, u64>>,
    /// Per-dispatch SSBO size in bytes.  Default 4 KiB.
    compute_output_capacity: std::sync::atomic::AtomicUsize,
    submissions: AtomicU64,
    presents:    AtomicU64,
    /// Total Draw / DrawIndexed records the frame-walker has
    /// dispatched against a Tier-2-bound pipeline. The D.3
    /// dispatch path is a stub — D.4 turns each dispatch into
    /// per-primitive `fill_image_triangle` calls — but the
    /// counter is observable now so the wire walker has unit
    /// tests of its own.
    draws_executed: AtomicU64,
    /// Draws / DrawIndexeds the walker decoded but skipped
    /// because no Tier-2 pipeline was bound at that point
    /// (e.g. a guest that submits geometry against a non-Tier-2
    /// pipeline). Observable for diagnostics.
    draws_skipped: AtomicU64,
    /// Per-vertex packed attribute bytes assembled by the most
    /// recent `dispatch_draw`. Exposed for D.4 testing; D.5
    /// turns this into the input of `fill_image_triangle`.
    last_assembled_vertices: Mutex<Option<AssembledVertices>>,
    /// Per-surface "most recently presented" frame: snapshot
    /// of the source image's pixels at the time the present
    /// envelope arrived, keyed by surface_id. The WSI bring-
    /// up shape -- a real Fresco hook-up forwards these bytes
    /// onto the compositor; tests read them back to verify
    /// the present wire worked.
    presented_frames: Mutex<HashMap<u64, PresentedFrame>>,
    /// Optional callback invoked on every successful
    /// `present`. The Tier2Backend's own snapshot still
    /// updates `presented_frames`; the callback is the push
    /// hook a downstream consumer (atrium-compositor's
    /// Fresco bridge, a screen-recorder, a test tap) plugs
    /// into to forward frames anywhere.
    present_callback: Mutex<Option<PresentCallback>>,
}

/// Boxed callback fired by `Tier2Backend::present`. Arguments
/// are `(surface_id, &PresentedFrame)`. Mutex-guarded; held
/// only across the per-frame dispatch, so taking the lock
/// inside the callback (e.g. to forward through another
/// Mutex-protected resource) is fine.
pub type PresentCallback =
    Box<dyn Fn(u64, &PresentedFrame) + Send + Sync + 'static>;

/// One presented frame's snapshot (width, height, RGBA8 pixels)
/// plus the frame_id from `vkQueuePresentKHR`.
#[derive(Debug, Clone)]
pub struct PresentedFrame {
    /// Source image width in pixels.
    pub width: u32,
    /// Source image height in pixels.
    pub height: u32,
    /// Tightly-packed RGBA8 pixels (length = width * height * 4).
    pub pixels: Vec<u8>,
    /// Frame id from the present envelope (Vulkan
    /// `pImageIndices` value).
    pub frame_id: u64,
}

/// Output of [`Tier2Backend::assemble_vertices`]: per-vertex
/// attribute bytes laid out by (location, format). Lengths:
/// `attribute_offsets.len() == layout.attributes.len() + 1`,
/// `bytes.len() == vertex_count * stride_per_vertex` where the
/// total stride is `attribute_offsets.last()`.
#[derive(Debug, Clone)]
pub struct AssembledVertices {
    /// Number of vertices packed.
    pub vertex_count: u32,
    /// Per-vertex stride in bytes (sum of all attribute sizes,
    /// densely packed in shader-`location` order).
    pub stride: u32,
    /// Byte offset of each attribute within one vertex; final
    /// entry equals `stride` (so `[i..i+1]` gives the i-th
    /// attribute's range).
    pub attribute_offsets: Vec<u32>,
    /// Packed bytes: `vertex_count` records of `stride` bytes.
    pub bytes: Vec<u8>,
}

/// Per-render-pass bound state assembled by the frame walker.
///
/// One instance per `BeginRenderPass ... EndRenderPass` span.
/// Mutated as the walker visits each frame op; consumed by
/// `Draw` / `DrawIndexed` dispatch.
#[derive(Debug, Default)]
struct PassState {
    /// Currently-bound pipeline ResourceId.raw(), if any.
    pipeline_raw: Option<u32>,
    /// Tier-2 fragment shader bound to the current pipeline,
    /// looked up via `pipeline_shaders` at BindPipeline time.
    tier2_shader: Option<Tier2ShaderId>,
    /// Tier-2 vertex shader bound to the current pipeline.
    tier2_vs_shader: Option<Tier2ShaderId>,
    /// Pipeline raster state (depth + blend), resolved at
    /// BindPipeline. `None` = no pipeline bound yet.
    raster: Option<PipelineRasterState>,
    /// Tier-2 compute binding (shader + workgroup size) for
    /// the currently-bound pipeline. `None` if the bound
    /// pipeline is graphics (or nothing's bound).
    tier2_compute: Option<(Tier2ShaderId, Tier2ComputeStateBlob)>,
    /// Depth attachment image_id for the current render
    /// pass (set by FrameOp::BindDepthAttachment). `None`
    /// means draws fall back to the per-pass scratch depth
    /// buffer.
    depth_attachment: Option<u32>,
    /// Vertex-buffer bindings keyed by slot number.
    vertex_buffers: HashMap<u32, BoundVertexBuffer>,
    /// Currently-bound index buffer.
    index_buffer: Option<BoundIndexBuffer>,
    /// Current viewport (may be unset for a malformed frame; the
    /// walker tolerates it but a real draw would error in D.4+).
    viewport: Option<SetViewportCmd>,
    /// Latest push-constants block; tier-2 shaders consume it
    /// as their uniform area.
    push_constants: Vec<u8>,
    /// Number of Draw / DrawIndexed records issued in this pass.
    /// Used to gate the legacy "BindPipeline alone implies a
    /// fullscreen FS fill" fallback at EndRenderPass.
    draws_in_pass: u32,
}

/// Per-pipeline raster state assembled at pipeline-create
/// time and consulted at Draw time. Defaults match the
/// implicit pre-D.6 behaviour: no depth attachment +
/// source-replace blend.
#[derive(Debug, Clone, Copy, Default)]
struct PipelineRasterState {
    depth: Option<Tier2DepthState>,
    blend: BlendState,
}

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // fields consumed by D.4+ vertex layout path
struct BoundVertexBuffer {
    buffer_raw: u32,
    offset: u64,
}

#[derive(Debug, Clone, Copy)]
struct BoundIndexBuffer {
    buffer_raw: u32,
    offset: u64,
    index_type: IndexType,
}

/// Per-image RGBA8 storage owned by the backend.
struct ImageStorage {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

/// Per-image depth (`D32_SFLOAT`-equivalent) storage.  One
/// `f32` per pixel.  Persists across draws and across
/// render passes -- the only reset point is an explicit
/// `BindDepthAttachment` whose `clear_value` re-fills the
/// buffer, and only the first time per pass.
#[allow(dead_code)] // width / height are debug-only for now
struct DepthImageStorage {
    width: u32,
    height: u32,
    pixels: Vec<f32>,
}

/// Per-buffer byte storage owned by the backend. `size` is the
/// declared capacity from `OP_GPU_BUFFER_CREATE`; `bytes` is
/// pre-zeroed to that size so partial writes via
/// `OP_GPU_BUFFER_WRITE` land at the right offsets without
/// needing growth.
struct BufferStorage {
    size: u64,
    bytes: Vec<u8>,
}

/// `VK_DESCRIPTOR_TYPE_STORAGE_IMAGE`.
const DESCRIPTOR_TYPE_STORAGE_IMAGE: u32 = 3;

/// Bytes per descriptor write in a `FrameOp::BindDescriptors`
/// body.  The ICD's `vkCmdBindDescriptorSets` emits, per
/// write, `{ binding u32, type u32, buffer_id u32,
/// image_id u32, sampler_id u32, offset u64, range u64 }` =
/// 5×4 + 2×8 = 36 bytes.  (The ICD's own comment says "32 B"
/// -- that is stale; the real layout is 36.)
const BIND_DESCRIPTORS_WRITE_BYTES: usize = 36;

/// Parse a `FrameOp::BindDescriptors` body and return the
/// `(binding, image_id)` pairs whose descriptor type is
/// `STORAGE_IMAGE`.  Body layout: an 8-byte header
/// `{ set_index u32, write_count u32 }` followed by
/// `write_count` 36-byte writes (see
/// `BIND_DESCRIPTORS_WRITE_BYTES`).  Non-storage-image
/// writes (SSBOs, samplers, UBOs) and writes with a zero
/// image id are skipped.
fn parse_bind_descriptors_storage_images(body: &[u8]) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    if body.len() < 8 { return out; }
    let u32_at = |o: usize| -> u32 {
        u32::from_le_bytes(body[o..o + 4].try_into().unwrap())
    };
    let write_count = u32_at(4) as usize;
    for w in 0..write_count {
        let off = 8 + w * BIND_DESCRIPTORS_WRITE_BYTES;
        if off + BIND_DESCRIPTORS_WRITE_BYTES > body.len() { break; }
        let binding  = u32_at(off);
        let dtype    = u32_at(off + 4);
        let image_id = u32_at(off + 12);
        if dtype == DESCRIPTOR_TYPE_STORAGE_IMAGE && image_id != 0 {
            out.push((binding, image_id));
        }
    }
    out
}

impl Tier2Backend {
    /// Construct a fresh Tier2Backend backed by the given
    /// registry. The registry can be shared across
    /// backends; image storage is per-backend.
    pub fn new(registry: Arc<Tier2Registry>) -> Self {
        Self {
            registry,
            images: Mutex::new(HashMap::new()),
            depth_images: Mutex::new(HashMap::new()),
            depth_clear_cleared: Mutex::new(HashMap::new()),
            buffers: Mutex::new(HashMap::new()),
            pipeline_shaders: Mutex::new(HashMap::new()),
            pipeline_vs_shaders: Mutex::new(HashMap::new()),
            pipeline_layouts: Mutex::new(HashMap::new()),
            pipeline_raster: Mutex::new(HashMap::new()),
            pipeline_compute: Mutex::new(HashMap::new()),
            cs_invocations: AtomicU64::new(0),
            last_compute_output: Mutex::new(None),
            last_compute_outputs_by_binding: Mutex::new(Vec::new()),
            compute_input_by_binding: Mutex::new(HashMap::new()),
            compute_image_by_binding: Mutex::new(HashMap::new()),
            compute_output_capacity: std::sync::atomic::AtomicUsize::new(4096),
            last_assembled_vertices: Mutex::new(None),
            presented_frames: Mutex::new(HashMap::new()),
            present_callback: Mutex::new(None),
            submissions: AtomicU64::new(0),
            presents:    AtomicU64::new(0),
            draws_executed: AtomicU64::new(0),
            draws_skipped:  AtomicU64::new(0),
        }
    }

    /// Cumulative count of Draw / DrawIndexed records executed
    /// against a Tier-2-bound pipeline by the frame walker.
    pub fn draw_count(&self) -> u64 {
        self.draws_executed.load(Ordering::Relaxed)
    }

    /// Cumulative count of Draw / DrawIndexed records the
    /// walker decoded but skipped because no Tier-2 pipeline
    /// was bound.
    pub fn draws_skipped(&self) -> u64 {
        self.draws_skipped.load(Ordering::Relaxed)
    }

    /// Read back a buffer's bytes (clone). Returns `None` if
    /// the buffer isn't registered. Used by tests + by the
    /// D.3 draw-walker to source vertex / index data.
    pub fn read_buffer_bytes(&self, buffer_id: ResourceId) -> Option<Vec<u8>> {
        let buffers = self.buffers.lock().unwrap();
        buffers.get(&buffer_id.raw()).map(|b| b.bytes.clone())
    }

    /// Snapshot of every registered buffer's (id, bytes).
    /// Convenience for integration tests that don't know the
    /// guest's pre-allocated ResourceId up front (the ICD
    /// path allocates buffer IDs on the daemon side).
    pub fn all_buffer_bytes(&self) -> Vec<(ResourceId, Vec<u8>)> {
        self.buffers.lock().unwrap().iter()
            .map(|(raw, b)| (ResourceId(*raw), b.bytes.clone()))
            .collect()
    }

    /// Associate a pipeline ResourceId with a Tier-2
    /// fragment shader. When `submit_frame` later sees a
    /// `BindPipeline` of this id followed by a draw, it
    /// fires `run_fragment_shader_into` for the active
    /// renderpass's target image.
    pub fn bind_pipeline(
        &self,
        pipeline_id: ResourceId,
        shader_id: Tier2ShaderId,
    ) {
        self.pipeline_shaders.lock().unwrap()
            .insert(pipeline_id.raw(), shader_id);
    }

    /// Associate a pipeline ResourceId with its Tier-2 vertex
    /// shader. The frame-walker uses both the VS + FS bindings
    /// when dispatching a `Draw` against `fill_image_triangle`.
    pub fn bind_pipeline_vs(
        &self,
        pipeline_id: ResourceId,
        shader_id: Tier2ShaderId,
    ) {
        self.pipeline_vs_shaders.lock().unwrap()
            .insert(pipeline_id.raw(), shader_id);
    }

    /// How many `submit_frame` calls have arrived. Useful
    /// for tests + diagnostics.
    pub fn submission_count(&self) -> u64 {
        self.submissions.load(Ordering::Relaxed)
    }

    /// How many `present` calls have arrived.
    pub fn present_count(&self) -> u64 {
        self.presents.load(Ordering::Relaxed)
    }

    /// Run a compiled fragment shader once per pixel of
    /// `image_id`, writing the result into the backend's
    /// image storage. The shader id and image must both
    /// have been registered (via `Tier2Registry::register`
    /// and `Backend::image_created` respectively).
    pub fn run_fragment_shader_into(
        &self,
        image_id: ResourceId,
        shader_id: Tier2ShaderId,
        push_constants: &[u8],
        uniforms: &[u8],
    ) -> Result<(), Tier2ExecError> {
        let mut images = self.images.lock().unwrap();
        let img = images.get_mut(&(image_id.raw() as u64))
            .ok_or(Tier2ExecError::UnknownShader(shader_id))?;
        self.registry.fill_image_fragment(
            shader_id,
            push_constants, uniforms,
            img.width, img.height,
            &mut img.pixels,
        )
    }

    /// Associate a pipeline ResourceId with a vertex-input
    /// layout (mirrors `bind_pipeline` but for geometry state).
    pub fn bind_layout(&self, pipeline_id: ResourceId, layout: VertexInputState) {
        self.pipeline_layouts.lock().unwrap()
            .insert(pipeline_id.raw(), layout);
    }

    /// Associate a pipeline with its depth + blend raster
    /// state. Either field is `None` to keep the default
    /// (no depth attachment / source-replace blending).
    pub fn bind_raster_state(
        &self,
        pipeline_id: ResourceId,
        depth: Option<Tier2DepthState>,
        blend: Option<WireBlendState>,
    ) {
        let blend = blend.map(convert_blend_state).unwrap_or_default();
        self.pipeline_raster.lock().unwrap().insert(
            pipeline_id.raw(),
            PipelineRasterState { depth, blend },
        );
    }

    /// Snapshot of the per-vertex bytes the most recent Draw
    /// assembled. `None` if no Draw has dispatched yet (or the
    /// last Draw was rejected before assembly).
    pub fn last_assembled_vertices(&self) -> Option<AssembledVertices> {
        self.last_assembled_vertices.lock().unwrap().clone()
    }

    /// Tier-2 vertex shader currently bound to `pipeline_id`,
    /// or `None` if no VS has been bound.
    pub fn pipeline_vs_shader(&self, pipeline_id: ResourceId) -> Option<Tier2ShaderId> {
        self.pipeline_vs_shaders.lock().unwrap()
            .get(&pipeline_id.raw()).copied()
    }

    /// Tier-2 fragment shader currently bound to `pipeline_id`.
    pub fn pipeline_fs_shader(&self, pipeline_id: ResourceId) -> Option<Tier2ShaderId> {
        self.pipeline_shaders.lock().unwrap()
            .get(&pipeline_id.raw()).copied()
    }

    /// True if a vertex-input layout has been registered for
    /// the given pipeline.
    pub fn pipeline_has_layout(&self, pipeline_id: ResourceId) -> bool {
        self.pipeline_layouts.lock().unwrap()
            .contains_key(&pipeline_id.raw())
    }

    /// Vertex-input layout currently bound to `pipeline_id`,
    /// cloned for inspection. `None` if no layout has been
    /// bound to this pipeline.
    pub fn pipeline_layout(&self, pipeline_id: ResourceId) -> Option<VertexInputState> {
        self.pipeline_layouts.lock().unwrap()
            .get(&pipeline_id.raw()).cloned()
    }

    /// Associate a pipeline with its Tier-2 compute shader +
    /// workgroup local-size state.
    pub fn bind_compute_pipeline(
        &self,
        pipeline_id: ResourceId,
        shader_id: Tier2ShaderId,
        compute_state: Tier2ComputeStateBlob,
    ) {
        self.pipeline_compute.lock().unwrap()
            .insert(pipeline_id.raw(), (shader_id, compute_state));
    }

    /// Look up a pipeline's compute binding, if any.
    pub fn pipeline_compute(&self, pipeline_id: ResourceId)
        -> Option<(Tier2ShaderId, Tier2ComputeStateBlob)>
    {
        self.pipeline_compute.lock().unwrap()
            .get(&pipeline_id.raw()).cloned()
    }

    /// Cumulative count of `cs_main` invocations driven by
    /// the frame-walker's Dispatch handler.  Sums to
    /// groupCount[xyz] * local_size[xyz] across the dispatch's
    /// lifetime.
    pub fn cs_invocation_count(&self) -> u64 {
        self.cs_invocations.load(Ordering::Relaxed)
    }

    /// Snapshot of the per-dispatch output buffer the most
    /// recent `dispatch_compute` filled (cloned).  Shaders
    /// write into this buffer via SSBOs (the `StorageBuffer`
    /// SPIR-V storage class); the backend dispatcher zeroes
    /// it before each dispatch.  Default capacity 4096 bytes;
    /// override via [`Tier2Backend::set_compute_output_capacity`].
    pub fn compute_output_bytes(&self) -> Option<Vec<u8>> {
        self.last_compute_output.lock().unwrap().clone()
    }

    /// Snapshot of the per-binding output buffer for binding
    /// `b` from the most recent multi-binding dispatch.
    /// Returns `None` if the last dispatch was single-binding
    /// (use [`Tier2Backend::compute_output_bytes`] for that)
    /// or if `b` is out of range.
    pub fn compute_output_bytes_for_binding(&self, b: u32) -> Option<Vec<u8>> {
        let g = self.last_compute_outputs_by_binding.lock().unwrap();
        g.get(b as usize).cloned()
    }

    /// Stash bytes the next dispatch should pre-fill into
    /// binding `b`'s output buffer before invoking
    /// `cs_main`.  Multiple calls accumulate (one per
    /// binding); the stash is *consumed* by the next
    /// dispatch (cleared so a follow-up dispatch starts
    /// from zero again).
    ///
    /// `bytes.len()` may be less than the dispatch capacity;
    /// the remainder stays zero.  Bytes past the dispatch
    /// capacity are silently truncated.
    pub fn set_compute_input_for_binding(&self, b: u32, bytes: Vec<u8>) {
        self.compute_input_by_binding.lock().unwrap().insert(b, bytes);
    }

    /// Bind a storage image (a previously `image_created`
    /// resource) to compute descriptor binding `b` for the
    /// next dispatch.  The dispatcher builds an `ImageDesc`
    /// table from these bindings and passes its base in the
    /// X0 cs_main slot, so `OpImageRead` / `OpImageWrite` in
    /// the shader reach the image's pixel storage.  Consumed
    /// (drained) per dispatch.
    pub fn bind_compute_storage_image(&self, b: u32, image_id: ResourceId) {
        self.compute_image_by_binding.lock().unwrap()
            .insert(b, image_id.raw() as u64);
    }

    /// Set the per-dispatch output-buffer capacity (bytes).
    /// Applied to subsequent dispatches; in-flight ones keep
    /// the prior size.
    pub fn set_compute_output_capacity(&self, capacity: usize) {
        self.compute_output_capacity.store(capacity, Ordering::Relaxed);
    }

    /// Most recent presented frame for a given Fresco
    /// surface_id (typically a window-id). `None` if nothing
    /// has been presented to that surface in this session.
    pub fn last_presented_frame(&self, surface_id: u64) -> Option<PresentedFrame> {
        self.presented_frames.lock().unwrap()
            .get(&surface_id).cloned()
    }

    /// Install (or replace) the per-frame present callback.
    /// The callback fires synchronously inside `present`
    /// after the per-surface snapshot is updated; consumers
    /// that need to keep up with display refresh should
    /// dispatch the actual blit through a channel from
    /// within the callback rather than blocking here.
    pub fn set_present_callback(&self, cb: PresentCallback) {
        *self.present_callback.lock().unwrap() = Some(cb);
    }

    /// Remove a previously-installed present callback.
    pub fn clear_present_callback(&self) {
        *self.present_callback.lock().unwrap() = None;
    }

    /// Read back a registered image's RGBA8 pixels.
    /// `None` if the image isn't registered.
    pub fn read_image_pixels(&self, image_id: ResourceId) -> Option<Vec<u8>> {
        let images = self.images.lock().unwrap();
        images.get(&(image_id.raw() as u64)).map(|img| img.pixels.clone())
    }

    /// Register a depth-format image. Called by the session
    /// when `OP_GPU_IMAGE_CREATE` arrives with a depth-aspect
    /// format (the existing `image_created` hook is RGBA8-
    /// only; depth needs `Vec<f32>` storage).
    pub fn register_depth_image(
        &self, image_id: ResourceId, width: u32, height: u32,
    ) {
        if width == 0 || height == 0 { return; }
        const MAX_DIM: u32 = 16 * 1024;
        if width > MAX_DIM || height > MAX_DIM { return; }
        let pixels = vec![f32::INFINITY;
            (width as usize) * (height as usize)];
        self.depth_images.lock().unwrap().insert(image_id.raw(),
            DepthImageStorage { width, height, pixels });
    }

    /// Drop a depth image's storage.
    pub fn unregister_depth_image(&self, image_id: ResourceId) {
        self.depth_images.lock().unwrap().remove(&image_id.raw());
        self.depth_clear_cleared.lock().unwrap().remove(&image_id.raw());
    }

    /// Snapshot of a depth image's f32 pixels.  Used by tests
    /// to verify depth was actually written / cleared.
    pub fn read_depth_image_pixels(&self, image_id: ResourceId)
        -> Option<Vec<f32>>
    {
        self.depth_images.lock().unwrap()
            .get(&image_id.raw()).map(|d| d.pixels.clone())
    }

    /// Run the per-pass state machine over `pass_bytes`. Walks
    /// every FrameOp record between BeginRenderPass and
    /// EndRenderPass, maintains bound state, and dispatches
    /// Draw / DrawIndexed against the bound Tier-2 shader.
    ///
    /// Legacy compat: if a Tier-2 pipeline is bound but no
    /// Draw is issued in this pass, fall back to a fullscreen
    /// FS fill at EndRenderPass (preserves the pre-D.3
    /// "BindPipeline implies fullscreen" shape used by
    /// existing integration tests; D.5+ migrates them to real
    /// Draw records).
    fn execute_pass(&self, target_id: ResourceId, pass_bytes: &[u8]) {
        let pipeline_shaders    = self.pipeline_shaders.lock().unwrap().clone();
        let pipeline_vs_shaders = self.pipeline_vs_shaders.lock().unwrap().clone();
        let pipeline_raster     = self.pipeline_raster.lock().unwrap().clone();
        let pipeline_compute    = self.pipeline_compute.lock().unwrap().clone();
        let mut state = PassState::default();
        // D.6: depth buffer is per-pass, allocated lazily on
        // first draw with depth-test enabled. Cleared to +inf
        // so any in-range fragment wins the first LESS test.
        let mut depth_buffer: Option<Vec<f32>> = None;
        let mut decoder = FrameDecoder::new(pass_bytes);

        loop {
            let rec = match decoder.next() {
                Ok(Some(r)) => r,
                Ok(None) => break,
                Err(e) => {
                    log::warn!("Tier2Backend::execute_pass: \
                                decoder error on target {target_id}: {e}");
                    break;
                }
            };
            let (op, body) = rec;
            match op {
                FrameOp::BindPipeline => {
                    if body.len() >= 4 {
                        let raw = u32::from_le_bytes([
                            body[0], body[1], body[2], body[3]
                        ]);
                        state.pipeline_raw    = Some(raw);
                        state.tier2_shader    = pipeline_shaders.get(&raw).copied();
                        state.tier2_vs_shader = pipeline_vs_shaders.get(&raw).copied();
                        state.raster          = pipeline_raster.get(&raw).copied();
                        state.tier2_compute   = pipeline_compute.get(&raw).cloned();
                    } else {
                        log::warn!("BindPipeline body too short ({} bytes)", body.len());
                    }
                }
                FrameOp::BindVertexBuf => match BindVertexBufCmd::from_bytes(body) {
                    Ok(cmd) => {
                        state.vertex_buffers.insert(cmd.binding, BoundVertexBuffer {
                            buffer_raw: cmd.buffer_id,
                            offset: cmd.offset,
                        });
                    }
                    Err(e) => log::warn!("malformed BindVertexBuf: {e}"),
                },
                FrameOp::BindIndexBuf => match BindIndexBufCmd::from_bytes(body) {
                    Ok(cmd) => {
                        state.index_buffer = Some(BoundIndexBuffer {
                            buffer_raw: cmd.buffer_id,
                            offset: cmd.offset,
                            index_type: cmd.index_type,
                        });
                    }
                    Err(e) => log::warn!("malformed BindIndexBuf: {e}"),
                },
                FrameOp::SetViewport => match SetViewportCmd::from_bytes(body) {
                    Ok(cmd) => state.viewport = Some(cmd),
                    Err(e) => log::warn!("malformed SetViewport: {e}"),
                },
                FrameOp::BindDepthAttachment => {
                    match BindDepthAttachmentCmd::from_bytes(body) {
                        Ok(cmd) => {
                            // Clear the depth image's pixels on the
                            // first bind of this render pass; later
                            // re-binds of the same image inside the
                            // pass leave existing depth alone (lets
                            // apps re-issue the bind without
                            // wiping work-in-progress).
                            let already_cleared = self.depth_clear_cleared
                                .lock().unwrap()
                                .get(&cmd.image_id).copied().unwrap_or(false);
                            if !already_cleared {
                                if let Some(d) = self.depth_images
                                    .lock().unwrap()
                                    .get_mut(&cmd.image_id)
                                {
                                    d.pixels.fill(cmd.clear_value);
                                }
                                self.depth_clear_cleared.lock().unwrap()
                                    .insert(cmd.image_id, true);
                            }
                            state.depth_attachment = Some(cmd.image_id);
                        }
                        Err(e) => log::warn!("malformed BindDepthAttachment: {e}"),
                    }
                }
                FrameOp::PushConstants => {
                    // Body shape: 4-byte header (stage_mask u8 +
                    // offset u8 + reserved u16) followed by the
                    // payload bytes. Tier-2 ignores stage_mask
                    // and offset for now (the bound shader's
                    // SPIR-V dictates layout already) -- strip
                    // the header so the FS / CS reads its data
                    // from offset 0.
                    if body.len() < 4 {
                        log::warn!("PushConstants body too short \
                                    ({} bytes; need >=4 for header)",
                                   body.len());
                    } else {
                        state.push_constants.clear();
                        state.push_constants.extend_from_slice(&body[4..]);
                    }
                }
                FrameOp::Draw => match DrawCmd::from_bytes(body) {
                    Ok(cmd) => {
                        state.draws_in_pass = state.draws_in_pass.saturating_add(1);
                        self.dispatch_draw(target_id, &state, cmd, &mut depth_buffer);
                    }
                    Err(e) => log::warn!("malformed Draw: {e}"),
                },
                FrameOp::DrawIndexed => match DrawIndexedCmd::from_bytes(body) {
                    Ok(cmd) => {
                        state.draws_in_pass = state.draws_in_pass.saturating_add(1);
                        self.dispatch_draw_indexed(
                            target_id, &state, cmd, &mut depth_buffer);
                    }
                    Err(e) => log::warn!("malformed DrawIndexed: {e}"),
                },
                FrameOp::Dispatch => match DispatchCmd::from_bytes(body) {
                    Ok(cmd) => {
                        // Vulkan allows Dispatch outside a render
                        // pass; the walker tolerates that -- the
                        // outer partition_renderpasses iteration
                        // visits each pass body individually, so
                        // a compute-only command buffer arrives
                        // here through partition's "no Begin/End"
                        // pass slice.
                        state.draws_in_pass = state.draws_in_pass.saturating_add(1);
                        self.dispatch_compute(&state, cmd);
                    }
                    Err(e) => log::warn!("malformed Dispatch: {e}"),
                },
                FrameOp::EndRenderPass => {
                    // Mark this pass's depth attachment as
                    // "needs clear next pass". Vulkan's
                    // default LoadOp is Clear, so the next
                    // pass that binds the same depth image
                    // expects fresh `clear_value` pixels.
                    if let Some(id) = state.depth_attachment {
                        self.depth_clear_cleared.lock().unwrap().remove(&id);
                    }
                    break;
                }
                // Ops we don't yet act on: SetScissor,
                // BindDescriptors (compute-only; handled by
                // execute_compute_ops), CopyBufToImg, CopyImgToBuf,
                // Blit, PipelineBarrier, Dispatch{,Indirect},
                // DrawIndirect, BeginRenderPass (handled by the
                // outer partition step). These will grow handlers
                // in later phases as needed.
                _ => {}
            }
        }

        // Legacy fallback: pre-D.3 tests bind a Tier-2 pipeline
        // and expect a fullscreen FS fill with no Draw. Preserve
        // that until D.5+ migrates them to real Draw records.
        if state.draws_in_pass == 0 {
            if let Some(shader_id) = state.tier2_shader {
                if let Err(e) = self.run_fragment_shader_into(
                    target_id, shader_id, &state.push_constants, &[])
                {
                    log::warn!("Tier2Backend: legacy fullscreen FS fill \
                                into {target_id} failed: {e}");
                }
            }
        }
    }

    /// Dispatch a `Draw` against the current pass state. D.4:
    /// looks up the bound pipeline's vertex-input layout, gathers
    /// per-vertex attribute bytes from each bound vertex buffer
    /// per the layout, packs them densely (location-order) into
    /// `last_assembled_vertices`. D.5 turns the packed bytes into
    /// `fill_image_triangle` calls.
    fn dispatch_draw(
        &self,
        target_id: ResourceId,
        state: &PassState,
        cmd: DrawCmd,
        depth_buffer: &mut Option<Vec<f32>>,
    ) {
        if state.tier2_shader.is_none() {
            self.draws_skipped.fetch_add(1, Ordering::Relaxed);
            log::debug!("Draw on target {target_id} skipped: no Tier-2 pipeline bound");
            return;
        }
        if cmd.vertex_count == 0 {
            log::debug!("Draw on target {target_id} skipped: vertex_count=0");
            return;
        }

        let pipeline_raw = match state.pipeline_raw {
            Some(p) => p,
            None => {
                log::warn!("Draw on target {target_id}: tier2 shader bound but \
                            no pipeline id recorded (impossible state)");
                return;
            }
        };
        let layout = match self.pipeline_layouts.lock().unwrap().get(&pipeline_raw).cloned() {
            Some(l) => l,
            None => {
                log::warn!("Draw on target {target_id}: pipeline {pipeline_raw:#x} \
                            has no vertex-input layout; skipping");
                return;
            }
        };

        let assembled = match self.assemble_vertices(
            &layout, &state.vertex_buffers, cmd.first_vertex, cmd.vertex_count,
        ) {
            Ok(a) => a,
            Err(e) => {
                log::warn!("Draw on target {target_id}: vertex assembly failed: {e}");
                return;
            }
        };

        *self.last_assembled_vertices.lock().unwrap() = Some(assembled.clone());
        self.draws_executed.fetch_add(1, Ordering::Relaxed);

        // D.5: if a Tier-2 VS is bound, treat the assembled
        // bytes as a triangle list and call fill_image_triangle
        // for each triangle. With no VS bound (i.e. only an FS
        // -- the legacy "fullscreen FS fill" pattern), the
        // post-loop fallback in execute_pass handles it.
        let Some(vs_shader_id) = state.tier2_vs_shader else { return };
        let Some(fs_shader_id) = state.tier2_shader else { return };

        if assembled.vertex_count % 3 != 0 {
            log::warn!("Draw target={target_id}: vertex_count {} is not a \
                        multiple of 3; non-triangle topologies aren't \
                        supported by tier-2 yet",
                       assembled.vertex_count);
            return;
        }

        let stride = assembled.stride as usize;
        let tri_count = (assembled.vertex_count / 3) as usize;

        // Lock the image storage for the duration of all
        // triangles in this Draw. Each fill_image_triangle
        // call mutates the target's pixel buffer; we hold the
        // map lock to avoid the map being reshaped under us.
        let mut images = self.images.lock().unwrap();
        let img = match images.get_mut(&(target_id.raw() as u64)) {
            Some(i) => i,
            None => {
                log::warn!("Draw target={target_id}: target image not registered");
                return;
            }
        };
        let width = img.width;
        let height = img.height;
        let pixels = &mut img.pixels[..];

        // D.6: raster state -> per-draw blend + depth.
        let raster = state.raster.unwrap_or_default();
        let depth_enabled = raster.depth.map(|d| d.test_enable).unwrap_or(false);

        // Depth source priority:
        //   1) Persisted depth attachment (BindDepthAttachment)
        //      -> hold depth_images lock for the loop, use the
        //         image's f32 pixels directly.
        //   2) Per-pass scratch buffer (depth-test enabled but
        //      no attachment bound) -- preserves the pre-
        //      attachment behaviour for tests that don't wire
        //      a depth image.
        //   3) None (depth-test disabled).
        let mut depth_lock = if depth_enabled {
            state.depth_attachment.and_then(|id|
                self.depth_images.lock().ok().map(|g| (id, g)))
        } else { None };

        if depth_enabled && depth_lock.is_none() && depth_buffer.is_none() {
            *depth_buffer = Some(vec![
                f32::INFINITY;
                (width as usize) * (height as usize)
            ]);
        }

        for t in 0..tri_count {
            let v0 = &assembled.bytes[(3*t)*stride   .. (3*t+1)*stride];
            let v1 = &assembled.bytes[(3*t+1)*stride .. (3*t+2)*stride];
            let v2 = &assembled.bytes[(3*t+2)*stride .. (3*t+3)*stride];
            let dt = DrawTriangle {
                vertex_attrs: [v0, v1, v2],
                push_constants: &state.push_constants,
                blend_state: raster.blend,
                ..Default::default()
            };
            let db_ref: Option<&mut [f32]> = if !depth_enabled {
                None
            } else if let Some((id, ref mut guard)) = depth_lock {
                guard.get_mut(&id).map(|d| &mut d.pixels[..])
            } else {
                depth_buffer.as_deref_mut()
            };
            if let Err(e) = self.registry.fill_image_triangle(
                vs_shader_id, fs_shader_id,
                &dt, width, height, pixels, db_ref,
            ) {
                log::warn!("Draw target={target_id}: triangle {t}/{tri_count} \
                            fill_image_triangle failed: {e}");
                return;
            }
        }
    }

    /// Gather per-vertex attribute bytes for `vertex_count`
    /// vertices starting at `first_vertex`, sourcing each
    /// attribute from the bound buffer at `attr.binding`,
    /// offset `vertex_buffers[binding].offset + first_vertex *
    /// binding_stride + attr.offset`. The output packs attributes
    /// in shader-location order with no padding between them.
    fn assemble_vertices(
        &self,
        layout: &VertexInputState,
        vertex_buffers: &HashMap<u32, BoundVertexBuffer>,
        first_vertex: u32,
        vertex_count: u32,
    ) -> Result<AssembledVertices, String> {
        let indices: Vec<u32> = (0..vertex_count)
            .map(|v| first_vertex + v).collect();
        self.assemble_vertices_by_index(layout, vertex_buffers, &indices)
    }

    /// Same as `assemble_vertices` but gathers vertices by an
    /// explicit per-output-slot vertex-index list. `Draw` uses
    /// a contiguous range; `DrawIndexed` builds the list by
    /// walking the bound index buffer + applying `vertex_offset`.
    fn assemble_vertices_by_index(
        &self,
        layout: &VertexInputState,
        vertex_buffers: &HashMap<u32, BoundVertexBuffer>,
        indices: &[u32],
    ) -> Result<AssembledVertices, String> {
        // Order attributes by shader location so the packed
        // record is shader-input-order (varying-friendly).
        let mut attrs: Vec<_> = layout.attributes.iter().collect();
        attrs.sort_by_key(|a| a.location);

        let mut out_offsets = Vec::with_capacity(attrs.len() + 1);
        let mut out_stride: u32 = 0;
        for a in &attrs {
            out_offsets.push(out_stride);
            out_stride += a.format.byte_size() as u32;
        }
        out_offsets.push(out_stride);

        let vertex_count = indices.len() as u32;
        let buffers = self.buffers.lock().unwrap();
        let mut bytes = vec![0u8; (vertex_count as usize) * (out_stride as usize)];

        for (v, &global_v) in indices.iter().enumerate() {
            let out_base = v * (out_stride as usize);
            for (ai, a) in attrs.iter().enumerate() {
                let bind = layout.bindings.iter().find(|b| b.binding == a.binding)
                    .ok_or_else(|| format!(
                        "no binding desc for slot {}", a.binding))?;
                let slot = vertex_buffers.get(&a.binding)
                    .ok_or_else(|| format!(
                        "vertex buffer not bound at slot {}", a.binding))?;
                let src = buffers.get(&slot.buffer_raw).ok_or_else(|| format!(
                    "vertex buffer {:#x} not in backend storage", slot.buffer_raw))?;
                let src_off = (slot.offset as usize)
                    + (global_v as usize) * (bind.stride as usize)
                    + (a.offset as usize);
                let size = a.format.byte_size();
                let src_end = src_off + size;
                if src_end > src.bytes.len() {
                    return Err(format!(
                        "attribute @location {} (vertex {}) reads bytes \
                         {}..{} past buffer end {}",
                        a.location, global_v, src_off, src_end,
                        src.bytes.len()));
                }
                let out_off = out_base + (out_offsets[ai] as usize);
                bytes[out_off..out_off + size]
                    .copy_from_slice(&src.bytes[src_off..src_end]);
            }
        }

        Ok(AssembledVertices {
            vertex_count, stride: out_stride,
            attribute_offsets: out_offsets, bytes,
        })
    }

    /// Read `count` indices from the bound index buffer at
    /// (offset + first_index * index_size), apply
    /// `vertex_offset` (signed add, saturating clamped), and
    /// return the resolved per-output-slot vertex indices.
    fn gather_indices(
        &self,
        bound: &BoundIndexBuffer,
        first_index: u32,
        count: u32,
        vertex_offset: i32,
    ) -> Result<Vec<u32>, String> {
        let buffers = self.buffers.lock().unwrap();
        let src = buffers.get(&bound.buffer_raw)
            .ok_or_else(|| format!(
                "index buffer {:#x} not in backend storage",
                bound.buffer_raw))?;
        let elem_size: usize = match bound.index_type {
            IndexType::Uint16 => 2,
            IndexType::Uint32 => 4,
        };
        let base = (bound.offset as usize)
            + (first_index as usize) * elem_size;
        let end = base + (count as usize) * elem_size;
        if end > src.bytes.len() {
            return Err(format!(
                "index buffer read {base}..{end} exceeds buffer size {}",
                src.bytes.len()));
        }
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count as usize {
            let off = base + i * elem_size;
            let idx_raw: u32 = match bound.index_type {
                IndexType::Uint16 => u16::from_le_bytes(
                    src.bytes[off..off+2].try_into().unwrap()) as u32,
                IndexType::Uint32 => u32::from_le_bytes(
                    src.bytes[off..off+4].try_into().unwrap()),
            };
            // Signed add (saturating): vkCmdDrawIndexed treats
            // the result as a u32 vertex index.
            let signed = idx_raw as i64 + vertex_offset as i64;
            if signed < 0 {
                return Err(format!(
                    "index {idx_raw} + vertex_offset {vertex_offset} \
                     wraps below 0"));
            }
            if signed > u32::MAX as i64 {
                return Err(format!(
                    "index {idx_raw} + vertex_offset {vertex_offset} \
                     exceeds u32::MAX"));
            }
            out.push(signed as u32);
        }
        Ok(out)
    }

    /// Dispatch a `vkCmdDispatch` against the bound compute
    /// pipeline. Drives `cs_main` once per (workgroup_id,
    /// local_id) pair, i.e. groupCount[xyz] * local_size[xyz]
    /// total invocations.  Pipeline must be Tier-2-compute-bound
    /// (graphics pipeline bound + Dispatch is a guest bug; we
    /// log and skip).
    fn dispatch_compute(&self, state: &PassState, cmd: DispatchCmd) {
        let Some((shader_id, cs_state)) = state.tier2_compute.clone() else {
            self.draws_skipped.fetch_add(1, Ordering::Relaxed);
            log::debug!("Dispatch skipped: no Tier-2 compute pipeline bound");
            return;
        };
        if cmd.group_count_x == 0 || cmd.group_count_y == 0 || cmd.group_count_z == 0 {
            return;
        }
        let loaded = match self.registry.get(shader_id) {
            Some(l) => l,
            None => {
                log::warn!("Dispatch: shader {shader_id:?} not in registry");
                return;
            }
        };
        let cs_main = match loaded.entry_points.cs_main {
            Some(f) => f,
            None => {
                log::warn!("Dispatch: bound shader {shader_id:?} has no cs_main");
                return;
            }
        };

        let pc_ptr = if state.push_constants.is_empty() {
            std::ptr::null()
        } else { state.push_constants.as_ptr() };

        // Storage-image descriptor table.  When the next
        // dispatch has images bound (via
        // `bind_compute_storage_image`), build an
        // `ImageDesc` table and pass its base in the X0
        // (`uniforms`) cs_main slot.  The `images` lock is
        // held for the whole dispatch so the pixel buffers
        // the ImageDescs point into stay put; `image_descs`
        // must not reallocate after the table references it,
        // so it is filled fully before the table is built.
        let img_bindings: Vec<(u32, u64)> = {
            let mut m = self.compute_image_by_binding.lock().unwrap();
            m.drain().collect()
        };
        let images_guard;
        let mut image_descs: Vec<atrium_spv_runtime::ImageDesc> = Vec::new();
        let mut image_table: Vec<u8>;
        let uni_ptr: *const u8 = if img_bindings.is_empty() {
            std::ptr::null()
        } else {
            images_guard = self.images.lock().unwrap();
            // Highest binding index decides the table size.
            let max_binding = img_bindings.iter()
                .map(|(b, _)| *b).max().unwrap_or(0);
            image_descs.reserve((max_binding as usize) + 1);
            // Build one ImageDesc per binding (in binding
            // order so slot N lands at index N).
            let mut slot_of: Vec<Option<usize>> =
                vec![None; (max_binding as usize) + 1];
            for &(binding, image_raw) in &img_bindings {
                if let Some(img) = images_guard.get(&image_raw) {
                    let idx = image_descs.len();
                    image_descs.push(atrium_spv_runtime::ImageDesc {
                        data: img.pixels.as_ptr() as *mut u8,
                        width: img.width,
                        height: img.height,
                        stride_bytes: img.width * 4,
                        // ImageStorage is RGBA8 (4 B/texel).
                        format: atrium_spv_runtime::StorageFormat::Rgba8Unorm
                            as u32,
                        // 2D image: single slice.
                        depth: 1,
                        slice_bytes: img.width * img.height * 4,
                        // Single-mip; mip_descs is null for
                        // the no-Lod path.
                        mip_count: 0,
                        mip_descs: std::ptr::null(),
                    });
                    slot_of[binding as usize] = Some(idx);
                }
            }
            image_table = atrium_spv_runtime::image_table_buffer(
                (max_binding as usize) + 1);
            // SAFETY: the helper fn pointers are in this
            // process; image_descs outlives the dispatch.
            unsafe {
                atrium_spv_runtime::write_image_helper_pointers(
                    &mut image_table,
                    atrium_spv_runtime::atrium_img_read_2d,
                    atrium_spv_runtime::atrium_img_write_2d,
                    atrium_spv_runtime::atrium_img_read_3d,
                    atrium_spv_runtime::atrium_img_write_3d,
                    atrium_spv_runtime::atrium_img_read_2d_lod,
                    atrium_spv_runtime::atrium_img_write_2d_lod,
                    atrium_spv_runtime::atrium_img_read_3d_lod,
                    atrium_spv_runtime::atrium_img_write_3d_lod);
                for (binding, maybe_idx) in slot_of.iter().enumerate() {
                    if let Some(idx) = maybe_idx {
                        atrium_spv_runtime::write_image_descriptor_slot(
                            &mut image_table, binding,
                            &image_descs[*idx] as *const _);
                    }
                }
            }
            image_table.as_ptr()
        };

        let total_invocations = (cmd.group_count_x as u64)
            * (cmd.group_count_y as u64) * (cmd.group_count_z as u64)
            * (cs_state.local_size_x as u64)
            * (cs_state.local_size_y as u64) * (cs_state.local_size_z as u64);
        // Soft cap: a 1024^3 dispatch with 32^3 local size is 2^35
        // invocations -- would lock the daemon for minutes.  Cap
        // at 2^24 (16M) for bring-up; real workloads will need
        // a proper async-dispatch path anyway.
        const MAX_INVOCATIONS: u64 = 1 << 24;
        if total_invocations > MAX_INVOCATIONS {
            log::warn!("Dispatch: {total_invocations} invocations exceeds \
                        bring-up cap {MAX_INVOCATIONS}; rejecting");
            return;
        }

        // SSBO output buffer(s) for this dispatch.  Zeroed
        // at the start, mutated by `cs_main` invocations
        // through the `StorageBuffer` storage class.
        //
        // Single-binding (legacy):
        //   X2 = pointer to a single output buffer.
        // Multi-binding (ssbo_binding_count >= 2):
        //   X2 = pointer to a descriptor table -- an array
        //   of u64 pointers, one per binding.  Each binding's
        //   buffer is allocated separately so per-binding
        //   readback works.
        let cap = self.compute_output_capacity.load(Ordering::Relaxed);
        let n_bindings = cs_state.ssbo_binding_count;
        let mut per_binding: Vec<Vec<u8>>;
        // descriptor_table must outlive the dispatch loop --
        // out_ptr borrows from it for the multi-binding case.
        let mut descriptor_table: Vec<u64> = Vec::new();
        let n_buffers_to_alloc = n_bindings.max(1) as usize;
        per_binding = (0..n_buffers_to_alloc).map(|_| vec![0u8; cap]).collect();
        // Apply pre-fill stash: copy bytes for each binding
        // that has a registered input, then clear the stash
        // so the next dispatch starts clean.
        {
            let mut stash = self.compute_input_by_binding.lock().unwrap();
            for (binding, bytes) in stash.drain() {
                let i = binding as usize;
                if let Some(buf) = per_binding.get_mut(i) {
                    let n = bytes.len().min(buf.len());
                    buf[..n].copy_from_slice(&bytes[..n]);
                }
            }
        }
        let out_ptr: *mut u8 = if n_bindings >= 2 {
            descriptor_table = per_binding.iter_mut()
                .map(|buf| buf.as_mut_ptr() as u64).collect();
            descriptor_table.as_mut_ptr() as *mut u8
        } else {
            per_binding[0].as_mut_ptr()
        };
        let _ = descriptor_table;

        // Workgroup-parallel dispatch.  Workgroups are
        // independent by Vulkan semantics (no cross-workgroup
        // synchronisation), so we partition the gz/gy/gx grid
        // across worker threads.  Within a workgroup the
        // lz/ly/lx loop stays serial so shared-memory and
        // ControlBarrier semantics are preserved per workgroup.
        //
        // Atomics lower to ARMv8.1 LSE instructions and are
        // race-safe across workgroups.  Each workgroup gets
        // its own zeroed slice of the per-thread workgroup
        // scratch buffer; since invocations within a workgroup
        // run serially on one thread, ControlBarrier is a
        // no-op (the causal order is already total).
        let total_workgroups = (cmd.group_count_x as u64)
            * (cmd.group_count_y as u64)
            * (cmd.group_count_z as u64);
        let n_threads = std::thread::available_parallelism()
            .map(|n| n.get()).unwrap_or(1).min(total_workgroups as usize).max(1);
        let out_addr = out_ptr as usize;
        let uni_addr = uni_ptr as *const u8 as usize;
        let pc_addr  = pc_ptr  as *const u8 as usize;
        let gx_n = cmd.group_count_x;
        let gy_n = cmd.group_count_y;
        let gz_n = cmd.group_count_z;
        let lx_n = cs_state.local_size_x;
        let ly_n = cs_state.local_size_y;
        let lz_n = cs_state.local_size_z;
        // Per-workgroup shared-memory scratch size.  Each
        // worker thread owns one buffer of this size, zeroed
        // before every workgroup it runs.
        let wg_bytes = cs_state.workgroup_size as usize;
        let chunk = total_workgroups.div_ceil(n_threads as u64);
        std::thread::scope(|s| {
            for t in 0..n_threads {
                let lo = (t as u64) * chunk;
                let hi = ((t as u64 + 1) * chunk).min(total_workgroups);
                if lo >= hi { continue; }
                s.spawn(move || {
                    let out_ptr = out_addr as *mut u8;
                    let uni_ptr = uni_addr as *const u8;
                    let pc_ptr  = pc_addr  as *const u8;
                    // This thread's private workgroup scratch.
                    let mut wg_buf: Vec<u8> = vec![0u8; wg_bytes];
                    for widx in lo..hi {
                        // Delinearise widx into (gx, gy, gz).
                        let gx = (widx % gx_n as u64) as u32;
                        let gy = ((widx / gx_n as u64) % gy_n as u64) as u32;
                        let gz = (widx / (gx_n as u64 * gy_n as u64)) as u32;
                        let _ = gz_n;
                        // Fresh shared memory per workgroup.
                        for b in wg_buf.iter_mut() { *b = 0; }
                        let wg_ptr = if wg_bytes == 0 {
                            std::ptr::null_mut()
                        } else {
                            wg_buf.as_mut_ptr()
                        };
                        for lz in 0..lz_n {
                            for ly in 0..ly_n {
                                for lx in 0..lx_n {
                                    // SAFETY: cs_main is a dlopened
                                    // C-ABI function whose signature
                                    // matches CsMain (checked at
                                    // open time); the output buffer
                                    // and descriptor table outlive
                                    // this scope; wg_buf outlives
                                    // the inner loop.
                                    unsafe {
                                        cs_main(
                                            uni_ptr, pc_ptr, out_ptr,
                                            gx, gy, gz,
                                            lx, ly, lz,
                                            wg_ptr,
                                        );
                                    }
                                }
                            }
                        }
                    }
                });
            }
        });
        if n_bindings >= 2 {
            *self.last_compute_output.lock().unwrap() = None;
            *self.last_compute_outputs_by_binding.lock().unwrap() = per_binding;
        } else {
            *self.last_compute_output.lock().unwrap() = Some(per_binding.remove(0));
            self.last_compute_outputs_by_binding.lock().unwrap().clear();
        }
        self.cs_invocations.fetch_add(total_invocations, Ordering::Relaxed);
        self.draws_executed.fetch_add(1, Ordering::Relaxed);
    }

    /// Walk the full frame buffer, but only act on ops that
    /// arrive OUTSIDE any render pass: BindPipeline (resolves
    /// to a compute pipeline -> stored), PushConstants
    /// (forwarded into compute uniforms), Dispatch (drives
    /// `dispatch_compute`).  Graphics ops are handled by the
    /// per-pass walker in `execute_pass`.
    fn execute_compute_ops(&self, frame_buf: &[u8]) {
        let pipeline_compute = self.pipeline_compute.lock().unwrap().clone();
        let mut state = PassState::default();
        let mut rp_depth: u32 = 0;
        let mut decoder = FrameDecoder::new(frame_buf);
        while let Ok(Some((op, body))) = decoder.next() {
            match op {
                FrameOp::BeginRenderPass => rp_depth += 1,
                FrameOp::EndRenderPass => {
                    rp_depth = rp_depth.saturating_sub(1);
                    // A render pass ends -- any previous
                    // compute-pipeline bind is conventionally
                    // still valid until the next BindPipeline.
                    // No state reset.
                }
                _ if rp_depth > 0 => {
                    // Inside a render pass -- handled by
                    // execute_pass.
                }
                FrameOp::BindPipeline => {
                    if body.len() >= 4 {
                        let raw = u32::from_le_bytes([
                            body[0], body[1], body[2], body[3],
                        ]);
                        state.pipeline_raw = Some(raw);
                        state.tier2_compute = pipeline_compute.get(&raw).cloned();
                    }
                }
                FrameOp::PushConstants => {
                    // Body shape: 4-byte header (stage_mask u8 +
                    // offset u8 + reserved u16) followed by the
                    // payload bytes. Tier-2 ignores stage_mask
                    // and offset for now (the bound shader's
                    // SPIR-V dictates layout already) -- strip
                    // the header so the FS / CS reads its data
                    // from offset 0.
                    if body.len() < 4 {
                        log::warn!("PushConstants body too short \
                                    ({} bytes; need >=4 for header)",
                                   body.len());
                    } else {
                        state.push_constants.clear();
                        state.push_constants.extend_from_slice(&body[4..]);
                    }
                }
                FrameOp::BindDescriptors => {
                    // Storage-image descriptor writes feed the
                    // compute image table.  vkCmdBindDescriptorSets
                    // arrives before the Dispatch it applies to.
                    for (binding, image_raw)
                        in parse_bind_descriptors_storage_images(body)
                    {
                        self.bind_compute_storage_image(
                            binding, ResourceId(image_raw));
                    }
                }
                FrameOp::Dispatch => match DispatchCmd::from_bytes(body) {
                    Ok(cmd) => self.dispatch_compute(&state, cmd),
                    Err(e) => log::warn!("malformed Dispatch: {e}"),
                },
                _ => {}
            }
        }
    }

    /// Dispatch a `DrawIndexed` against the current pass state.
    /// D.3 stub; same shape as `dispatch_draw`. D.8 wires the
    /// index buffer slice.
    fn dispatch_draw_indexed(
        &self,
        target_id: ResourceId,
        state: &PassState,
        cmd: DrawIndexedCmd,
        depth_buffer: &mut Option<Vec<f32>>,
    ) {
        if state.tier2_shader.is_none() {
            self.draws_skipped.fetch_add(1, Ordering::Relaxed);
            log::debug!("DrawIndexed on target {target_id} skipped: no Tier-2 pipeline bound");
            return;
        }
        if cmd.index_count == 0 { return; }
        let bound_idx = match state.index_buffer {
            Some(b) => b,
            None => {
                log::warn!("DrawIndexed on target {target_id}: no index buffer bound");
                return;
            }
        };

        let pipeline_raw = match state.pipeline_raw {
            Some(p) => p,
            None => return,
        };
        let layout = match self.pipeline_layouts.lock().unwrap()
            .get(&pipeline_raw).cloned()
        {
            Some(l) => l,
            None => {
                log::warn!("DrawIndexed on target {target_id}: pipeline \
                            {pipeline_raw:#x} has no vertex-input layout");
                return;
            }
        };

        // Read indices from the bound index buffer, applying
        // vertex_offset.
        let indices = match self.gather_indices(
            &bound_idx, cmd.first_index, cmd.index_count, cmd.vertex_offset,
        ) {
            Ok(v) => v,
            Err(e) => {
                log::warn!("DrawIndexed target={target_id}: index gather: {e}");
                return;
            }
        };

        let assembled = match self.assemble_vertices_by_index(
            &layout, &state.vertex_buffers, &indices,
        ) {
            Ok(a) => a,
            Err(e) => {
                log::warn!("DrawIndexed target={target_id}: vertex assembly: {e}");
                return;
            }
        };

        *self.last_assembled_vertices.lock().unwrap() = Some(assembled.clone());
        self.draws_executed.fetch_add(1, Ordering::Relaxed);

        let Some(vs_shader_id) = state.tier2_vs_shader else { return };
        let Some(fs_shader_id) = state.tier2_shader else { return };

        if assembled.vertex_count % 3 != 0 {
            log::warn!("DrawIndexed target={target_id}: index_count {} \
                        is not a multiple of 3", assembled.vertex_count);
            return;
        }
        let stride = assembled.stride as usize;
        let tri_count = (assembled.vertex_count / 3) as usize;

        let mut images = self.images.lock().unwrap();
        let img = match images.get_mut(&(target_id.raw() as u64)) {
            Some(i) => i,
            None => {
                log::warn!("DrawIndexed target={target_id}: target image not registered");
                return;
            }
        };
        let width = img.width;
        let height = img.height;
        let pixels = &mut img.pixels[..];

        let raster = state.raster.unwrap_or_default();
        let depth_enabled = raster.depth.map(|d| d.test_enable).unwrap_or(false);
        if depth_enabled && depth_buffer.is_none() {
            *depth_buffer = Some(vec![
                f32::INFINITY;
                (width as usize) * (height as usize)
            ]);
        }

        for t in 0..tri_count {
            let v0 = &assembled.bytes[(3*t)*stride   .. (3*t+1)*stride];
            let v1 = &assembled.bytes[(3*t+1)*stride .. (3*t+2)*stride];
            let v2 = &assembled.bytes[(3*t+2)*stride .. (3*t+3)*stride];
            let dt = DrawTriangle {
                vertex_attrs: [v0, v1, v2],
                push_constants: &state.push_constants,
                blend_state: raster.blend,
                ..Default::default()
            };
            let db_ref = if depth_enabled {
                depth_buffer.as_deref_mut()
            } else { None };
            if let Err(e) = self.registry.fill_image_triangle(
                vs_shader_id, fs_shader_id,
                &dt, width, height, pixels, db_ref,
            ) {
                log::warn!("DrawIndexed target={target_id}: triangle {t}/{tri_count} \
                            fill_image_triangle failed: {e}");
                return;
            }
        }
    }
}

impl Backend for Tier2Backend {
    fn identity(&self) -> BackendId {
        BackendId::new(GpuVendor::Software, 2)
    }
    fn caps(&self) -> u64 { 0 }
    fn max_frame_bytes(&self) -> u32 { 1 << 20 }
    fn max_fences_inflight(&self) -> u32 { 16 }

    fn allocate_memory(&self, _size: u64, _usage: u8) -> [u8; 32] {
        let n = self.submissions.load(Ordering::Relaxed);
        let mut tok = [0u8; 32];
        tok[..8].copy_from_slice(&n.to_le_bytes());
        tok[31] = 0xC2;
        tok
    }

    fn image_created(&self, image_id: ResourceId, width: u32, height: u32) {
        if width == 0 || height == 0 { return; }
        const MAX_DIM: u32 = 16 * 1024;
        if width > MAX_DIM || height > MAX_DIM { return; }
        let pixels = vec![0u8; (width as usize) * (height as usize) * 4];
        self.images.lock().unwrap().insert(image_id.raw() as u64, ImageStorage {
            width, height, pixels,
        });
    }

    fn image_destroyed(&self, image_id: ResourceId) {
        self.images.lock().unwrap().remove(&(image_id.raw() as u64));
    }

    fn depth_image_created(&self, image_id: ResourceId, width: u32, height: u32) {
        self.register_depth_image(image_id, width, height);
    }

    fn depth_image_destroyed(&self, image_id: ResourceId) {
        self.unregister_depth_image(image_id);
    }

    fn buffer_created(&self, buffer_id: ResourceId, size: u64) {
        // Cap at 256 MiB to bound a misbehaving guest. Real
        // limits are negotiated via OP_GPU_HANDSHAKE in a
        // later phase; this is a safety floor for bring-up.
        const MAX_BYTES: u64 = 256 * 1024 * 1024;
        if size == 0 || size > MAX_BYTES {
            log::warn!("Tier2Backend::buffer_created: buffer {buffer_id} \
                        size {size} out of range (max {MAX_BYTES})");
            return;
        }
        self.buffers.lock().unwrap().insert(buffer_id.raw(), BufferStorage {
            size, bytes: vec![0u8; size as usize],
        });
    }

    fn buffer_destroyed(&self, buffer_id: ResourceId) {
        self.buffers.lock().unwrap().remove(&buffer_id.raw());
    }

    fn buffer_write_bytes(
        &self,
        buffer_id: ResourceId,
        offset: u64,
        bytes: &[u8],
    ) -> Result<(), String> {
        let mut buffers = self.buffers.lock().unwrap();
        let buf = buffers.get_mut(&buffer_id.raw())
            .ok_or_else(|| format!("buffer {buffer_id} not registered"))?;
        let end = offset.checked_add(bytes.len() as u64)
            .ok_or_else(|| "buffer write offset+len overflows u64".to_string())?;
        if end > buf.size {
            return Err(format!(
                "buffer write end {end} exceeds size {}", buf.size,
            ));
        }
        let off = offset as usize;
        buf.bytes[off..off + bytes.len()].copy_from_slice(bytes);
        Ok(())
    }

    fn submit_frame(
        &self,
        _fence_id: ResourceId,
        _timeline: u64,
        frame_buf: &[u8],
    ) -> bool {
        self.submissions.fetch_add(1, Ordering::Relaxed);

        // Partition into renderpasses (reuses the shared
        // helper used by SoftwareBackend).
        let passes = match crate::backend::partition_renderpasses(frame_buf) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("Tier2Backend::submit_frame: partition error: {e}");
                return true;
            }
        };

        for pass in &passes {
            let pass_bytes = &frame_buf[pass.byte_range.clone()];
            self.execute_pass(pass.target_id, pass_bytes);
        }
        // Compute ops live OUTSIDE render passes -- partition
        // doesn't produce slices for them, so a separate pass
        // over the whole buffer handles BindPipeline +
        // PushConstants + Dispatch when we're between
        // EndRenderPass and the next BeginRenderPass (or
        // before the first / after the last RP).
        self.execute_compute_ops(frame_buf);
        true
    }

    fn present(
        &self,
        image_id: ResourceId,
        surface_id: u64,
        frame_id: u64,
    ) {
        self.presents.fetch_add(1, Ordering::Relaxed);

        // Snapshot the source image's pixels into the per-
        // surface presented-frames map. In a wired-up
        // deployment this is where the tier-2 backend would
        // forward the pixels onto the Fresco compositor; here
        // it's the test-friendly bring-up shape.
        let snap = {
            let images = self.images.lock().unwrap();
            images.get(&(image_id.raw() as u64)).map(|img| PresentedFrame {
                width: img.width, height: img.height,
                pixels: img.pixels.clone(),
                frame_id,
            })
        };
        if let Some(frame) = snap {
            self.presented_frames.lock().unwrap()
                .insert(surface_id, frame.clone());
            // Fire the registered callback (if any).  The
            // callback runs synchronously; downstream consumers
            // that can't keep up should hand work off through a
            // channel.
            let cb = self.present_callback.lock().unwrap();
            if let Some(f) = cb.as_ref() {
                f(surface_id, &frame);
            }
        } else {
            log::warn!("Tier2Backend::present: image {image_id} not in storage; \
                        skipping snapshot for surface {surface_id}");
        }
    }

    fn bind_pipeline_tier2(
        &self,
        pipeline_id: ResourceId,
        tier2_shader_id: Tier2ShaderId,
    ) {
        self.bind_pipeline(pipeline_id, tier2_shader_id);
    }

    fn bind_pipeline_tier2_vs(
        &self,
        pipeline_id: ResourceId,
        tier2_shader_id: Tier2ShaderId,
    ) {
        self.bind_pipeline_vs(pipeline_id, tier2_shader_id);
    }

    fn bind_pipeline_layout(
        &self,
        pipeline_id: ResourceId,
        layout: VertexInputState,
    ) {
        self.bind_layout(pipeline_id, layout);
    }

    fn bind_pipeline_raster_state(
        &self,
        pipeline_id: ResourceId,
        depth: Option<Tier2DepthState>,
        blend: Option<WireBlendState>,
    ) {
        self.bind_raster_state(pipeline_id, depth, blend);
    }

    fn bind_pipeline_tier2_compute(
        &self,
        pipeline_id: ResourceId,
        tier2_shader_id: Tier2ShaderId,
        compute_state: Tier2ComputeStateBlob,
    ) {
        self.bind_compute_pipeline(pipeline_id, tier2_shader_id, compute_state);
    }
}

fn convert_blend_factor(f: WireBlendFactor) -> BlendFactor {
    match f {
        WireBlendFactor::Zero             => BlendFactor::Zero,
        WireBlendFactor::One              => BlendFactor::One,
        WireBlendFactor::SrcColor         => BlendFactor::SrcColor,
        WireBlendFactor::OneMinusSrcColor => BlendFactor::OneMinusSrcColor,
        WireBlendFactor::DstColor         => BlendFactor::DstColor,
        WireBlendFactor::OneMinusDstColor => BlendFactor::OneMinusDstColor,
        WireBlendFactor::SrcAlpha         => BlendFactor::SrcAlpha,
        WireBlendFactor::OneMinusSrcAlpha => BlendFactor::OneMinusSrcAlpha,
        WireBlendFactor::DstAlpha         => BlendFactor::DstAlpha,
        WireBlendFactor::OneMinusDstAlpha => BlendFactor::OneMinusDstAlpha,
    }
}

fn convert_blend_op(o: WireBlendOp) -> BlendOp {
    match o {
        WireBlendOp::Add => BlendOp::Add,
    }
}

fn convert_blend_state(s: WireBlendState) -> BlendState {
    BlendState {
        enable: s.enable,
        color: BlendFactorPair {
            src: convert_blend_factor(s.color_src),
            dst: convert_blend_factor(s.color_dst),
        },
        alpha: BlendFactorPair {
            src: convert_blend_factor(s.alpha_src),
            dst: convert_blend_factor(s.alpha_dst),
        },
        color_op: convert_blend_op(s.color_op),
        alpha_op: convert_blend_op(s.alpha_op),
        write_mask: ColorWriteMask {
            r: s.write_mask_rgba[0], g: s.write_mask_rgba[1],
            b: s.write_mask_rgba[2], a: s.write_mask_rgba[3],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::parse_bind_descriptors_storage_images;

    /// Build a BindDescriptors body: 8-byte header + 32-byte
    /// writes `{binding, type, buffer_id, image_id,
    /// sampler_id, offset u64, range u64}`.
    fn body(writes: &[(u32, u32, u32)]) -> Vec<u8> {
        // each write tuple = (binding, descriptor_type, image_id)
        let mut v = Vec::new();
        v.extend_from_slice(&0u32.to_le_bytes());                 // set_index
        v.extend_from_slice(&(writes.len() as u32).to_le_bytes()); // write_count
        for &(binding, dtype, image_id) in writes {
            v.extend_from_slice(&binding.to_le_bytes());
            v.extend_from_slice(&dtype.to_le_bytes());
            v.extend_from_slice(&0u32.to_le_bytes());   // buffer_id
            v.extend_from_slice(&image_id.to_le_bytes());
            v.extend_from_slice(&0u32.to_le_bytes());   // sampler_id
            v.extend_from_slice(&0u64.to_le_bytes());   // offset
            v.extend_from_slice(&0u64.to_le_bytes());   // range
        }
        v
    }

    #[test]
    fn bind_descriptors_picks_only_storage_images() {
        // type 3 = STORAGE_IMAGE, 7 = STORAGE_BUFFER, 1 = sampler.
        let b = body(&[
            (0, 7, 100),   // SSBO -- ignored
            (1, 3, 200),   // storage image -- kept
            (2, 1, 300),   // sampler -- ignored
            (5, 3, 400),   // storage image -- kept
        ]);
        let got = parse_bind_descriptors_storage_images(&b);
        assert_eq!(got, vec![(1, 200), (5, 400)]);
    }

    #[test]
    fn bind_descriptors_skips_zero_image_id() {
        let b = body(&[(0, 3, 0)]);
        assert!(parse_bind_descriptors_storage_images(&b).is_empty());
    }

    #[test]
    fn bind_descriptors_tolerates_short_and_empty_bodies() {
        assert!(parse_bind_descriptors_storage_images(&[]).is_empty());
        assert!(parse_bind_descriptors_storage_images(&[0u8; 3]).is_empty());
        // Header claims 2 writes but only one is present.
        let mut b = body(&[(1, 3, 200)]);
        b[4..8].copy_from_slice(&2u32.to_le_bytes());
        assert_eq!(parse_bind_descriptors_storage_images(&b), vec![(1, 200)]);
    }
}
