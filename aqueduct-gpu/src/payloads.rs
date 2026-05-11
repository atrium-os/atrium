//! Typed payloads for each `OP_GPU_*` envelope op.
//!
//! All payloads serialise via postcard, matching `fresco-protocol`'s
//! convention. Wire encoding is deterministic; field order in these
//! structs is the field order on the wire.
//!
//! Phase 1 covers the ops needed for vestibulum to render end-to-end:
//! handshake, memory regions, images/buffers, samplers, shaders,
//! pipelines, fences, frame submit, fence wait. Bundle ops and
//! surface-share are wire-defined but their full schemas land in
//! Phase 2 alongside the Vulkan ICD work.
//!
//! See `docs/spec/aqueduct-gpu.md` §4 for the wire format and §5
//! for the in-frame command stream encoded inside `SubmitFramePayload`.

use serde::{Deserialize, Serialize};

use crate::backends::BackendId;
use crate::ids::ResourceId;

// ───── Handshake ──────────────────────────────────────────────────

/// Client → server: initial handshake. The first envelope on every
/// connection. Carries the protocol version the client wants and the
/// kind of consumer (frescod renderer vs Vulkan ICD vs other) so the
/// host can apply appropriate defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandshakePayload {
    /// Client-supported protocol version. Currently the only value
    /// is 1; the wire spec lives in `aqueduct-gpu.md` and bumps in
    /// lockstep with any breaking change.
    pub protocol_version: u32,
    /// Type of consumer attaching. The host may apply different
    /// resource quotas / sandbox defaults depending on the kind.
    pub client_kind: ClientKind,
}

/// What kind of aqueduct-gpu client is connecting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum ClientKind {
    /// frescod's compositor renderer.
    FrescodRenderer = 1,
    /// atrium-vk-icd serving a Vulkan app.
    VulkanIcd       = 2,
    /// Bundle dispatcher (third-party scene-graph extension).
    BundleDispatch  = 3,
    /// Debug/diagnostic tool — no guarantees about resource quotas.
    DebugTool       = 4,
}

/// Server → client: handshake response. Communicates which backend
/// is in use, what protocol version the host speaks, and the
/// format-rules table (whose layout is opaque at this layer — clients
/// either know it from their build or query specific entries via
/// follow-up ops).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandshakeResponse {
    /// Negotiated protocol version (== min(client, server)).
    pub protocol_version: u32,
    /// The backend the host endpoint will execute commands on.
    pub backend: BackendId,
    /// Capability flags advertised by the host. Bit set = supported.
    /// Bit assignments live in this struct's `caps_*` constants.
    pub caps: u64,
    /// Maximum frame command stream size in bytes. Clients clamp
    /// their FrameBuilder against this.
    pub max_frame_bytes: u32,
    /// Maximum number of concurrent in-flight fences.
    pub max_fences_inflight: u32,
}

impl HandshakeResponse {
    /// Host supports `OP_GPU_BUNDLE_LOAD` (third-party bundles).
    pub const CAPS_BUNDLE_LOAD:     u64 = 1 << 0;
    /// Host supports `OP_GPU_SHARE_SURFACE` (compositor present).
    pub const CAPS_SHARE_SURFACE:   u64 = 1 << 1;
    /// Host supports compute shaders.
    pub const CAPS_COMPUTE:         u64 = 1 << 2;
    /// Host supports SPIR-V upload (the cold path). Hosts that
    /// require AOT-only can clear this bit.
    pub const CAPS_SPIRV_UPLOAD:    u64 = 1 << 3;
    /// Host supports surface-share with the compositor.
    /// (Bring-up: yes; thin-client/headless host: no.)
    pub const CAPS_COMPOSITION:     u64 = 1 << 4;

    // ─── Tier-1 software backend capabilities ─────────────────────
    //
    // Bits set by SoftwareBackend (`aqueduct-gpu-host`) to advertise
    // which Atrium-native bundle ops it can rasterise on CPU via
    // tiny-skia. See `docs/spec/aqueduct-gpu.md` §6.5. Clients use
    // these to gate fallback-vs-refuse decisions when running on a
    // SW host. Tier-2 (general SW Vulkan) is a separate capability
    // that no current backend advertises.

