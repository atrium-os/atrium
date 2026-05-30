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
    CompareOp, CullMode, DrawTriangle, FrontFace, Scissor,
    StencilFaceState, StencilOp, StencilState, Tier2ExecError,
    Tier2Registry, Tier2ShaderId, Viewport,
};

use aqueduct_gpu::frame::{
    BindDepthAttachmentCmd, BindIndexBufCmd, BindVertexBufCmd, DispatchCmd,
    DrawCmd, DrawIndexedCmd, FrameDecoder, IndexType, SetScissorCmd,
    SetViewportCmd,
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
    /// Per-sampler runtime descriptor.  Populated from
    /// `Backend::sampler_created`'s `VkSamplerCreateInfo`-shaped
    /// arguments at sampler-create time.  Keyed by
    /// `ResourceId::raw()`.  Read at dispatch_draw time to
    /// fill the COMBINED_IMAGE_SAMPLER descriptor slots in the
    /// uniforms buffer the runtime's `atrium_tex_sample_*`
    /// helpers consume.
    samplers: Mutex<HashMap<u32, atrium_spv_runtime::SamplerDesc>>,
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
    /// Per-graphics-pipeline per-attachment blend for MRT
    /// attachments 1..N (attachment 0's blend lives in
    /// `PipelineRasterState::blend`).  Kept out of
    /// `PipelineRasterState` so that struct stays `Copy`.
    pipeline_blend_extra: Mutex<HashMap<u32, Vec<BlendState>>>,
    /// Per-graphics-pipeline: bytes the VS writes through
    /// Location-decorated Output variables.  Populated when
    /// the session decodes a `Tier2PipelineStateBlob`.  Used
    /// at Draw time to allocate per-vertex varying capture
    /// buffers + tell `fill_image_triangle` how many varyings
    /// to interpolate.  Missing or 0 means "no varyings; FS
    /// gets null in_varyings_ptr" (the legacy single-attribute
    /// graphics_roundtrip path).
    pipeline_vs_varying_bytes: Mutex<HashMap<u32, u32>>,
    /// Per-graphics-pipeline: whether the FS samples with
    /// implicit LOD.  Gates the rasterizer's per-pixel mip
    /// selection so explicit-LOD shaders keep their shared
    /// descriptor intact.
    pipeline_fs_implicit_lod: Mutex<HashMap<u32, bool>>,
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
    /// `(binding -> buffer_id)` map populated by
    /// `FrameOp::BindDescriptors` for SSBO writes.  Consumed
    /// (drained) per dispatch: pre-fill `compute_input_by_
    /// binding` from each bound buffer's current bytes, then
    /// after the shader runs copy `per_binding[b]` back into
    /// the buffer so the next `OP_GPU_BUFFER_READ` sees the
    /// shader's writes.  Mirror of `compute_image_by_binding`.
    compute_buffer_by_binding: Mutex<HashMap<u32, u64>>,
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
    /// MRT secondary colour attachment image IDs (colour
    /// attachments 1..N; attachment 0 is the pass's primary
    /// target).  Set by `FrameOp::BindColorAttachments`.
    /// Empty for single-attachment passes.
    extra_color_targets: Vec<u32>,
    /// Per-attachment blend for MRT attachments 1..N,
    /// snapshotted from the bound pipeline at BindPipeline
    /// time (attachment 0's blend rides in the resolved
    /// `PipelineRasterState`).  Empty for single-attachment.
    blend_extra: Vec<BlendState>,
    /// BeginRenderPass colour clear value (RGBA8).  `None`
    /// when the pass set `BEGIN_RP_FLAG_NO_CLEAR` (partial-
    /// redraw).  Used to clear secondary colour attachments
    /// (the primary is cleared in the BeginRenderPass arm).
    clear_color: Option<[u8; 4]>,
    /// Vertex-buffer bindings keyed by slot number.
    vertex_buffers: HashMap<u32, BoundVertexBuffer>,
    /// Currently-bound index buffer.
    index_buffer: Option<BoundIndexBuffer>,
    /// Current viewport (may be unset for a malformed frame; the
    /// walker tolerates it but a real draw would error in D.4+).
    viewport: Option<SetViewportCmd>,
    /// Current scissor rect.  None ⇒ full-framebuffer scissor
    /// (legacy behaviour).
    scissor: Option<SetScissorCmd>,
    /// Dynamic cull mode override (`vkCmdSetCullMode`).  When
    /// `Some`, takes precedence over the pipeline's static
    /// `Tier2RasterState::cull_mode` for subsequent draws.
    cull_mode_override: Option<CullMode>,
    /// Dynamic front-face override (`vkCmdSetFrontFace`).
    /// Same precedence rule as cull_mode_override.
    front_face_override: Option<FrontFace>,
    /// Dynamic depth-test toggle (`vkCmdSetDepthTestEnable`).
    /// `Some(true)` forces the depth test on for subsequent
    /// draws (overriding the pipeline's static
    /// `Tier2DepthState::test_enable`); `Some(false)` forces
    /// it off; `None` defers to the pipeline.
    depth_test_enable_override: Option<bool>,
    /// Dynamic depth-write toggle (`vkCmdSetDepthWriteEnable`).
    /// Mirrors `depth_test_enable_override`'s precedence
    /// rule; honoured only when the depth test itself is
    /// active (matches Vulkan's interaction between the two).
    depth_write_enable_override: Option<bool>,
    /// Dynamic depth compare op (`vkCmdSetDepthCompareOp`).
    /// Takes precedence over `Tier2DepthState::compare_op`
    /// from the bound pipeline.
    depth_compare_op_override: Option<CompareOp>,
    /// Dynamic primitive topology
    /// (`vkCmdSetPrimitiveTopology`).  Takes precedence over
    /// the pipeline's static topology when set.
    topology_override: Option<PrimitiveTopology>,
    /// Dynamic rasterizer-discard toggle
    /// (`vkCmdSetRasterizerDiscardEnable`).  When `Some(true)`,
    /// subsequent draws short-circuit before vertex assembly
    /// (the daemon doesn't model transform-feedback side
    /// effects, so dropping the entire dispatch is sound).
    /// `Some(false)` forces the rasterizer back on regardless
    /// of the pipeline's static state.
    rasterizer_discard_override: Option<bool>,
    /// Dynamic depth-bounds-test toggle
    /// (`vkCmdSetDepthBoundsTestEnable`).
    bounds_test_enable_override: Option<bool>,
    /// Dynamic depth-bounds range (`vkCmdSetDepthBounds`):
    /// `(min, max)`.  Takes precedence over the pipeline-
    /// static `min_depth_bounds` / `max_depth_bounds`.
    bounds_range_override: Option<(f32, f32)>,
    /// Dynamic stencil-test toggle
    /// (`vkCmdSetStencilTestEnable`).
    stencil_test_enable_override: Option<bool>,
    /// Dynamic per-face stencil overrides applied on top of
    /// the pipeline-static `Tier2StencilState`.  Each field
    /// is `None` ⇒ defer to the pipeline; `Some` ⇒ takes
    /// precedence.  Front and back are tracked independently
    /// per Vulkan's `VkStencilFaceFlags` semantics.
    stencil_front_override: StencilFaceOverride,
    stencil_back_override:  StencilFaceOverride,
    /// Dynamic depth-bias toggle (`vkCmdSetDepthBiasEnable`).
    depth_bias_enable_override: Option<bool>,
    /// Dynamic depth-bias factors
    /// (`vkCmdSetDepthBias`): (constant, clamp, slope).
    depth_bias_override: Option<(f32, f32, f32)>,
    /// Dynamic primitive-restart toggle
    /// (`vkCmdSetPrimitiveRestartEnable`).
    primitive_restart_enable_override: Option<bool>,
    /// Latest push-constants block; tier-2 shaders consume it
    /// as their uniform area.
    push_constants: Vec<u8>,
    /// Combined-image-sampler descriptor bindings recorded by
    /// `FrameOp::BindDescriptors` for the current pass.
    /// Keyed by descriptor binding slot.  Value is
    /// `(image_id, sampler_id)`.  Consumed at dispatch_draw
    /// to build the uniforms-buffer descriptor table the
    /// runtime's `atrium_tex_sample_*` helpers read.
    bound_textures: HashMap<u32, (u32, u32)>,
    /// UNIFORM_BUFFER descriptor bindings recorded by
    /// `FrameOp::BindDescriptors` for the current pass.
    /// Keyed by descriptor binding slot.  Value is the
    /// buffer's daemon-side ResourceId raw.  At dispatch_draw
    /// time the buffer's bytes are copied into the uniforms
    /// scratch buffer; the backend resolves
    /// `StorageClass::Uniform` to `params[1]` (= scratch ptr)
    /// and OpAccessChain through the Block adds member
    /// offsets.  v1 only honours one UBO binding (the lowest
    /// numbered one) and is mutually exclusive with
    /// combined-image-sampler descriptors (both stake out
    /// the prefix of the same uniforms buffer).
    bound_uniforms: HashMap<u32, u32>,
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
    cull_mode: CullMode,
    front_face: FrontFace,
    topology: PrimitiveTopology,
    /// Mirror of `Tier2RasterState::rasterizer_discard`.
    /// When true, the draw short-circuits before vertex
    /// assembly unless the cmdbuf re-enabled the rasterizer
    /// dynamically.
    rasterizer_discard: bool,
    /// Pipeline-static stencil per-face state, already
    /// converted to the daemon's `StencilState` shape.
    /// `None` ⇒ the pipeline omitted the depth-stencil
    /// block entirely; a dynamic `vkCmdSetStencilTestEnable`
    /// can't conjure ops out of nothing, so the test stays
    /// off.  Stored independently of `stencil_test_enable`
    /// because apps commonly bake the ops + masks into the
    /// pipeline and toggle the enable dynamically.
    stencil: Option<StencilState>,
    /// Pipeline-static `stencilTestEnable`.  The dynamic
    /// `vkCmdSetStencilTestEnable` overrides this.
    stencil_test_enable: bool,
    /// Pipeline-static depth-bias.
    depth_bias_enable: bool,
    depth_bias_constant_factor: f32,
    depth_bias_clamp: f32,
    depth_bias_slope_factor: f32,
    /// Pipeline-static `primitiveRestartEnable` from
    /// `VkPipelineInputAssemblyStateCreateInfo`.  Honoured
    /// only with TriangleStrip topology in v1.
    primitive_restart_enable: bool,
}

/// Partial per-face stencil override.  Each field comes from
/// a different dynamic-state setter (`vkCmdSetStencilOp` /
/// `vkCmdSetStencilCompareMask` / `vkCmdSetStencilWriteMask`
/// / `vkCmdSetStencilReference`) and can be set independently;
/// any `None` falls back to the pipeline-static value.
#[derive(Debug, Clone, Copy, Default)]
struct StencilFaceOverride {
    /// `vkCmdSetStencilOp`: (fail, pass, depth_fail, compare).
    ops: Option<(StencilOp, StencilOp, StencilOp, CompareOp)>,
    /// `vkCmdSetStencilCompareMask`.
    compare_mask: Option<u8>,
    /// `vkCmdSetStencilWriteMask`.
    write_mask: Option<u8>,
    /// `vkCmdSetStencilReference`.
    reference: Option<u8>,
}

impl StencilFaceOverride {
    /// Merge this partial override on top of a pipeline-static
    /// face state, producing the effective per-draw face state.
    fn apply(&self, base: StencilFaceState) -> StencilFaceState {
        let (fail_op, pass_op, depth_fail_op, compare_op) =
            self.ops.unwrap_or((
                base.fail_op, base.pass_op, base.depth_fail_op, base.compare_op,
            ));
        StencilFaceState {
            fail_op, pass_op, depth_fail_op, compare_op,
            compare_mask: self.compare_mask.unwrap_or(base.compare_mask),
            write_mask:   self.write_mask.unwrap_or(base.write_mask),
            reference:    self.reference.unwrap_or(base.reference),
        }
    }
}

/// Daemon-local primitive topology.  Only the two triangle
/// modes are wired today; `Other` is accepted on the wire
/// (so the daemon doesn't reject a pipeline that declares,
/// say, LineList) and falls back to TriangleList rasterization
/// without explicit failure.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum PrimitiveTopology {
    /// Independent triangles (default).
    #[default]
    TriangleList,
    /// Triangle strip; consecutive triangles share an edge.
    TriangleStrip,
    /// Reserved / unimplemented; rasterizes as TriangleList.
    Other,
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
    /// Base-level (mip 0) pixels.  For layered images
    /// (`array_layers > 1`, e.g. cubemaps + 2D arrays) the
    /// layers are stored contiguously layer-major:
    /// `pixels[layer * width * height * 4 ..]`.  `slice_bytes`
    /// (= width * height * 4) is the per-layer stride.
    pixels: Vec<u8>,
    /// Array layer count (1 = plain 2D, 6 = cubemap, N = 2D
    /// array).  `pixels.len() == width * height * 4 *
    /// array_layers`.
    array_layers: u32,
    /// Mip levels 1..N (level 0 lives in `pixels` directly).
    /// Allocated lazily on the first `CopyBufToImg` that
    /// targets `mipLevel > 0`.  Each entry stores its own
    /// (width, height) since mip dimensions are
    /// `max(1, base >> level)`.  v1 only carries mips for
    /// the base array layer.
    mip_levels: Vec<MipLevel>,
}

/// One stored mip level beyond the base.  Level index =
/// position in `ImageStorage::mip_levels` + 1.
struct MipLevel {
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

/// `VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER`.  Bound by
/// `vkCmdBindDescriptorSets` when a shader declares a
/// `sampler2D` / `texture2D + sampler` binding.  At dispatch
/// time the daemon resolves these to runtime `TexDesc` +
/// `SamplerDesc` pairs and writes them into the uniforms
/// buffer at the offsets `atrium_tex_sample_2d` (and its
/// siblings) expect.
const DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER: u32 = 1;
/// `VK_DESCRIPTOR_TYPE_STORAGE_IMAGE`.
const DESCRIPTOR_TYPE_STORAGE_IMAGE: u32 = 3;
/// `VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER`.  Bound by
/// `vkCmdBindDescriptorSets` when a shader declares a
/// `layout(set=N, binding=M) uniform Block { ... }`.  At
/// dispatch time the daemon copies the buffer's bytes into
/// the uniforms scratch at offset 0; the backend resolves
/// `StorageClass::Uniform` to `params[1]` and OpAccessChain
/// adds member offsets within the Block.  v1 only honours
/// the first UBO binding (no co-existence with texture
/// descriptors yet -- both want the prefix of the same
/// uniforms buffer).
const DESCRIPTOR_TYPE_UNIFORM_BUFFER: u32 = 6;
/// `VK_DESCRIPTOR_TYPE_STORAGE_BUFFER`.  The SSBO descriptor
/// type a compute shader's `layout(binding=N) buffer { ... }`
/// resolves to.
const DESCRIPTOR_TYPE_STORAGE_BUFFER: u32 = 7;

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

/// Parse a `FrameOp::BindDescriptors` body and return
/// `(binding, image_id, sampler_id)` triples whose
/// descriptor type is `COMBINED_IMAGE_SAMPLER`.  Same body
/// layout as `parse_bind_descriptors_storage_images`; both
/// `image_id` (at off+12) and `sampler_id` (at off+16) get
/// read out.  Triples with either id zero are skipped (the
/// daemon's tex-table builder would emit a null pointer and
/// the runtime sample helper would deref it).
fn parse_bind_descriptors_combined_image_samplers(
    body: &[u8],
) -> Vec<(u32, u32, u32)> {
    let mut out = Vec::new();
    if body.len() < 8 { return out; }
    let u32_at = |o: usize| -> u32 {
        u32::from_le_bytes(body[o..o + 4].try_into().unwrap())
    };
    let write_count = u32_at(4) as usize;
    for w in 0..write_count {
        let off = 8 + w * BIND_DESCRIPTORS_WRITE_BYTES;
        if off + BIND_DESCRIPTORS_WRITE_BYTES > body.len() { break; }
        let binding    = u32_at(off);
        let dtype      = u32_at(off + 4);
        let image_id   = u32_at(off + 12);
        let sampler_id = u32_at(off + 16);
        if dtype == DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER
            && image_id != 0 && sampler_id != 0
        {
            out.push((binding, image_id, sampler_id));
        }
    }
    out
}

/// Parse a `FrameOp::BindDescriptors` body and return the
/// `(binding, buffer_id)` pairs whose descriptor type is
/// `UNIFORM_BUFFER`.  Same body layout as the other
/// `parse_bind_descriptors_*` helpers; we just filter on the
/// UBO type code (6) instead.
fn parse_bind_descriptors_uniform_buffers(body: &[u8]) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    if body.len() < 8 { return out; }
    let u32_at = |o: usize| -> u32 {
        u32::from_le_bytes(body[o..o + 4].try_into().unwrap())
    };
    let write_count = u32_at(4) as usize;
    for w in 0..write_count {
        let off = 8 + w * BIND_DESCRIPTORS_WRITE_BYTES;
        if off + BIND_DESCRIPTORS_WRITE_BYTES > body.len() { break; }
        let binding   = u32_at(off);
        let dtype     = u32_at(off + 4);
        let buffer_id = u32_at(off + 8);
        if dtype == DESCRIPTOR_TYPE_UNIFORM_BUFFER && buffer_id != 0 {
            out.push((binding, buffer_id));
        }
    }
    out
}