    /// SoftwareBackend can rasterise atrium-core's rect op.
    pub const CAPS_TIER1_RECT:      u64 = 1 << 8;
    /// SoftwareBackend can rasterise atrium-text's glyph_run op.
    pub const CAPS_TIER1_TEXT:      u64 = 1 << 9;
    /// SoftwareBackend can rasterise atrium-core's textured-rect op.
    pub const CAPS_TIER1_TEXTURE:   u64 = 1 << 10;
    /// SoftwareBackend can rasterise atrium-core's path op.
    pub const CAPS_TIER1_PATH:      u64 = 1 << 11;
}

// ───── Memory regions ─────────────────────────────────────────────

/// Client → server: allocate a shared-memory region.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryCreatePayload {
    /// Client-assigned ID for this region.
    pub region_id: ResourceId,
    /// Size in bytes; will be rounded up to host page size.
    pub size:      u64,
    /// Intended use; influences allocator choice host-side.
    pub usage:     MemoryUsage,
}

/// Server → client: response to memory create — carries the import
/// token the kmod uses to expose the region to userspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryCreateResponse {
    /// Echoes the client's pre-assigned region_id.
    pub region_id: ResourceId,
    /// Page-aligned actual size (may be larger than requested).
    pub size: u64,
    /// Hint for VA mapping stability. The kmod is free to ignore it.
    pub host_va_hint: u64,
    /// Opaque token the guest kmod resolves via `IOC_GPU_IMPORT_REGION`.
    pub atrium_gpu_token: [u8; 32],
}

/// What category of memory this region is for. Influences whether
/// the host endpoint allocates from a GPU-visible heap, a CPU-coherent
/// heap, etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum MemoryUsage {
    /// Vertex/index/uniform/storage buffer backing.
    BufferBacking = 1,
    /// Image (texture / render target) backing.
    ImageBacking  = 2,
    /// Staging / upload buffer (CPU-mutable, GPU-readable).
    Staging       = 3,
    /// Scanout target — restricted via Portcullis `gpu.scanout` cap.
    /// See `docs/spec/aqueduct-gpu.md` §12.4.
    Scanout       = 4,
}

/// Destroy a region. After this envelope is processed, the region_id
/// is invalid; mapping it via the kmod fails with `ENOENT`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryDestroyPayload {
    /// The region to release.
    pub region_id: ResourceId,
}

// ───── Images ─────────────────────────────────────────────────────

/// Client → server: create an image. ID pre-assigned; no response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageCreatePayload {
    /// Client-assigned ID.
    pub image_id: ResourceId,
    /// Memory region backing this image. Region must be `ImageBacking`
    /// or `Scanout`; offset within the region is `region_offset`.
    pub backing_region: ResourceId,
    /// Byte offset within the backing region.
    pub region_offset: u64,
    /// Pixel format. Encoded as the Vulkan VkFormat value for
    /// portability; the host endpoint maps to its backend's
    /// equivalent (MTLPixelFormat etc.).
    pub format: u32,
    /// Image extent.
    pub width: u32,
    /// Image extent.
    pub height: u32,
    /// Image extent (1 for 2D, >1 for 3D).
    pub depth: u32,
    /// Mipmap level count (1 = no mips).
    pub mip_levels: u32,
    /// Array layer count (1 = non-array, 6 = cubemap).
    pub array_layers: u32,
    /// Image usage flags. Encoded as VkImageUsageFlags bitset for
    /// portability.
    pub usage: u32,
}

/// Destroy an image.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageDestroyPayload {
    /// The image to release.
    pub image_id: ResourceId,
}

// ───── Buffers ────────────────────────────────────────────────────

/// Client → server: create a buffer. ID pre-assigned; no response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BufferCreatePayload {
    /// Client-assigned ID.
    pub buffer_id: ResourceId,
    /// Memory region backing this buffer.
    pub backing_region: ResourceId,
    /// Byte offset within the backing region.
    pub region_offset: u64,
    /// Buffer size in bytes.
    pub size: u64,
    /// Usage flags. Encoded as VkBufferUsageFlags bitset.
    pub usage: u32,
}

/// Destroy a buffer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BufferDestroyPayload {
    /// The buffer to release.
    pub buffer_id: ResourceId,
}

// ───── Samplers ───────────────────────────────────────────────────

/// Client → server: create a sampler. ID pre-assigned; no response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SamplerCreatePayload {
    /// Client-assigned ID.
    pub sampler_id: ResourceId,
    /// Min/mag filter modes. Encoded as VkFilter values.
    pub min_filter: u8,
    /// Min/mag filter modes.
    pub mag_filter: u8,
    /// Mipmap filter mode. Encoded as VkSamplerMipmapMode.
    pub mip_filter: u8,
    /// U/V/W address modes. Encoded as VkSamplerAddressMode each.
    pub address_modes: [u8; 3],
    /// Anisotropy level (0 = disabled).
    pub max_anisotropy: f32,
    /// Mip level clamps.
    pub min_lod: f32,
    /// Mip level clamps.
    pub max_lod: f32,
}

/// Destroy a sampler.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SamplerDestroyPayload {
    /// The sampler to release.
    pub sampler_id: ResourceId,
}

// ───── Shaders ────────────────────────────────────────────────────

/// Client → server: resolve a shader by content hash + target backend.
/// Returns immediately with a shader_id on cache hit; on miss the
/// response carries `ShaderResolveStatus::NotCached`, prompting the
/// client to follow up with `OP_GPU_SHADER_UPLOAD`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShaderResolvePayload {
    /// SHA-256 of the shader bytecode (SPIR-V or NIR).
    pub bytecode_hash: [u8; 32],
    /// Bytecode encoding.
    pub kind: ShaderKind,
    /// Target backend (matches the handshake's reported backend
    /// except in rare cross-backend resolves; defaults to the
    /// connection's negotiated backend).
    pub backend: BackendId,
}

/// Server → client: resolve response. ID is `None` on cache miss.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShaderResolveResponse {
    /// Echoes the requested hash for client correlation.
    pub bytecode_hash: [u8; 32],
    /// Status of the resolve attempt.
    pub status: ShaderResolveStatus,
    /// On Hit, the resolved shader_id the client uses in
    /// subsequent `OP_GPU_PIPELINE_CREATE`. Unused on Miss.
    pub shader_id: Option<ResourceId>,
}

/// Whether the host had a cached compiled binary for this hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum ShaderResolveStatus {
    /// Host has the compiled binary; `shader_id` is valid.
    Hit  = 1,
    /// Host doesn't have it. Client should follow up with
    /// `OP_GPU_SHADER_UPLOAD` carrying the bytecode.
    Miss = 2,
}

/// Bytecode encoding for shader upload / resolve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum ShaderKind {
    /// Khronos SPIR-V bytecode.
    SpirV = 1,
    /// Mesa NIR (serialised form). Faster on the host side
    /// because no SPIR-V → NIR pass is required.
    Nir   = 2,
}

/// Client → server: upload bytecode for a previously-missed shader.
/// Slow path; should be rare for shipped apps because atrium-pkg's
/// install hook pre-warms via Tessera.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShaderUploadPayload {
    /// SHA-256 of the bytecode. Host verifies on receipt.
    pub bytecode_hash: [u8; 32],
    /// Encoding.
    pub kind: ShaderKind,
    /// Target backend.
    pub backend: BackendId,
    /// The bytecode itself, inline.
    pub bytecode: Vec<u8>,
}

/// Server → client: upload result. Carries the resolved shader_id
/// on success, or a sandbox-rejection diagnostic on failure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShaderUploadResponse {
    /// Echoes the requested hash.
    pub bytecode_hash: [u8; 32],
    /// Resolved shader_id, or `None` if compilation/sandbox failed.
    pub shader_id: Option<ResourceId>,
    /// Diagnostic message on failure (empty on success).
    pub diagnostic: String,
}

// ───── Pipelines ──────────────────────────────────────────────────

/// Client → server: create a pipeline. ID pre-assigned; no response.
/// Pipeline state is encoded inline; shader references are by ID.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineCreatePayload {
    /// Client-assigned ID.
    pub pipeline_id: ResourceId,
    /// Pipeline kind.
    pub kind: PipelineKind,
    /// Resolved shader IDs (from `OP_GPU_SHADER_RESOLVE`).
    /// For graphics pipelines this is `[vertex, fragment, ...]`;
    /// for compute, a single-element vector.
    pub shaders: Vec<ResourceId>,
    /// Opaque, backend-aware state blob — encoded by the client's
    /// pipeline-state encoder. The host endpoint understands this
    /// for its specific backend (e.g., MTLRenderPipelineDescriptor
    /// equivalent for MoltenVK).
    pub state_blob: Vec<u8>,
}

/// Pipeline kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum PipelineKind {
    /// Graphics pipeline (vertex + fragment, optionally + geometry/tess).
    Graphics = 1,
    /// Compute pipeline.
    Compute  = 2,
}

/// Destroy a pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineDestroyPayload {
    /// The pipeline to release.
    pub pipeline_id: ResourceId,
}