/// Parse a `FrameOp::BindDescriptors` body and return the
/// `(binding, buffer_id)` pairs whose descriptor type is
/// `STORAGE_BUFFER`.  Same body layout as
/// `parse_bind_descriptors_storage_images`; the only
/// difference is which descriptor-type code we filter on +
/// which u32 in the write record we read out.
fn parse_bind_descriptors_storage_buffers(body: &[u8]) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    if body.len() < 8 { return out; }
    let u32_at = |o: usize| -> u32 {
        u32::from_le_bytes(body[o..o + 4].try_into().unwrap())
    };
    let write_count = u32_at(4) as usize;
    for w in 0..write_count {
        let off = 8 + w * BIND_DESCRIPTORS_WRITE_BYTES;
        if off + BIND_DESCRIPTORS_WRITE_BYTES > body.len() { break; }
        let binding   = u32_at(off);
        let dtype     = u32_at(off + 4);
        let buffer_id = u32_at(off + 8);
        if dtype == DESCRIPTOR_TYPE_STORAGE_BUFFER && buffer_id != 0 {
            out.push((binding, buffer_id));
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
            samplers: Mutex::new(HashMap::new()),
            depth_clear_cleared: Mutex::new(HashMap::new()),
            buffers: Mutex::new(HashMap::new()),
            pipeline_shaders: Mutex::new(HashMap::new()),
            pipeline_vs_shaders: Mutex::new(HashMap::new()),
            pipeline_layouts: Mutex::new(HashMap::new()),
            pipeline_raster: Mutex::new(HashMap::new()),
            pipeline_blend_extra: Mutex::new(HashMap::new()),
            pipeline_vs_varying_bytes: Mutex::new(HashMap::new()),
            pipeline_fs_implicit_lod: Mutex::new(HashMap::new()),
            pipeline_compute: Mutex::new(HashMap::new()),
            cs_invocations: AtomicU64::new(0),
            last_compute_output: Mutex::new(None),
            last_compute_outputs_by_binding: Mutex::new(Vec::new()),
            compute_input_by_binding: Mutex::new(HashMap::new()),
            compute_image_by_binding: Mutex::new(HashMap::new()),
            compute_buffer_by_binding: Mutex::new(HashMap::new()),
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
        blend_extra: &[WireBlendState],
        raster: Option<aqueduct_gpu::Tier2RasterState>,
        topology: aqueduct_gpu::Tier2PrimitiveTopology,
        stencil: Option<aqueduct_gpu::Tier2StencilState>,
        primitive_restart_enable: bool,
    ) {
        let blend = blend.map(convert_blend_state).unwrap_or_default();
        // Per-attachment blend for MRT attachments 1..N.
        if !blend_extra.is_empty() {
            let converted: Vec<BlendState> = blend_extra.iter()
                .map(|b| convert_blend_state(*b)).collect();
            self.pipeline_blend_extra.lock().unwrap()
                .insert(pipeline_id.raw(), converted);
        }
        let (cull_mode, front_face, rasterizer_discard,
             depth_bias_enable, depth_bias_constant_factor,
             depth_bias_clamp, depth_bias_slope_factor) = match raster {
            Some(r) => (
                match r.cull_mode {
                    aqueduct_gpu::Tier2CullMode::None         => CullMode::None,
                    aqueduct_gpu::Tier2CullMode::Front        => CullMode::Front,
                    aqueduct_gpu::Tier2CullMode::Back         => CullMode::Back,
                    aqueduct_gpu::Tier2CullMode::FrontAndBack => CullMode::FrontAndBack,
                },
                match r.front_face {
                    aqueduct_gpu::Tier2FrontFace::CounterClockwise => FrontFace::CounterClockwise,
                    aqueduct_gpu::Tier2FrontFace::Clockwise        => FrontFace::Clockwise,
                },
                r.rasterizer_discard,
                r.depth_bias_enable,
                r.depth_bias_constant_factor,
                r.depth_bias_clamp,
                r.depth_bias_slope_factor,
            ),
            None => (CullMode::None, FrontFace::CounterClockwise, false,
                     false, 0.0, 0.0, 0.0),
        };
        let topology = match topology {
            aqueduct_gpu::Tier2PrimitiveTopology::TriangleList  => PrimitiveTopology::TriangleList,
            aqueduct_gpu::Tier2PrimitiveTopology::TriangleStrip => PrimitiveTopology::TriangleStrip,
            aqueduct_gpu::Tier2PrimitiveTopology::Other         => PrimitiveTopology::Other,
        };
        // Stencil conversion from wire to daemon-local types.
        // We always store the front/back state when the
        // pipeline supplied them, even if test_enable=false
        // -- a dynamic `vkCmdSetStencilTestEnable(true)` then
        // turns the test on without re-binding a pipeline.
        let (stencil_state, stencil_test_enable) = match stencil {
            Some(s) => (
                Some(StencilState {
                    front: convert_stencil_face(s.front),
                    back:  convert_stencil_face(s.back),
                }),
                s.test_enable,
            ),
            None => (None, false),
        };
        self.pipeline_raster.lock().unwrap().insert(
            pipeline_id.raw(),
            PipelineRasterState {
                depth, blend, cull_mode, front_face, topology, rasterizer_discard,
                stencil: stencil_state,
                stencil_test_enable,
                depth_bias_enable, depth_bias_constant_factor,
                depth_bias_clamp,  depth_bias_slope_factor,
                primitive_restart_enable,
            },
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
        let pipeline_blend_extra = self.pipeline_blend_extra.lock().unwrap().clone();
        let pipeline_compute    = self.pipeline_compute.lock().unwrap().clone();
        let mut state = PassState::default();
        // D.6: depth buffer is per-pass, allocated lazily on
        // first draw with depth-test enabled. Cleared to +inf
        // so any in-range fragment wins the first LESS test.
        let mut depth_buffer: Option<Vec<f32>> = None;
        // Stencil buffer: same lifetime as depth_buffer.
        // Allocated lazily on first draw with stencil
        // enabled, cleared to 0 (Vulkan's default
        // `ClearDepthStencilValue::stencil`).
        let mut stencil_buffer: Option<Vec<u8>> = None;
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
                FrameOp::BeginRenderPass => {
                    // Body: target_image_id u32, clear_rgba8
                    // [u8;4], flags u32 (12 B; flags optional).
                    // Apply the colour clear to the primary
                    // attachment unless BEGIN_RP_FLAG_NO_CLEAR
                    // (0x1) is set.  Secondary attachments are
                    // cleared when BindColorAttachments arrives
                    // (the clear colour is stashed here).
                    if body.len() >= 8 {
                        let flags = if body.len() >= 12 {
                            u32::from_le_bytes(body[8..12].try_into().unwrap())
                        } else { 0 };
                        const BEGIN_RP_FLAG_NO_CLEAR: u32 = 0x1;
                        if flags & BEGIN_RP_FLAG_NO_CLEAR == 0 {
                            let rgba = [body[4], body[5], body[6], body[7]];
                            state.clear_color = Some(rgba);
                            // Clear the primary colour target.
                            if let Some(img) = self.images.lock().unwrap()
                                .get_mut(&(target_id.raw() as u64))
                            {
                                fill_rgba8(&mut img.pixels, rgba);
                            }
                        } else {
                            state.clear_color = None;
                        }
                    }
                }
                FrameOp::BindPipeline => {
                    if body.len() >= 4 {
                        let raw = u32::from_le_bytes([
                            body[0], body[1], body[2], body[3]
                        ]);
                        state.pipeline_raw    = Some(raw);
                        state.tier2_shader    = pipeline_shaders.get(&raw).copied();
                        state.tier2_vs_shader = pipeline_vs_shaders.get(&raw).copied();
                        state.raster          = pipeline_raster.get(&raw).copied();
                        state.blend_extra     = pipeline_blend_extra.get(&raw)
                            .cloned().unwrap_or_default();
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
                FrameOp::BindDescriptors => {
                    // Graphics-side: stash COMBINED_IMAGE_SAMPLER
                    // and UNIFORM_BUFFER bindings.  dispatch_draw
                    // consults each to build the uniforms-buffer
                    // contents.  STORAGE_BUFFER descriptors aren't
                    // yet wired into the graphics path (deferred
                    // to when a shader actually needs them); SSBO
                    // entries arriving here are silently dropped.
                    for (binding, image_id, sampler_id)
                        in parse_bind_descriptors_combined_image_samplers(body)
                    {
                        state.bound_textures
                            .insert(binding, (image_id, sampler_id));
                    }
                    for (binding, buffer_id)
                        in parse_bind_descriptors_uniform_buffers(body)
                    {
                        state.bound_uniforms.insert(binding, buffer_id);
                    }
                }
                FrameOp::SetViewport => match SetViewportCmd::from_bytes(body) {
                    Ok(cmd) => state.viewport = Some(cmd),
                    Err(e) => log::warn!("malformed SetViewport: {e}"),
                },
                FrameOp::SetScissor => match SetScissorCmd::from_bytes(body) {
                    Ok(cmd) => state.scissor = Some(cmd),
                    Err(e) => log::warn!("malformed SetScissor: {e}"),
                },
                FrameOp::SetCullMode => {
                    if body.len() == 4 {
                        let flags = u32::from_le_bytes(body.try_into().unwrap());
                        // Match VkCullModeFlags: NONE=0, FRONT=1,
                        // BACK=2, FRONT_AND_BACK=3.
                        let cm = match flags {
                            0 => CullMode::None,
                            1 => CullMode::Front,
                            2 => CullMode::Back,
                            3 => CullMode::FrontAndBack,
                            _ => CullMode::None,
                        };
                        state.cull_mode_override = Some(cm);
                    } else {
                        log::warn!("malformed SetCullMode body length: {}", body.len());
                    }
                }
                FrameOp::SetFrontFace => {
                    if body.len() == 4 {
                        let v = u32::from_le_bytes(body.try_into().unwrap());
                        // VkFrontFace: COUNTER_CLOCKWISE=0,
                        // CLOCKWISE=1.
                        let ff = match v {
                            0 => FrontFace::CounterClockwise,
                            1 => FrontFace::Clockwise,
                            _ => FrontFace::CounterClockwise,
                        };
                        state.front_face_override = Some(ff);
                    } else {
                        log::warn!("malformed SetFrontFace body length: {}", body.len());
                    }
                }
                FrameOp::SetDepthTestEnable => {
                    if body.len() == 4 {
                        let v = u32::from_le_bytes(body.try_into().unwrap());
                        state.depth_test_enable_override = Some(v != 0);
                    } else {
                        log::warn!("malformed SetDepthTestEnable body length: {}", body.len());
                    }
                }
                FrameOp::SetDepthWriteEnable => {
                    if body.len() == 4 {
                        let v = u32::from_le_bytes(body.try_into().unwrap());
                        state.depth_write_enable_override = Some(v != 0);
                    } else {
                        log::warn!("malformed SetDepthWriteEnable body length: {}", body.len());
                    }
                }
                FrameOp::SetRasterizerDiscardEnable => {
                    if body.len() == 4 {
                        let v = u32::from_le_bytes(body.try_into().unwrap());
                        state.rasterizer_discard_override = Some(v != 0);
                    } else {
                        log::warn!("malformed SetRasterizerDiscardEnable body length: {}", body.len());
                    }
                }
                FrameOp::SetDepthBoundsTestEnable => {
                    if body.len() == 4 {
                        let v = u32::from_le_bytes(body.try_into().unwrap());
                        state.bounds_test_enable_override = Some(v != 0);
                    } else {
                        log::warn!("malformed SetDepthBoundsTestEnable body length: {}", body.len());
                    }
                }
                FrameOp::SetDepthBounds => {
                    if body.len() == 8 {
                        let min_b = f32::from_le_bytes(body[0..4].try_into().unwrap());
                        let max_b = f32::from_le_bytes(body[4..8].try_into().unwrap());
                        state.bounds_range_override = Some((min_b, max_b));
                    } else {
                        log::warn!("malformed SetDepthBounds body length: {}", body.len());
                    }
                }
                FrameOp::SetStencilTestEnable => {
                    if body.len() == 4 {
                        let v = u32::from_le_bytes(body.try_into().unwrap());
                        state.stencil_test_enable_override = Some(v != 0);
                    } else {
                        log::warn!("malformed SetStencilTestEnable body length: {}", body.len());
                    }
                }
                FrameOp::SetStencilOp => {
                    if body.len() == 20 {
                        let face_mask = u32::from_le_bytes(body[ 0.. 4].try_into().unwrap());
                        let fail_op   = decode_stencil_op(
                            u32::from_le_bytes(body[ 4.. 8].try_into().unwrap()));
                        let pass_op   = decode_stencil_op(
                            u32::from_le_bytes(body[ 8..12].try_into().unwrap()));
                        let depth_fail_op = decode_stencil_op(
                            u32::from_le_bytes(body[12..16].try_into().unwrap()));
                        let compare_op = decode_compare_op(
                            u32::from_le_bytes(body[16..20].try_into().unwrap()));
                        let payload = (fail_op, pass_op, depth_fail_op, compare_op);
                        if face_mask & 0x1 != 0 { state.stencil_front_override.ops = Some(payload); }
                        if face_mask & 0x2 != 0 { state.stencil_back_override.ops  = Some(payload); }
                    } else {
                        log::warn!("malformed SetStencilOp body length: {}", body.len());
                    }
                }
                FrameOp::SetStencilCompareMask => {
                    if body.len() == 8 {
                        let face_mask = u32::from_le_bytes(body[0..4].try_into().unwrap());
                        let value     = u32::from_le_bytes(body[4..8].try_into().unwrap()) as u8;
                        if face_mask & 0x1 != 0 { state.stencil_front_override.compare_mask = Some(value); }
                        if face_mask & 0x2 != 0 { state.stencil_back_override.compare_mask  = Some(value); }
                    } else {
                        log::warn!("malformed SetStencilCompareMask body length: {}", body.len());
                    }
                }
                FrameOp::SetStencilWriteMask => {
                    if body.len() == 8 {
                        let face_mask = u32::from_le_bytes(body[0..4].try_into().unwrap());
                        let value     = u32::from_le_bytes(body[4..8].try_into().unwrap()) as u8;
                        if face_mask & 0x1 != 0 { state.stencil_front_override.write_mask = Some(value); }
                        if face_mask & 0x2 != 0 { state.stencil_back_override.write_mask  = Some(value); }
                    } else {
                        log::warn!("malformed SetStencilWriteMask body length: {}", body.len());
                    }
                }
                FrameOp::SetStencilReference => {
                    if body.len() == 8 {
                        let face_mask = u32::from_le_bytes(body[0..4].try_into().unwrap());
                        let value     = u32::from_le_bytes(body[4..8].try_into().unwrap()) as u8;
                        if face_mask & 0x1 != 0 { state.stencil_front_override.reference = Some(value); }
                        if face_mask & 0x2 != 0 { state.stencil_back_override.reference  = Some(value); }
                    } else {
                        log::warn!("malformed SetStencilReference body length: {}", body.len());
                    }
                }
                FrameOp::SetDepthBiasEnable => {
                    if body.len() == 4 {
                        let v = u32::from_le_bytes(body.try_into().unwrap());
                        state.depth_bias_enable_override = Some(v != 0);
                    } else {
                        log::warn!("malformed SetDepthBiasEnable body length: {}", body.len());
                    }
                }
                FrameOp::SetDepthBias => {
                    if body.len() == 12 {
                        let c = f32::from_le_bytes(body[0..4].try_into().unwrap());
                        let cl = f32::from_le_bytes(body[4..8].try_into().unwrap());
                        let s = f32::from_le_bytes(body[8..12].try_into().unwrap());
                        state.depth_bias_override = Some((c, cl, s));
                    } else {
                        log::warn!("malformed SetDepthBias body length: {}", body.len());
                    }
                }
                FrameOp::SetPrimitiveRestartEnable => {
                    if body.len() == 4 {
                        let v = u32::from_le_bytes(body.try_into().unwrap());
                        state.primitive_restart_enable_override = Some(v != 0);
                    } else {
                        log::warn!("malformed SetPrimitiveRestartEnable body length: {}", body.len());
                    }
                }
                FrameOp::SetPrimitiveTopology => {
                    if body.len() == 4 {
                        let v = u32::from_le_bytes(body.try_into().unwrap());
                        // VkPrimitiveTopology: 3 = TRIANGLE_LIST,
                        // 4 = TRIANGLE_STRIP (per spec).  Anything
                        // else falls back to TriangleList.
                        let t = match v {
                            3 => PrimitiveTopology::TriangleList,
                            4 => PrimitiveTopology::TriangleStrip,
                            _ => PrimitiveTopology::Other,
                        };
                        state.topology_override = Some(t);
                    } else {
                        log::warn!("malformed SetPrimitiveTopology body length: {}", body.len());
                    }
                }
                FrameOp::SetDepthCompareOp => {
                    if body.len() == 4 {
                        let v = u32::from_le_bytes(body.try_into().unwrap());
                        // VkCompareOp encoding (spec):
                        //  0=NEVER, 1=LESS, 2=EQUAL, 3=LEQUAL,
                        //  4=GREATER, 5=NOT_EQUAL, 6=GEQUAL, 7=ALWAYS.
                        let op = match v {
                            0 => CompareOp::Never,
                            1 => CompareOp::Less,
                            2 => CompareOp::Equal,
                            3 => CompareOp::LessOrEqual,
                            4 => CompareOp::Greater,
                            5 => CompareOp::NotEqual,
                            6 => CompareOp::GreaterOrEqual,
                            7 => CompareOp::Always,
                            _ => CompareOp::Less,
                        };
                        state.depth_compare_op_override = Some(op);
                    } else {
                        log::warn!("malformed SetDepthCompareOp body length: {}", body.len());
                    }
                }
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
                FrameOp::BindColorAttachments => {
                    // Body: count u32, then count × image_id u32.
                    if body.len() >= 4 {
                        let count = u32::from_le_bytes(
                            body[0..4].try_into().unwrap()) as usize;
                        let mut ids = Vec::with_capacity(count);
                        for i in 0..count {
                            let off = 4 + i * 4;
                            if off + 4 > body.len() { break; }
                            ids.push(u32::from_le_bytes(
                                body[off..off+4].try_into().unwrap()));
                        }
                        // Clear each secondary attachment to
                        // the pass clear colour (mirrors the
                        // primary clear in the BeginRenderPass
                        // arm).  Vulkan clears every attachment
                        // with loadOp=CLEAR; without this MRT
                        // attachments 1..N kept stale / zeroed
                        // contents.
                        if let Some(rgba) = state.clear_color {
                            let mut images = self.images.lock().unwrap();
                            for &eid in &ids {
                                if let Some(img) = images.get_mut(&(eid as u64)) {
                                    fill_rgba8(&mut img.pixels, rgba);
                                }
                            }
                        }
                        state.extra_color_targets = ids;
                    } else {
                        log::warn!("malformed BindColorAttachments body length: {}", body.len());
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
                        self.dispatch_draw(target_id, &state, cmd,
                            &mut depth_buffer, &mut stencil_buffer);
                    }
                    Err(e) => log::warn!("malformed Draw: {e}"),
                },
                FrameOp::DrawIndexed => match DrawIndexedCmd::from_bytes(body) {
                    Ok(cmd) => {
                        state.draws_in_pass = state.draws_in_pass.saturating_add(1);
                        self.dispatch_draw_indexed(
                            target_id, &state, cmd,
                            &mut depth_buffer, &mut stencil_buffer);
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
                // Ops we don't yet act on:
                // BindDescriptors (compute-only; handled by
                // execute_compute_ops), CopyBufToImg,
                // CopyImgToBuf (handled by execute_compute_ops's
                // outside-pass walker), Blit, PipelineBarrier,
                // Dispatch{,Indirect}, DrawIndirect,
                // BeginRenderPass (handled by the outer
                // partition step). These will grow handlers in
                // later phases as needed.
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
        stencil_buffer: &mut Option<Vec<u8>>,
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
        // Rasterizer-discard short-circuit.  Pipeline static
        // wins by default; the cmdbuf can override via
        // `vkCmdSetRasterizerDiscardEnable`.  When discard is
        // active, the daemon has no transform-feedback side
        // effects to model, so dropping the entire dispatch is
        // sound.  (VS still nominally runs in real Vulkan; for
        // tier-2 it's a pure function so skipping is observable
        // only via "nothing painted".)
        let pipeline_discard = state.raster
            .map(|r| r.rasterizer_discard).unwrap_or(false);
        if state.rasterizer_discard_override.unwrap_or(pipeline_discard) {
            self.draws_skipped.fetch_add(1, Ordering::Relaxed);
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

        let stride = assembled.stride as usize;
        // Primitive topology: dynamic override wins, then
        // pipeline-static, default TriangleList.
        let pipeline_topology = state.raster
            .map(|r| r.topology).unwrap_or_default();
        let topology = state.topology_override.unwrap_or(pipeline_topology);
        let n_verts = assembled.vertex_count as usize;
        let tri_count = match topology {
            PrimitiveTopology::TriangleStrip => n_verts.saturating_sub(2),
            // Other / TriangleList: floor(n / 3) independent triangles.
            _                                 => n_verts / 3,
        };
        // TriangleList requires vertex_count % 3 == 0 (extras
        // are silently dropped by the floor division above).
        // TriangleStrip needs vertex_count >= 3 (already handled
        // by the saturating_sub).  Both produce 0 triangles
        // safely for malformed input.
        if matches!(topology, PrimitiveTopology::TriangleList) && n_verts % 3 != 0 {
            log::warn!("Draw target={target_id}: vertex_count {} is not a \
                        multiple of 3 under TriangleList; dropping {} trailing \
                        vertices",
                       assembled.vertex_count, n_verts - tri_count * 3);
        }

        // Pre-snapshot bound-texture image data BEFORE the
        // images-lock acquisition below.  We just need stable
        // raw pointers (TexDesc.data) for the sampled textures
        // -- Vec<u8> heap addresses survive HashMap reshapes,
        // so the raw pointers remain valid for the duration
        // of fill_image_triangle even after we release this
        // read lock.  Keeping this OUTSIDE the mut-lock below
        // avoids reacquiring `self.images` recursively (a
        // deadlock on the same thread).
        // Snapshot includes base level pointer + dimensions
        // and per-mip pointers + dimensions for levels >= 1.
        // Vec<u8> heap addresses survive HashMap reshapes,
        // so the raw pointers stay valid through the
        // per-triangle loop.
        // (binding, base_ptr, w, h, array_layers, mips)
        type TextureSnap = (u32, *const u8, u32, u32, u32, Vec<(*const u8, u32, u32)>);
        let texture_snapshots: Vec<TextureSnap> = {
            let images = self.images.lock().unwrap();
            state.bound_textures.iter()
                .map(|(b, (iid, _))| {
                    let (data, w, h, layers, mips) = images.get(&(*iid as u64))
                        .map(|img| (
                            img.pixels.as_ptr(),
                            img.width, img.height,
                            img.array_layers,
                            img.mip_levels.iter()
                                .map(|m| (m.pixels.as_ptr(), m.width, m.height))
                                .collect::<Vec<_>>(),
                        ))
                        .unwrap_or((std::ptr::null(), 1, 1, 1, Vec::new()));
                    (*b, data, w, h, layers, mips)
                })
                .collect()
        };

        // Lock the image storage for the duration of all
        // triangles in this Draw.  For MRT we remove the
        // primary + every secondary colour attachment from
        // the map into owned storages so we can hold N
        // disjoint `&mut` borrows (HashMap can't hand out
        // more than one get_mut at a time); they're
        // re-inserted after the render block.  The lock is
        // held throughout so no other thread reshapes the
        // map or re-creates a removed id mid-draw.
        let mut images = self.images.lock().unwrap();
        let mut primary = match images.remove(&(target_id.raw() as u64)) {
            Some(i) => i,
            None => {
                log::warn!("Draw target={target_id}: target image not registered");
                return;
            }
        };
        let mut extra_storages: Vec<(u32, ImageStorage)> =
            state.extra_color_targets.iter()
                .filter(|&&e| e as u64 != target_id.raw() as u64)
                .filter_map(|&e| images.remove(&(e as u64)).map(|s| (e, s)))
                .collect();
        let width = primary.width;
        let height = primary.height;
        // Render block: scopes the `&mut` borrows of primary
        // + extra_storages so they end before re-insertion.
        let render_ok = {
        let pixels = &mut primary.pixels[..];
        let mut extra_slices: Vec<&mut [u8]> = extra_storages
            .iter_mut().map(|(_, s)| &mut s.pixels[..]).collect();

        // D.6: raster state -> per-draw blend + depth.
        let raster = state.raster.unwrap_or_default();
        // Pipeline-static depth state.
        let static_test  = raster.depth.map(|d| d.test_enable).unwrap_or(false);
        let static_write = raster.depth.map(|d| d.write_enable).unwrap_or(false);
        let static_cmp   = raster.depth
            .map(|d| convert_compare_op(d.compare_op))
            .unwrap_or(CompareOp::Less);
        // Dynamic overrides (`vkCmdSetDepthTestEnable` /
        // `vkCmdSetDepthWriteEnable` /
        // `vkCmdSetDepthCompareOp`) take precedence.  Depth
        // writes additionally gate on the depth test being
        // active (Vulkan spec: write_enable has no effect
        // when the test is off).
        let depth_enabled = state.depth_test_enable_override.unwrap_or(static_test);
        let depth_write   = depth_enabled
            && state.depth_write_enable_override.unwrap_or(static_write);
        let depth_cmp     = state.depth_compare_op_override.unwrap_or(static_cmp);
        // Depth bounds test: dynamic + static merge.  An app
        // can toggle the enable flag and adjust the range
        // separately, so the effective `(min, max)` comes
        // from whichever source was most recently set.
        let static_bounds_enable = raster.depth
            .map(|d| d.bounds_test_enable).unwrap_or(false);
        let static_bounds_range  = raster.depth
            .map(|d| (d.min_depth_bounds, d.max_depth_bounds))
            .unwrap_or((0.0, 1.0));
        let bounds_enable = state.bounds_test_enable_override
            .unwrap_or(static_bounds_enable);
        let depth_bounds = if bounds_enable {
            Some(state.bounds_range_override.unwrap_or(static_bounds_range))
        } else { None };
        // Depth bias: enable + factors merge from static +
        // dynamic.  An app can flip enable independently of
        // setting factors and vice versa, so each is its own
        // override.
        let bias_enable = state.depth_bias_enable_override
            .unwrap_or(raster.depth_bias_enable);
        let depth_bias = if bias_enable {
            Some(state.depth_bias_override.unwrap_or((
                raster.depth_bias_constant_factor,
                raster.depth_bias_clamp,
                raster.depth_bias_slope_factor,
            )))
        } else { None };

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

        // Effective stencil state for this draw.  Merges:
        //   - pipeline-static front/back face data
        //   - pipeline-static `stencil_test_enable`
        //   - dynamic `vkCmdSetStencilTestEnable` override
        //   - dynamic per-field per-face overrides
        //     (`vkCmdSetStencilOp` /
        //     `vkCmdSetStencilCompareMask` /
        //     `vkCmdSetStencilWriteMask` /
        //     `vkCmdSetStencilReference`)
        //
        // No persistent stencil attachment in v1; the per-
        // pass scratch is lazy-allocated to zeros on the
        // first stencil-using draw and reused across draws.
        let stencil_state = compute_effective_stencil(state);
        if stencil_state.is_some() && stencil_buffer.is_none() {
            *stencil_buffer = Some(vec![
                0u8;
                (width as usize) * (height as usize)
            ]);
        }

        // Per-pipeline VS varying-byte count (sum of byte
        // sizes of Location-decorated VS outputs).  Tells
        // fill_image_triangle how many bytes to capture from
        // its vary_scratch + how many f32 lanes to
        // interpolate.  0 = the VS only emits BuiltIn outputs
        // (gl_Position-only); rasterizer takes the null-
        // varyings fast path.
        let varying_f32_count = (self.pipeline_vs_varying_bytes.lock().unwrap()
            .get(&pipeline_raw).copied().unwrap_or(0) as usize) / 4;
        let fs_implicit_lod = self.pipeline_fs_implicit_lod.lock().unwrap()
            .get(&pipeline_raw).copied().unwrap_or(false);

        // Build the uniforms buffer if any COMBINED_IMAGE_SAMPLER
        // bindings are live.  Layout (atrium-spv-runtime
        // UNIFORMS_HELPERS_BASE / UNIFORMS_DESC_BASE):
        //   bytes  0..64 : helper fn ptr table (atrium_tex_
        //                  sample_2d, _fetch_2d, _sample_2d_lod,
        //                  _sample_2d_array, _sample_cube,
        //                  _gather_2d, _sample_2d_array_lod,
        //                  _sample_cube_lod)
        //   bytes 64+B*16: (TexDesc*, SamplerDesc*) for binding B
        //
        // `tex_descs` + `sampler_descs` own the raw `TexDesc` /
        // `SamplerDesc` structs the table pointers reference;
        // they're held in named bindings (NOT `let _ = ...`)
        // through the per-triangle loop so the heap stays
        // alive while `fill_image_triangle` deref's the
        // pointers.  Same shape as the storage-image
        // dispatcher's image-table lifetime fix (commit
        // e0dce68).
        let tex_descs: Vec<atrium_spv_runtime::TexDesc>;
        let sampler_descs: Vec<atrium_spv_runtime::SamplerDesc>;
        // Per-binding per-mip TexDesc arrays.  Owned heap
        // here so `mip_descs` raw pointers survive the
        // per-triangle loop.  Index = position in `td`.
        let mip_desc_arrays: Vec<Vec<atrium_spv_runtime::TexDesc>>;
        let uniforms_buf: Vec<u8>;
        if !state.bound_textures.is_empty() {
            // Sort the snapshot by binding so per-binding
            // slot indices line up with the FS's `Binding`
            // decorations.
            let mut snaps = texture_snapshots.clone();
            snaps.sort_by_key(|(b, _, _, _, _, _)| *b);
            let max_binding = snaps.iter()
                .map(|(b, _, _, _, _, _)| *b as usize).max().unwrap_or(0);
            let slot_count = max_binding + 1;
            let samplers_guard = self.samplers.lock().unwrap();
            let mut td: Vec<atrium_spv_runtime::TexDesc> =
                Vec::with_capacity(snaps.len());
            let mut sd: Vec<atrium_spv_runtime::SamplerDesc> =
                Vec::with_capacity(snaps.len());
            let mut mip_arrays: Vec<Vec<atrium_spv_runtime::TexDesc>> =
                Vec::with_capacity(snaps.len());
            let placeholder_samp = atrium_spv_runtime::SamplerDesc {
                mag_filter: 0, min_filter: 0, wrap_s: 0, wrap_t: 0,
                compare_enable: 0, compare_op: 0,
            };
            for (b, data, w, h, layers, mips) in &snaps {
                let (_, sampler_id) = *state.bound_textures.get(b)
                    .unwrap_or(&(0, 0));
                // depth = array-layer / cube-face count;
                // slice_bytes = per-layer stride.  The runtime
                // array / cube helpers (`atrium_tex_sample_2d_
                // array`, `_sample_cube`) read these to address
                // `data + layer * slice_bytes`.
                let depth = (*layers).max(1);
                let slice_bytes = if depth > 1 { *w * *h * 4 } else { 0 };
                // Build the per-mip TexDesc array.  Level 0 is
                // duplicated at slot 0 to keep
                // `pick_tex_mip(lod=0)` consistent (it falls
                // back to the base when lod=0); levels 1..N
                // come from the snapshot's mip pointers.
                let mut mips_v: Vec<atrium_spv_runtime::TexDesc> =
                    Vec::with_capacity(1 + mips.len());
                mips_v.push(atrium_spv_runtime::TexDesc {
                    data: *data,
                    width: *w, height: *h,
                    stride_bytes: *w * 4,
                    format: atrium_spv_runtime::TexFormat::Rgba8Unorm as u32,
                    mip_count: 0, mip_descs: std::ptr::null(),
                    depth, slice_bytes,
                });
                for (mptr, mw, mh) in mips {
                    mips_v.push(atrium_spv_runtime::TexDesc {
                        data: *mptr,
                        width: *mw, height: *mh,
                        stride_bytes: *mw * 4,
                        format: atrium_spv_runtime::TexFormat::Rgba8Unorm as u32,
                        mip_count: 0, mip_descs: std::ptr::null(),
                        depth, slice_bytes: if depth > 1 { *mw * *mh * 4 } else { 0 },
                    });
                }
                let mip_count = mips_v.len() as u32;
                let mips_ptr = if mip_count > 1 { mips_v.as_ptr() }
                               else { std::ptr::null() };
                td.push(atrium_spv_runtime::TexDesc {
                    data: *data,
                    width: *w, height: *h,
                    stride_bytes: *w * 4,
                    format: atrium_spv_runtime::TexFormat::Rgba8Unorm as u32,
                    mip_count, mip_descs: mips_ptr,
                    depth, slice_bytes,
                });
                mip_arrays.push(mips_v);
                sd.push(samplers_guard.get(&sampler_id).copied()
                    .unwrap_or(placeholder_samp));
            }
            tex_descs = td;
            sampler_descs = sd;
            mip_desc_arrays = mip_arrays;
            // Allocate + populate.
            let mut buf = atrium_spv_runtime::descriptor_table_buffer(slot_count);
            unsafe {
                atrium_spv_runtime::write_helper_pointers(
                    &mut buf,
                    atrium_spv_runtime::atrium_tex_sample_2d,
                    atrium_spv_runtime::atrium_tex_fetch_2d,
                    atrium_spv_runtime::atrium_tex_sample_2d_lod,
                    atrium_spv_runtime::atrium_tex_sample_2d_array,
                    atrium_spv_runtime::atrium_tex_sample_cube,
                    atrium_spv_runtime::atrium_tex_gather_2d,
                    atrium_spv_runtime::atrium_tex_sample_2d_array_lod,
                    atrium_spv_runtime::atrium_tex_sample_cube_lod,
                    atrium_spv_runtime::atrium_tex_sample_2d_dref,
                );
                for (i, (binding, _, _, _, _, _)) in snaps.iter().enumerate() {
                    atrium_spv_runtime::write_descriptor_slot(
                        &mut buf, *binding as usize,
                        &tex_descs[i] as *const _,
                        &sampler_descs[i] as *const _,
                    );
                }
            }
            uniforms_buf = buf;
        } else if !state.bound_uniforms.is_empty() {
            // UBO-only path: copy the lowest-numbered binding's
            // buffer bytes into the uniforms scratch.  The
            // backend resolves StorageClass::Uniform to
            // params[1] (= scratch ptr); OpAccessChain through
            // the Block adds member offsets within the data.
            // Restricted to one UBO + no textures in v1 -- the
            // two share the prefix of the same scratch and a
            // proper layout discipline (UBO data lives after
            // the per-binding descriptor table) is its own arc.
            tex_descs = Vec::new();
            sampler_descs = Vec::new();
            let lowest_binding = state.bound_uniforms.keys().min().copied().unwrap_or(0);
            let buffer_id = state.bound_uniforms[&lowest_binding];
            let ubo_bytes = self.buffers.lock().unwrap()
                .get(&buffer_id).map(|b| b.bytes.clone())
                .unwrap_or_default();
            if ubo_bytes.is_empty() {
                log::warn!("Draw target={target_id}: UBO buffer {buffer_id} \
                            not registered or empty; binding {lowest_binding}");
            }
            uniforms_buf = ubo_bytes;
            mip_desc_arrays = Vec::new();
        } else {
            tex_descs = Vec::new();
            sampler_descs = Vec::new();
            mip_desc_arrays = Vec::new();
            uniforms_buf = Vec::new();
        }
        // CRITICAL: keep tex_descs / sampler_descs / mip_desc_
        // arrays alive through the loop -- the raw pointers
        // inside uniforms_buf + tex_descs[i].mip_descs
        // reference these Vecs' heap allocations.  Named
        // bindings here, NOT `let _ = ...` (same trap Arc 154's
        // commit e0dce68 documented for image_descs).
        let _retain_tex_state = (&tex_descs, &sampler_descs, &mip_desc_arrays);

        // Convert the captured `SetViewportCmd` (if any) into
        // the rasterizer's `Viewport` shape.  `None` falls
        // back to a fullscreen viewport in `fill_image_triangle`.
        let dt_viewport = state.viewport.map(|v| Viewport {
            x: v.x, y: v.y, width: v.width, height: v.height,
            min_depth: v.min_depth, max_depth: v.max_depth,
        });
        let dt_scissor = state.scissor.map(|s| Scissor {
            x: s.x, y: s.y, width: s.width, height: s.height,
        });
        for t in 0..tri_count {
            // Vertex-index triple per topology.  TriangleList:
            // (3t, 3t+1, 3t+2).  TriangleStrip: (t, t+1, t+2)
            // with v0 / v1 swapped on odd triangles so all
            // produced triangles keep the input's winding
            // (Vulkan spec).
            let (i0, i1, i2) = match topology {
                PrimitiveTopology::TriangleStrip => if t & 1 == 0 {
                    (t, t + 1, t + 2)
                } else {
                    (t + 1, t, t + 2)
                },
                _ => (3*t, 3*t + 1, 3*t + 2),
            };
            let v0 = &assembled.bytes[i0*stride .. (i0+1)*stride];
            let v1 = &assembled.bytes[i1*stride .. (i1+1)*stride];
            let v2 = &assembled.bytes[i2*stride .. (i2+1)*stride];
            let dt = DrawTriangle {
                vertex_attrs: [v0, v1, v2],
                push_constants: &state.push_constants,
                blend_state: raster.blend,
                blend_extra: &state.blend_extra,
                varying_f32_count,
                uniforms: &uniforms_buf,
                viewport: dt_viewport,
                scissor: dt_scissor,
                cull_mode: state.cull_mode_override.unwrap_or(raster.cull_mode),
                front_face: state.front_face_override.unwrap_or(raster.front_face),
                depth_write,
                depth_compare_op: depth_cmp,
                depth_bounds,
                stencil: stencil_state,
                depth_bias,
                compute_implicit_lod: fs_implicit_lod
                    && !state.bound_textures.is_empty()
                    && varying_f32_count >= 2,
                ..Default::default()
            };
            let db_ref: Option<&mut [f32]> = if !depth_enabled {
                None
            } else if let Some((id, ref mut guard)) = depth_lock {
                guard.get_mut(&id).map(|d| &mut d.pixels[..])
            } else {
                depth_buffer.as_deref_mut()
            };
            let sb_ref: Option<&mut [u8]> = stencil_buffer.as_deref_mut();
            if let Err(e) = self.registry.fill_image_triangle(
                vs_shader_id, fs_shader_id,
                &dt, width, height, pixels, db_ref, sb_ref,
                &mut extra_slices,
            ) {
                log::warn!("Draw target={target_id}: triangle {t}/{tri_count} \
                            fill_image_triangle failed: {e}");
                // Don't early-return: fall through so the
                // owned attachment storages are re-inserted.
                break;
            }
        }
        true
        }; // end render block -- pixels / extra_slices borrows end here
        let _ = render_ok;
        // Re-insert the owned attachment storages.
        images.insert(target_id.raw() as u64, primary);
        for (eid, st) in extra_storages {
            images.insert(eid as u64, st);
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
                let src_size = a.format.source_byte_size();
                let packed_size = a.format.byte_size();
                let src_end = src_off + src_size;
                if src_end > src.bytes.len() {
                    return Err(format!(
                        "attribute @location {} (vertex {}) reads bytes \
                         {}..{} past buffer end {}",
                        a.location, global_v, src_off, src_end,
                        src.bytes.len()));
                }
                let out_off = out_base + (out_offsets[ai] as usize);
                let src_bytes = &src.bytes[src_off..src_end];
                let dst_bytes = &mut bytes[out_off..out_off + packed_size];
                // Format-specific decode into the f32 packed
                // stream the VS reads.  SFLOAT formats are
                // already f32; just memcpy.  UNORM formats
                // expand each lane to f32 via `byte / 255.0`.
                match a.format {
                    aqueduct_gpu::VertexFormat::R32Sfloat
                    | aqueduct_gpu::VertexFormat::R32g32Sfloat
                    | aqueduct_gpu::VertexFormat::R32g32b32Sfloat
                    | aqueduct_gpu::VertexFormat::R32g32b32a32Sfloat => {
                        dst_bytes.copy_from_slice(src_bytes);
                    }
                    aqueduct_gpu::VertexFormat::R8g8b8a8Unorm => {
                        // 4 u8 -> 4 f32, each lane / 255.0.
                        for lane in 0..4 {
                            let v = src_bytes[lane] as f32 / 255.0;
                            let b = v.to_le_bytes();
                            dst_bytes[lane * 4..lane * 4 + 4]
                                .copy_from_slice(&b);
                        }
                    }
                }
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
    /// Read indices out of the bound index buffer + apply
    /// `vertex_offset`.  When `restart_enable` is true, the
    /// type-max sentinel (`0xFFFF` for u16, `0xFFFFFFFF`
    /// for u32) is preserved as `u32::MAX` -- without the
    /// vertex_offset addition -- so the caller can split
    /// the resulting strip into segments.  Pre-condition:
    /// apps that enable primitive restart must not use
    /// the type-max as a "real" vertex index (matches
    /// Vulkan's spec).
    fn gather_indices(
        &self,
        bound: &BoundIndexBuffer,
        first_index: u32,
        count: u32,
        vertex_offset: i32,
        restart_enable: bool,
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
        // Per-type restart sentinel value in the buffer.
        let restart_sentinel: u32 = match bound.index_type {
            IndexType::Uint16 => 0xFFFF,
            IndexType::Uint32 => 0xFFFF_FFFF,
        };
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count as usize {
            let off = base + i * elem_size;
            let idx_raw: u32 = match bound.index_type {
                IndexType::Uint16 => u16::from_le_bytes(
                    src.bytes[off..off+2].try_into().unwrap()) as u32,
                IndexType::Uint32 => u32::from_le_bytes(
                    src.bytes[off..off+4].try_into().unwrap()),
            };
            // Primitive-restart: the type-max sentinel
            // bypasses vertex_offset and is preserved as
            // u32::MAX so the dispatcher can split strips.
            if restart_enable && idx_raw == restart_sentinel {
                out.push(u32::MAX);
                continue;
            }
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
        // Always allocate the image-table buffer (Arc 150
        // commit 3) -- the bespoke backend's Op::Barrier
        // lowering loads `atrium_barrier` from
        // [X19, #IMG_TABLE_BARRIER_OFFSET], which requires
        // X0 (the image-table base) to be non-null even for
        // shaders that don't use storage images.  Pre-150
        // we passed NULL when no image bindings existed;
        // now we still allocate at least the header so the
        // barrier slot is reachable.
        let images_guard;
        let image_descs: Vec<atrium_spv_runtime::ImageDesc>;
        let mut image_table: Vec<u8>;
        let slot_count = img_bindings.iter()
            .map(|(b, _)| *b as usize + 1).max().unwrap_or(0);
        image_table = atrium_spv_runtime::image_table_buffer(slot_count);

        if !img_bindings.is_empty() {
            images_guard = Some(self.images.lock().unwrap());
            let mut descs: Vec<atrium_spv_runtime::ImageDesc> =
                Vec::with_capacity(slot_count);
            let mut slot_of: Vec<Option<usize>> = vec![None; slot_count];
            for &(binding, image_raw) in &img_bindings {
                if let Some(img) = images_guard.as_ref().unwrap().get(&image_raw) {
                    let idx = descs.len();
                    descs.push(atrium_spv_runtime::ImageDesc {
                        data: img.pixels.as_ptr() as *mut u8,
                        width: img.width,
                        height: img.height,
                        stride_bytes: img.width * 4,
                        format: atrium_spv_runtime::StorageFormat::Rgba8Unorm
                            as u32,
                        depth: 1,
                        slice_bytes: img.width * img.height * 4,
                        mip_count: 0,
                        mip_descs: std::ptr::null(),
                    });
                    slot_of[binding as usize] = Some(idx);
                }
            }
            // SAFETY: helper fn pointers are static-lifetime
            // statics in atrium-spv-runtime; descs outlive the
            // dispatch (held by `image_descs` below).
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
                            &descs[*idx] as *const _);
                    }
                }
            }
            image_descs = descs;
        } else {
            images_guard = None;
            image_descs = Vec::new();
        }

        // Populate the IMG_TABLE_BARRIER_OFFSET slot with the
        // atrium_barrier fn ptr.  The bespoke backend's
        // Op::Barrier lowering emits `ldr x9, [X19, #64]; blr
        // x9` against this slot.  When no barrier is in flight
        // (single-invocation case) atrium_barrier returns
        // immediately, so populating unconditionally is safe
        // for every compute dispatch.  Arc 150.
        // SAFETY: atrium_barrier is a static-lifetime extern
        // "C" function in atrium-spv-runtime.
        unsafe {
            atrium_spv_runtime::write_barrier_helper_ptr(
                &mut image_table,
                atrium_spv_runtime::atrium_barrier,
            );
        }
        let uni_ptr: *const u8 = image_table.as_ptr();
        // CRITICAL: must be a NAMED binding to keep these
        // alive until end of scope.  `let _ = (...)` would
        // drop both immediately -- freeing the `image_descs`
        // Vec's heap while `image_table` still holds raw
        // pointers into it, so cs_main would deref garbage.
        let _retain_image_state = (images_guard, &image_descs);

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
        // Arc 150 parallel-lane mode is gated on the shader
        // declaring at least one OpControlBarrier.  Without
        // a barrier the lanes are free-running and any order
        // is spec-legal -- the cheaper serial loop is the
        // right pick AND it gives a deterministic "last-
        // writer wins" ordering for shaders that race on a
        // single SSBO slot (e.g. early bring-up tests that
        // pre-date Arc 150).
        let uses_barrier = cs_state.uses_barrier;
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
                        // Lanes within a workgroup execute in
                        // one of two modes:
                        //
                        // (a) Serial -- when lx*ly*lz == 1.  No
                        //     possible cross-lane synchronisation;
                        //     just call cs_main once.  Skips the
                        //     OS-thread spawn overhead for the
                        //     common single-invocation case
                        //     (every Arc 6-149 rung).
                        //
                        // (b) Parallel -- when lx*ly*lz > 1.  One
                        //     std::thread per invocation, sharing
                        //     an Arc<std::sync::Barrier> via the
                        //     atrium-spv-runtime's THREAD_BARRIER
                        //     TLS slot.  atrium_barrier (called
                        //     from compiled cs_main on Op::Barrier)
                        //     reads the TLS and waits, so
                        //     cross-lane shared-memory writes
                        //     before the barrier are visible to
                        //     reads after.  Arc 150 commit 3.
                        let n_lanes = (lx_n * ly_n * lz_n) as usize;
                        if n_lanes == 1 || !uses_barrier {
                            // Serial-within-workgroup path.  Walks
                            // (lz, ly, lx) inside one cs_main call
                            // chain so a shader that races on a
                            // single output gets a stable
                            // "last-writer wins" ordering.
                            // SAFETY: cs_main is a dlopened
                            // C-ABI function whose signature
                            // matches CsMain (checked at open
                            // time); output buffer + descriptor
                            // table outlive this scope; wg_buf
                            // outlives the inner block.
                            for lz in 0..lz_n {
                                for ly in 0..ly_n {
                                    for lx in 0..lx_n {
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
                        } else {
                            let barrier = std::sync::Arc::new(
                                std::sync::Barrier::new(n_lanes));
                            let uni_a = uni_ptr as usize;
                            let pc_a  = pc_ptr  as usize;
                            let out_a = out_ptr as usize;
                            let wg_a  = wg_ptr  as usize;
                            std::thread::scope(|ws| {
                                for lz in 0..lz_n {
                                    for ly in 0..ly_n {
                                        for lx in 0..lx_n {
                                            let barrier = barrier.clone();
                                            ws.spawn(move || {
                                                atrium_spv_runtime::set_thread_barrier(barrier);
                                                // SAFETY: same as the
                                                // serial arm above;
                                                // additionally, the
                                                // ImageDesc table base
                                                // (in uni_a) is
                                                // shared-read-only,
                                                // and the per-binding
                                                // output buffers were
                                                // sized at workgroup-
                                                // count granularity
                                                // so cross-lane
                                                // writes within the
                                                // same workgroup are
                                                // a use-pattern the
                                                // shader's atomics +
                                                // barriers manage.
                                                unsafe {
                                                    cs_main(
                                                        uni_a as *const u8,
                                                        pc_a  as *const u8,
                                                        out_a as *mut u8,
                                                        gx, gy, gz,
                                                        lx, ly, lz,
                                                        wg_a as *mut u8,
                                                    );
                                                }
                                                atrium_spv_runtime::clear_thread_barrier();
                                            });
                                        }
                                    }
                                }
                            });
                        }
                    }
                });
            }
        });
        // Write-back to bound SSBOs.  For every (binding ->
        // buffer_id) the BindDescriptors handler stashed, copy
        // per_binding[b] into buffers[buffer_id].bytes (clamped
        // to the buffer's declared size).  After this point,
        // OP_GPU_BUFFER_READ from the client will see the
        // shader's writes.  Drained: the next dispatch starts
        // with no SSBO bindings until the next
        // vkCmdBindDescriptorSets fires.
        let bound_buffers: Vec<(u32, u64)> = {
            let mut m = self.compute_buffer_by_binding.lock().unwrap();
            m.drain().collect()
        };
        if !bound_buffers.is_empty() {
            let mut buffers = self.buffers.lock().unwrap();
            for (binding, buffer_raw) in bound_buffers {
                let Some(src) = per_binding.get(binding as usize) else {
                    continue;
                };
                let Some(buf) = buffers.get_mut(&(buffer_raw as u32)) else {
                    continue;
                };
                let n = src.len().min(buf.bytes.len());
                buf.bytes[..n].copy_from_slice(&src[..n]);
            }
        }

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
                    // Storage-buffer descriptor writes.  Pre-fill
                    // the shader's input slot for this binding
                    // from the buffer's current bytes (so shaders
                    // that read-modify-write see what the client
                    // wrote via OP_GPU_BUFFER_WRITE), and remember
                    // binding -> buffer_id so the post-dispatch
                    // pass can copy the shader's output back into
                    // the buffer.
                    for (binding, buffer_raw)
                        in parse_bind_descriptors_storage_buffers(body)
                    {
                        let bytes = self.buffers.lock().unwrap()
                            .get(&buffer_raw)
                            .map(|b| b.bytes.clone())
                            .unwrap_or_default();
                        self.set_compute_input_for_binding(binding, bytes);
                        self.compute_buffer_by_binding.lock().unwrap()
                            .insert(binding, buffer_raw as u64);
                    }
                }
                FrameOp::Dispatch => match DispatchCmd::from_bytes(body) {
                    Ok(cmd) => self.dispatch_compute(&state, cmd),
                    Err(e) => log::warn!("malformed Dispatch: {e}"),
                },
                FrameOp::CopyImgToBuf => self.execute_copy_image_to_buffer(body),
                // CopyBufToImg is handled by the pre-pass
                // walker (see `execute_upload_ops`) so texture
                // uploads land BEFORE the render passes that
                // sample them; we deliberately don't run it
                // again here.
                _ => {}
            }
        }
    }

    /// `FrameOp::CopyImgToBuf` -- image readback into a buffer.
    /// Body shape (from the ICD's `vkCmdCopyImageToBuffer`):
    /// `src_image_id u32 + dst_buffer_id u32 + src_layout u32 +
    /// region_count u32 + per-region 56 B (VkBufferImageCopy)`.
    ///
    /// VkBufferImageCopy layout:
    ///   0   bufferOffset:u64
    ///   8   bufferRowLength:u32     (0 = tight pack at extent.w)
    ///  12   bufferImageHeight:u32   (0 = tight pack at extent.h)
    ///  16   imageSubresource:16 B   (aspectMask/mipLevel/baseArrayLayer/layerCount)
    ///  32   imageOffset:VkOffset3D  (x:i32, y:i32, z:i32)
    ///  44   imageExtent:VkExtent3D  (w:u32, h:u32, d:u32)
    ///
    /// Today we only support 2D RGBA8 (4 bytes/texel) -- the
    /// existing `ImageStorage` shape.  Mip / array / 3D land
    /// when those format paths land too.
    fn execute_copy_image_to_buffer(&self, body: &[u8]) {
        if body.len() < 16 {
            log::warn!("CopyImgToBuf: body too short ({} bytes)", body.len());
            return;
        }
        let src_id = u32::from_le_bytes(body[0..4].try_into().unwrap());
        let dst_id = u32::from_le_bytes(body[4..8].try_into().unwrap());
        let _src_layout = u32::from_le_bytes(body[8..12].try_into().unwrap());
        let region_count = u32::from_le_bytes(body[12..16].try_into().unwrap());
        if region_count == 0 { return; }

        // Snapshot src pixels under the images lock; drop the
        // lock before touching `buffers` so the two never
        // collide on later concurrent paths.
        let src_snap: Option<(u32, u32, Vec<u8>)> = {
            let images = self.images.lock().unwrap();
            images.get(&(src_id as u64)).map(|img|
                (img.width, img.height, img.pixels.clone()))
        };
        let Some((src_w, src_h, src_pixels)) = src_snap else {
            log::warn!("CopyImgToBuf: src image {src_id} not registered");
            return;
        };
        let bpp: usize = 4; // RGBA8 only for now.
        let src_stride = src_w as usize * bpp;

        let mut buffers = self.buffers.lock().unwrap();
        let Some(dst_buf) = buffers.get_mut(&dst_id) else {
            log::warn!("CopyImgToBuf: dst buffer {dst_id} not registered");
            return;
        };

        for r in 0..region_count as usize {
            let off = 16 + r * 56;
            if off + 56 > body.len() {
                log::warn!("CopyImgToBuf: region {r} truncated");
                break;
            }
            let region = &body[off..off + 56];
            let buf_offset = u64::from_le_bytes(region[0..8].try_into().unwrap()) as usize;
            let buf_row_length = u32::from_le_bytes(region[8..12].try_into().unwrap());
            let img_x = i32::from_le_bytes(region[32..36].try_into().unwrap()).max(0) as u32;
            let img_y = i32::from_le_bytes(region[36..40].try_into().unwrap()).max(0) as u32;
            let ext_w = u32::from_le_bytes(region[44..48].try_into().unwrap());
            let ext_h = u32::from_le_bytes(region[48..52].try_into().unwrap());
            let copy_w = ext_w.min(src_w.saturating_sub(img_x));
            let copy_h = ext_h.min(src_h.saturating_sub(img_y));
            let row_bytes = copy_w as usize * bpp;
            let dst_row_pitch = if buf_row_length == 0 {
                row_bytes
            } else {
                buf_row_length as usize * bpp
            };
            for y in 0..copy_h as usize {
                let src_off = (img_y as usize + y) * src_stride
                    + img_x as usize * bpp;
                let dst_off = buf_offset + y * dst_row_pitch;
                if src_off + row_bytes > src_pixels.len() { break; }
                if dst_off + row_bytes > dst_buf.bytes.len() { break; }
                dst_buf.bytes[dst_off..dst_off + row_bytes]
                    .copy_from_slice(&src_pixels[src_off..src_off + row_bytes]);
            }
        }
    }

    /// Pre-pass walker: process outside-renderpass upload
    /// ops (today: `FrameOp::CopyBufToImg`).  Runs before
    /// `execute_pass` so a frame that uploads texture data
    /// then draws-while-sampling sees the texture content the
    /// app populated.  Single-purpose by design: don't touch
    /// state-tracking ops here (BindPipeline / PushConstants /
    /// BindDescriptors); those are pass-local and live in
    /// `execute_pass` / `execute_compute_ops`.
    fn execute_upload_ops(&self, frame_buf: &[u8]) {
        let mut decoder = FrameDecoder::new(frame_buf);
        let mut rp_depth: u32 = 0;
        while let Ok(Some((op, body))) = decoder.next() {
            match op {
                FrameOp::BeginRenderPass => rp_depth += 1,
                FrameOp::EndRenderPass => {
                    rp_depth = rp_depth.saturating_sub(1);
                }
                _ if rp_depth > 0 => {} // inside RP -- skip
                FrameOp::CopyBufToImg =>
                    self.execute_copy_buffer_to_image(body),
                _ => {}
            }
        }
    }

    /// `FrameOp::CopyBufToImg` -- buffer-to-image copy, the
    /// standard Vulkan path for uploading texture content from
    /// a HOST_VISIBLE staging buffer to a DEVICE_LOCAL image.
    /// Mirror of `execute_copy_image_to_buffer`; same 16-byte
    /// header + 56-byte VkBufferImageCopy regions.  RGBA8 only
    /// (matches `ImageStorage`'s hardcoded 4-bytes-per-pixel
    /// layout); deeper format support lands with the
    /// storage-image format-aware ABI arc.
    fn execute_copy_buffer_to_image(&self, body: &[u8]) {
        if body.len() < 16 {
            log::warn!("CopyBufToImg: body too short ({} bytes)", body.len());
            return;
        }
        let src_id = u32::from_le_bytes(body[0..4].try_into().unwrap());
        let dst_id = u32::from_le_bytes(body[4..8].try_into().unwrap());
        let _dst_layout = u32::from_le_bytes(body[8..12].try_into().unwrap());
        let region_count = u32::from_le_bytes(body[12..16].try_into().unwrap());
        if region_count == 0 { return; }

        // Snapshot src bytes under the buffers lock; drop the
        // lock before grabbing `images` so they never collide.
        let src_snap: Option<Vec<u8>> = {
            let buffers = self.buffers.lock().unwrap();
            buffers.get(&src_id).map(|b| b.bytes.clone())
        };
        let Some(src_bytes) = src_snap else {
            log::warn!("CopyBufToImg: src buffer {src_id} not registered");
            return;
        };

        let mut images = self.images.lock().unwrap();
        let Some(dst_img) = images.get_mut(&(dst_id as u64)) else {
            log::warn!("CopyBufToImg: dst image {dst_id} not registered");
            return;
        };
        let bpp: usize = 4; // RGBA8 only.
        let base_w = dst_img.width;
        let base_h = dst_img.height;

        for r in 0..region_count as usize {
            let off = 16 + r * 56;
            if off + 56 > body.len() {
                log::warn!("CopyBufToImg: region {r} truncated");
                break;
            }
            let region = &body[off..off + 56];
            let buf_offset = u64::from_le_bytes(region[0..8].try_into().unwrap()) as usize;
            let buf_row_length = u32::from_le_bytes(region[8..12].try_into().unwrap());
            // image_subresource (16 B): aspect_mask + mip_level
            // + base_array_layer + layer_count.
            let mip_level = u32::from_le_bytes(region[20..24].try_into().unwrap());
            let base_array_layer = u32::from_le_bytes(region[24..28].try_into().unwrap());
            let img_x = i32::from_le_bytes(region[32..36].try_into().unwrap()).max(0) as u32;
            let img_y = i32::from_le_bytes(region[36..40].try_into().unwrap()).max(0) as u32;
            let ext_w = u32::from_le_bytes(region[44..48].try_into().unwrap());
            let ext_h = u32::from_le_bytes(region[48..52].try_into().unwrap());

            // Per-layer byte offset into the base pixel buffer
            // (layer-major).  Only meaningful for mip 0; v1
            // doesn't carry array layers per mip level.
            let layer = base_array_layer.min(dst_img.array_layers.saturating_sub(1));
            let layer_slice_bytes =
                (base_w as usize) * (base_h as usize) * bpp;
            let layer_base_off = (layer as usize) * layer_slice_bytes;

            // Resolve mip target: level 0 writes the base
            // pixels (at the chosen array-layer slice); level
            // >0 lazily allocates a MipLevel entry sized
            // `max(1, base >> level)`.
            let (dst_w, dst_h, layer_off, dst_pixels): (u32, u32, usize, &mut Vec<u8>) = if mip_level == 0 {
                (base_w, base_h, layer_base_off, &mut dst_img.pixels)
            } else {
                let idx = (mip_level - 1) as usize;
                while dst_img.mip_levels.len() <= idx {
                    let next_level = dst_img.mip_levels.len() as u32 + 1;
                    let mw = (base_w >> next_level).max(1);
                    let mh = (base_h >> next_level).max(1);
                    dst_img.mip_levels.push(MipLevel {
                        width: mw, height: mh,
                        pixels: vec![0u8; (mw as usize) * (mh as usize) * bpp],
                    });
                }
                let m = &mut dst_img.mip_levels[idx];
                (m.width, m.height, 0usize, &mut m.pixels)
            };

            let dst_stride = dst_w as usize * bpp;
            let copy_w = ext_w.min(dst_w.saturating_sub(img_x));
            let copy_h = ext_h.min(dst_h.saturating_sub(img_y));
            let row_bytes = copy_w as usize * bpp;
            let src_row_pitch = if buf_row_length == 0 {
                row_bytes
            } else {
                buf_row_length as usize * bpp
            };
            for y in 0..copy_h as usize {
                let dst_off = layer_off
                    + (img_y as usize + y) * dst_stride
                    + img_x as usize * bpp;
                let src_off = buf_offset + y * src_row_pitch;
                if src_off + row_bytes > src_bytes.len() { break; }
                if dst_off + row_bytes > dst_pixels.len() { break; }
                dst_pixels[dst_off..dst_off + row_bytes]
                    .copy_from_slice(&src_bytes[src_off..src_off + row_bytes]);
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
        stencil_buffer: &mut Option<Vec<u8>>,
    ) {
        if state.tier2_shader.is_none() {
            self.draws_skipped.fetch_add(1, Ordering::Relaxed);
            log::debug!("DrawIndexed on target {target_id} skipped: no Tier-2 pipeline bound");
            return;
        }
        if cmd.index_count == 0 { return; }
        // Rasterizer-discard short-circuit (same shape as
        // dispatch_draw -- see comment there).
        let pipeline_discard = state.raster
            .map(|r| r.rasterizer_discard).unwrap_or(false);
        if state.rasterizer_discard_override.unwrap_or(pipeline_discard) {
            self.draws_skipped.fetch_add(1, Ordering::Relaxed);
            return;
        }
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

        // Effective primitive-restart enable (dynamic
        // override + pipeline static).  Only meaningful in
        // combination with TriangleStrip topology.
        let pipeline_restart = state.raster
            .map(|r| r.primitive_restart_enable).unwrap_or(false);
        let restart_enable = state.primitive_restart_enable_override
            .unwrap_or(pipeline_restart);

        // Read indices from the bound index buffer, applying
        // vertex_offset (and preserving sentinels when
        // restart is on).
        let raw_indices = match self.gather_indices(
            &bound_idx, cmd.first_index, cmd.index_count, cmd.vertex_offset,
            restart_enable,
        ) {
            Ok(v) => v,
            Err(e) => {
                log::warn!("DrawIndexed target={target_id}: index gather: {e}");
                return;
            }
        };
        // For vertex assembly we need a sentinel-free copy
        // (u32::MAX would be an invalid index into the
        // vertex buffer).  Replace with 0 -- those bytes
        // are placeholder filler; the strip-walk below
        // never reads them because it skips sentinel
        // positions when forming triangles.
        let indices: Vec<u32> = raw_indices.iter()
            .map(|&i| if i == u32::MAX { 0 } else { i })
            .collect();

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

        let stride = assembled.stride as usize;
        let pipeline_topology = state.raster
            .map(|r| r.topology).unwrap_or_default();
        let topology = state.topology_override.unwrap_or(pipeline_topology);
        let n_verts = assembled.vertex_count as usize;
        let tri_count = match topology {
            PrimitiveTopology::TriangleStrip => n_verts.saturating_sub(2),
            _                                 => n_verts / 3,
        };
        if matches!(topology, PrimitiveTopology::TriangleList) && n_verts % 3 != 0 {
            log::warn!("DrawIndexed target={target_id}: index_count {} \
                        is not a multiple of 3 under TriangleList; dropping {} \
                        trailing indices",
                       assembled.vertex_count, n_verts - tri_count * 3);
        }

        // MRT: same owned-removal pattern as dispatch_draw --
        // see the comment there.
        let mut images = self.images.lock().unwrap();
        let mut primary = match images.remove(&(target_id.raw() as u64)) {
            Some(i) => i,
            None => {
                log::warn!("DrawIndexed target={target_id}: target image not registered");
                return;
            }
        };
        let mut extra_storages: Vec<(u32, ImageStorage)> =
            state.extra_color_targets.iter()
                .filter(|&&e| e as u64 != target_id.raw() as u64)
                .filter_map(|&e| images.remove(&(e as u64)).map(|s| (e, s)))
                .collect();
        let width = primary.width;
        let height = primary.height;
        let render_ok = {
        let pixels = &mut primary.pixels[..];
        let mut extra_slices: Vec<&mut [u8]> = extra_storages
            .iter_mut().map(|(_, s)| &mut s.pixels[..]).collect();

        let raster = state.raster.unwrap_or_default();
        // Pipeline-static depth state.
        let static_test  = raster.depth.map(|d| d.test_enable).unwrap_or(false);
        let static_write = raster.depth.map(|d| d.write_enable).unwrap_or(false);
        let static_cmp   = raster.depth
            .map(|d| convert_compare_op(d.compare_op))
            .unwrap_or(CompareOp::Less);
        // Dynamic overrides (`vkCmdSetDepthTestEnable` /
        // `vkCmdSetDepthWriteEnable` /
        // `vkCmdSetDepthCompareOp`) take precedence.  Depth
        // writes additionally gate on the depth test being
        // active (Vulkan spec: write_enable has no effect
        // when the test is off).
        let depth_enabled = state.depth_test_enable_override.unwrap_or(static_test);
        let depth_write   = depth_enabled
            && state.depth_write_enable_override.unwrap_or(static_write);
        let depth_cmp     = state.depth_compare_op_override.unwrap_or(static_cmp);
        // Depth bounds test: dynamic + static merge.  An app
        // can toggle the enable flag and adjust the range
        // separately, so the effective `(min, max)` comes
        // from whichever source was most recently set.
        let static_bounds_enable = raster.depth
            .map(|d| d.bounds_test_enable).unwrap_or(false);
        let static_bounds_range  = raster.depth
            .map(|d| (d.min_depth_bounds, d.max_depth_bounds))
            .unwrap_or((0.0, 1.0));
        let bounds_enable = state.bounds_test_enable_override
            .unwrap_or(static_bounds_enable);
        let depth_bounds = if bounds_enable {
            Some(state.bounds_range_override.unwrap_or(static_bounds_range))
        } else { None };
        // Depth bias: enable + factors merge from static +
        // dynamic.  An app can flip enable independently of
        // setting factors and vice versa, so each is its own
        // override.
        let bias_enable = state.depth_bias_enable_override
            .unwrap_or(raster.depth_bias_enable);
        let depth_bias = if bias_enable {
            Some(state.depth_bias_override.unwrap_or((
                raster.depth_bias_constant_factor,
                raster.depth_bias_clamp,
                raster.depth_bias_slope_factor,
            )))
        } else { None };
        if depth_enabled && depth_buffer.is_none() {
            *depth_buffer = Some(vec![
                f32::INFINITY;
                (width as usize) * (height as usize)
            ]);
        }
        // Stencil scratch (lazy alloc, mirror of
        // dispatch_draw's setup).
        let stencil_state = state.raster
            .and_then(|r| r.stencil);
        if stencil_state.is_some() && stencil_buffer.is_none() {
            *stencil_buffer = Some(vec![
                0u8;
                (width as usize) * (height as usize)
            ]);
        }

        // Same varying-bytes plumbing as dispatch_draw -- without
        // this an indexed draw with a VS that emits Location-
        // decorated varyings would crash the FS via null
        // in_varyings_ptr (see Rung H's commit for the long
        // version).
        let varying_f32_count = (self.pipeline_vs_varying_bytes.lock().unwrap()
            .get(&pipeline_raw).copied().unwrap_or(0) as usize) / 4;
        let fs_implicit_lod = self.pipeline_fs_implicit_lod.lock().unwrap()
            .get(&pipeline_raw).copied().unwrap_or(false);

        let dt_viewport = state.viewport.map(|v| Viewport {
            x: v.x, y: v.y, width: v.width, height: v.height,
            min_depth: v.min_depth, max_depth: v.max_depth,
        });
        let dt_scissor = state.scissor.map(|s| Scissor {
            x: s.x, y: s.y, width: s.width, height: s.height,
        });

        // Build the list of (i0, i1, i2) triples honouring
        // topology + primitive restart.  Triangle list uses
        // the legacy `(3t, 3t+1, 3t+2)` walk; triangle strip
        // slides a 3-wide window with parity-driven swap,
        // breaking the window on restart sentinels in
        // `raw_indices` (`u32::MAX`).
        let mut triples: Vec<(usize, usize, usize)> =
            Vec::with_capacity(tri_count);
        match topology {
            PrimitiveTopology::TriangleStrip => {
                let mut win: [usize; 3] = [0; 3];
                let mut win_len: usize = 0;
                let mut parity = false;
                for (pos, &raw) in raw_indices.iter().enumerate() {
                    if restart_enable && raw == u32::MAX {
                        win_len = 0; parity = false;
                        continue;
                    }
                    if win_len < 3 {
                        win[win_len] = pos;
                        win_len += 1;
                    } else {
                        win[0] = win[1]; win[1] = win[2]; win[2] = pos;
                    }
                    if win_len == 3 {
                        let (a, b, c) = (win[0], win[1], win[2]);
                        triples.push(if parity { (b, a, c) } else { (a, b, c) });
                        parity = !parity;
                    }
                }
            }
            _ => {
                for t in 0..tri_count {
                    triples.push((3*t, 3*t + 1, 3*t + 2));
                }
            }
        }

        for &(i0, i1, i2) in &triples {
            let v0 = &assembled.bytes[i0*stride .. (i0+1)*stride];
            let v1 = &assembled.bytes[i1*stride .. (i1+1)*stride];
            let v2 = &assembled.bytes[i2*stride .. (i2+1)*stride];
            let dt = DrawTriangle {
                vertex_attrs: [v0, v1, v2],
                push_constants: &state.push_constants,
                blend_state: raster.blend,
                blend_extra: &state.blend_extra,
                varying_f32_count,
                viewport: dt_viewport,
                scissor: dt_scissor,
                cull_mode: state.cull_mode_override.unwrap_or(raster.cull_mode),
                front_face: state.front_face_override.unwrap_or(raster.front_face),
                depth_write,
                depth_compare_op: depth_cmp,
                depth_bounds,
                stencil: stencil_state,
                depth_bias,
                compute_implicit_lod: fs_implicit_lod
                    && !state.bound_textures.is_empty()
                    && varying_f32_count >= 2,
                ..Default::default()
            };
            let db_ref = if depth_enabled {
                depth_buffer.as_deref_mut()
            } else { None };
            let sb_ref: Option<&mut [u8]> = stencil_buffer.as_deref_mut();
            if let Err(e) = self.registry.fill_image_triangle(
                vs_shader_id, fs_shader_id,
                &dt, width, height, pixels, db_ref, sb_ref,
                &mut extra_slices,
            ) {
                log::warn!("DrawIndexed target={target_id}: triangle \
                            fill_image_triangle failed: {e}");
                break;
            }
        }
        true
        }; // end render block
        let _ = render_ok;
        images.insert(target_id.raw() as u64, primary);
        for (eid, st) in extra_storages {
            images.insert(eid as u64, st);
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
        self.image_created_layered(image_id, width, height, 1);
    }

    fn image_created_layered(
        &self, image_id: ResourceId, width: u32, height: u32, array_layers: u32,
    ) {
        if width == 0 || height == 0 { return; }
        const MAX_DIM: u32 = 16 * 1024;
        if width > MAX_DIM || height > MAX_DIM { return; }
        let layers = array_layers.max(1);
        let pixels = vec![0u8;
            (width as usize) * (height as usize) * 4 * (layers as usize)];
        self.images.lock().unwrap().insert(image_id.raw() as u64, ImageStorage {
            width, height, pixels,
            array_layers: layers,
            mip_levels: Vec::new(),
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

    fn sampler_created(
        &self,
        sampler_id:    ResourceId,
        min_filter:    u8,
        mag_filter:    u8,
        _mip_filter:   u8,
        address_modes: [u8; 3],
        _max_anisotropy: f32,
        _min_lod:      f32,
        _max_lod:      f32,
        compare_enable: u8,
        compare_op:    u32,
    ) {
        // VkFilter -> runtime FilterMode.  Same wire form
        // (0=Nearest, 1=Linear) so the cast is direct.
        let to_filter = |f: u8| -> u32 { f as u32 };
        // VkSamplerAddressMode -> runtime WrapMode.  Mapping:
        //   VK_REPEAT(0)               -> Repeat(1)
        //   VK_MIRRORED_REPEAT(1)      -> Mirror(2)
        //   VK_CLAMP_TO_EDGE(2)        -> ClampToEdge(0)
        //   VK_CLAMP_TO_BORDER(3)      -> ClampToEdge (no border yet)
        //   VK_MIRROR_CLAMP_TO_EDGE(4) -> ClampToEdge
        let to_wrap = |a: u8| -> u32 {
            match a {
                0 => 1, // Repeat
                1 => 2, // Mirror
                _ => 0, // ClampToEdge
            }
        };
        let desc = atrium_spv_runtime::SamplerDesc {
            mag_filter: to_filter(mag_filter),
            min_filter: to_filter(min_filter),
            wrap_s: to_wrap(address_modes[0]),
            wrap_t: to_wrap(address_modes[1]),
            compare_enable: compare_enable as u32,
            compare_op,
        };
        self.samplers.lock().unwrap()
            .insert(sampler_id.raw(), desc);
    }

    fn sampler_destroyed(&self, sampler_id: ResourceId) {
        self.samplers.lock().unwrap()
            .remove(&sampler_id.raw());
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

    fn buffer_read_bytes(
        &self,
        buffer_id: ResourceId,
        offset: u64,
        size: u64,
    ) -> Result<Vec<u8>, String> {
        let buffers = self.buffers.lock().unwrap();
        let buf = buffers.get(&buffer_id.raw())
            .ok_or_else(|| format!("buffer {buffer_id} not registered"))?;
        let end = offset.checked_add(size)
            .ok_or_else(|| "buffer read offset+size overflows u64".to_string())?;
        if end > buf.size {
            return Err(format!(
                "buffer read end {end} exceeds size {}", buf.size,
            ));
        }
        let off = offset as usize;
        let sz  = size   as usize;
        Ok(buf.bytes[off..off + sz].to_vec())
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

        // Pre-pass: upload ops that should land BEFORE the
        // render passes that consume them (e.g.
        // vkCmdCopyBufferToImage uploading texture data before
        // a draw that samples that texture).  Walk the whole
        // frame and execute only CopyBufToImg outside any
        // render pass; the post-pass execute_compute_ops sweep
        // handles everything else, INCLUDING the CopyImgToBuf
        // readback ops that depend on render output.
        //
        // Without this split, an app that does the standard
        // "upload texture, then draw with it" sequence in
        // ONE command buffer would see the texture remain
        // unmodified during the draw -- the post-pass walker
        // would apply the upload AFTER the FS had already run
        // against the cleared texel storage.
        self.execute_upload_ops(frame_buf);

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
        blend_extra: &[WireBlendState],
        raster: Option<aqueduct_gpu::Tier2RasterState>,
        topology: aqueduct_gpu::Tier2PrimitiveTopology,
        stencil: Option<aqueduct_gpu::Tier2StencilState>,
        primitive_restart_enable: bool,
    ) {
        self.bind_raster_state(pipeline_id, depth, blend, blend_extra, raster,
                               topology, stencil, primitive_restart_enable);
    }

    fn bind_pipeline_vs_varying_bytes(
        &self,
        pipeline_id: ResourceId,
        bytes: u32,
    ) {
        self.pipeline_vs_varying_bytes.lock().unwrap()
            .insert(pipeline_id.raw(), bytes);
    }

    fn bind_pipeline_fs_implicit_lod(
        &self,
        pipeline_id: ResourceId,
        uses_implicit_lod: bool,
    ) {
        self.pipeline_fs_implicit_lod.lock().unwrap()
            .insert(pipeline_id.raw(), uses_implicit_lod);
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

/// Merge the pipeline-static stencil state with the per-
/// cmdbuf dynamic overrides (test enable + per-face ops /
/// masks / reference) and return the effective per-draw
/// stencil state.  `None` ⇒ no stencil test runs at all
/// (either the pipeline omitted the depth-stencil block AND
/// the cmdbuf didn't ask for stencil, or the dynamic
/// override explicitly disabled the test).
fn compute_effective_stencil(state: &PassState) -> Option<StencilState> {
    let raster   = state.raster?;
    let test_on  = state.stencil_test_enable_override
        .unwrap_or(raster.stencil_test_enable);
    if !test_on { return None; }
    // Default face state when the pipeline didn't supply
    // one but the dynamic override turned the test on.
    let base = raster.stencil.unwrap_or(StencilState {
        front: StencilFaceState {
            fail_op: StencilOp::Keep,
            pass_op: StencilOp::Keep,
            depth_fail_op: StencilOp::Keep,
            compare_op: CompareOp::Always,
            compare_mask: 0xff,
            write_mask:   0xff,
            reference:    0,
        },
        back: StencilFaceState {
            fail_op: StencilOp::Keep,
            pass_op: StencilOp::Keep,
            depth_fail_op: StencilOp::Keep,
            compare_op: CompareOp::Always,
            compare_mask: 0xff,
            write_mask:   0xff,
            reference:    0,
        },
    });
    Some(StencilState {
        front: state.stencil_front_override.apply(base.front),
        back:  state.stencil_back_override.apply(base.back),
    })
}

/// VkStencilOp wire value -> daemon-local StencilOp.
fn decode_stencil_op(v: u32) -> StencilOp {
    match v {
        0 => StencilOp::Keep,
        1 => StencilOp::Zero,
        2 => StencilOp::Replace,
        3 => StencilOp::IncrementAndClamp,
        4 => StencilOp::DecrementAndClamp,
        5 => StencilOp::Invert,
        6 => StencilOp::IncrementAndWrap,
        7 => StencilOp::DecrementAndWrap,
        _ => StencilOp::Keep,
    }
}

/// VkCompareOp wire value -> daemon-local CompareOp.
fn decode_compare_op(v: u32) -> CompareOp {
    match v {
        0 => CompareOp::Never,
        1 => CompareOp::Less,
        2 => CompareOp::Equal,
        3 => CompareOp::LessOrEqual,
        4 => CompareOp::Greater,
        5 => CompareOp::NotEqual,
        6 => CompareOp::GreaterOrEqual,
        7 => CompareOp::Always,
        _ => CompareOp::Less,
    }
}

/// Fill an RGBA8 pixel buffer with a single colour (used for
/// BeginRenderPass colour clears).
fn fill_rgba8(pixels: &mut [u8], rgba: [u8; 4]) {
    for px in pixels.chunks_exact_mut(4) {
        px.copy_from_slice(&rgba);
    }
}

fn convert_stencil_op(o: aqueduct_gpu::Tier2StencilOp) -> StencilOp {
    use aqueduct_gpu::Tier2StencilOp as W;
    match o {
        W::Keep              => StencilOp::Keep,
        W::Zero              => StencilOp::Zero,
        W::Replace           => StencilOp::Replace,
        W::IncrementAndClamp => StencilOp::IncrementAndClamp,
        W::DecrementAndClamp => StencilOp::DecrementAndClamp,
        W::Invert            => StencilOp::Invert,
        W::IncrementAndWrap  => StencilOp::IncrementAndWrap,
        W::DecrementAndWrap  => StencilOp::DecrementAndWrap,
    }
}

fn convert_stencil_face(s: aqueduct_gpu::Tier2StencilOpState) -> StencilFaceState {
    StencilFaceState {
        fail_op:       convert_stencil_op(s.fail_op),
        pass_op:       convert_stencil_op(s.pass_op),
        depth_fail_op: convert_stencil_op(s.depth_fail_op),
        compare_op:    convert_compare_op(s.compare_op),
        // Vulkan stencil masks / reference are u32 on the
        // wire but tier-2 stencil is u8 per pixel; truncate.
        compare_mask:  s.compare_mask as u8,
        write_mask:    s.write_mask   as u8,
        reference:     s.reference    as u8,
    }
}

fn convert_compare_op(o: aqueduct_gpu::Tier2CompareOp) -> CompareOp {
    use aqueduct_gpu::Tier2CompareOp as W;
    match o {
        W::Never          => CompareOp::Never,
        W::Less           => CompareOp::Less,
        W::Equal          => CompareOp::Equal,
        W::LessOrEqual    => CompareOp::LessOrEqual,
        W::Greater        => CompareOp::Greater,
        W::NotEqual       => CompareOp::NotEqual,
        W::GreaterOrEqual => CompareOp::GreaterOrEqual,
        W::Always         => CompareOp::Always,
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