// ───── Fences ─────────────────────────────────────────────────────

/// Client → server: create a fence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FenceCreatePayload {
    /// Client-assigned ID.
    pub fence_id: ResourceId,
}

/// Destroy a fence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FenceDestroyPayload {
    /// The fence to release.
    pub fence_id: ResourceId,
}

// ───── Frame submit + wait ────────────────────────────────────────

/// Client → server: submit a frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitFramePayload {
    /// Fence to signal when the frame's commands complete.
    pub fence_id: ResourceId,
    /// Monotonic client-assigned ordering counter. Host uses it for
    /// in-order processing within a connection.
    pub timeline: u64,
    /// The packed frame command stream. Records framed by the
    /// `frame` module's [`FrameBuilder`].
    pub command_buf: Vec<u8>,
}

/// Client → server: wait for a fence with an optional timeout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitFencePayload {
    /// The fence to wait on.
    pub fence_id: ResourceId,
    /// Maximum wait time. 0 = poll, `u64::MAX` = block indefinitely.
    pub timeout_ns: u64,
}

/// Server → client: wait result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitFenceResponse {
    /// Echoes the queried fence_id.
    pub fence_id: ResourceId,
    /// `true` if the fence is signalled, `false` if the timeout
    /// expired with the fence still unsignalled.
    pub signalled: bool,
}

// ───── Surface share ──────────────────────────────────────────────

/// Client → server: hand frescod a reference to an image.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareSurfacePayload {
    /// The image to share.
    pub image_id: ResourceId,
    /// Tag identifying what the surface is for (debug / UX).
    pub purpose: String,
}

/// Server → client: share response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareSurfaceResponse {
    /// Token the client passes to frescod via fresco-protocol's
    /// `slot_set_texture` mechanism. The token is opaque to the
    /// client; frescod knows how to redeem it for the underlying
    /// image.
    pub share_token: [u8; 32],
}

// ───── Bundle lifecycle ───────────────────────────────────────────

/// Client → server: load a third-party bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleLoadPayload {
    /// CAS hash of the bundle manifest. Pre-warmed in Tessera by
    /// atrium-pkg's install hook; transitively references each
    /// declared shader hash.
    pub manifest_cas_hash: [u8; 32],
    /// Client-provided bundle name (for diagnostics).
    pub display_name: String,
}

/// Server → client: bundle load response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleLoadResponse {
    /// Echoes the manifest hash.
    pub manifest_cas_hash: [u8; 32],
    /// The namespace assigned to this bundle's resources (0x1..=0xE),
    /// or `None` if the load failed. Failures are surfaced via
    /// `OP_GPU_BUNDLE_LOAD_ERR` events first.
    pub bundle_namespace: Option<u8>,
}

/// Client → server: unload a bundle. All resources in the bundle's
/// namespace are immediately invalid after this op.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleUnloadPayload {
    /// The bundle's namespace tag (from `BundleLoadResponse`).
    pub bundle_namespace: u8,
}

// ───── Async events (server → client) ─────────────────────────────

/// Server → client: a fence is signalled. Sent out-of-band; clients
/// can poll via `OP_GPU_WAIT_FENCE` or listen for these.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FenceSignaledEvent {
    /// The signalled fence.
    pub fence_id: ResourceId,
    /// Timeline value of the frame that signalled this fence
    /// (matches `SubmitFramePayload::timeline`).
    pub timeline: u64,
}

/// Server → client: device-lost notification. All resources on
/// this connection are invalid; client must reconnect.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceLostEvent {
    /// Diagnostic explaining the cause.
    pub diagnostic: String,
}

/// Server → client: validation failure for a previously-submitted
/// fire-and-forget op. The offending op and resource are identified
/// so the ICD can map back to a Vulkan error return.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationErrEvent {
    /// The wire opcode (`OP_GPU_*`) that failed.
    pub opcode: u16,
    /// The resource ID involved, if any.
    pub resource_id: Option<ResourceId>,
    /// Diagnostic message.
    pub diagnostic: String,
}

/// Server → client: per-resource validation failure during
/// `OP_GPU_BUNDLE_LOAD`. The bundle load response itself may
/// still succeed if subsequent resources validate; clients aggregate
/// these events with the load response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleLoadErrEvent {
    /// The bundle being loaded (manifest hash).
    pub manifest_cas_hash: [u8; 32],
    /// The local bundle resource ID that failed validation.
    pub bundle_local_id: u32,
    /// Diagnostic.
    pub diagnostic: String,
}
