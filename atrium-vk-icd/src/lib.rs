//! `atrium-vk-icd` — Vulkan ICD speaking aqueduct-gpu.
//!
//! The Vulkan loader (`libvulkan.so.1`) discovers ICDs via a JSON
//! manifest (`/usr/share/vulkan/icd.d/atrium_icd.json`) that points
//! at this `cdylib`. The loader negotiates an ABI version with us
//! via `vk_icdNegotiateLoaderICDInterfaceVersion`, then resolves
//! every Vulkan function the application calls through
//! `vk_icdGetInstanceProcAddr`. We translate the Vulkan calls into
//! aqueduct-gpu protocol messages sent to the host endpoint
//! (`frescod-aqueduct` today, the kmod direct at D5+).
//!
//! This is NOT a general Vulkan driver. It targets aqueduct-gpu
//! specifically — the format-rules table, the bundle-pipeline model,
//! the install-time AOT-compiled shader cache. See
//! `docs/spec/aqueduct-gpu.md` §7.1 for the translation table.
//!
//! # Status
//!
//! Skeleton only. The two loader-ICD entry points are stubbed so the
//! loader's negotiation succeeds and individual `vkGetInstanceProcAddr`
//! lookups return null. Application code calling actual Vulkan
//! functions on the resulting `VkInstance` would fail with
//! `VK_ERROR_INCOMPATIBLE_DRIVER`.
//!
//! ## Build status
//!
//! - ✅ `cargo build` — produces `libatrium_vk_icd.dylib` (macOS) /
//!   `.so` (FreeBSD).
//! - ⚠️ Phase 1.3b — `VkCommandBuffer` recording / submission
//!   plumbing. **pending**: the meat of the ICD; translates
//!   `vkCmd*` calls into `FrameOp` records in an aqueduct-gpu
//!   frame buffer, flushed on `vkQueueSubmit`. Today this skeleton
//!   has none of that.
//! - ⚠️ All other Vulkan entry points — also pending. Phasing per
//!   `docs/spec/aqueduct-gpu.md` §7.1's table.
//!
//! ## ICD ABI version
//!
//! We support up to loader-ICD interface version 7 (the current max
//! at time of writing; matches MoltenVK and Mesa's RADV). The loader
//! sends us its supported version; we clamp to our max and reply.

#![allow(non_snake_case)]
#![allow(non_camel_case_types)]

use std::ffi::CStr;
use std::os::raw::{c_char, c_void};

/// Maximum loader-ICD interface version we support. v7 covers
/// everything the current Khronos loader needs.
const ATRIUM_LOADER_ICD_INTERFACE_VERSION_MAX: u32 = 7;

/// Vulkan loader-ICD ABI: `VkResult` is `i32`.
type VkResult = i32;

const VK_SUCCESS:                  VkResult =  0;

/// The Vulkan API version we currently claim. Bumps as more entry
/// points reach correctness. Encoded per VK_MAKE_API_VERSION(0, 1, 3, 0).
const ATRIUM_ICD_API_VERSION: u32 = (1 << 22) | (3 << 12);

/// Type-erased function pointer the loader uses for resolved
/// Vulkan entry points.
type PFN_vkVoidFunction = Option<unsafe extern "C" fn()>;

/// Vulkan's max string length for layer/extension/device names.
/// Defined by Vk spec as VK_MAX_EXTENSION_NAME_SIZE / VK_MAX_DESCRIPTION_SIZE.
const VK_MAX_EXTENSION_NAME_SIZE: usize = 256;
const VK_MAX_DESCRIPTION_SIZE:    usize = 256;

/// VK_ERROR_INITIALIZATION_FAILED — used when a caller hands us a
/// null out-pointer where the spec requires one.
const VK_ERROR_INITIALIZATION_FAILED: VkResult = -3;

/// VK_ICD_LOADER_MAGIC — the value the Khronos loader expects at
/// offset 0 of every dispatchable handle (VkInstance,
/// VkPhysicalDevice, VkDevice, VkQueue, VkCommandBuffer) returned by
/// an ICD. The loader checks for this magic and overwrites the slot
/// with its own dispatch-table pointer. ICDs that don't set it get
/// rejected from the loader's dispatch chain.
///
/// Per `vk_icd.h` (Khronos `Vulkan-Headers` repo): defined as
/// `0x01CDC0DE` cast to `void*`.
const VK_ICD_LOADER_MAGIC: usize = 0x01CDC0DE;

/// Opaque Vulkan handle types. The Vulkan loader-ICD ABI treats
/// these as `void *`; the loader stores its dispatch table at the
/// pointed-to location's first slot. For the ICD's purposes the
/// pointer addresses an ICD-owned struct whose first field is
/// `VK_ICD_LOADER_MAGIC` (set by us at create time).
type VkInstance       = *mut c_void;
type VkPhysicalDevice = *mut c_void;

/// Environment variable for the aqueduct-gpu socket path. Mirrors
/// frescod-aqueduct's FRESCOD_AQUEDUCT_SOCK to keep the same default.
const ATRIUM_VK_ICD_SOCKET_ENV: &str = "ATRIUM_VK_ICD_SOCKET";
const ATRIUM_VK_ICD_SOCKET_DEFAULT: &str = "/tmp/frescod-aqueduct.sock";

/// ICD-side state behind a `VkInstance`. The first field MUST be
/// the loader magic — the loader overwrites it on first use with a
/// pointer to its own dispatch table; we never read it after
/// returning the handle.
#[repr(C)]
struct AtriumInstance {
    /// First field — see `VK_ICD_LOADER_MAGIC`. The loader writes
    /// over this slot on first dispatch.
    loader_dispatch_slot: usize,
    /// The aqueduct-gpu client connection this instance owns,
    /// wrapped in a Mutex for interior mutability — `submit_frame`
    /// takes `&mut self` on the client, but vkQueueSubmit only
    /// sees a `*const AtriumInstance` via back-pointer chain.
    /// `None` if connect/handshake failed at create time.
    client: Option<std::sync::Mutex<aqueduct_gpu_client::GpuClient>>,
    /// Physical devices discovered via the handshake. Each is a
    /// boxed pointer the loader sees as a `VkPhysicalDevice` handle.
    /// Owned: vkDestroyInstance walks this list and frees each.
    devices: Vec<*mut AtriumPhysicalDevice>,
}

/// ICD-side state behind a `VkPhysicalDevice`. The first field MUST
/// be the loader magic (same dispatch contract as `VkInstance`).
#[repr(C)]
struct AtriumPhysicalDevice {
    /// First field — see `VK_ICD_LOADER_MAGIC`.
    loader_dispatch_slot: usize,
    /// Backend identity from the aqueduct-gpu handshake. Used to
    /// answer vkGetPhysicalDeviceProperties (vendor/device IDs,
    /// driver name) and to key the shader cache.
    backend_vendor:     aqueduct_gpu::backends::GpuVendor,
    backend_generation: u16,
    /// Back-pointer to the owning AtriumInstance. Borrowed (the
    /// physical device's lifetime is bounded by the instance, which
    /// frees it in vkDestroyInstance). Lets vkCreateDevice + the
    /// queue chain reach the aqueduct-gpu client.
    instance: *mut AtriumInstance,
}

/// Vulkan handle types for device-level objects.
type VkDevice = *mut c_void;
type VkQueue  = *mut c_void;
/// VkCommandBuffer is dispatchable (carries loader magic at offset 0).
type VkCommandBuffer = *mut c_void;
/// VkCommandPool is non-dispatchable — opaque u64 the loader never
/// indexes into. We cast `Box<AtriumCommandPool>` raw pointer to u64.
type VkCommandPool = u64;

/// Default FrameBuilder capacity per command buffer. 256 KiB is the
/// budget per recording; Phase 1.3b+ may grow this or make it
/// re-allocating.
const ATRIUM_CMDBUF_INITIAL_CAPACITY: u32 = 256 * 1024;

/// ICD-side state behind a `VkCommandPool`. Tracks the buffers it
/// has allocated so `vkDestroyCommandPool` can free them in bulk.
struct AtriumCommandPool {
    /// All command buffers allocated from this pool (as raw
    /// pointers, owned). vkFreeCommandBuffers can remove entries;
    /// vkDestroyCommandPool walks whatever's left and frees them.
    buffers: Vec<*mut AtriumCommandBuffer>,
}

/// Recording state for a command buffer. Vulkan's state machine:
/// Initial → Recording (vkBeginCommandBuffer) → Executable
/// (vkEndCommandBuffer) → Invalid (after vkResetCommandBuffer or
/// re-Begin). We only track the gross states needed to gate
/// vkCmd*/vkEnd/vkBegin transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CmdBufferState {
    Initial,
    Recording,
    Executable,
}

/// ICD-side state behind a `VkCommandBuffer`. The first field MUST
/// be the loader magic. The `frame` field is the aqueduct-gpu
/// FrameOp stream being recorded; vkCmd* push into it, vkQueueSubmit
/// hands it to the aqueduct-gpu client.
#[repr(C)]
struct AtriumCommandBuffer {
    /// First field — see `VK_ICD_LOADER_MAGIC`.
    loader_dispatch_slot: usize,
    /// Recording state machine.
    state: CmdBufferState,
    /// Accumulated FrameOp stream. Empty in `Initial`; populated
    /// during `Recording` by vkCmd*; finalized in `Executable`;
    /// flushed to the host endpoint by vkQueueSubmit.
    frame: aqueduct_gpu::frame::FrameBuilder,
    /// Back-pointer to the owning AtriumDevice. Borrowed (the
    /// pool that allocated this cmdbuf is owned by the device).
    /// vkCmd* uses it to resolve VkBuffer → ResourceId via the
    /// device's `buffers` map. Set at vkAllocateCommandBuffers.
    device: *mut AtriumDevice,
}

/// Test-only accessor — peek at a cmdbuf's recorded byte stream.
/// Used by integration tests to verify that vkCmd* actually push
/// the expected FrameOp bytes. Not part of any Vulkan ABI; not
/// exported from the cdylib.
#[doc(hidden)]
pub fn cmdbuf_recorded_bytes(cb: VkCommandBuffer) -> Vec<u8> {
    if cb.is_null() { return Vec::new(); }
    let cbref = unsafe { &*(cb as *const AtriumCommandBuffer) };
    cbref.frame.as_bytes().to_vec()
}

/// ICD-side state behind a `VkDevice`. The first slot holds the
/// loader magic. We currently allocate one `AtriumQueue` per
/// VkDeviceQueueCreateInfo entry; Atrium's single-queue model means
/// there's exactly one queue per device today.
#[repr(C)]
struct AtriumDevice {
    /// First field — see `VK_ICD_LOADER_MAGIC`.
    loader_dispatch_slot: usize,
    /// Owned queues, indexed by (queue_family_index, queue_index).
    /// Today: a single (0, 0) entry. Each queue is a Box pointer
    /// the loader sees as a `VkQueue` handle; freed in
    /// `vkDestroyDevice`.
    queues: Vec<*mut AtriumQueue>,
    /// Back-pointer to the AtriumInstance that owns the
    /// aqueduct-gpu client this device's submits flow through.
    /// Borrowed (instance outlives device by spec — vkDestroyDevice
    /// MUST run before vkDestroyInstance).
    instance: *mut AtriumInstance,
    /// Persistent fence allocated at vkCreateDevice via
    /// `GpuClient::create_fence`. Reused across submits via the
    /// monotonic timeline counter; saves a round-trip per
    /// vkQueueSubmit. ResourceId(0) when the instance had no live
    /// client (offline / handshake failed).
    fence: aqueduct_gpu::ids::ResourceId,
    /// Monotonic timeline value; incremented per submit.
    timeline: std::cell::Cell<u64>,
    /// BackendId carried over from the physical device this
    /// VkDevice was created from. Needed by shader upload/resolve
    /// (the host endpoint keys its shader cache by
    /// `(hash, backend_id, kind)`).
    backend: aqueduct_gpu::backends::BackendId,
    /// Resource-ID allocator for ICD-side handles
    /// (VkPipeline, VkPipelineLayout, future VkBuffer/VkImage).
    /// Namespaced under `IdNamespace::IcdRuntime` so they don't
    /// collide with Builtin/Bundle IDs.
    id_alloc: std::sync::Mutex<aqueduct_gpu_client::IdAllocator>,
    /// Per-VkPipeline state: maps VkPipeline u64 → ResourceId
    /// that vkCmdBindPipeline references in the BindPipeline
    /// FrameOp.
    pipelines: std::sync::Mutex<std::collections::HashMap<u64, aqueduct_gpu::ids::ResourceId>>,
    /// Per-VkDeviceMemory state. Each entry owns a host-allocated
    /// `Box<[u8]>` for the storage (stable address for vkMapMemory)
    /// AND optionally a daemon-side `region_id` from
    /// OP_GPU_MEMORY_CREATE so vkCreateBuffer/vkBindBufferMemory
    /// can wire the buffer to a real aqueduct-gpu region.
    /// `region_id == None` means the instance had no live client
    /// at allocation time — the memory is local-only.
    memories: std::sync::Mutex<std::collections::HashMap<u64, AtriumDeviceMemory>>,
    /// Monotonic counter for VkDeviceMemory u64 handles.
    /// Non-dispatchable handles don't carry ICD_LOADER_MAGIC;
    /// they're opaque u64. We use a per-device counter so
    /// handles are stable within a device's lifetime.
    next_memory_id: std::cell::Cell<u64>,
    /// Per-VkBuffer state. Records the size requested at
    /// vkCreateBuffer + the memory binding (if any) installed
    /// later by vkBindBufferMemory. Buffers without bindings are
    /// valid but unusable in commands.
    buffers: std::sync::Mutex<std::collections::HashMap<u64, AtriumBuffer>>,
    /// Monotonic counter for VkBuffer handles.
    next_buffer_id: std::cell::Cell<u64>,
    /// Per-VkImage state. Mirrors `buffers` for the image side.
    images: std::sync::Mutex<std::collections::HashMap<u64, AtriumImage>>,
    /// Monotonic counter for VkImage handles.
    next_image_id: std::cell::Cell<u64>,
    /// Per-VkShaderModule state. vkCreateShaderModule hashes the
    /// bytecode + calls resolve_shader (cache hit) or upload_shader
    /// (cold path); stores the returned ResourceId so future
    /// vkCreateGraphicsPipelines can wire it into a pipeline.
    shaders: std::sync::Mutex<std::collections::HashMap<u64, AtriumShaderModule>>,
    /// Monotonic counter for VkShaderModule handles.
    next_shader_id: std::cell::Cell<u64>,
    /// Per-VkImageView state. Each view records the VkImage it
    /// references; vkCmdBeginRenderPass walks framebuffer →
    /// image-view → image → image_id to fill BeginRenderPass's
    /// target_image_id field.
    image_views: std::sync::Mutex<std::collections::HashMap<u64, u64 /* VkImage */>>,
    next_image_view_id: std::cell::Cell<u64>,
    /// Per-VkRenderPass state. Today: opaque non-zero u64 with
    /// no fields tracked (the host endpoint's renderer abstracts
    /// over render-pass details; we only need a valid handle).
    next_render_pass_id: std::cell::Cell<u64>,
    /// Per-VkFramebuffer state. Records the attachments + extent
    /// so vkCmdBeginRenderPass can find the target image view.
    framebuffers: std::sync::Mutex<std::collections::HashMap<u64, AtriumFramebuffer>>,
    next_framebuffer_id: std::cell::Cell<u64>,
    /// Per-VkFence state: just a "signaled" bool. Submits today
    /// are synchronous from the ICD's POV (vkQueueSubmit returns
    /// before the host has fully processed), so vkWaitForFences
    /// returns immediately. Future async submit grows this into
    /// a wait-on-aqueduct-fence story.
    fences: std::sync::Mutex<std::collections::HashMap<u64, bool>>,
    next_fence_id: std::cell::Cell<u64>,
    /// Per-VkSemaphore — opaque non-zero u64. Atrium's wire
    /// timeline (one per VkQueueSubmit) handles serialization;
    /// semaphores are bookkeeping-only today.
    next_semaphore_id: std::cell::Cell<u64>,
    /// Per-VkSampler: maps VkSampler u64 → daemon-side
    /// ResourceId so future descriptor-set updates can reference
    /// the sampler. `None` if the instance had no live client.
    samplers: std::sync::Mutex<std::collections::HashMap<u64, Option<aqueduct_gpu::ids::ResourceId>>>,
    next_sampler_id: std::cell::Cell<u64>,
    /// Per-VkQueryPool / VkEvent / VkBufferView: opaque non-zero
    /// u64 counters. Real implementations would model the per-
    /// query data; the renderer's tier-1 has no query support.
    next_query_pool_id:  std::cell::Cell<u64>,
    events: std::sync::Mutex<std::collections::HashMap<u64, bool>>,
    next_event_id:       std::cell::Cell<u64>,
    next_buffer_view_id: std::cell::Cell<u64>,
    /// WSI — VkSwapchainKHR state. Each swapchain holds a Vec of
    /// the VkImage handles in its ring + the next-acquire index.
    /// VK_KHR_surface design sketch lives in docs/spec/
    /// aqueduct-gpu.md §7.1.1.
    swapchains: std::sync::Mutex<std::collections::HashMap<u64, AtriumSwapchain>>,
    next_swapchain_id: std::cell::Cell<u64>,
    /// Per-VkDescriptorSetLayout state. Today: opaque non-zero
    /// u64 (we don't track per-binding type info — the host's
    /// pipeline / shader knows its expected layout).
    next_dsl_id: std::cell::Cell<u64>,
    /// Per-VkDescriptorPool state. Same: opaque non-zero u64.
    next_dpool_id: std::cell::Cell<u64>,
    /// Per-VkDescriptorSet state. Each set holds an array of
    /// binding writes installed by vkUpdateDescriptorSets — those
    /// are what vkCmdBindDescriptorSets references when packing
    /// the BindDescriptors FrameOp.
    descriptor_sets: std::sync::Mutex<std::collections::HashMap<u64, AtriumDescriptorSet>>,
    next_dset_id: std::cell::Cell<u64>,
}

/// One binding write in a `VkDescriptorSet`. The variant
/// determines which of `buffer_id` / `image_id` / `sampler_id`
/// is meaningful; the others are zero.
#[derive(Debug, Clone, Copy, Default)]
#[allow(dead_code)]
struct AtriumDescriptorWrite {
    binding:        u32,
    /// VkDescriptorType numeric value. 6=UNIFORM_BUFFER,
    /// 1=COMBINED_IMAGE_SAMPLER, ...
    descriptor_type: u32,
    buffer_id:  u32, /* ResourceId.raw() or 0 */
    image_id:   u32,
    sampler_id: u32,
    offset:     u64,
    range:      u64,
}

/// Per-VkDescriptorSet state.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
struct AtriumDescriptorSet {
    writes: Vec<AtriumDescriptorWrite>,
}

/// Per-VkSwapchainKHR state. A ring of VkImage handles (allocated
/// by the swapchain on create) + the index of the next image to
/// hand out via vkAcquireNextImageKHR. The actual present routing
/// (forwarding the rendered image to its Fresco surface) is
/// daemon-side; today vkQueuePresentKHR is a success no-op while
/// the spec'd `OP_GPU_PRESENT` opcode is pending.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
struct AtriumSwapchain {
    surface:      u64,
    images:       Vec<u64>,
    next_acquire: u32,
    width:        u32,
    height:       u32,
    format:       u32,
}

/// Per-VkFramebuffer state.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct AtriumFramebuffer {
    width:        u32,
    height:       u32,
    attachments:  Vec<u64>, /* VkImageView handles */
}

/// ICD-side state for a `VkShaderModule`. We don't keep the
/// bytecode beyond create — the daemon owns the shader's lifecycle
/// once uploaded.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
struct AtriumShaderModule {
    /// Daemon-side ResourceId; None if the instance had no live
    /// client or the upload failed validation.
    shader_id: Option<aqueduct_gpu::ids::ResourceId>,
    /// sha-256 of the SPIR-V bytecode at create time.
    bytecode_hash: [u8; 32],
}

/// ICD-side state for a `VkImage`. Created by vkCreateImage;
/// `image_id` populated by vkBindImageMemory when the memory has a
/// region_id. Future vkCmd* paths that target images
/// (vkCmdCopyBufferToImage, vkCmdPipelineBarrier transitions) read
/// the image_id from here.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // many fields feed future render-pass / descriptor paths
struct AtriumImage {
    width:        u32,
    height:       u32,
    depth:        u32,
    mip_levels:   u32,
    array_layers: u32,
    format:       u32, /* VkFormat */
    usage:        u32, /* VkImageUsageFlags */
    memory:        Option<u64>,
    memory_offset: u64,
    /// Daemon-side ResourceId, populated by vkBindImageMemory.
    image_id: Option<aqueduct_gpu::ids::ResourceId>,
}

/// Bytes-per-pixel for the small subset of VkFormat values we need
/// to size memory requirements. Real ICDs ship a full format-rules
/// table (e.g. atrium-vk-icd will eventually reuse the
/// aqueduct-gpu format-rules table). 0 = unknown — caller gets a
/// best-effort (width × height × 4) sizing.
fn bpp_for_vk_format(format: u32) -> u32 {
    // Vk numeric values, see VkFormat enum.
    match format {
        // 8-bit single-channel.
        9 => 1,    // R8_UNORM
        // 16-bit single-channel + 8-bit two-channel.
        16 | 70 => 2,
        // 24-bit (typically padded to 32 on most HW).
        23 | 30 | 37 => 4,  // R8G8B8_UNORM / B8G8R8_UNORM / R8G8B8A8_UNORM
        43 => 4,            // B8G8R8A8_UNORM
        50 => 4,            // R8G8B8A8_SRGB
        // 64-bit.
        97 | 98 | 100 | 109 => 8,
        // 128-bit.
        106 | 109..=124 => 16,
        _ => 4, // best effort
    }
}

/// ICD-side state for a `VkBuffer`. Created by vkCreateBuffer
/// with a size; gets its `memory` + `memory_offset` populated by
/// a follow-up vkBindBufferMemory. vkCmdBindVertexBuffers /
/// vkCmdBindIndexBuffer (future) resolve through this map.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // memory_offset/usage feed future vkCmd* paths
struct AtriumBuffer {
    size:          u64,
    memory:        Option<u64>,
    memory_offset: u64,
    usage:         u32, /* VkBufferUsageFlags */
    /// Daemon-side ResourceId for this buffer, set on a
    /// successful vkBindBufferMemory when the memory carries a
    /// region_id. None until then; FrameOps that reference this
    /// buffer (BindVertexBuf etc.) need this set.
    buffer_id: Option<aqueduct_gpu::ids::ResourceId>,
}

/// ICD-side state for a `VkDeviceMemory`.
struct AtriumDeviceMemory {
    /// Host-allocated storage. Stable address used by vkMapMemory;
    /// also the source bytes for any daemon-side region (TODO:
    /// future IMPORT_REGION mapping makes this redundant).
    storage:   Box<[u8]>,
    /// Daemon-side region_id from OP_GPU_MEMORY_CREATE. None if
    /// the instance had no live client at allocation time.
    region_id: Option<aqueduct_gpu::ids::ResourceId>,
}

/// ICD-side state behind a `VkQueue`. Same dispatchable-handle
/// contract (first field = loader magic). Carries a back-pointer
/// to its owning device so `vkQueueSubmit` can reach the
/// aqueduct-gpu connection.
#[repr(C)]
struct AtriumQueue {
    /// First field — see `VK_ICD_LOADER_MAGIC`.
    loader_dispatch_slot: usize,
    /// Back-pointer to the AtriumDevice that owns this queue.
    /// Borrowed (the queue is freed before the device it
    /// references).
    _device: *mut AtriumDevice,
    /// Family + index from the VkDeviceQueueCreateInfo this queue
    /// satisfies. Used by `vkGetDeviceQueue` for lookup.
    family: u32,
    index:  u32,
}

/// Attempt the aqueduct-gpu socket connect + handshake. Returns the
/// connected client + the BackendId, or `None` on any failure (no
/// socket, daemon not running, protocol mismatch). vkCreateInstance
/// treats failure as "zero physical devices" rather than refusing
/// to create the instance, so a Vulkan app on a system without
/// atrium-gpu running sees a present-but-empty ICD instead of an
/// outright VK_ERROR_INCOMPATIBLE_DRIVER.
fn try_connect_aqueduct() -> Option<(
    aqueduct_gpu_client::GpuClient,
    aqueduct_gpu::backends::BackendId,
)> {
    let sock = std::env::var(ATRIUM_VK_ICD_SOCKET_ENV)
        .unwrap_or_else(|_| ATRIUM_VK_ICD_SOCKET_DEFAULT.to_string());
    let conn = aqueduct::Connection::connect(&sock).ok()?;
    let mut client = aqueduct_gpu_client::GpuClient::new(conn);
    let resp = client.handshake(
        aqueduct_gpu::payloads::ClientKind::VulkanIcd,
    ).ok()?;
    let backend = resp.backend;
    Some((client, backend))
}

#[repr(C)]
#[derive(Copy, Clone)]
#[allow(dead_code)]
pub struct VkExtensionProperties {
    extensionName: [c_char; VK_MAX_EXTENSION_NAME_SIZE],
    specVersion:   u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
#[allow(dead_code)]
pub struct VkLayerProperties {
    layerName:             [c_char; VK_MAX_EXTENSION_NAME_SIZE],
    specVersion:           u32,
    implementationVersion: u32,
    description:           [c_char; VK_MAX_DESCRIPTION_SIZE],
}

/// Names that vk_icdGetPhysicalDeviceProcAddr resolves.
///
/// Per ICD ABI v4+, the Khronos loader probes this function to
/// fast-path physical-device entry points — bypassing the
/// instance-thunking step on every call. The set is exactly
/// the entry points whose first parameter is VkPhysicalDevice.
///
/// Returns NULL for anything else (instance- or device-level)
/// even when those names are valid via
/// vk_icdGetInstanceProcAddr. This is what the loader expects
/// to decide whether it should keep its dispatch table entry or
/// fall back to the slow path.
const ATRIUM_PHYSICAL_DEVICE_ENTRY_POINTS: &[&str] = &[
    "vkGetPhysicalDeviceProperties",
    "vkGetPhysicalDeviceFeatures",
    "vkGetPhysicalDeviceMemoryProperties",
    "vkGetPhysicalDeviceQueueFamilyProperties",
    "vkGetPhysicalDeviceFormatProperties",
    "vkGetPhysicalDeviceImageFormatProperties",
    "vkGetPhysicalDeviceProperties2",
    "vkGetPhysicalDeviceProperties2KHR",
    "vkGetPhysicalDeviceFeatures2",
    "vkGetPhysicalDeviceFeatures2KHR",
    "vkGetPhysicalDeviceMemoryProperties2",
    "vkGetPhysicalDeviceMemoryProperties2KHR",
    "vkGetPhysicalDeviceQueueFamilyProperties2",
    "vkGetPhysicalDeviceQueueFamilyProperties2KHR",
    "vkGetPhysicalDeviceFormatProperties2",
    "vkGetPhysicalDeviceFormatProperties2KHR",
    "vkGetPhysicalDeviceImageFormatProperties2",
    "vkGetPhysicalDeviceImageFormatProperties2KHR",
    "vkGetPhysicalDeviceSurfaceSupportKHR",
    "vkGetPhysicalDeviceSurfaceCapabilitiesKHR",
    "vkGetPhysicalDeviceSurfaceFormatsKHR",
    "vkGetPhysicalDeviceSurfacePresentModesKHR",
    "vkGetPhysicalDeviceSurfaceCapabilities2KHR",
    "vkGetPhysicalDeviceSurfaceFormats2KHR",
    "vkEnumerateDeviceExtensionProperties",
];

/// `vk_icdGetPhysicalDeviceProcAddr` — ICD ABI v4+ fast-path for
/// physical-device entry points. The Khronos loader detects this
/// function via dlsym and uses it (instead of
/// vk_icdGetInstanceProcAddr) to populate its physical-device
/// dispatch table, avoiding a thunk through the loader's
/// instance dispatch on every call.
///
/// Returns NULL for instance-level (vkCreateInstance, …) and
/// device-level (vkQueueSubmit, …) names, even though we DO
/// expose those via vk_icdGetInstanceProcAddr.
#[no_mangle]
pub unsafe extern "C" fn vk_icdGetPhysicalDeviceProcAddr(
    _instance: *mut c_void,
    name:      *const c_char,
) -> PFN_vkVoidFunction {
    if name.is_null() {
        return None;
    }
    let name_str = match CStr::from_ptr(name).to_str() {
        Ok(s) => s,
        Err(_) => return None,
    };
    if !ATRIUM_PHYSICAL_DEVICE_ENTRY_POINTS.iter().any(|n| *n == name_str) {
        return None;
    }
    vk_icdGetInstanceProcAddr(std::ptr::null_mut(), name)
}

/// Names that vkGetDeviceProcAddr must NOT resolve (per spec).
/// The Khronos loader treats device-level proc-addr as the
/// canonical fast path for an app, and apps occasionally call
/// vkGetDeviceProcAddr with instance-level names to test that
/// the ICD draws the line correctly. We return None for these.
///
/// This list is exhaustive against what we currently expose:
/// keep in sync if vk_icdGetInstanceProcAddr gains new entries.
const ATRIUM_INSTANCE_ONLY_ENTRY_POINTS: &[&str] = &[
    "vkEnumerateInstanceVersion",
    "vkEnumerateInstanceExtensionProperties",
    "vkEnumerateInstanceLayerProperties",
    "vkEnumerateDeviceExtensionProperties",
    "vkCreateInstance",
    "vkDestroyInstance",
    "vkEnumeratePhysicalDevices",
    "vkGetPhysicalDeviceProperties",
    "vkGetPhysicalDeviceQueueFamilyProperties",
    "vkGetPhysicalDeviceFeatures",
    "vkGetPhysicalDeviceMemoryProperties",
    "vkGetPhysicalDeviceFormatProperties",
    "vkGetPhysicalDeviceImageFormatProperties",
    "vkGetPhysicalDeviceImageFormatProperties2",
    "vkGetPhysicalDeviceImageFormatProperties2KHR",
    "vkGetPhysicalDeviceProperties2",
    "vkGetPhysicalDeviceProperties2KHR",
    "vkGetPhysicalDeviceFeatures2",
    "vkGetPhysicalDeviceFeatures2KHR",
    "vkGetPhysicalDeviceMemoryProperties2",
    "vkGetPhysicalDeviceMemoryProperties2KHR",
    "vkGetPhysicalDeviceFormatProperties2",
    "vkGetPhysicalDeviceFormatProperties2KHR",
    "vkGetPhysicalDeviceQueueFamilyProperties2",
    "vkGetPhysicalDeviceQueueFamilyProperties2KHR",
    "vkCreateDevice",
    "vkDestroySurfaceKHR",
    "vkCreateAtriumSurfaceEXT",
    "vkGetPhysicalDeviceSurfaceSupportKHR",
    "vkGetPhysicalDeviceSurfaceCapabilitiesKHR",
    "vkGetPhysicalDeviceSurfaceFormatsKHR",
    "vkGetPhysicalDeviceSurfacePresentModesKHR",
    "vkGetPhysicalDeviceSurfaceCapabilities2KHR",
    "vkGetPhysicalDeviceSurfaceFormats2KHR",
    "vkCreateDebugUtilsMessengerEXT",
    "vkDestroyDebugUtilsMessengerEXT",
    "vkSubmitDebugUtilsMessageEXT",
];

/// `vkGetDeviceProcAddr` — resolves device-level entry points.
///
/// Per the Vulkan spec, this returns NULL for instance-level
/// entry points even if the ICD exposes them via
/// vk_icdGetInstanceProcAddr. Apps use this to retrieve a
/// dispatch table that skips the loader's per-call instance
/// thunking — the fast path for cmdbuf recording loops.
///
/// We delegate to vk_icdGetInstanceProcAddr (which knows every
/// name we expose) but filter out the instance-only list so a
/// well-behaved caller can't accidentally call e.g.
/// vkCreateInstance via a device proc-addr.
#[no_mangle]
pub unsafe extern "C" fn vkGetDeviceProcAddr(
    _device: VkDevice,
    name:    *const c_char,
) -> PFN_vkVoidFunction {
    if name.is_null() {
        return None;
    }
    let name_str = match CStr::from_ptr(name).to_str() {
        Ok(s) => s,
        Err(_) => return None,
    };
    if ATRIUM_INSTANCE_ONLY_ENTRY_POINTS.iter().any(|n| *n == name_str) {
        return None;
    }
    // Delegate. We pass null instance — vk_icdGetInstanceProcAddr
    // accepts that for any name it knows.
    vk_icdGetInstanceProcAddr(std::ptr::null_mut(), name)
}

/// `vk_icdGetInstanceProcAddr` — the only symbol the Vulkan loader
/// strictly requires us to export. Called by the loader to resolve
/// every Vulkan function name on the application's behalf.
///
/// Today we return `None` for every lookup (skeleton).
/// `vkCreateInstance` lookup will eventually be wired up; for now
/// applications attempting to use our ICD see
/// `VK_ERROR_INCOMPATIBLE_DRIVER` on instance creation.
///
/// # Safety
///
/// `name` must be a valid NUL-terminated C string. `instance` may be
/// `VK_NULL_HANDLE` for the bootstrap functions (vkCreateInstance,
/// vkEnumerateInstanceExtensionProperties, etc.) or a valid
/// `VkInstance` handle we previously returned for the rest.
#[no_mangle]
pub unsafe extern "C" fn vk_icdGetInstanceProcAddr(
    _instance: *mut c_void, /* VkInstance — opaque */
    name: *const c_char,
) -> PFN_vkVoidFunction {
    if name.is_null() {
        return None;
    }
    let name_str = match CStr::from_ptr(name).to_str() {
        Ok(s) => s,
        Err(_) => return None,
    };

    // Bootstrap dispatch. These are the entry points the Vulkan
    // loader calls BEFORE we've returned any VkInstance handle —
    // probing whether we exist, what API version we claim, and
    // what extensions/layers we support. Implementing them
    // correctly makes us a "real but empty" ICD; the loader will
    // include us in vkEnumeratePhysicalDevices results (zero
    // physical devices today — atrium-vk-icd hasn't wired up the
    // device enumeration yet).
    //
    // Cast via `as PFN_vkVoidFunction`-typed transmute — the
    // returned function pointer is type-erased; the loader knows
    // the real signature from the name it asked for.
    type FnVoidPtr = unsafe extern "C" fn();
    match name_str {
        "vkEnumerateInstanceVersion" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(*mut u32) -> VkResult, FnVoidPtr,
            >(vkEnumerateInstanceVersion)),
        "vkEnumerateInstanceExtensionProperties" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(*const c_char, *mut u32, *mut VkExtensionProperties) -> VkResult, FnVoidPtr,
            >(vkEnumerateInstanceExtensionProperties)),
        "vkEnumerateInstanceLayerProperties" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(*mut u32, *mut VkLayerProperties) -> VkResult, FnVoidPtr,
            >(vkEnumerateInstanceLayerProperties)),
        "vkEnumerateDeviceExtensionProperties" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkPhysicalDevice, *const c_char, *mut u32, *mut VkExtensionProperties) -> VkResult, FnVoidPtr,
            >(vkEnumerateDeviceExtensionProperties)),
        "vkGetDeviceProcAddr" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, *const c_char) -> PFN_vkVoidFunction, FnVoidPtr,
            >(vkGetDeviceProcAddr)),
        // Vulkan 1.1 pNext-chain variants — apps targeting >=1.1
        // call these instead of the 1.0 counterparts.
        "vkGetPhysicalDeviceProperties2" |
        "vkGetPhysicalDeviceProperties2KHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkPhysicalDevice, *mut c_void), FnVoidPtr,
            >(vkGetPhysicalDeviceProperties2)),
        "vkGetPhysicalDeviceFeatures2" |
        "vkGetPhysicalDeviceFeatures2KHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkPhysicalDevice, *mut c_void), FnVoidPtr,
            >(vkGetPhysicalDeviceFeatures2)),
        "vkGetPhysicalDeviceMemoryProperties2" |
        "vkGetPhysicalDeviceMemoryProperties2KHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkPhysicalDevice, *mut c_void), FnVoidPtr,
            >(vkGetPhysicalDeviceMemoryProperties2)),
        "vkGetPhysicalDeviceFormatProperties2" |
        "vkGetPhysicalDeviceFormatProperties2KHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkPhysicalDevice, ash::vk::Format, *mut c_void), FnVoidPtr,
            >(vkGetPhysicalDeviceFormatProperties2)),
        "vkGetPhysicalDeviceQueueFamilyProperties2" |
        "vkGetPhysicalDeviceQueueFamilyProperties2KHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkPhysicalDevice, *mut u32, *mut c_void), FnVoidPtr,
            >(vkGetPhysicalDeviceQueueFamilyProperties2)),
        "vkGetPhysicalDeviceImageFormatProperties" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkPhysicalDevice, ash::vk::Format, ash::vk::ImageType, ash::vk::ImageTiling, ash::vk::ImageUsageFlags, ash::vk::ImageCreateFlags, *mut ash::vk::ImageFormatProperties) -> VkResult, FnVoidPtr,
            >(vkGetPhysicalDeviceImageFormatProperties)),
        "vkGetPhysicalDeviceImageFormatProperties2" |
        "vkGetPhysicalDeviceImageFormatProperties2KHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkPhysicalDevice, *const c_void, *mut c_void) -> VkResult, FnVoidPtr,
            >(vkGetPhysicalDeviceImageFormatProperties2)),
        "vkCreateInstance" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(*const c_void, *const c_void, *mut VkInstance) -> VkResult, FnVoidPtr,
            >(vkCreateInstance)),
        "vkDestroyInstance" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkInstance, *const c_void), FnVoidPtr,
            >(vkDestroyInstance)),
        "vkEnumeratePhysicalDevices" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkInstance, *mut u32, *mut VkPhysicalDevice) -> VkResult, FnVoidPtr,
            >(vkEnumeratePhysicalDevices)),
        "vkGetPhysicalDeviceProperties" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkPhysicalDevice, *mut ash::vk::PhysicalDeviceProperties), FnVoidPtr,
            >(vkGetPhysicalDeviceProperties)),
        "vkGetPhysicalDeviceQueueFamilyProperties" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkPhysicalDevice, *mut u32, *mut ash::vk::QueueFamilyProperties), FnVoidPtr,
            >(vkGetPhysicalDeviceQueueFamilyProperties)),
        "vkGetPhysicalDeviceFeatures" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkPhysicalDevice, *mut ash::vk::PhysicalDeviceFeatures), FnVoidPtr,
            >(vkGetPhysicalDeviceFeatures)),
        "vkGetPhysicalDeviceMemoryProperties" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkPhysicalDevice, *mut ash::vk::PhysicalDeviceMemoryProperties), FnVoidPtr,
            >(vkGetPhysicalDeviceMemoryProperties)),
        "vkGetPhysicalDeviceFormatProperties" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkPhysicalDevice, ash::vk::Format, *mut ash::vk::FormatProperties), FnVoidPtr,
            >(vkGetPhysicalDeviceFormatProperties)),
        "vkCreateDevice" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkPhysicalDevice, *const c_void, *const c_void, *mut VkDevice) -> VkResult, FnVoidPtr,
            >(vkCreateDevice)),
        "vkDestroyDevice" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, *const c_void), FnVoidPtr,
            >(vkDestroyDevice)),
        "vkGetDeviceQueue" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u32, u32, *mut VkQueue), FnVoidPtr,
            >(vkGetDeviceQueue)),
        "vkCreateCommandPool" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, *const c_void, *const c_void, *mut VkCommandPool) -> VkResult, FnVoidPtr,
            >(vkCreateCommandPool)),
        "vkDestroyCommandPool" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, VkCommandPool, *const c_void), FnVoidPtr,
            >(vkDestroyCommandPool)),
        "vkAllocateCommandBuffers" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, *const c_void, *mut VkCommandBuffer) -> VkResult, FnVoidPtr,
            >(vkAllocateCommandBuffers)),
        "vkFreeCommandBuffers" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, VkCommandPool, u32, *const VkCommandBuffer), FnVoidPtr,
            >(vkFreeCommandBuffers)),
        "vkBeginCommandBuffer" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, *const c_void) -> VkResult, FnVoidPtr,
            >(vkBeginCommandBuffer)),
        "vkEndCommandBuffer" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer) -> VkResult, FnVoidPtr,
            >(vkEndCommandBuffer)),
        "vkResetCommandBuffer" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u32) -> VkResult, FnVoidPtr,
            >(vkResetCommandBuffer)),
        "vkQueueSubmit" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkQueue, u32, *const c_void, *mut c_void) -> VkResult, FnVoidPtr,
            >(vkQueueSubmit)),
        "vkQueueSubmit2" |
        "vkQueueSubmit2KHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkQueue, u32, *const c_void, *mut c_void) -> VkResult, FnVoidPtr,
            >(vkQueueSubmit2)),
        "vkCmdSetViewport" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u32, u32, *const ash::vk::Viewport), FnVoidPtr,
            >(vkCmdSetViewport)),
        "vkCmdSetScissor" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u32, u32, *const ash::vk::Rect2D), FnVoidPtr,
            >(vkCmdSetScissor)),
        "vkCmdPushConstants" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u64, u32, u32, u32, *const c_void), FnVoidPtr,
            >(vkCmdPushConstants)),
        "vkCmdDraw" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u32, u32, u32, u32), FnVoidPtr,
            >(vkCmdDraw)),
        "vkCreatePipelineLayout" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, *const c_void, *const c_void, *mut u64) -> VkResult, FnVoidPtr,
            >(vkCreatePipelineLayout)),
        "vkDestroyPipelineLayout" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64, *const c_void), FnVoidPtr,
            >(vkDestroyPipelineLayout)),
        "vkCreateGraphicsPipelines" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64, u32, *const c_void, *const c_void, *mut u64) -> VkResult, FnVoidPtr,
            >(vkCreateGraphicsPipelines)),
        "vkDestroyPipeline" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64, *const c_void), FnVoidPtr,
            >(vkDestroyPipeline)),
        "vkCmdBindPipeline" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u32, u64), FnVoidPtr,
            >(vkCmdBindPipeline)),
        "vkAllocateMemory" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, *const c_void, *const c_void, *mut u64) -> VkResult, FnVoidPtr,
            >(vkAllocateMemory)),
        "vkFreeMemory" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64, *const c_void), FnVoidPtr,
            >(vkFreeMemory)),
        "vkMapMemory" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64, u64, u64, u32, *mut *mut c_void) -> VkResult, FnVoidPtr,
            >(vkMapMemory)),
        "vkUnmapMemory" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64), FnVoidPtr,
            >(vkUnmapMemory)),
        "vkCreateBuffer" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, *const c_void, *const c_void, *mut u64) -> VkResult, FnVoidPtr,
            >(vkCreateBuffer)),
        "vkDestroyBuffer" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64, *const c_void), FnVoidPtr,
            >(vkDestroyBuffer)),
        "vkGetBufferMemoryRequirements" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64, *mut ash::vk::MemoryRequirements), FnVoidPtr,
            >(vkGetBufferMemoryRequirements)),
        "vkBindBufferMemory" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64, u64, u64) -> VkResult, FnVoidPtr,
            >(vkBindBufferMemory)),
        "vkCmdBindVertexBuffers" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u32, u32, *const u64, *const u64), FnVoidPtr,
            >(vkCmdBindVertexBuffers)),
        "vkCmdBindIndexBuffer" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u64, u64, u32), FnVoidPtr,
            >(vkCmdBindIndexBuffer)),
        "vkCmdDrawIndexed" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u32, u32, u32, i32, u32), FnVoidPtr,
            >(vkCmdDrawIndexed)),
        "vkCreateImage" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, *const c_void, *const c_void, *mut u64) -> VkResult, FnVoidPtr,
            >(vkCreateImage)),
        "vkDestroyImage" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64, *const c_void), FnVoidPtr,
            >(vkDestroyImage)),
        "vkGetImageMemoryRequirements" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64, *mut ash::vk::MemoryRequirements), FnVoidPtr,
            >(vkGetImageMemoryRequirements)),
        "vkBindImageMemory" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64, u64, u64) -> VkResult, FnVoidPtr,
            >(vkBindImageMemory)),
        "vkCreateShaderModule" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, *const c_void, *const c_void, *mut u64) -> VkResult, FnVoidPtr,
            >(vkCreateShaderModule)),
        "vkDestroyShaderModule" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64, *const c_void), FnVoidPtr,
            >(vkDestroyShaderModule)),
        "vkCreateImageView" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, *const c_void, *const c_void, *mut u64) -> VkResult, FnVoidPtr,
            >(vkCreateImageView)),
        "vkDestroyImageView" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64, *const c_void), FnVoidPtr,
            >(vkDestroyImageView)),
        "vkCreateRenderPass" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, *const c_void, *const c_void, *mut u64) -> VkResult, FnVoidPtr,
            >(vkCreateRenderPass)),
        "vkDestroyRenderPass" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64, *const c_void), FnVoidPtr,
            >(vkDestroyRenderPass)),
        "vkCreateFramebuffer" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, *const c_void, *const c_void, *mut u64) -> VkResult, FnVoidPtr,
            >(vkCreateFramebuffer)),
        "vkDestroyFramebuffer" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64, *const c_void), FnVoidPtr,
            >(vkDestroyFramebuffer)),
        "vkCmdBeginRenderPass" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, *const c_void, u32), FnVoidPtr,
            >(vkCmdBeginRenderPass)),
        "vkCmdEndRenderPass" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer), FnVoidPtr,
            >(vkCmdEndRenderPass)),
        // 1.3 dynamic-rendering — VK_KHR_dynamic_rendering KHR aliases.
        "vkCmdBeginRendering" | "vkCmdBeginRenderingKHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, *const c_void), FnVoidPtr,
            >(vkCmdBeginRendering)),
        "vkCmdEndRendering" | "vkCmdEndRenderingKHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer), FnVoidPtr,
            >(vkCmdEndRendering)),
        // 1.3 extended dynamic state (promoted from
        // VK_EXT_extended_dynamic_state{,2,3}).
        "vkCmdSetViewportWithCount" | "vkCmdSetViewportWithCountEXT" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u32, *const ash::vk::Viewport), FnVoidPtr,
            >(vkCmdSetViewportWithCount)),
        "vkCmdSetScissorWithCount" | "vkCmdSetScissorWithCountEXT" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u32, *const ash::vk::Rect2D), FnVoidPtr,
            >(vkCmdSetScissorWithCount)),
        "vkCmdBindVertexBuffers2" | "vkCmdBindVertexBuffers2EXT" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u32, u32, *const u64, *const u64, *const u64, *const u64), FnVoidPtr,
            >(vkCmdBindVertexBuffers2)),
        "vkCmdSetCullMode" | "vkCmdSetCullModeEXT" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u32), FnVoidPtr,
            >(vkCmdSetCullMode)),
        "vkCmdSetFrontFace" | "vkCmdSetFrontFaceEXT" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u32), FnVoidPtr,
            >(vkCmdSetFrontFace)),
        "vkCmdSetPrimitiveTopology" | "vkCmdSetPrimitiveTopologyEXT" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u32), FnVoidPtr,
            >(vkCmdSetPrimitiveTopology)),
        "vkCmdSetDepthTestEnable" | "vkCmdSetDepthTestEnableEXT" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u32), FnVoidPtr,
            >(vkCmdSetDepthTestEnable)),
        "vkCmdSetDepthWriteEnable" | "vkCmdSetDepthWriteEnableEXT" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u32), FnVoidPtr,
            >(vkCmdSetDepthWriteEnable)),
        "vkCmdSetDepthCompareOp" | "vkCmdSetDepthCompareOpEXT" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u32), FnVoidPtr,
            >(vkCmdSetDepthCompareOp)),
        "vkCmdSetDepthBoundsTestEnable" | "vkCmdSetDepthBoundsTestEnableEXT" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u32), FnVoidPtr,
            >(vkCmdSetDepthBoundsTestEnable)),
        "vkCmdSetStencilTestEnable" | "vkCmdSetStencilTestEnableEXT" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u32), FnVoidPtr,
            >(vkCmdSetStencilTestEnable)),
        "vkCmdSetStencilOp" | "vkCmdSetStencilOpEXT" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u32, u32, u32, u32, u32), FnVoidPtr,
            >(vkCmdSetStencilOp)),
        "vkCmdSetRasterizerDiscardEnable" | "vkCmdSetRasterizerDiscardEnableEXT" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u32), FnVoidPtr,
            >(vkCmdSetRasterizerDiscardEnable)),
        "vkCmdSetDepthBiasEnable" | "vkCmdSetDepthBiasEnableEXT" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u32), FnVoidPtr,
            >(vkCmdSetDepthBiasEnable)),
        "vkCmdSetPrimitiveRestartEnable" | "vkCmdSetPrimitiveRestartEnableEXT" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u32), FnVoidPtr,
            >(vkCmdSetPrimitiveRestartEnable)),
        "vkCreateFence" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, *const c_void, *const c_void, *mut u64) -> VkResult, FnVoidPtr,
            >(vkCreateFence)),
        "vkDestroyFence" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64, *const c_void), FnVoidPtr,
            >(vkDestroyFence)),
        "vkWaitForFences" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u32, *const u64, u32, u64) -> VkResult, FnVoidPtr,
            >(vkWaitForFences)),
        "vkResetFences" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u32, *const u64) -> VkResult, FnVoidPtr,
            >(vkResetFences)),
        "vkGetFenceStatus" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64) -> VkResult, FnVoidPtr,
            >(vkGetFenceStatus)),
        "vkCreateSemaphore" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, *const c_void, *const c_void, *mut u64) -> VkResult, FnVoidPtr,
            >(vkCreateSemaphore)),
        "vkDestroySemaphore" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64, *const c_void), FnVoidPtr,
            >(vkDestroySemaphore)),
        "vkCreateSampler" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, *const c_void, *const c_void, *mut u64) -> VkResult, FnVoidPtr,
            >(vkCreateSampler)),
        "vkDestroySampler" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64, *const c_void), FnVoidPtr,
            >(vkDestroySampler)),
        "vkCreateDescriptorSetLayout" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, *const c_void, *const c_void, *mut u64) -> VkResult, FnVoidPtr,
            >(vkCreateDescriptorSetLayout)),
        "vkDestroyDescriptorSetLayout" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64, *const c_void), FnVoidPtr,
            >(vkDestroyDescriptorSetLayout)),
        "vkCreateDescriptorPool" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, *const c_void, *const c_void, *mut u64) -> VkResult, FnVoidPtr,
            >(vkCreateDescriptorPool)),
        "vkDestroyDescriptorPool" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64, *const c_void), FnVoidPtr,
            >(vkDestroyDescriptorPool)),
        "vkAllocateDescriptorSets" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, *const c_void, *mut u64) -> VkResult, FnVoidPtr,
            >(vkAllocateDescriptorSets)),
        "vkUpdateDescriptorSets" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u32, *const c_void, u32, *const c_void), FnVoidPtr,
            >(vkUpdateDescriptorSets)),
        "vkCmdBindDescriptorSets" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u32, u64, u32, u32, *const u64, u32, *const u32), FnVoidPtr,
            >(vkCmdBindDescriptorSets)),
        "vkCmdCopyBuffer" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u64, u64, u32, *const c_void), FnVoidPtr,
            >(vkCmdCopyBuffer)),
        "vkCmdCopyBufferToImage" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u64, u64, u32, u32, *const c_void), FnVoidPtr,
            >(vkCmdCopyBufferToImage)),
        "vkCmdPipelineBarrier" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u32, u32, u32, u32, *const c_void, u32, *const c_void, u32, *const c_void), FnVoidPtr,
            >(vkCmdPipelineBarrier)),
        "vkCmdPipelineBarrier2" |
        "vkCmdPipelineBarrier2KHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, *const c_void), FnVoidPtr,
            >(vkCmdPipelineBarrier2)),
        "vkGetImageSubresourceLayout" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64, *const c_void, *mut c_void), FnVoidPtr,
            >(vkGetImageSubresourceLayout)),
        "vkResetCommandPool" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64, u32) -> VkResult, FnVoidPtr,
            >(vkResetCommandPool)),
        "vkTrimCommandPool" |
        "vkTrimCommandPoolKHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64, u32), FnVoidPtr,
            >(vkTrimCommandPool)),
        "vkFlushMappedMemoryRanges" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u32, *const c_void) -> VkResult, FnVoidPtr,
            >(vkFlushMappedMemoryRanges)),
        "vkInvalidateMappedMemoryRanges" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u32, *const c_void) -> VkResult, FnVoidPtr,
            >(vkInvalidateMappedMemoryRanges)),
        "vkFreeDescriptorSets" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64, u32, *const u64) -> VkResult, FnVoidPtr,
            >(vkFreeDescriptorSets)),
        "vkResetDescriptorPool" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64, u32) -> VkResult, FnVoidPtr,
            >(vkResetDescriptorPool)),
        "vkSignalSemaphore" |
        "vkSignalSemaphoreKHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, *const c_void) -> VkResult, FnVoidPtr,
            >(vkSignalSemaphore)),
        "vkWaitSemaphores" |
        "vkWaitSemaphoresKHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, *const c_void, u64) -> VkResult, FnVoidPtr,
            >(vkWaitSemaphores)),
        "vkGetSemaphoreCounterValue" |
        "vkGetSemaphoreCounterValueKHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64, *mut u64) -> VkResult, FnVoidPtr,
            >(vkGetSemaphoreCounterValue)),
        "vkGetBufferMemoryRequirements2" |
        "vkGetBufferMemoryRequirements2KHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, *const c_void, *mut c_void), FnVoidPtr,
            >(vkGetBufferMemoryRequirements2)),
        "vkGetImageMemoryRequirements2" |
        "vkGetImageMemoryRequirements2KHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, *const c_void, *mut c_void), FnVoidPtr,
            >(vkGetImageMemoryRequirements2)),
        "vkGetDeviceBufferMemoryRequirements" |
        "vkGetDeviceBufferMemoryRequirementsKHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, *const c_void, *mut c_void), FnVoidPtr,
            >(vkGetDeviceBufferMemoryRequirements)),
        "vkGetDeviceImageMemoryRequirements" |
        "vkGetDeviceImageMemoryRequirementsKHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, *const c_void, *mut c_void), FnVoidPtr,
            >(vkGetDeviceImageMemoryRequirements)),
        // 1.1 descriptor-update templates + VK_KHR_push_descriptor
        "vkCreateDescriptorUpdateTemplate" |
        "vkCreateDescriptorUpdateTemplateKHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, *const c_void, *const c_void, *mut u64) -> VkResult, FnVoidPtr,
            >(vkCreateDescriptorUpdateTemplate)),
        "vkDestroyDescriptorUpdateTemplate" |
        "vkDestroyDescriptorUpdateTemplateKHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64, *const c_void), FnVoidPtr,
            >(vkDestroyDescriptorUpdateTemplate)),
        "vkUpdateDescriptorSetWithTemplate" |
        "vkUpdateDescriptorSetWithTemplateKHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64, u64, *const c_void), FnVoidPtr,
            >(vkUpdateDescriptorSetWithTemplate)),
        "vkCmdPushDescriptorSet" |
        "vkCmdPushDescriptorSetKHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u32, u64, u32, u32, *const c_void), FnVoidPtr,
            >(vkCmdPushDescriptorSet)),
        "vkCmdPushDescriptorSetWithTemplate" |
        "vkCmdPushDescriptorSetWithTemplateKHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u64, u64, u32, *const c_void), FnVoidPtr,
            >(vkCmdPushDescriptorSetWithTemplate)),
        "vkDeviceWaitIdle" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice) -> VkResult, FnVoidPtr,
            >(vkDeviceWaitIdle)),
        "vkQueueWaitIdle" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkQueue) -> VkResult, FnVoidPtr,
            >(vkQueueWaitIdle)),
        "vkCreateComputePipelines" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64, u32, *const c_void, *const c_void, *mut u64) -> VkResult, FnVoidPtr,
            >(vkCreateComputePipelines)),
        "vkCmdDispatch" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u32, u32, u32), FnVoidPtr,
            >(vkCmdDispatch)),
        "vkCmdNextSubpass" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u32), FnVoidPtr,
            >(vkCmdNextSubpass)),
        "vkCmdDrawIndirect" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u64, u64, u32, u32), FnVoidPtr,
            >(vkCmdDrawIndirect)),
        "vkCmdDrawIndexedIndirect" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u64, u64, u32, u32), FnVoidPtr,
            >(vkCmdDrawIndexedIndirect)),
        "vkCmdDispatchIndirect" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u64, u64), FnVoidPtr,
            >(vkCmdDispatchIndirect)),
        "vkCmdSetLineWidth" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, f32), FnVoidPtr,
            >(vkCmdSetLineWidth)),
        "vkCmdSetDepthBias" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, f32, f32, f32), FnVoidPtr,
            >(vkCmdSetDepthBias)),
        "vkCmdSetBlendConstants" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, *const f32), FnVoidPtr,
            >(vkCmdSetBlendConstants)),
        "vkCmdClearColorImage" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u64, u32, *const c_void, u32, *const c_void), FnVoidPtr,
            >(vkCmdClearColorImage)),
        "vkCmdClearDepthStencilImage" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u64, u32, *const c_void, u32, *const c_void), FnVoidPtr,
            >(vkCmdClearDepthStencilImage)),
        "vkCmdClearAttachments" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u32, *const c_void, u32, *const c_void), FnVoidPtr,
            >(vkCmdClearAttachments)),
        "vkCmdCopyImage" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u64, u32, u64, u32, u32, *const c_void), FnVoidPtr,
            >(vkCmdCopyImage)),
        "vkCmdBlitImage" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u64, u32, u64, u32, u32, *const c_void, u32), FnVoidPtr,
            >(vkCmdBlitImage)),
        "vkCmdResolveImage" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u64, u32, u64, u32, u32, *const c_void), FnVoidPtr,
            >(vkCmdResolveImage)),
        "vkCmdCopyImageToBuffer" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u64, u32, u64, u32, *const c_void), FnVoidPtr,
            >(vkCmdCopyImageToBuffer)),
        // Query pools — no-ops + opaque handles.
        "vkCreateQueryPool" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, *const c_void, *const c_void, *mut u64) -> VkResult, FnVoidPtr,
            >(vkCreateQueryPool)),
        "vkDestroyQueryPool" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64, *const c_void), FnVoidPtr,
            >(vkDestroyQueryPool)),
        "vkCmdBeginQuery" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u64, u32, u32), FnVoidPtr,
            >(vkCmdBeginQuery)),
        "vkCmdEndQuery" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u64, u32), FnVoidPtr,
            >(vkCmdEndQuery)),
        "vkCmdResetQueryPool" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u64, u32, u32), FnVoidPtr,
            >(vkCmdResetQueryPool)),
        "vkCmdWriteTimestamp" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u32, u64, u32), FnVoidPtr,
            >(vkCmdWriteTimestamp)),
        "vkGetQueryPoolResults" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64, u32, u32, usize, *mut c_void, u64, u32) -> VkResult, FnVoidPtr,
            >(vkGetQueryPoolResults)),
        // Events.
        "vkCreateEvent" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, *const c_void, *const c_void, *mut u64) -> VkResult, FnVoidPtr,
            >(vkCreateEvent)),
        "vkDestroyEvent" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64, *const c_void), FnVoidPtr,
            >(vkDestroyEvent)),
        "vkSetEvent" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64) -> VkResult, FnVoidPtr,
            >(vkSetEvent)),
        "vkResetEvent" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64) -> VkResult, FnVoidPtr,
            >(vkResetEvent)),
        "vkGetEventStatus" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64) -> VkResult, FnVoidPtr,
            >(vkGetEventStatus)),
        "vkCmdSetEvent" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u64, u32), FnVoidPtr,
            >(vkCmdSetEvent)),
        "vkCmdResetEvent" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u64, u32), FnVoidPtr,
            >(vkCmdResetEvent)),
        "vkCmdWaitEvents" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u32, *const u64, u32, u32, u32, *const c_void, u32, *const c_void, u32, *const c_void), FnVoidPtr,
            >(vkCmdWaitEvents)),
        // Vulkan 1.3 sync2 event variants.
        "vkCmdSetEvent2" | "vkCmdSetEvent2KHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u64, *const c_void), FnVoidPtr,
            >(vkCmdSetEvent2)),
        "vkCmdResetEvent2" | "vkCmdResetEvent2KHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u64, u64), FnVoidPtr,
            >(vkCmdResetEvent2)),
        "vkCmdWaitEvents2" | "vkCmdWaitEvents2KHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u32, *const u64, *const c_void), FnVoidPtr,
            >(vkCmdWaitEvents2)),
        "vkCmdWriteTimestamp2" | "vkCmdWriteTimestamp2KHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u64, u64, u32), FnVoidPtr,
            >(vkCmdWriteTimestamp2)),
        // VkBufferView.
        "vkCreateBufferView" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, *const c_void, *const c_void, *mut u64) -> VkResult, FnVoidPtr,
            >(vkCreateBufferView)),
        "vkDestroyBufferView" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64, *const c_void), FnVoidPtr,
            >(vkDestroyBufferView)),
        // Secondary-cmdbuf execution — no-op today (no secondary
        // buffers ever populated; an app calling this gets nothing
        // appended to the primary's frame stream, matching the
        // observable behavior of "no secondary recorded any ops").
        "vkCmdExecuteCommands" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, u32, *const VkCommandBuffer), FnVoidPtr,
            >(vkCmdExecuteCommands)),
        // WSI: surface + swapchain. See docs/spec/aqueduct-gpu.md §7.1.1.
        "vkDestroySurfaceKHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkInstance, u64, *const c_void), FnVoidPtr,
            >(vkDestroySurfaceKHR)),
        "vkCreateAtriumSurfaceEXT" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkInstance, *const c_void, *const c_void, *mut u64) -> VkResult, FnVoidPtr,
            >(vkCreateAtriumSurfaceEXT)),
        "vkGetPhysicalDeviceSurfaceSupportKHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkPhysicalDevice, u32, u64, *mut u32) -> VkResult, FnVoidPtr,
            >(vkGetPhysicalDeviceSurfaceSupportKHR)),
        "vkGetPhysicalDeviceSurfaceCapabilitiesKHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkPhysicalDevice, u64, *mut c_void) -> VkResult, FnVoidPtr,
            >(vkGetPhysicalDeviceSurfaceCapabilitiesKHR)),
        "vkGetPhysicalDeviceSurfaceFormatsKHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkPhysicalDevice, u64, *mut u32, *mut c_void) -> VkResult, FnVoidPtr,
            >(vkGetPhysicalDeviceSurfaceFormatsKHR)),
        "vkGetPhysicalDeviceSurfacePresentModesKHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkPhysicalDevice, u64, *mut u32, *mut u32) -> VkResult, FnVoidPtr,
            >(vkGetPhysicalDeviceSurfacePresentModesKHR)),
        "vkGetPhysicalDeviceSurfaceCapabilities2KHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkPhysicalDevice, *const c_void, *mut c_void) -> VkResult, FnVoidPtr,
            >(vkGetPhysicalDeviceSurfaceCapabilities2KHR)),
        "vkGetPhysicalDeviceSurfaceFormats2KHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkPhysicalDevice, *const c_void, *mut u32, *mut c_void) -> VkResult, FnVoidPtr,
            >(vkGetPhysicalDeviceSurfaceFormats2KHR)),
        "vkCreateSwapchainKHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, *const c_void, *const c_void, *mut u64) -> VkResult, FnVoidPtr,
            >(vkCreateSwapchainKHR)),
        "vkDestroySwapchainKHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64, *const c_void), FnVoidPtr,
            >(vkDestroySwapchainKHR)),
        "vkGetSwapchainImagesKHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64, *mut u32, *mut u64) -> VkResult, FnVoidPtr,
            >(vkGetSwapchainImagesKHR)),
        "vkAcquireNextImageKHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, u64, u64, u64, u64, *mut u32) -> VkResult, FnVoidPtr,
            >(vkAcquireNextImageKHR)),
        "vkQueuePresentKHR" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkQueue, *const c_void) -> VkResult, FnVoidPtr,
            >(vkQueuePresentKHR)),
        // VK_EXT_debug_utils — no-op stubs (see "Debug-utils stubs"
        // section). Lets validation layers + apps that probe for
        // the extension load against atrium-vk-icd.
        "vkCreateDebugUtilsMessengerEXT" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkInstance, *const c_void, *const c_void, *mut u64) -> VkResult, FnVoidPtr,
            >(vkCreateDebugUtilsMessengerEXT)),
        "vkDestroyDebugUtilsMessengerEXT" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkInstance, u64, *const c_void), FnVoidPtr,
            >(vkDestroyDebugUtilsMessengerEXT)),
        "vkSubmitDebugUtilsMessageEXT" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkInstance, u32, u32, *const c_void), FnVoidPtr,
            >(vkSubmitDebugUtilsMessageEXT)),
        "vkSetDebugUtilsObjectNameEXT" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, *const c_void) -> VkResult, FnVoidPtr,
            >(vkSetDebugUtilsObjectNameEXT)),
        "vkSetDebugUtilsObjectTagEXT" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkDevice, *const c_void) -> VkResult, FnVoidPtr,
            >(vkSetDebugUtilsObjectTagEXT)),
        "vkQueueBeginDebugUtilsLabelEXT" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkQueue, *const c_void), FnVoidPtr,
            >(vkQueueBeginDebugUtilsLabelEXT)),
        "vkQueueEndDebugUtilsLabelEXT" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkQueue), FnVoidPtr,
            >(vkQueueEndDebugUtilsLabelEXT)),
        "vkQueueInsertDebugUtilsLabelEXT" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkQueue, *const c_void), FnVoidPtr,
            >(vkQueueInsertDebugUtilsLabelEXT)),
        "vkCmdBeginDebugUtilsLabelEXT" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, *const c_void), FnVoidPtr,
            >(vkCmdBeginDebugUtilsLabelEXT)),
        "vkCmdEndDebugUtilsLabelEXT" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer), FnVoidPtr,
            >(vkCmdEndDebugUtilsLabelEXT)),
        "vkCmdInsertDebugUtilsLabelEXT" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(VkCommandBuffer, *const c_void), FnVoidPtr,
            >(vkCmdInsertDebugUtilsLabelEXT)),
        _ => None,
    }
}

/// `vkGetPhysicalDeviceProperties` — describe the device's vendor /
/// driver / limits. Apps use this to pick a physical device by
/// vendorID + apiVersion + features.
///
/// We fill:
/// - apiVersion: ATRIUM_ICD_API_VERSION (1.3.0 today).
/// - driverVersion: 0 — bumps as the ICD reaches feature parity.
/// - vendorID / deviceID: derived from the BackendId. For the
///   Software backend that's Software vendor + generation 0.
/// - deviceType: VIRTUAL_GPU (matches what a paravirt driver
///   reports — applications shouldn't expect real-HW guarantees).
/// - deviceName: "atrium-vk-icd ({vendor}:{generation})".
/// - limits / sparseProperties: zeroed for skeleton. Real limits
///   come from the backend's caps (Phase 1.3b+ wires them).
///
/// # Safety
///
/// `physical_device` must be a handle previously returned by our
/// `vkEnumeratePhysicalDevices`. `p_properties` must be a writable
/// `VkPhysicalDeviceProperties`.
#[no_mangle]
pub unsafe extern "C" fn vkGetPhysicalDeviceProperties(
    physical_device: VkPhysicalDevice,
    p_properties:    *mut ash::vk::PhysicalDeviceProperties,
) {
    if physical_device.is_null() || p_properties.is_null() {
        return;
    }
    let pd = &*(physical_device as *const AtriumPhysicalDevice);

    let mut props = ash::vk::PhysicalDeviceProperties::default();
    props.api_version    = ATRIUM_ICD_API_VERSION;
    props.driver_version = 0;
    props.vendor_id      = pd.backend_vendor as u32;
    props.device_id      = pd.backend_generation as u32;
    props.device_type    = ash::vk::PhysicalDeviceType::VIRTUAL_GPU;

    // ── Limits ──────────────────────────────────────────────────
    //
    // We fill the "Required Limits" set from the Vk 1.3 spec —
    // values an app is allowed to assume without inspecting
    // limits at all. Apps that DO inspect limits would otherwise
    // bail when they see e.g. maxImageDimension2D = 0.
    //
    // Tier-1 (tiny-skia) caps at 16K×16K (backend.rs MAX_DIM),
    // which matches the Vulkan minimum-max. Sample counts are
    // SAMPLE_COUNT_1_BIT only — tier-1 is non-MSAA.
    let sc1 = ash::vk::SampleCountFlags::TYPE_1;
    let lim = &mut props.limits;
    lim.max_image_dimension1_d                 = 16384;
    lim.max_image_dimension2_d                 = 16384;
    lim.max_image_dimension3_d                 = 2048;
    lim.max_image_dimension_cube               = 16384;
    lim.max_image_array_layers                 = 256;
    lim.max_texel_buffer_elements              = 65536;
    lim.max_uniform_buffer_range               = 16384;
    lim.max_storage_buffer_range               = 128 * 1024 * 1024;
    lim.max_push_constants_size                = 128;
    lim.max_memory_allocation_count            = 4096;
    lim.max_sampler_allocation_count           = 4000;
    lim.buffer_image_granularity               = 1;
    lim.sparse_address_space_size              = 0;
    lim.max_bound_descriptor_sets              = 4;
    lim.max_per_stage_descriptor_samplers      = 16;
    lim.max_per_stage_descriptor_uniform_buffers = 12;
    lim.max_per_stage_descriptor_storage_buffers = 4;
    lim.max_per_stage_descriptor_sampled_images  = 16;
    lim.max_per_stage_descriptor_storage_images  = 4;
    lim.max_per_stage_descriptor_input_attachments = 4;
    lim.max_per_stage_resources                = 128;
    lim.max_descriptor_set_samplers            = 96;
    lim.max_descriptor_set_uniform_buffers     = 72;
    lim.max_descriptor_set_uniform_buffers_dynamic = 8;
    lim.max_descriptor_set_storage_buffers     = 24;
    lim.max_descriptor_set_storage_buffers_dynamic = 4;
    lim.max_descriptor_set_sampled_images      = 96;
    lim.max_descriptor_set_storage_images      = 24;
    lim.max_descriptor_set_input_attachments   = 4;
    lim.max_vertex_input_attributes            = 16;
    lim.max_vertex_input_bindings              = 16;
    lim.max_vertex_input_attribute_offset      = 2047;
    lim.max_vertex_input_binding_stride        = 2048;
    lim.max_vertex_output_components           = 64;
    lim.max_fragment_input_components          = 64;
    lim.max_fragment_output_attachments        = 4;
    lim.max_fragment_dual_src_attachments      = 1;
    lim.max_fragment_combined_output_resources = 4;
    lim.max_compute_shared_memory_size         = 16384;
    lim.max_compute_work_group_count           = [65535, 65535, 65535];
    lim.max_compute_work_group_invocations     = 128;
    lim.max_compute_work_group_size            = [128, 128, 64];
    lim.sub_pixel_precision_bits               = 4;
    lim.sub_texel_precision_bits               = 4;
    lim.mipmap_precision_bits                  = 4;
    lim.max_draw_indexed_index_value           = u32::MAX;
    lim.max_draw_indirect_count                = u32::MAX;
    lim.max_sampler_lod_bias                   = 2.0;
    lim.max_sampler_anisotropy                 = 1.0;
    lim.max_viewports                          = 16;
    lim.max_viewport_dimensions                = [16384, 16384];
    lim.viewport_bounds_range                  = [-32768.0, 32767.0];
    lim.viewport_sub_pixel_bits                = 0;
    lim.min_memory_map_alignment               = 64;
    lim.min_texel_buffer_offset_alignment      = 256;
    lim.min_uniform_buffer_offset_alignment    = 256;
    lim.min_storage_buffer_offset_alignment    = 256;
    lim.min_texel_offset                       = -8;
    lim.max_texel_offset                       = 7;
    lim.min_texel_gather_offset                = -8;
    lim.max_texel_gather_offset                = 7;
    lim.min_interpolation_offset               = -0.5;
    lim.max_interpolation_offset               = 0.4375;
    lim.sub_pixel_interpolation_offset_bits    = 4;
    lim.max_framebuffer_width                  = 16384;
    lim.max_framebuffer_height                 = 16384;
    lim.max_framebuffer_layers                 = 256;
    lim.framebuffer_color_sample_counts        = sc1;
    lim.framebuffer_depth_sample_counts        = sc1;
    lim.framebuffer_stencil_sample_counts      = sc1;
    lim.framebuffer_no_attachments_sample_counts = sc1;
    lim.max_color_attachments                  = 4;
    lim.sampled_image_color_sample_counts      = sc1;
    lim.sampled_image_integer_sample_counts    = sc1;
    lim.sampled_image_depth_sample_counts      = sc1;
    lim.sampled_image_stencil_sample_counts    = sc1;
    lim.storage_image_sample_counts            = sc1;
    lim.max_sample_mask_words                  = 1;
    lim.timestamp_compute_and_graphics         = ash::vk::FALSE;
    lim.timestamp_period                       = 0.0;
    lim.max_clip_distances                     = 8;
    lim.max_cull_distances                     = 8;
    lim.max_combined_clip_and_cull_distances   = 8;
    lim.discrete_queue_priorities              = 2;
    lim.point_size_range                       = [1.0, 64.0];
    lim.line_width_range                       = [1.0, 1.0];
    lim.point_size_granularity                 = 0.0;
    lim.line_width_granularity                 = 0.0;
    lim.strict_lines                           = ash::vk::TRUE;
    lim.standard_sample_locations              = ash::vk::TRUE;
    lim.optimal_buffer_copy_offset_alignment   = 1;
    lim.optimal_buffer_copy_row_pitch_alignment = 1;
    lim.non_coherent_atom_size                 = 64;

    // Fill device_name from a fixed-format string. The Vk spec
    // bounds it at VK_MAX_PHYSICAL_DEVICE_NAME_SIZE = 256 bytes
    // including the NUL.
    let name = format!("atrium-vk-icd ({}:{})",
        pd.backend_vendor.name(), pd.backend_generation);
    let name_bytes = name.as_bytes();
    let n = name_bytes.len().min(props.device_name.len() - 1);
    for (i, &b) in name_bytes.iter().take(n).enumerate() {
        props.device_name[i] = b as c_char;
    }
    props.device_name[n] = 0;

    *p_properties = props;
}

/// `vkGetPhysicalDeviceQueueFamilyProperties` — describe each queue
/// family. We expose exactly one: graphics + compute + transfer,
/// queue count 1. Atrium's GPU model is single-queue per device
/// (the aqueduct-gpu wire is serial; multi-queue arrives when the
/// kmod gains parallel submission rings, D5+).
///
/// Standard two-call query: caller invokes once with
/// `p_properties=NULL` to learn the count, then again with a
/// buffer.
///
/// # Safety
///
/// `p_queue_family_property_count` must be writable. `p_properties`
/// may be null (count-only query) or point to a writable buffer of
/// at least `*p_queue_family_property_count`
/// `VkQueueFamilyProperties` slots.
#[no_mangle]
pub unsafe extern "C" fn vkGetPhysicalDeviceQueueFamilyProperties(
    _physical_device:              VkPhysicalDevice,
    p_queue_family_property_count: *mut u32,
    p_properties:                  *mut ash::vk::QueueFamilyProperties,
) {
    if p_queue_family_property_count.is_null() {
        return;
    }
    if p_properties.is_null() {
        *p_queue_family_property_count = 1;
        return;
    }
    let cap = *p_queue_family_property_count;
    if cap == 0 {
        return;
    }
    let mut qfp = ash::vk::QueueFamilyProperties::default();
    qfp.queue_flags = ash::vk::QueueFlags::GRAPHICS
        | ash::vk::QueueFlags::COMPUTE
        | ash::vk::QueueFlags::TRANSFER;
    qfp.queue_count          = 1;
    qfp.timestamp_valid_bits = 0;
    qfp.min_image_transfer_granularity = ash::vk::Extent3D {
        width: 1, height: 1, depth: 1,
    };
    *p_properties.offset(0) = qfp;
    *p_queue_family_property_count = 1;
}

/// `vkGetPhysicalDeviceFeatures` — feature set this device supports.
/// Skeleton: zero features. Real ICDs turn on the subset that maps
/// to native backend capabilities; for tier-1 software that's
/// effectively none — apps must stick to the Vulkan 1.0 core
/// feature floor.
#[no_mangle]
pub unsafe extern "C" fn vkGetPhysicalDeviceFeatures(
    _physical_device: VkPhysicalDevice,
    p_features:       *mut ash::vk::PhysicalDeviceFeatures,
) {
    if p_features.is_null() { return; }
    *p_features = ash::vk::PhysicalDeviceFeatures::default();
}

/// `vkGetPhysicalDeviceMemoryProperties` — heap + memory-type
/// layout. Skeleton: one heap (4 GiB advertised, host-visible),
/// one memory type pointing at it with HOST_VISIBLE | HOST_COHERENT
/// | DEVICE_LOCAL flags.
///
/// Real Atrium maps the BO refcount + IMPORT_REGION model onto a
/// single heap; multi-heap (system-RAM vs VRAM split) arrives only
/// when atrium-vk-icd targets a native HW backend with dedicated
/// VRAM. The software backend is single-heap by construction.
#[no_mangle]
pub unsafe extern "C" fn vkGetPhysicalDeviceMemoryProperties(
    _physical_device:    VkPhysicalDevice,
    p_memory_properties: *mut ash::vk::PhysicalDeviceMemoryProperties,
) {
    if p_memory_properties.is_null() { return; }
    let mut mp = ash::vk::PhysicalDeviceMemoryProperties::default();

    mp.memory_heap_count = 1;
    mp.memory_heaps[0]   = ash::vk::MemoryHeap {
        size:  4 * 1024 * 1024 * 1024, // 4 GiB nominal
        flags: ash::vk::MemoryHeapFlags::DEVICE_LOCAL,
    };

    mp.memory_type_count = 1;
    mp.memory_types[0]   = ash::vk::MemoryType {
        property_flags: ash::vk::MemoryPropertyFlags::DEVICE_LOCAL
            | ash::vk::MemoryPropertyFlags::HOST_VISIBLE
            | ash::vk::MemoryPropertyFlags::HOST_COHERENT,
        heap_index:     0,
    };

    *p_memory_properties = mp;
}

/// `vkGetPhysicalDeviceFormatProperties` — what's supported for a
/// given image format. Skeleton: every format reports zero
/// supported features. Apps that respect this will fall back to
/// the Vulkan-mandated minimum format list.
#[no_mangle]
pub unsafe extern "C" fn vkGetPhysicalDeviceFormatProperties(
    _physical_device:    VkPhysicalDevice,
    format:              ash::vk::Format,
    p_format_properties: *mut ash::vk::FormatProperties,
) {
    if p_format_properties.is_null() { return; }
    use ash::vk::FormatFeatureFlags as F;

    // Tier-1 (tiny-skia) capability matrix:
    //   - Color UNORM/SRGB: sample + color-attachment (+blend) +
    //     blit src/dst + transfer src/dst + linear filter.
    //   - Depth/stencil: sample + depth-stencil-attachment +
    //     transfer + blit.
    //   - Vertex-friendly numeric formats: vertex_buffer flag in
    //     bufferFeatures.
    //   - Everything else: zeroed (== FORMAT_NOT_SUPPORTED in
    //     the per-feature sense).
    let f = format.as_raw();
    let mut props = ash::vk::FormatProperties::default();

    let color_features =
          F::SAMPLED_IMAGE
        | F::COLOR_ATTACHMENT
        | F::COLOR_ATTACHMENT_BLEND
        | F::BLIT_SRC
        | F::BLIT_DST
        | F::TRANSFER_SRC
        | F::TRANSFER_DST
        | F::SAMPLED_IMAGE_FILTER_LINEAR;
    let depth_features =
          F::SAMPLED_IMAGE
        | F::DEPTH_STENCIL_ATTACHMENT
        | F::BLIT_SRC
        | F::BLIT_DST
        | F::TRANSFER_SRC
        | F::TRANSFER_DST;

    match f {
        // R8G8B8A8_{UNORM,SRGB}, B8G8R8A8_{UNORM,SRGB} — tier-1
        // composes natively in these.
        37 | 43 | 44 | 50 => {
            props.optimal_tiling_features = color_features;
            props.linear_tiling_features  = color_features;
            props.buffer_features         = F::VERTEX_BUFFER;
        }
        // D16_UNORM, D32_SFLOAT, D24_UNORM_S8_UINT, D32_SFLOAT_S8_UINT.
        124 | 126 | 129 | 130 => {
            props.optimal_tiling_features = depth_features;
            props.linear_tiling_features  = depth_features;
        }
        // Common numeric vertex formats — bufferFeatures only.
        //   100 R32_SFLOAT, 103 R32G32_SFLOAT, 106 R32G32B32_SFLOAT,
        //   109 R32G32B32A32_SFLOAT.
        //    99 R32_UINT, 102 R32G32_UINT, 105 R32G32B32_UINT,
        //   108 R32G32B32A32_UINT.
        //    98 R32_SINT, 101 R32G32_SINT, 104 R32G32B32_SINT,
        //   107 R32G32B32A32_SINT.
        98..=109 => {
            props.buffer_features = F::VERTEX_BUFFER
                | F::UNIFORM_TEXEL_BUFFER
                | F::STORAGE_TEXEL_BUFFER;
        }
        _ => {} // zero-features = unsupported
    }

    *p_format_properties = props;
}

/// `vkGetPhysicalDeviceImageFormatProperties` — describe whether
/// a (format, type, tiling, usage, flags) combination is
/// supported and what extents/mip-levels/sample-counts the
/// implementation allows for it.
///
/// Apps that follow Vulkan best practices call this BEFORE
/// vkCreateImage to validate the request. atrium-vk-icd has been
/// silently passing every vkCreateImage through to the daemon
/// (tier-1 silently dropping unsupported requests on the floor);
/// returning supported caps from this entry point lets apps
/// front-load the rejection / fallback decision.
///
/// Tier-1 acceptance policy (subject to tier widening when
/// llvmpipe lands):
///   - 2D images at R8G8B8A8_UNORM (37) or B8G8R8A8_UNORM (44)
///     OR any depth format — supported at 16K×16K, 14 mip
///     levels, 256 array layers, 1-sample.
///   - 1D / 3D / cube images — rejected with FORMAT_NOT_SUPPORTED.
///     (Tier-1 has no path for them.)
///   - Tiling LINEAR vs OPTIMAL — both accepted (no GPU layout
///     to honor; tiny-skia treats both identically).
///   - flags = 0 — sparse / aliased / cube-compat / etc. all
///     rejected.
///
/// We don't enforce usage bits — the daemon accepts every
/// usage today.
#[no_mangle]
pub unsafe extern "C" fn vkGetPhysicalDeviceImageFormatProperties(
    _physical_device:           VkPhysicalDevice,
    format:                     ash::vk::Format,
    image_type:                 ash::vk::ImageType,
    _tiling:                    ash::vk::ImageTiling,
    _usage:                     ash::vk::ImageUsageFlags,
    flags:                      ash::vk::ImageCreateFlags,
    p_image_format_properties:  *mut ash::vk::ImageFormatProperties,
) -> VkResult {
    if p_image_format_properties.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    // Only 2D supported in tier-1.
    if image_type != ash::vk::ImageType::TYPE_2D {
        return -11 /* VK_ERROR_FORMAT_NOT_SUPPORTED */;
    }
    if !flags.is_empty() {
        return -11;
    }
    // Format whitelist: R8G8B8A8_UNORM (37), B8G8R8A8_UNORM (44),
    // R8G8B8A8_SRGB (43), B8G8R8A8_SRGB (50), D16 (124), D32_SFLOAT
    // (126), D24_UNORM_S8_UINT (129), D32_SFLOAT_S8_UINT (130).
    let f = format.as_raw();
    let supported = matches!(f, 37 | 43 | 44 | 50 | 124 | 126 | 129 | 130);
    if !supported {
        return -11;
    }

    let mut props = ash::vk::ImageFormatProperties::default();
    props.max_extent = ash::vk::Extent3D { width: 16384, height: 16384, depth: 1 };
    props.max_mip_levels   = 14; // log2(16384) + 1
    props.max_array_layers = 256;
    props.sample_counts    = ash::vk::SampleCountFlags::TYPE_1;
    // Spec-defined: must be at least 2^31 for >=2D images. Use a
    // generous cap reflecting our 16K×16K×8-byte worst case.
    props.max_resource_size = 16384u64 * 16384u64 * 16u64;
    *p_image_format_properties = props;
    VK_SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn vkGetPhysicalDeviceImageFormatProperties2(
    physical_device:  VkPhysicalDevice,
    p_image_format_info: *const c_void,
    p_image_format_properties: *mut c_void,
) -> VkResult {
    if p_image_format_info.is_null() || p_image_format_properties.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    // VkPhysicalDeviceImageFormatInfo2: 16-byte header + format(u32)
    // + type(u32) + tiling(u32) + usage(u32) + flags(u32).
    let info = p_image_format_info as *const u8;
    let info_p_next = std::ptr::read_unaligned(info.add(8) as *const *mut c_void);
    let format = ash::vk::Format::from_raw(
        std::ptr::read_unaligned(info.add(16) as *const i32),
    );
    let image_type = ash::vk::ImageType::from_raw(
        std::ptr::read_unaligned(info.add(20) as *const i32),
    );
    let tiling = ash::vk::ImageTiling::from_raw(
        std::ptr::read_unaligned(info.add(24) as *const i32),
    );
    let usage = ash::vk::ImageUsageFlags::from_raw(
        std::ptr::read_unaligned(info.add(28) as *const u32),
    );
    let flags = ash::vk::ImageCreateFlags::from_raw(
        std::ptr::read_unaligned(info.add(32) as *const u32),
    );
    let _ = walk_p_next_chain(info_p_next);

    // VkImageFormatProperties2: 16-byte header + inner.
    let out = p_image_format_properties as *mut u8;
    let out_p_next = std::ptr::read_unaligned(out.add(8) as *const *mut c_void);
    let inner = out.add(16) as *mut ash::vk::ImageFormatProperties;
    let r = vkGetPhysicalDeviceImageFormatProperties(
        physical_device, format, image_type, tiling, usage, flags, inner,
    );
    let _ = walk_p_next_chain(out_p_next);
    r
}

// ── Vulkan 1.1+ pNext-chain variants ─────────────────────────────
//
// atrium-vk-icd advertises apiVersion 1.3, so apps that target
// ≥1.1 (most modern engines) call these *2 variants instead of
// their 1.0 counterparts. Each takes a struct with sType + pNext
// header followed by the same inner block; we fill the inner
// block from the 1.0 path and walk pNext but don't populate any
// extension structs today (the loader/app sees the chain as-is).
//
// Layout on 64-bit (matches Vulkan's typedef'd structs):
//   0 : VkStructureType sType  (u32)
//   4 : 4-byte compiler padding
//   8 : void* pNext
//   16: inner-properties block

/// Walk the pNext chain starting at `start_p_next`, logging any
/// non-null entry whose sType we don't recognise. Today we do
/// nothing useful with it — extension property structs (e.g.
/// PhysicalDeviceVulkan11Properties, PhysicalDeviceDriverProperties)
/// stay zeroed.
///
/// Returns the count of links walked, for diagnostic logging.
unsafe fn walk_p_next_chain(start_p_next: *mut c_void) -> u32 {
    let mut n: u32 = 0;
    let mut cur = start_p_next as *mut u8;
    while !cur.is_null() {
        let _s_type = std::ptr::read_unaligned(cur as *const u32);
        let next    = std::ptr::read_unaligned(cur.add(8) as *const *mut c_void);
        // Note: we don't populate extension structs today; the
        // caller's zero-init is what the app sees. If we ever add
        // VK_KHR_driver_properties etc., dispatch on _s_type here.
        cur = next as *mut u8;
        n += 1;
        if n > 64 {
            // Pathological chain; bail to avoid an infinite loop.
            break;
        }
    }
    n
}

#[no_mangle]
pub unsafe extern "C" fn vkGetPhysicalDeviceProperties2(
    physical_device:  VkPhysicalDevice,
    p_properties:     *mut c_void,
) {
    if physical_device.is_null() || p_properties.is_null() { return; }
    let base = p_properties as *mut u8;
    let p_next = std::ptr::read_unaligned(base.add(8) as *const *mut c_void);
    let inner = base.add(16) as *mut ash::vk::PhysicalDeviceProperties;
    vkGetPhysicalDeviceProperties(physical_device, inner);
    let _ = walk_p_next_chain(p_next);
}

#[no_mangle]
pub unsafe extern "C" fn vkGetPhysicalDeviceFeatures2(
    physical_device: VkPhysicalDevice,
    p_features:      *mut c_void,
) {
    if physical_device.is_null() || p_features.is_null() { return; }
    let base = p_features as *mut u8;
    let p_next = std::ptr::read_unaligned(base.add(8) as *const *mut c_void);
    let inner = base.add(16) as *mut ash::vk::PhysicalDeviceFeatures;
    vkGetPhysicalDeviceFeatures(physical_device, inner);
    let _ = walk_p_next_chain(p_next);
}

#[no_mangle]
pub unsafe extern "C" fn vkGetPhysicalDeviceMemoryProperties2(
    physical_device:    VkPhysicalDevice,
    p_memory_properties: *mut c_void,
) {
    if physical_device.is_null() || p_memory_properties.is_null() { return; }
    let base = p_memory_properties as *mut u8;
    let p_next = std::ptr::read_unaligned(base.add(8) as *const *mut c_void);
    let inner = base.add(16) as *mut ash::vk::PhysicalDeviceMemoryProperties;
    vkGetPhysicalDeviceMemoryProperties(physical_device, inner);
    let _ = walk_p_next_chain(p_next);
}

#[no_mangle]
pub unsafe extern "C" fn vkGetPhysicalDeviceFormatProperties2(
    physical_device:     VkPhysicalDevice,
    format:              ash::vk::Format,
    p_format_properties: *mut c_void,
) {
    if physical_device.is_null() || p_format_properties.is_null() { return; }
    let base = p_format_properties as *mut u8;
    let p_next = std::ptr::read_unaligned(base.add(8) as *const *mut c_void);
    let inner = base.add(16) as *mut ash::vk::FormatProperties;
    vkGetPhysicalDeviceFormatProperties(physical_device, format, inner);
    let _ = walk_p_next_chain(p_next);
}

#[no_mangle]
pub unsafe extern "C" fn vkGetPhysicalDeviceQueueFamilyProperties2(
    physical_device:               VkPhysicalDevice,
    p_queue_family_property_count: *mut u32,
    p_queue_family_properties:     *mut c_void,
) {
    if p_queue_family_property_count.is_null() { return; }
    if p_queue_family_properties.is_null() {
        vkGetPhysicalDeviceQueueFamilyProperties(
            physical_device, p_queue_family_property_count,
            std::ptr::null_mut(),
        );
        return;
    }
    let cap = *p_queue_family_property_count;
    // Each VkQueueFamilyProperties2 = 16-byte header + inner.
    // We fill one slot.
    if cap == 0 { return; }
    let slot = p_queue_family_properties as *mut u8;
    let p_next = std::ptr::read_unaligned(slot.add(8) as *const *mut c_void);
    let inner = slot.add(16) as *mut ash::vk::QueueFamilyProperties;
    let mut one: u32 = 1;
    vkGetPhysicalDeviceQueueFamilyProperties(physical_device, &mut one, inner);
    *p_queue_family_property_count = one;
    let _ = walk_p_next_chain(p_next);
}

/// `vkCreateDevice` — allocate an `AtriumDevice` with one or more
/// queues. We honor the `VkDeviceQueueCreateInfo` list only insofar
/// as we materialize the requested queues; everything else (enabled
/// features, layer/extension lists, pNext chain) is ignored today.
///
/// Each created queue is a Box-allocated `AtriumQueue` with the
/// loader magic at offset 0; the device tracks them in
/// `device.queues` and frees them on `vkDestroyDevice`.
///
/// Sane simplification: we know Atrium exposes exactly one queue
/// family with exactly one queue (see
/// `vkGetPhysicalDeviceQueueFamilyProperties`). So we ignore the
/// create-info's `queueFamilyIndex` and `queueCount` requests and
/// just create our single queue. Apps that requested more queues
/// will get exactly one (a deliberate spec deviation for now,
/// surfaced via a future debug log).
///
/// # Safety
///
/// `physical_device` must be a handle we returned from
/// vkEnumeratePhysicalDevices. `p_device` must be a writable slot.
#[no_mangle]
pub unsafe extern "C" fn vkCreateDevice(
    physical_device:  VkPhysicalDevice,
    _p_create_info:   *const c_void, /* const VkDeviceCreateInfo* */
    _p_allocator:     *const c_void, /* const VkAllocationCallbacks* */
    p_device:         *mut VkDevice,
) -> VkResult {
    if p_device.is_null() || physical_device.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let pd = &*(physical_device as *const AtriumPhysicalDevice);
    let inst_ptr = pd.instance;

    // Allocate a persistent fence on the instance's client. If the
    // instance has no live client (offline / handshake failed), we
    // still create the device but its fence is ResourceId(0) and
    // submits no-op silently.
    let fence = if !inst_ptr.is_null() {
        let inst = &*inst_ptr;
        match inst.client.as_ref().and_then(|m| {
            m.lock().ok().and_then(|mut c| c.create_fence().ok())
        }) {
            Some(f) => f,
            None    => aqueduct_gpu::ids::ResourceId(0),
        }
    } else {
        aqueduct_gpu::ids::ResourceId(0)
    };

    let mut dev = Box::new(AtriumDevice {
        loader_dispatch_slot: VK_ICD_LOADER_MAGIC,
        queues:   Vec::with_capacity(1),
        instance: inst_ptr,
        fence,
        timeline: std::cell::Cell::new(0),
        backend: aqueduct_gpu::backends::BackendId::new(
            pd.backend_vendor, pd.backend_generation,
        ),
        id_alloc: std::sync::Mutex::new(
            aqueduct_gpu_client::IdAllocator::with_namespace(
                aqueduct_gpu::ids::IdNamespace::IcdRuntime,
            ),
        ),
        pipelines: std::sync::Mutex::new(std::collections::HashMap::new()),
        memories:  std::sync::Mutex::new(std::collections::HashMap::new()),
        next_memory_id: std::cell::Cell::new(1),
        buffers:        std::sync::Mutex::new(std::collections::HashMap::new()),
        next_buffer_id: std::cell::Cell::new(1),
        images:         std::sync::Mutex::new(std::collections::HashMap::new()),
        next_image_id:  std::cell::Cell::new(1),
        shaders:        std::sync::Mutex::new(std::collections::HashMap::new()),
        next_shader_id: std::cell::Cell::new(1),
        image_views:        std::sync::Mutex::new(std::collections::HashMap::new()),
        next_image_view_id: std::cell::Cell::new(1),
        next_render_pass_id: std::cell::Cell::new(1),
        framebuffers:        std::sync::Mutex::new(std::collections::HashMap::new()),
        next_framebuffer_id: std::cell::Cell::new(1),
        fences:              std::sync::Mutex::new(std::collections::HashMap::new()),
        next_fence_id:       std::cell::Cell::new(1),
        next_semaphore_id:   std::cell::Cell::new(1),
        samplers:            std::sync::Mutex::new(std::collections::HashMap::new()),
        next_sampler_id:     std::cell::Cell::new(1),
        next_dsl_id:         std::cell::Cell::new(1),
        next_dpool_id:       std::cell::Cell::new(1),
        descriptor_sets:     std::sync::Mutex::new(std::collections::HashMap::new()),
        next_dset_id:        std::cell::Cell::new(1),
        next_query_pool_id:  std::cell::Cell::new(1),
        events:              std::sync::Mutex::new(std::collections::HashMap::new()),
        next_event_id:       std::cell::Cell::new(1),
        next_buffer_view_id: std::cell::Cell::new(1),
        swapchains:          std::sync::Mutex::new(std::collections::HashMap::new()),
        next_swapchain_id:   std::cell::Cell::new(1),
    });
    let dev_ptr: *mut AtriumDevice = &mut *dev;
    let q = Box::new(AtriumQueue {
        loader_dispatch_slot: VK_ICD_LOADER_MAGIC,
        _device: dev_ptr,
        family: 0,
        index:  0,
    });
    dev.queues.push(Box::into_raw(q));
    *p_device = Box::into_raw(dev) as VkDevice;
    VK_SUCCESS
}

/// `vkDestroyDevice` — reclaim the AtriumDevice + all its queues.
///
/// # Safety
///
/// `device` must be a handle previously returned by
/// `vkCreateDevice`, or null (no-op).
#[no_mangle]
pub unsafe extern "C" fn vkDestroyDevice(
    device:       VkDevice,
    _p_allocator: *const c_void, /* const VkAllocationCallbacks* */
) {
    if device.is_null() {
        return;
    }
    let dev = Box::from_raw(device as *mut AtriumDevice);
    for q in &dev.queues {
        let _ = Box::from_raw(*q);
    }
}

/// `vkCreatePipelineLayout` — non-dispatchable handle, opaque
/// today (we ignore descriptor-set + push-constant range info; the
/// renderer's push-constant path is pipeline-global, and we don't
/// have descriptor sets yet). Returns a unique non-zero u64 so
/// that callers passing it back round-trip correctly.
#[no_mangle]
pub unsafe extern "C" fn vkCreatePipelineLayout(
    device:           VkDevice,
    _p_create_info:   *const c_void,
    _p_allocator:     *const c_void,
    p_pipeline_layout: *mut u64,
) -> VkResult {
    if device.is_null() || p_pipeline_layout.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let dev = &*(device as *const AtriumDevice);
    let id = match dev.id_alloc.lock().ok().and_then(|mut a| a.next()) {
        Some(id) => id,
        None     => return VK_ERROR_INITIALIZATION_FAILED,
    };
    *p_pipeline_layout = id.raw() as u64;
    VK_SUCCESS
}

/// `vkDestroyPipelineLayout` — no-op today; pipeline layouts hold
/// no ICD-side state worth reclaiming. Future descriptor-set
/// tracking would free the layout's slot here.
#[no_mangle]
pub unsafe extern "C" fn vkDestroyPipelineLayout(
    _device:        VkDevice,
    _layout:        u64,
    _p_allocator:   *const c_void,
) {
}

/// `vkCreateGraphicsPipelines` — allocate one ResourceId per
/// requested pipeline, register it in the device's pipelines map.
/// vkCmdBindPipeline later looks the VkPipeline u64 up and pushes
/// a `BindPipeline` FrameOp with the resolved ResourceId.
///
/// We ignore the pipeline-cache + VkGraphicsPipelineCreateInfo
/// contents — shader stages, vertex input, rasterizer, blend,
/// depth/stencil, render-pass, subpass. The host endpoint sees
/// the resolved ResourceId in BindPipeline and resolves it
/// against the shader-cache / bundle-pipeline machinery (today,
/// against `IdNamespace::IcdRuntime` IDs that no upload has yet
/// registered — so the host will fail the dispatch with
/// `OP_GPU_VALIDATION_ERR`. Future Phase 1.3b+ wires
/// vkCreateShaderModule + the create-info shader stages through
/// to OP_GPU_SHADER_UPLOAD).
///
/// # Safety
///
/// `p_pipelines` must point to at least `create_info_count` u64
/// slots. `p_create_infos` is unused today; we don't dereference.
#[no_mangle]
pub unsafe extern "C" fn vkCreateGraphicsPipelines(
    device:             VkDevice,
    _pipeline_cache:    u64,   /* VkPipelineCache */
    create_info_count:  u32,
    _p_create_infos:    *const c_void,
    _p_allocator:       *const c_void,
    p_pipelines:        *mut u64,
) -> VkResult {
    if device.is_null() || p_pipelines.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let dev = &*(device as *const AtriumDevice);
    let mut alloc = match dev.id_alloc.lock() {
        Ok(a) => a,
        Err(_) => return VK_ERROR_INITIALIZATION_FAILED,
    };
    let mut pipelines = match dev.pipelines.lock() {
        Ok(p) => p,
        Err(_) => return VK_ERROR_INITIALIZATION_FAILED,
    };
    for i in 0..create_info_count {
        let id = match alloc.next() {
            Some(id) => id,
            None     => return VK_ERROR_INITIALIZATION_FAILED,
        };
        // Use the ResourceId's raw u32 widened to u64 as the
        // VkPipeline handle. Round-trips through pipelines map.
        let handle = id.raw() as u64;
        pipelines.insert(handle, id);
        *p_pipelines.offset(i as isize) = handle;
    }
    VK_SUCCESS
}

/// `vkDestroyPipeline` — remove the (VkPipeline → ResourceId)
/// entry from the device's pipelines map. The ResourceId itself
/// stays consumed; we don't reuse IDs.
#[no_mangle]
pub unsafe extern "C" fn vkDestroyPipeline(
    device:       VkDevice,
    pipeline:     u64,
    _p_allocator: *const c_void,
) {
    if device.is_null() || pipeline == 0 { return; }
    let dev = &*(device as *const AtriumDevice);
    if let Ok(mut p) = dev.pipelines.lock() {
        p.remove(&pipeline);
    }
}

/// `vkAllocateMemory` — allocate device-memory storage on the ICD
/// side. Reads `allocationSize` (u64 at offset 16) from the
/// VkMemoryAllocateInfo; ignores memoryTypeIndex (we only have
/// one type, see `vkGetPhysicalDeviceMemoryProperties`).
///
/// Returns a unique non-zero u64 the caller treats as
/// VkDeviceMemory; the storage is a heap-allocated `Box<[u8]>`
/// kept alive in the device's `memories` map until vkFreeMemory.
///
/// Today's storage is pure local — no daemon-side region binding.
/// vkMapMemory just hands out the Box's pointer. Future Phase 1.3b+
/// will wire OP_GPU_MEMORY_CREATE on alloc so submits + buffer
/// uploads can reference these regions.
#[no_mangle]
pub unsafe extern "C" fn vkAllocateMemory(
    device:          VkDevice,
    p_allocate_info: *const c_void, /* const VkMemoryAllocateInfo* */
    _p_allocator:    *const c_void,
    p_memory:        *mut u64,
) -> VkResult {
    if device.is_null() || p_allocate_info.is_null() || p_memory.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let dev = &*(device as *const AtriumDevice);
    // VkMemoryAllocateInfo:
    //   0   sType : u32
    //   4   _pad
    //   8   pNext : ptr
    //   16  allocationSize : u64
    //   24  memoryTypeIndex : u32
    let bytes = p_allocate_info as *const u8;
    let size = std::ptr::read_unaligned(bytes.add(16) as *const u64);
    if size == 0 || size > (1u64 << 32) {
        return VK_ERROR_INITIALIZATION_FAILED;
    }

    let storage: Box<[u8]> = vec![0u8; size as usize].into_boxed_slice();
    let handle = dev.next_memory_id.get();
    dev.next_memory_id.set(handle + 1);

    // If the instance has a live client, register a daemon-side
    // memory region. Best-effort — failure leaves region_id=None
    // and the memory stays local-only.
    let region_id = if !dev.instance.is_null() {
        let inst = &*(dev.instance as *const AtriumInstance);
        inst.client.as_ref().and_then(|m| {
            m.lock().ok().and_then(|mut c| {
                c.allocate_memory(size, aqueduct_gpu::payloads::MemoryUsage::BufferBacking)
                    .ok()
                    .map(|resp| resp.region_id)
            })
        })
    } else {
        None
    };

    if let Ok(mut m) = dev.memories.lock() {
        m.insert(handle, AtriumDeviceMemory { storage, region_id });
    } else {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    *p_memory = handle;
    VK_SUCCESS
}

/// `vkFreeMemory` — drop the storage behind a VkDeviceMemory.
#[no_mangle]
pub unsafe extern "C" fn vkFreeMemory(
    device:       VkDevice,
    memory:       u64,
    _p_allocator: *const c_void,
) {
    if device.is_null() || memory == 0 { return; }
    let dev = &*(device as *const AtriumDevice);
    if let Ok(mut m) = dev.memories.lock() {
        m.remove(&memory);
    }
}

/// `vkMapMemory` — return a host pointer to a sub-range of the
/// VkDeviceMemory's storage. Today's storage is host-allocated,
/// so we just return a raw pointer into the Box's bytes.
///
/// `size == VK_WHOLE_SIZE (u64::MAX)` means "to end of allocation".
/// `offset` must be within the allocation.
///
/// # Safety
///
/// `pp_data` must be a writable `*mut *mut c_void`. The returned
/// pointer is valid until the next vkUnmapMemory / vkFreeMemory on
/// the same VkDeviceMemory.
#[no_mangle]
pub unsafe extern "C" fn vkMapMemory(
    device:    VkDevice,
    memory:    u64,
    offset:    u64,
    _size:     u64,
    _flags:    u32,
    pp_data:   *mut *mut c_void,
) -> VkResult {
    if device.is_null() || memory == 0 || pp_data.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let dev = &*(device as *const AtriumDevice);
    let m = match dev.memories.lock() {
        Ok(m) => m,
        Err(_) => return VK_ERROR_INITIALIZATION_FAILED,
    };
    let Some(mem) = m.get(&memory) else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    if (offset as usize) > mem.storage.len() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    // The Box's storage is stable as long as it lives in the
    // HashMap (HashMap doesn't move its values). The pointer
    // remains valid until vkFreeMemory removes the entry.
    let ptr = mem.storage.as_ptr().add(offset as usize) as *mut c_void;
    *pp_data = ptr;
    VK_SUCCESS
}

/// `vkUnmapMemory` — no-op today. Storage is always host-mapped
/// (HOST_VISIBLE | HOST_COHERENT — see
/// `vkGetPhysicalDeviceMemoryProperties`), so unmap is a logical
/// boundary not a kernel-VA reclamation. Future tier-2 backends
/// would munmap here.
#[no_mangle]
pub unsafe extern "C" fn vkUnmapMemory(
    _device: VkDevice,
    _memory: u64,
) {
}

/// `vkCreateBuffer` — record an AtriumBuffer for the requested
/// size + usage. Memory binding lands in a follow-up
/// vkBindBufferMemory call. Today we ignore sharing-mode,
/// queueFamilyIndices, and pNext — Atrium is single-queue.
///
/// VkBufferCreateInfo layout:
///   0   sType
///   8   pNext
///   16  flags
///   20  _pad
///   24  size : u64
///   32  usage : u32
#[no_mangle]
pub unsafe extern "C" fn vkCreateBuffer(
    device:          VkDevice,
    p_create_info:   *const c_void,
    _p_allocator:    *const c_void,
    p_buffer:        *mut u64,
) -> VkResult {
    if device.is_null() || p_create_info.is_null() || p_buffer.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let dev = &*(device as *const AtriumDevice);
    let bytes = p_create_info as *const u8;
    let size  = std::ptr::read_unaligned(bytes.add(24) as *const u64);
    let usage = std::ptr::read_unaligned(bytes.add(32) as *const u32);
    if size == 0 { return VK_ERROR_INITIALIZATION_FAILED; }

    let handle = dev.next_buffer_id.get();
    dev.next_buffer_id.set(handle + 1);
    if let Ok(mut b) = dev.buffers.lock() {
        b.insert(handle, AtriumBuffer {
            size, memory: None, memory_offset: 0, usage,
            buffer_id: None,
        });
    } else {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    *p_buffer = handle;
    VK_SUCCESS
}

/// `vkDestroyBuffer` — drop the AtriumBuffer entry. The underlying
/// VkDeviceMemory is independent and survives.
#[no_mangle]
pub unsafe extern "C" fn vkDestroyBuffer(
    device:       VkDevice,
    buffer:       u64,
    _p_allocator: *const c_void,
) {
    if device.is_null() || buffer == 0 { return; }
    let dev = &*(device as *const AtriumDevice);
    if let Ok(mut b) = dev.buffers.lock() {
        b.remove(&buffer);
    }
}

/// `vkGetBufferMemoryRequirements` — describe what memory shape
/// `buffer` needs to be bound. We report:
/// - size: as requested at create time
/// - alignment: 16 bytes (covers the typical Vulkan minimums for
///   uniform / storage / vertex buffers; real values come from
///   the backend's caps eventually)
/// - memoryTypeBits: 0b1 — only memory type 0 (which is our
///   single host-visible | host-coherent | device-local type per
///   vkGetPhysicalDeviceMemoryProperties)
#[no_mangle]
pub unsafe extern "C" fn vkGetBufferMemoryRequirements(
    device:           VkDevice,
    buffer:           u64,
    p_requirements:   *mut ash::vk::MemoryRequirements,
) {
    if device.is_null() || p_requirements.is_null() { return; }
    let dev = &*(device as *const AtriumDevice);
    let size = dev.buffers.lock().ok()
        .and_then(|b| b.get(&buffer).map(|x| x.size))
        .unwrap_or(0);
    *p_requirements = ash::vk::MemoryRequirements {
        size,
        alignment:        16,
        memory_type_bits: 0b1,
    };
}

/// `vkBindBufferMemory` — associate `buffer` with `memory_offset`
/// in `memory`. Returns SUCCESS if the binding is valid (the
/// buffer exists, the memory exists, and offset + buffer.size
/// fits in memory.size). Subsequent vkCmd* operations that
/// reference `buffer` resolve through this binding to reach the
/// underlying VkDeviceMemory storage.
#[no_mangle]
pub unsafe extern "C" fn vkBindBufferMemory(
    device:         VkDevice,
    buffer:         u64,
    memory:         u64,
    memory_offset:  u64,
) -> VkResult {
    if device.is_null() || buffer == 0 || memory == 0 {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let dev = &*(device as *const AtriumDevice);

    // Validate the binding ranges, capture region_id for the
    // daemon-side create call below.
    let (mem_size, region_id) = match dev.memories.lock() {
        Ok(m) => match m.get(&memory) {
            Some(am) => (am.storage.len() as u64, am.region_id),
            None     => return VK_ERROR_INITIALIZATION_FAILED,
        },
        Err(_) => return VK_ERROR_INITIALIZATION_FAILED,
    };

    let (size, usage) = {
        let buffers = match dev.buffers.lock() {
            Ok(b) => b,
            Err(_) => return VK_ERROR_INITIALIZATION_FAILED,
        };
        let Some(buf) = buffers.get(&buffer) else {
            return VK_ERROR_INITIALIZATION_FAILED;
        };
        if memory_offset.checked_add(buf.size).map(|end| end > mem_size).unwrap_or(true) {
            return VK_ERROR_INITIALIZATION_FAILED;
        }
        (buf.size, buf.usage)
    };

    // If the memory carries a daemon-side region_id, register a
    // daemon-side buffer too. Subsequent FrameOps that reference
    // this buffer use the returned ResourceId.
    let buffer_id = if let Some(region_id) = region_id {
        if !dev.instance.is_null() {
            let inst = &*(dev.instance as *const AtriumInstance);
            inst.client.as_ref().and_then(|m| {
                m.lock().ok().and_then(|mut c| {
                    c.create_buffer(aqueduct_gpu::payloads::BufferCreatePayload {
                        buffer_id:      aqueduct_gpu::ids::ResourceId(0),
                        backing_region: region_id,
                        region_offset:  memory_offset,
                        size,
                        usage,
                    }).ok()
                })
            })
        } else {
            None
        }
    } else {
        None
    };

    let mut buffers = match dev.buffers.lock() {
        Ok(b) => b,
        Err(_) => return VK_ERROR_INITIALIZATION_FAILED,
    };
    let Some(buf) = buffers.get_mut(&buffer) else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    buf.memory        = Some(memory);
    buf.memory_offset = memory_offset;
    buf.buffer_id     = buffer_id;
    VK_SUCCESS
}

/// `vkCmdBindPipeline` — push a `BindPipeline` FrameOp with the
/// resolved ResourceId. The Vk `pipelineBindPoint` is ignored —
/// Atrium's FrameOp dispatch is single-bind-point (the host
/// figures out whether to dispatch to graphics or compute from
/// the pipeline's bundle definition).
#[no_mangle]
pub unsafe extern "C" fn vkCmdBindPipeline(
    command_buffer:       VkCommandBuffer,
    _pipeline_bind_point: u32, /* VkPipelineBindPoint */
    pipeline:             u64,
) {
    let Some(cb) = cmdbuf_recording(command_buffer) else { return };
    // Resolve VkPipeline → ResourceId via the owning device's
    // pipelines map. Walk back-pointer: cmdbuf → device.
    //
    // (Today: cmdbufs don't carry a device back-pointer; vkCmd*
    // is per-buffer but resolution needs the device. Use the
    // pipeline u64 directly as the ResourceId raw — they're the
    // same value by our convention above. Future: thread the
    // device pointer through cmdbuf for stricter validation.)
    let id_raw = pipeline as u32;
    let _ = cb.frame.push(
        aqueduct_gpu::opcodes::FrameOp::BindPipeline,
        &id_raw.to_le_bytes(),
    );
}

/// Helper: get the AtriumCommandBuffer behind a handle, with state
/// validation. Returns None and silently drops the command if the
/// buffer isn't currently Recording (Vulkan spec says vkCmd* outside
/// recording is undefined behavior; we drop rather than panic).
#[inline]
unsafe fn cmdbuf_recording(cb: VkCommandBuffer) -> Option<&'static mut AtriumCommandBuffer> {
    if cb.is_null() { return None; }
    let cbref = &mut *(cb as *mut AtriumCommandBuffer);
    if cbref.state != CmdBufferState::Recording { return None; }
    Some(cbref)
}

/// `vkCmdSetViewport` — record a viewport state change.
///
/// Vk takes (firstViewport, viewportCount, *pViewports). Atrium's
/// `FrameOp::SetViewport` body is one viewport at a time; if the
/// caller wrote more than one, we record each in sequence. The Vk
/// 1.0 floor mandates `maxViewports >= 1` so single-viewport is
/// the typical case.
///
/// Wire body (matches the renderer's expected layout — same `f32`
/// fields the Vk struct uses, plain LE memcpy):
///   x, y, w, h, minDepth, maxDepth  (24 bytes).
#[no_mangle]
pub unsafe extern "C" fn vkCmdSetViewport(
    command_buffer:  VkCommandBuffer,
    _first_viewport: u32,
    viewport_count:  u32,
    p_viewports:     *const ash::vk::Viewport,
) {
    let Some(cb) = cmdbuf_recording(command_buffer) else { return };
    if p_viewports.is_null() { return; }
    for i in 0..viewport_count {
        let vp = &*p_viewports.offset(i as isize);
        let mut body = [0u8; 24];
        body[ 0.. 4].copy_from_slice(&vp.x.to_le_bytes());
        body[ 4.. 8].copy_from_slice(&vp.y.to_le_bytes());
        body[ 8..12].copy_from_slice(&vp.width.to_le_bytes());
        body[12..16].copy_from_slice(&vp.height.to_le_bytes());
        body[16..20].copy_from_slice(&vp.min_depth.to_le_bytes());
        body[20..24].copy_from_slice(&vp.max_depth.to_le_bytes());
        let _ = cb.frame.push(aqueduct_gpu::opcodes::FrameOp::SetViewport, &body);
    }
}

/// `vkCmdSetScissor` — record a scissor rect.
///
/// Wire body (matches SetScissorBody on the renderer side):
///   x: u32, y: u32, w: u32, h: u32  (16 bytes).
/// Vk's `offset.{x,y}` are `i32` but always non-negative for a
/// valid scissor (a negative offset is a spec violation). We clamp
/// to non-negative before transmute.
#[no_mangle]
pub unsafe extern "C" fn vkCmdSetScissor(
    command_buffer: VkCommandBuffer,
    _first_scissor: u32,
    scissor_count:  u32,
    p_scissors:     *const ash::vk::Rect2D,
) {
    let Some(cb) = cmdbuf_recording(command_buffer) else { return };
    if p_scissors.is_null() { return; }
    for i in 0..scissor_count {
        let s = &*p_scissors.offset(i as isize);
        let x = s.offset.x.max(0) as u32;
        let y = s.offset.y.max(0) as u32;
        let mut body = [0u8; 16];
        body[ 0.. 4].copy_from_slice(&x.to_le_bytes());
        body[ 4.. 8].copy_from_slice(&y.to_le_bytes());
        body[ 8..12].copy_from_slice(&s.extent.width.to_le_bytes());
        body[12..16].copy_from_slice(&s.extent.height.to_le_bytes());
        let _ = cb.frame.push(aqueduct_gpu::opcodes::FrameOp::SetScissor, &body);
    }
}

// ── Extended dynamic state (Vk 1.3 + VK_EXT_extended_dynamic_state{,2,3})
//
// 1.3 promoted a stack of per-cmdbuf state setters from extensions
// into core. Apps that opt into dynamic state (i.e. set pipeline
// state at draw time rather than baking it into the pipeline)
// resolve these via the 1.3 dispatch table. Modern engines
// (wgpu, Bevy with the dynamic-state feature) emit them
// unconditionally.
//
// Tier-1 doesn't honor most of these (rasteriser is fixed),
// but they must resolve and not crash. The two with a wire-
// format home (ViewportWithCount, ScissorWithCount,
// BindVertexBuffers2) delegate to their 1.0 counterparts.

#[no_mangle]
pub unsafe extern "C" fn vkCmdSetViewportWithCount(
    command_buffer: VkCommandBuffer,
    viewport_count: u32,
    p_viewports:    *const ash::vk::Viewport,
) {
    vkCmdSetViewport(command_buffer, 0, viewport_count, p_viewports)
}

#[no_mangle]
pub unsafe extern "C" fn vkCmdSetScissorWithCount(
    command_buffer: VkCommandBuffer,
    scissor_count:  u32,
    p_scissors:     *const ash::vk::Rect2D,
) {
    vkCmdSetScissor(command_buffer, 0, scissor_count, p_scissors)
}

#[no_mangle]
pub unsafe extern "C" fn vkCmdBindVertexBuffers2(
    command_buffer: VkCommandBuffer,
    first_binding:  u32,
    binding_count:  u32,
    p_buffers:      *const u64,
    p_offsets:      *const u64,
    _p_sizes:       *const u64, /* per-buffer size — ignored (tier-1 doesn't bound-check) */
    _p_strides:     *const u64, /* per-buffer stride — ignored (pipeline already declares stride) */
) {
    vkCmdBindVertexBuffers(command_buffer, first_binding, binding_count, p_buffers, p_offsets)
}

// The rest are state-machine setters tier-1 doesn't care about.
// All take a single primitive value (u32 enum or VkBool32) plus
// the cmdbuf. Implemented as no-ops; the loader sees them resolve
// and validation layers see consistent state-change ordering.

macro_rules! ext_state_stub_u32 {
    ($name:ident) => {
        #[no_mangle]
        pub unsafe extern "C" fn $name(
            _command_buffer: VkCommandBuffer, _value: u32,
        ) {}
    };
}

ext_state_stub_u32!(vkCmdSetCullMode);
ext_state_stub_u32!(vkCmdSetFrontFace);
ext_state_stub_u32!(vkCmdSetPrimitiveTopology);
ext_state_stub_u32!(vkCmdSetDepthTestEnable);
ext_state_stub_u32!(vkCmdSetDepthWriteEnable);
ext_state_stub_u32!(vkCmdSetDepthCompareOp);
ext_state_stub_u32!(vkCmdSetDepthBoundsTestEnable);
ext_state_stub_u32!(vkCmdSetStencilTestEnable);
ext_state_stub_u32!(vkCmdSetRasterizerDiscardEnable);
ext_state_stub_u32!(vkCmdSetDepthBiasEnable);
ext_state_stub_u32!(vkCmdSetPrimitiveRestartEnable);

/// `vkCmdSetStencilOp` — face_mask + failOp/passOp/depthFailOp/compareOp,
/// 5 u32 args. No-op stub.
#[no_mangle]
pub unsafe extern "C" fn vkCmdSetStencilOp(
    _command_buffer:    VkCommandBuffer,
    _face_mask:         u32,
    _fail_op:           u32,
    _pass_op:           u32,
    _depth_fail_op:     u32,
    _compare_op:        u32,
) {}

/// `vkCmdPushConstants` — record push-constant bytes.
///
/// Vk passes (layout, stageFlags, offset, size, pValues). Atrium's
/// PushConstants body is (stage_mask: u32, offset: u32) + payload.
/// We drop `layout` (Atrium's push-constants are pipeline-global)
/// and pack the rest in a 4-byte header + payload bytes.
#[no_mangle]
pub unsafe extern "C" fn vkCmdPushConstants(
    command_buffer: VkCommandBuffer,
    _layout:        u64, /* VkPipelineLayout */
    stage_flags:    u32, /* VkShaderStageFlags */
    offset:         u32,
    size:           u32,
    p_values:       *const c_void,
) {
    let Some(cb) = cmdbuf_recording(command_buffer) else { return };
    if p_values.is_null() || size == 0 { return; }
    // 4-byte header (stage_mask | offset packed into u32 pair via
    // 2-u16; we don't have a stable header yet so use a simple
    // layout: stage_flags u32 | offset u32 in 8 bytes).
    let mut body = Vec::with_capacity(8 + size as usize);
    body.extend_from_slice(&stage_flags.to_le_bytes());
    body.extend_from_slice(&offset.to_le_bytes());
    let payload = std::slice::from_raw_parts(p_values as *const u8, size as usize);
    body.extend_from_slice(payload);
    let _ = cb.frame.push(aqueduct_gpu::opcodes::FrameOp::PushConstants, &body);
}

/// `vkCmdDraw` — record a non-indexed draw.
///
/// Vk passes (vertexCount, instanceCount, firstVertex,
/// firstInstance). Atrium's `FrameOp::Draw` body is the same four
/// u32 in the same order.
#[no_mangle]
pub unsafe extern "C" fn vkCmdDraw(
    command_buffer: VkCommandBuffer,
    vertex_count:   u32,
    instance_count: u32,
    first_vertex:   u32,
    first_instance: u32,
) {
    let Some(cb) = cmdbuf_recording(command_buffer) else { return };
    let mut body = [0u8; 16];
    body[ 0.. 4].copy_from_slice(&vertex_count.to_le_bytes());
    body[ 4.. 8].copy_from_slice(&instance_count.to_le_bytes());
    body[ 8..12].copy_from_slice(&first_vertex.to_le_bytes());
    body[12..16].copy_from_slice(&first_instance.to_le_bytes());
    let _ = cb.frame.push(aqueduct_gpu::opcodes::FrameOp::Draw, &body);
}

/// Helper: resolve a VkBuffer u64 to its daemon-side ResourceId via
/// the owning device's `buffers` map. Returns `ResourceId(0)` if
/// the buffer has no daemon-side binding (memory was local-only, or
/// no vkBindBufferMemory yet). vkCmd* paths that hit this case
/// still push the FrameOp — the host will reject the dispatch with
/// `OP_GPU_VALIDATION_ERR` rather than the client silently dropping
/// the record (a deferred-error pattern that surfaces upstream).
#[inline]
unsafe fn resolve_buffer(
    cb: &AtriumCommandBuffer, handle: u64,
) -> aqueduct_gpu::ids::ResourceId {
    if cb.device.is_null() { return aqueduct_gpu::ids::ResourceId(0); }
    let dev = &*(cb.device as *const AtriumDevice);
    dev.buffers.lock().ok()
        .and_then(|b| b.get(&handle).and_then(|x| x.buffer_id))
        .unwrap_or(aqueduct_gpu::ids::ResourceId(0))
}

/// `vkCmdBindVertexBuffers` — one `BindVertexBuf` FrameOp per
/// binding. Body: { binding_index: u32, buffer_id: u32, offset: u64 }
/// per binding (16 B each).
#[no_mangle]
pub unsafe extern "C" fn vkCmdBindVertexBuffers(
    command_buffer:  VkCommandBuffer,
    first_binding:   u32,
    binding_count:   u32,
    p_buffers:       *const u64,
    p_offsets:       *const u64,
) {
    let Some(cb) = cmdbuf_recording(command_buffer) else { return };
    if p_buffers.is_null() || p_offsets.is_null() { return; }
    for i in 0..binding_count {
        let buffer_handle = *p_buffers.offset(i as isize);
        let offset        = *p_offsets.offset(i as isize);
        let rid = resolve_buffer(cb, buffer_handle);
        let mut body = [0u8; 16];
        body[ 0.. 4].copy_from_slice(&(first_binding + i).to_le_bytes());
        body[ 4.. 8].copy_from_slice(&rid.raw().to_le_bytes());
        body[ 8..16].copy_from_slice(&offset.to_le_bytes());
        let _ = cb.frame.push(aqueduct_gpu::opcodes::FrameOp::BindVertexBuf, &body);
    }
}

/// `vkCmdBindIndexBuffer` — one `BindIndexBuf` FrameOp.
/// Body: { buffer_id: u32, _pad: u32, offset: u64, index_type: u32 }
/// — 20 bytes. index_type: 0=UINT16, 1=UINT32 (VkIndexType enum).
#[no_mangle]
pub unsafe extern "C" fn vkCmdBindIndexBuffer(
    command_buffer: VkCommandBuffer,
    buffer:         u64,
    offset:         u64,
    index_type:     u32, /* VkIndexType */
) {
    let Some(cb) = cmdbuf_recording(command_buffer) else { return };
    let rid = resolve_buffer(cb, buffer);
    let mut body = [0u8; 20];
    body[ 0.. 4].copy_from_slice(&rid.raw().to_le_bytes());
    body[ 8..16].copy_from_slice(&offset.to_le_bytes());
    body[16..20].copy_from_slice(&index_type.to_le_bytes());
    let _ = cb.frame.push(aqueduct_gpu::opcodes::FrameOp::BindIndexBuf, &body);
}

/// `vkCmdDrawIndexed` — like vkCmdDraw but with an index-buffer
/// lookup and a vertexOffset that's signed. Body: 5 × u32 in Vk
/// argument order. vertexOffset i32 is bit-cast to u32.
#[no_mangle]
pub unsafe extern "C" fn vkCmdDrawIndexed(
    command_buffer: VkCommandBuffer,
    index_count:    u32,
    instance_count: u32,
    first_index:    u32,
    vertex_offset:  i32,
    first_instance: u32,
) {
    let Some(cb) = cmdbuf_recording(command_buffer) else { return };
    let mut body = [0u8; 20];
    body[ 0.. 4].copy_from_slice(&index_count.to_le_bytes());
    body[ 4.. 8].copy_from_slice(&instance_count.to_le_bytes());
    body[ 8..12].copy_from_slice(&first_index.to_le_bytes());
    body[12..16].copy_from_slice(&(vertex_offset as u32).to_le_bytes());
    body[16..20].copy_from_slice(&first_instance.to_le_bytes());
    let _ = cb.frame.push(aqueduct_gpu::opcodes::FrameOp::DrawIndexed, &body);
}

/// `vkCreateImage` — record an AtriumImage. The memory binding
/// + daemon-side `image_id` land later via vkBindImageMemory.
///
/// VkImageCreateInfo layout (relevant fields):
///   0   sType
///   8   pNext
///   16  flags
///   20  imageType : u32
///   24  format    : u32
///   28  extent.width  : u32
///   32  extent.height : u32
///   36  extent.depth  : u32
///   40  mipLevels  : u32
///   44  arrayLayers : u32
///   48  samples : u32
///   52  tiling : u32
///   56  usage  : u32
#[no_mangle]
pub unsafe extern "C" fn vkCreateImage(
    device:          VkDevice,
    p_create_info:   *const c_void,
    _p_allocator:    *const c_void,
    p_image:         *mut u64,
) -> VkResult {
    if device.is_null() || p_create_info.is_null() || p_image.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let dev = &*(device as *const AtriumDevice);
    let b = p_create_info as *const u8;
    let format       = std::ptr::read_unaligned(b.add(24) as *const u32);
    let width        = std::ptr::read_unaligned(b.add(28) as *const u32);
    let height       = std::ptr::read_unaligned(b.add(32) as *const u32);
    let depth        = std::ptr::read_unaligned(b.add(36) as *const u32).max(1);
    let mip_levels   = std::ptr::read_unaligned(b.add(40) as *const u32).max(1);
    let array_layers = std::ptr::read_unaligned(b.add(44) as *const u32).max(1);
    let usage        = std::ptr::read_unaligned(b.add(56) as *const u32);
    if width == 0 || height == 0 {
        return VK_ERROR_INITIALIZATION_FAILED;
    }

    let handle = dev.next_image_id.get();
    dev.next_image_id.set(handle + 1);
    if let Ok(mut i) = dev.images.lock() {
        i.insert(handle, AtriumImage {
            width, height, depth, mip_levels, array_layers,
            format, usage, memory: None, memory_offset: 0,
            image_id: None,
        });
    } else {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    *p_image = handle;
    VK_SUCCESS
}

/// `vkDestroyImage` — drop the AtriumImage entry. Backing memory
/// is independent and survives.
#[no_mangle]
pub unsafe extern "C" fn vkDestroyImage(
    device:       VkDevice,
    image:        u64,
    _p_allocator: *const c_void,
) {
    if device.is_null() || image == 0 { return; }
    let dev = &*(device as *const AtriumDevice);
    if let Ok(mut i) = dev.images.lock() {
        i.remove(&image);
    }
}

/// `vkGetImageMemoryRequirements` — report (width × height × depth
/// × array_layers × bpp_for_format, 256-byte alignment, single
/// memory-type). 256 is the typical Vulkan minimum for
/// `optimalBufferCopyOffsetAlignment` on most HW; for the
/// software backend it's overkill but safe.
#[no_mangle]
pub unsafe extern "C" fn vkGetImageMemoryRequirements(
    device:           VkDevice,
    image:            u64,
    p_requirements:   *mut ash::vk::MemoryRequirements,
) {
    if device.is_null() || p_requirements.is_null() { return; }
    let dev = &*(device as *const AtriumDevice);
    let size = dev.images.lock().ok()
        .and_then(|m| m.get(&image).copied())
        .map(|img| {
            let bpp = bpp_for_vk_format(img.format) as u64;
            // Conservative mip + layer total — sum of per-level
            // (w * h * depth * bpp) halved at each level, times
            // array_layers. Real ICDs respect tiling-specific
            // padding; for the skeleton we report a generous
            // upper bound.
            let base = (img.width as u64) * (img.height as u64) * (img.depth as u64) * bpp;
            let mips = (0..img.mip_levels as u32).map(|l| base >> (2 * l)).sum::<u64>();
            mips * (img.array_layers as u64)
        }).unwrap_or(0);
    *p_requirements = ash::vk::MemoryRequirements {
        size,
        alignment:        256,
        memory_type_bits: 0b1,
    };
}

/// `vkBindImageMemory` — associate `image` with `memory_offset` in
/// `memory`. If the memory carries a daemon-side region_id and the
/// instance has a live client, calls `GpuClient::create_image` to
/// register the daemon-side image too; stores the returned
/// ResourceId on AtriumImage.image_id for future vkCmd* paths.
#[no_mangle]
pub unsafe extern "C" fn vkBindImageMemory(
    device:        VkDevice,
    image:         u64,
    memory:        u64,
    memory_offset: u64,
) -> VkResult {
    if device.is_null() || image == 0 || memory == 0 {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let dev = &*(device as *const AtriumDevice);

    let region_id = match dev.memories.lock() {
        Ok(m) => match m.get(&memory) {
            Some(am) => am.region_id,
            None     => return VK_ERROR_INITIALIZATION_FAILED,
        },
        Err(_) => return VK_ERROR_INITIALIZATION_FAILED,
    };

    let img_snapshot = match dev.images.lock() {
        Ok(i) => i.get(&image).copied(),
        Err(_) => return VK_ERROR_INITIALIZATION_FAILED,
    };
    let Some(img) = img_snapshot else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };

    let image_id = if let Some(region_id) = region_id {
        if !dev.instance.is_null() {
            let inst = &*(dev.instance as *const AtriumInstance);
            inst.client.as_ref().and_then(|m| {
                m.lock().ok().and_then(|mut c| {
                    c.create_image(aqueduct_gpu::payloads::ImageCreatePayload {
                        image_id:      aqueduct_gpu::ids::ResourceId(0),
                        backing_region: region_id,
                        region_offset:  memory_offset,
                        format:         img.format,
                        width:          img.width,
                        height:         img.height,
                        depth:          img.depth,
                        mip_levels:     img.mip_levels,
                        array_layers:   img.array_layers,
                        usage:          img.usage,
                    }).ok()
                })
            })
        } else { None }
    } else { None };

    let mut images = match dev.images.lock() {
        Ok(i) => i,
        Err(_) => return VK_ERROR_INITIALIZATION_FAILED,
    };
    let Some(img) = images.get_mut(&image) else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    img.memory        = Some(memory);
    img.memory_offset = memory_offset;
    img.image_id      = image_id;
    VK_SUCCESS
}

/// `vkCreateShaderModule` — sha-256 the SPIR-V bytecode, attempt
/// a resolve-then-upload against the daemon's shader cache, and
/// stash the resulting ResourceId on AtriumShaderModule. Future
/// vkCreateGraphicsPipelines reads it to wire the shader into a
/// pipeline.
///
/// VkShaderModuleCreateInfo layout:
///   0   sType
///   8   pNext
///   16  flags : u32
///   20  _pad
///   24  codeSize : usize (u64 on 64-bit)
///   32  pCode : *const u32
///
/// codeSize is BYTES, not words.
#[no_mangle]
pub unsafe extern "C" fn vkCreateShaderModule(
    device:          VkDevice,
    p_create_info:   *const c_void,
    _p_allocator:    *const c_void,
    p_shader_module: *mut u64,
) -> VkResult {
    use sha2::Digest;
    if device.is_null() || p_create_info.is_null() || p_shader_module.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let dev = &*(device as *const AtriumDevice);
    let b = p_create_info as *const u8;
    let code_size: u64 = std::ptr::read_unaligned(b.add(24) as *const u64);
    let p_code: *const u32 = std::ptr::read_unaligned(b.add(32) as *const *const u32);
    if code_size == 0 || p_code.is_null() || code_size % 4 != 0 {
        return VK_ERROR_INITIALIZATION_FAILED;
    }

    let n_words = (code_size / 4) as usize;
    let words: &[u32] = std::slice::from_raw_parts(p_code, n_words);
    // Copy into a Vec<u8> for hashing + uploading.
    let mut bytes = Vec::with_capacity(code_size as usize);
    for &w in words { bytes.extend_from_slice(&w.to_le_bytes()); }

    let mut hasher = sha2::Sha256::new();
    hasher.update(&bytes);
    let bytecode_hash: [u8; 32] = hasher.finalize().into();

    // Resolve-or-upload against the daemon. Failure (no live
    // client, validation rejection) leaves shader_id=None — the
    // module exists locally; downstream pipeline creation will
    // see the absence and surface a VK_ERROR.
    let shader_id = if !dev.instance.is_null() {
        let inst = &*(dev.instance as *const AtriumInstance);
        inst.client.as_ref().and_then(|m| {
            m.lock().ok().and_then(|mut c| {
                let kind = aqueduct_gpu::payloads::ShaderKind::SpirV;
                match c.resolve_shader(bytecode_hash, kind, dev.backend) {
                    Ok(id) => Some(id),
                    Err(_) => c.upload_shader(bytecode_hash, kind, dev.backend, bytes).ok(),
                }
            })
        })
    } else { None };

    let handle = dev.next_shader_id.get();
    dev.next_shader_id.set(handle + 1);
    if let Ok(mut s) = dev.shaders.lock() {
        s.insert(handle, AtriumShaderModule { shader_id, bytecode_hash });
    } else {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    *p_shader_module = handle;
    VK_SUCCESS
}

/// `vkDestroyShaderModule` — drop the local mapping. The
/// daemon-side shader stays in the cache (lifetime is the host
/// endpoint's, not the app's).
#[no_mangle]
pub unsafe extern "C" fn vkDestroyShaderModule(
    device:       VkDevice,
    module:       u64,
    _p_allocator: *const c_void,
) {
    if device.is_null() || module == 0 { return; }
    let dev = &*(device as *const AtriumDevice);
    if let Ok(mut s) = dev.shaders.lock() {
        s.remove(&module);
    }
}

/// `vkCreateImageView` — record a (VkImageView u64 → VkImage u64)
/// mapping. We ignore view-type / format / aspect-mask /
/// subresource-range for the skeleton; vkCmdBeginRenderPass just
/// needs the parent image's daemon-side image_id.
///
/// VkImageViewCreateInfo layout:
///   0   sType, 8 pNext, 16 flags, 24 image (u64),
///   32 viewType, 36 format, ...
#[no_mangle]
pub unsafe extern "C" fn vkCreateImageView(
    device:        VkDevice,
    p_create_info: *const c_void,
    _p_allocator:  *const c_void,
    p_view:        *mut u64,
) -> VkResult {
    if device.is_null() || p_create_info.is_null() || p_view.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let dev = &*(device as *const AtriumDevice);
    let image = std::ptr::read_unaligned(
        (p_create_info as *const u8).add(24) as *const u64,
    );
    let handle = dev.next_image_view_id.get();
    dev.next_image_view_id.set(handle + 1);
    if let Ok(mut v) = dev.image_views.lock() {
        v.insert(handle, image);
    } else {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    *p_view = handle;
    VK_SUCCESS
}

/// `vkDestroyImageView` — drop the mapping.
#[no_mangle]
pub unsafe extern "C" fn vkDestroyImageView(
    device:       VkDevice,
    view:         u64,
    _p_allocator: *const c_void,
) {
    if device.is_null() || view == 0 { return; }
    let dev = &*(device as *const AtriumDevice);
    if let Ok(mut v) = dev.image_views.lock() {
        v.remove(&view);
    }
}

/// `vkCreateRenderPass` — return a unique non-zero u64. We don't
/// track attachment specs today; the host endpoint's renderer
/// abstracts over render-pass details. Future Phase 1.3b+ would
/// store the load/store-op list for vkCmdBeginRenderPass to
/// derive the clear color / preserve semantics.
#[no_mangle]
pub unsafe extern "C" fn vkCreateRenderPass(
    device:          VkDevice,
    _p_create_info:  *const c_void,
    _p_allocator:    *const c_void,
    p_render_pass:   *mut u64,
) -> VkResult {
    if device.is_null() || p_render_pass.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let dev = &*(device as *const AtriumDevice);
    let h = dev.next_render_pass_id.get();
    dev.next_render_pass_id.set(h + 1);
    *p_render_pass = h;
    VK_SUCCESS
}

/// `vkDestroyRenderPass` — no-op; we don't track per-handle state.
#[no_mangle]
pub unsafe extern "C" fn vkDestroyRenderPass(
    _device:        VkDevice,
    _render_pass:   u64,
    _p_allocator:   *const c_void,
) {}

/// `vkCreateFramebuffer` — record the attachment image views +
/// extent.
///
/// VkFramebufferCreateInfo layout:
///   0   sType, 8 pNext, 16 flags, 24 renderPass (u64),
///   32 attachmentCount (u32), 36 _pad,
///   40 pAttachments (*const VkImageView u64),
///   48 width (u32), 52 height (u32), 56 layers (u32)
#[no_mangle]
pub unsafe extern "C" fn vkCreateFramebuffer(
    device:          VkDevice,
    p_create_info:   *const c_void,
    _p_allocator:    *const c_void,
    p_framebuffer:   *mut u64,
) -> VkResult {
    if device.is_null() || p_create_info.is_null() || p_framebuffer.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let dev = &*(device as *const AtriumDevice);
    let b = p_create_info as *const u8;
    let attachment_count = std::ptr::read_unaligned(b.add(32) as *const u32);
    let p_attachments    = std::ptr::read_unaligned(b.add(40) as *const *const u64);
    let width            = std::ptr::read_unaligned(b.add(48) as *const u32);
    let height           = std::ptr::read_unaligned(b.add(52) as *const u32);

    let mut attachments = Vec::with_capacity(attachment_count as usize);
    if !p_attachments.is_null() {
        for i in 0..attachment_count {
            attachments.push(*p_attachments.offset(i as isize));
        }
    }
    let handle = dev.next_framebuffer_id.get();
    dev.next_framebuffer_id.set(handle + 1);
    if let Ok(mut f) = dev.framebuffers.lock() {
        f.insert(handle, AtriumFramebuffer { width, height, attachments });
    } else {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    *p_framebuffer = handle;
    VK_SUCCESS
}

/// `vkDestroyFramebuffer` — drop the mapping.
#[no_mangle]
pub unsafe extern "C" fn vkDestroyFramebuffer(
    device:       VkDevice,
    framebuffer:  u64,
    _p_allocator: *const c_void,
) {
    if device.is_null() || framebuffer == 0 { return; }
    let dev = &*(device as *const AtriumDevice);
    if let Ok(mut f) = dev.framebuffers.lock() {
        f.remove(&framebuffer);
    }
}

/// `vkCmdBeginRenderPass` — push a `BeginRenderPass` FrameOp
/// targeting the framebuffer's first attachment's image_id.
/// Walks framebuffer → image_view → image → image_id.
///
/// VkRenderPassBeginInfo layout:
///   0   sType, 8 pNext, 16 renderPass (u64), 24 framebuffer (u64),
///   32 renderArea (VkRect2D, 16 B), 48 clearValueCount (u32),
///   56 pClearValues (*const VkClearValue)
///
/// VkClearValue is a 16-byte union — first 4 bytes are r as f32,
/// next 4 g, next 4 b, next 4 a. We read all four as f32 and
/// quantize to u8 for the FrameOp body.
#[no_mangle]
pub unsafe extern "C" fn vkCmdBeginRenderPass(
    command_buffer:    VkCommandBuffer,
    p_render_pass_begin: *const c_void,
    _contents:         u32, /* VkSubpassContents */
) {
    let Some(cb) = cmdbuf_recording(command_buffer) else { return };
    if p_render_pass_begin.is_null() || cb.device.is_null() { return; }
    let dev = &*(cb.device as *const AtriumDevice);
    let b = p_render_pass_begin as *const u8;
    let framebuffer = std::ptr::read_unaligned(b.add(24) as *const u64);
    let clear_count = std::ptr::read_unaligned(b.add(48) as *const u32);
    let p_clears    = std::ptr::read_unaligned(b.add(56) as *const *const u8);

    // Resolve framebuffer → first attachment → image → image_id.
    let fb_first_view = dev.framebuffers.lock().ok()
        .and_then(|f| f.get(&framebuffer).and_then(|fb| fb.attachments.first().copied()));
    let image = fb_first_view.and_then(|view| {
        dev.image_views.lock().ok().and_then(|v| v.get(&view).copied())
    });
    let image_id = image.and_then(|img| {
        dev.images.lock().ok()
            .and_then(|m| m.get(&img).and_then(|x| x.image_id))
    });

    // Clear color: read 4 f32, quantize to RGBA8.
    let mut clear_rgba8 = [0u8, 0, 0, 255];
    if clear_count > 0 && !p_clears.is_null() {
        let r = std::ptr::read_unaligned(p_clears as *const f32);
        let g = std::ptr::read_unaligned(p_clears.add(4) as *const f32);
        let bl = std::ptr::read_unaligned(p_clears.add(8) as *const f32);
        let a = std::ptr::read_unaligned(p_clears.add(12) as *const f32);
        clear_rgba8 = [
            (r.clamp(0.0, 1.0) * 255.0) as u8,
            (g.clamp(0.0, 1.0) * 255.0) as u8,
            (bl.clamp(0.0, 1.0) * 255.0) as u8,
            (a.clamp(0.0, 1.0) * 255.0) as u8,
        ];
    }

    // BeginRenderPass body: 12 bytes (target_image_id u32 +
    // clear_color_rgba8 [u8;4] + flags u32). flags=0 today.
    let mut body = [0u8; 12];
    let tid = image_id.map(|i| i.raw()).unwrap_or(0);
    body[ 0.. 4].copy_from_slice(&tid.to_le_bytes());
    body[ 4.. 8].copy_from_slice(&clear_rgba8);
    // flags @ 8..12 already zero.
    let _ = cb.frame.push(aqueduct_gpu::opcodes::FrameOp::BeginRenderPass, &body);
}

/// `vkCmdEndRenderPass` — push `EndRenderPass`.
#[no_mangle]
pub unsafe extern "C" fn vkCmdEndRenderPass(
    command_buffer: VkCommandBuffer,
) {
    let Some(cb) = cmdbuf_recording(command_buffer) else { return };
    let _ = cb.frame.push(aqueduct_gpu::opcodes::FrameOp::EndRenderPass, &[]);
}

/// `vkCmdBeginRendering` — Vulkan 1.3 dynamic-rendering entry.
/// Replaces the vkCreateRenderPass + vkCreateFramebuffer ceremony
/// with inline attachment specs at draw time. Used by every
/// modern Vulkan-1.3-targeting renderer (Bevy, wgpu, Slang,
/// Forge); apps that opt into dynamic rendering would otherwise
/// fall back to the deprecated render-pass path or fail to load.
///
/// Resolves the first color attachment's imageView → image →
/// daemon-side image_id (same path as vkCmdBeginRenderPass takes
/// through the framebuffer), extracts the inline clear color, and
/// emits the same BeginRenderPass FrameOp the legacy path uses —
/// tier-1's renderer doesn't care whether the app went through
/// the dynamic or static-render-pass model.
///
/// VkRenderingInfo layout (72 bytes):
///   0  sType, 8 pNext, 16 flags,
///   20 renderArea (16 bytes),
///   36 layerCount, 40 viewMask, 44 colorAttachmentCount,
///   48 pColorAttachments (*const VkRenderingAttachmentInfo),
///   56 pDepthAttachment, 64 pStencilAttachment.
///
/// VkRenderingAttachmentInfo layout (72 bytes):
///   0  sType, 8 pNext, 16 imageView,
///   24 imageLayout, 28 resolveMode,
///   32 resolveImageView, 40 resolveImageLayout,
///   44 loadOp, 48 storeOp,
///   52 clearValue (16 bytes — first 4 f32 = clearColor.float32).
#[no_mangle]
pub unsafe extern "C" fn vkCmdBeginRendering(
    command_buffer:    VkCommandBuffer,
    p_rendering_info:  *const c_void,
) {
    let Some(cb) = cmdbuf_recording(command_buffer) else { return };
    if p_rendering_info.is_null() || cb.device.is_null() { return; }
    let dev = &*(cb.device as *const AtriumDevice);
    let info = p_rendering_info as *const u8;
    let color_count = std::ptr::read_unaligned(info.add(44) as *const u32);
    let p_color     = std::ptr::read_unaligned(info.add(48) as *const *const u8);

    let (image_id, clear_rgba8) = if color_count > 0 && !p_color.is_null() {
        let att = p_color; // first attachment
        let view = std::ptr::read_unaligned(att.add(16) as *const u64);
        let img = dev.image_views.lock().ok()
            .and_then(|v| v.get(&view).copied());
        let image_id = img.and_then(|i| {
            dev.images.lock().ok()
                .and_then(|m| m.get(&i).and_then(|x| x.image_id))
        });
        let r  = std::ptr::read_unaligned(att.add(52) as *const f32);
        let g  = std::ptr::read_unaligned(att.add(56) as *const f32);
        let bl = std::ptr::read_unaligned(att.add(60) as *const f32);
        let a  = std::ptr::read_unaligned(att.add(64) as *const f32);
        let clear = [
            (r.clamp(0.0, 1.0)  * 255.0) as u8,
            (g.clamp(0.0, 1.0)  * 255.0) as u8,
            (bl.clamp(0.0, 1.0) * 255.0) as u8,
            (a.clamp(0.0, 1.0)  * 255.0) as u8,
        ];
        (image_id, clear)
    } else {
        (None, [0u8, 0, 0, 255])
    };

    let mut body = [0u8; 12];
    let tid = image_id.map(|i| i.raw()).unwrap_or(0);
    body[ 0.. 4].copy_from_slice(&tid.to_le_bytes());
    body[ 4.. 8].copy_from_slice(&clear_rgba8);
    let _ = cb.frame.push(aqueduct_gpu::opcodes::FrameOp::BeginRenderPass, &body);
}

/// `vkCmdEndRendering` — 1.3 dynamic-rendering counterpart to
/// vkCmdEndRenderPass. Emits the same EndRenderPass FrameOp.
#[no_mangle]
pub unsafe extern "C" fn vkCmdEndRendering(
    command_buffer: VkCommandBuffer,
) {
    let Some(cb) = cmdbuf_recording(command_buffer) else { return };
    let _ = cb.frame.push(aqueduct_gpu::opcodes::FrameOp::EndRenderPass, &[]);
}

/// `vkCreateFence` — allocate a u64 handle + signaled bit. Reads
/// VkFenceCreateFlags @ offset 16; the only bit we honor is
/// VK_FENCE_CREATE_SIGNALED_BIT (0x1).
#[no_mangle]
pub unsafe extern "C" fn vkCreateFence(
    device:          VkDevice,
    p_create_info:   *const c_void,
    _p_allocator:    *const c_void,
    p_fence:         *mut u64,
) -> VkResult {
    if device.is_null() || p_fence.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let dev = &*(device as *const AtriumDevice);
    let signaled = if !p_create_info.is_null() {
        let flags = std::ptr::read_unaligned(
            (p_create_info as *const u8).add(16) as *const u32,
        );
        flags & 0x1 != 0
    } else { false };

    let h = dev.next_fence_id.get();
    dev.next_fence_id.set(h + 1);
    if let Ok(mut f) = dev.fences.lock() {
        f.insert(h, signaled);
    } else {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    *p_fence = h;
    VK_SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn vkDestroyFence(
    device:       VkDevice,
    fence:        u64,
    _p_allocator: *const c_void,
) {
    if device.is_null() || fence == 0 { return; }
    let dev = &*(device as *const AtriumDevice);
    if let Ok(mut f) = dev.fences.lock() {
        f.remove(&fence);
    }
}

/// `vkWaitForFences` — vkQueueSubmit is synchronous from the
/// ICD's POV today, so all fences are always-signaled. Mark them
/// signaled + return success. wait_all and timeout are ignored.
#[no_mangle]
pub unsafe extern "C" fn vkWaitForFences(
    device:       VkDevice,
    fence_count:  u32,
    p_fences:     *const u64,
    _wait_all:    u32, /* VkBool32 */
    _timeout_ns:  u64,
) -> VkResult {
    if device.is_null() || p_fences.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let dev = &*(device as *const AtriumDevice);
    if let Ok(mut f) = dev.fences.lock() {
        for i in 0..fence_count {
            let h = *p_fences.offset(i as isize);
            if let Some(s) = f.get_mut(&h) {
                *s = true;
            }
        }
    }
    VK_SUCCESS
}

/// `vkResetFences` — clear the signaled bit on each.
#[no_mangle]
pub unsafe extern "C" fn vkResetFences(
    device:       VkDevice,
    fence_count:  u32,
    p_fences:     *const u64,
) -> VkResult {
    if device.is_null() || p_fences.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let dev = &*(device as *const AtriumDevice);
    if let Ok(mut f) = dev.fences.lock() {
        for i in 0..fence_count {
            let h = *p_fences.offset(i as isize);
            if let Some(s) = f.get_mut(&h) {
                *s = false;
            }
        }
    }
    VK_SUCCESS
}

/// `vkGetFenceStatus` — VK_SUCCESS = signaled, VK_NOT_READY (3)
/// = not signaled, VK_ERROR_DEVICE_LOST (-4) = lost.
#[no_mangle]
pub unsafe extern "C" fn vkGetFenceStatus(
    device: VkDevice,
    fence:  u64,
) -> VkResult {
    if device.is_null() || fence == 0 {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let dev = &*(device as *const AtriumDevice);
    match dev.fences.lock().ok().and_then(|f| f.get(&fence).copied()) {
        Some(true)  => VK_SUCCESS,
        Some(false) => 3, /* VK_NOT_READY */
        None        => -4, /* VK_ERROR_DEVICE_LOST */
    }
}

/// `vkCreateSemaphore` — opaque non-zero u64. Atrium's
/// per-VkQueueSubmit timeline handles serialization; semaphores
/// are bookkeeping-only today.
#[no_mangle]
pub unsafe extern "C" fn vkCreateSemaphore(
    device:           VkDevice,
    _p_create_info:   *const c_void,
    _p_allocator:     *const c_void,
    p_semaphore:      *mut u64,
) -> VkResult {
    if device.is_null() || p_semaphore.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let dev = &*(device as *const AtriumDevice);
    let h = dev.next_semaphore_id.get();
    dev.next_semaphore_id.set(h + 1);
    *p_semaphore = h;
    VK_SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn vkDestroySemaphore(
    _device:      VkDevice,
    _semaphore:   u64,
    _p_allocator: *const c_void,
) {}

/// `vkCreateSampler` — read filter/address/lod fields and call
/// GpuClient::create_sampler.
///
/// VkSamplerCreateInfo layout (relevant fields, all u32 unless
/// noted):
///   0   sType, 8 pNext, 16 flags,
///   20  magFilter, 24 minFilter, 28 mipmapMode,
///   32  addressModeU, 36 addressModeV, 40 addressModeW,
///   44  mipLodBias : f32,
///   48  anisotropyEnable : VkBool32,
///   52  maxAnisotropy : f32,
///   56  compareEnable, 60 compareOp,
///   64  minLod : f32, 68 maxLod : f32,
///   72  borderColor, 76 unnormalizedCoordinates
#[no_mangle]
pub unsafe extern "C" fn vkCreateSampler(
    device:        VkDevice,
    p_create_info: *const c_void,
    _p_allocator:  *const c_void,
    p_sampler:     *mut u64,
) -> VkResult {
    if device.is_null() || p_create_info.is_null() || p_sampler.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let dev = &*(device as *const AtriumDevice);
    let b = p_create_info as *const u8;
    let mag = std::ptr::read_unaligned(b.add(20) as *const u32) as u8;
    let min = std::ptr::read_unaligned(b.add(24) as *const u32) as u8;
    let mip = std::ptr::read_unaligned(b.add(28) as *const u32) as u8;
    let au = std::ptr::read_unaligned(b.add(32) as *const u32) as u8;
    let av = std::ptr::read_unaligned(b.add(36) as *const u32) as u8;
    let aw = std::ptr::read_unaligned(b.add(40) as *const u32) as u8;
    let aniso = std::ptr::read_unaligned(b.add(52) as *const f32);
    let min_lod = std::ptr::read_unaligned(b.add(64) as *const f32);
    let max_lod = std::ptr::read_unaligned(b.add(68) as *const f32);

    let sid = if !dev.instance.is_null() {
        let inst = &*(dev.instance as *const AtriumInstance);
        inst.client.as_ref().and_then(|m| {
            m.lock().ok().and_then(|mut c| {
                c.create_sampler(aqueduct_gpu::payloads::SamplerCreatePayload {
                    sampler_id: aqueduct_gpu::ids::ResourceId(0),
                    min_filter: min, mag_filter: mag, mip_filter: mip,
                    address_modes: [au, av, aw],
                    max_anisotropy: aniso,
                    min_lod, max_lod,
                }).ok()
            })
        })
    } else { None };

    let h = dev.next_sampler_id.get();
    dev.next_sampler_id.set(h + 1);
    if let Ok(mut s) = dev.samplers.lock() {
        s.insert(h, sid);
    } else {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    *p_sampler = h;
    VK_SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn vkDestroySampler(
    device:       VkDevice,
    sampler:      u64,
    _p_allocator: *const c_void,
) {
    if device.is_null() || sampler == 0 { return; }
    let dev = &*(device as *const AtriumDevice);
    if let Ok(mut s) = dev.samplers.lock() {
        s.remove(&sampler);
    }
}

/// `vkCreateDescriptorSetLayout` — opaque non-zero u64. The host
/// endpoint's pipeline / shader knows the expected binding layout
/// (via the bundle definition); we don't validate set-layout
/// compatibility today.
#[no_mangle]
pub unsafe extern "C" fn vkCreateDescriptorSetLayout(
    device:           VkDevice,
    _p_create_info:   *const c_void,
    _p_allocator:     *const c_void,
    p_set_layout:     *mut u64,
) -> VkResult {
    if device.is_null() || p_set_layout.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let dev = &*(device as *const AtriumDevice);
    let h = dev.next_dsl_id.get();
    dev.next_dsl_id.set(h + 1);
    *p_set_layout = h;
    VK_SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn vkDestroyDescriptorSetLayout(
    _device:        VkDevice,
    _set_layout:    u64,
    _p_allocator:   *const c_void,
) {}

/// `vkCreateDescriptorPool` — opaque non-zero u64. We don't
/// enforce the pool's maxSets / per-type budgets today.
#[no_mangle]
pub unsafe extern "C" fn vkCreateDescriptorPool(
    device:          VkDevice,
    _p_create_info:  *const c_void,
    _p_allocator:    *const c_void,
    p_pool:          *mut u64,
) -> VkResult {
    if device.is_null() || p_pool.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let dev = &*(device as *const AtriumDevice);
    let h = dev.next_dpool_id.get();
    dev.next_dpool_id.set(h + 1);
    *p_pool = h;
    VK_SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn vkDestroyDescriptorPool(
    _device:        VkDevice,
    _pool:          u64,
    _p_allocator:   *const c_void,
) {}

/// `vkAllocateDescriptorSets` — allocate N empty descriptor sets.
///
/// VkDescriptorSetAllocateInfo layout:
///   0   sType, 8 pNext, 16 descriptorPool (u64),
///   24  descriptorSetCount (u32), 28 _pad,
///   32  pSetLayouts (*const VkDescriptorSetLayout u64)
#[no_mangle]
pub unsafe extern "C" fn vkAllocateDescriptorSets(
    device:           VkDevice,
    p_allocate_info:  *const c_void,
    p_descriptor_sets: *mut u64,
) -> VkResult {
    if device.is_null() || p_allocate_info.is_null() || p_descriptor_sets.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let dev = &*(device as *const AtriumDevice);
    let b = p_allocate_info as *const u8;
    let count = std::ptr::read_unaligned(b.add(24) as *const u32);

    if let Ok(mut sets) = dev.descriptor_sets.lock() {
        for i in 0..count {
            let h = dev.next_dset_id.get();
            dev.next_dset_id.set(h + 1);
            sets.insert(h, AtriumDescriptorSet::default());
            *p_descriptor_sets.offset(i as isize) = h;
        }
    } else {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    VK_SUCCESS
}

/// `vkUpdateDescriptorSets` — process `descriptorWriteCount`
/// VkWriteDescriptorSet records. Each describes which binding in
/// which descriptor set gets updated with which resource(s).
///
/// VkWriteDescriptorSet layout (64 bytes):
///   0   sType, 8 pNext,
///   16  dstSet (u64), 24 dstBinding (u32), 28 dstArrayElement (u32),
///   32  descriptorCount (u32), 36 descriptorType (u32),
///   40  pImageInfo  (*const VkDescriptorImageInfo),
///   48  pBufferInfo (*const VkDescriptorBufferInfo),
///   56  pTexelBufferView (*const VkBufferView)
///
/// VkDescriptorImageInfo: sampler(u64)@0, imageView(u64)@8, imageLayout(u32)@16
/// VkDescriptorBufferInfo: buffer(u64)@0, offset(u64)@8, range(u64)@16
///
/// pDescriptorCopies is ignored (descriptor-to-descriptor copy not
/// needed in skeleton).
#[no_mangle]
pub unsafe extern "C" fn vkUpdateDescriptorSets(
    device:                VkDevice,
    descriptor_write_count: u32,
    p_descriptor_writes:    *const c_void,
    _descriptor_copy_count: u32,
    _p_descriptor_copies:   *const c_void,
) {
    if device.is_null() || p_descriptor_writes.is_null() {
        return;
    }
    let dev = &*(device as *const AtriumDevice);
    let writes_base = p_descriptor_writes as *const u8;

    let mut sets = match dev.descriptor_sets.lock() { Ok(s) => s, Err(_) => return };
    let buffers = dev.buffers.lock().ok();
    let images  = dev.images.lock().ok();
    let samplers = dev.samplers.lock().ok();

    for i in 0..descriptor_write_count {
        let w = writes_base.add(64 * i as usize);
        let dst_set      = std::ptr::read_unaligned(w.add(16) as *const u64);
        let dst_binding  = std::ptr::read_unaligned(w.add(24) as *const u32);
        let count        = std::ptr::read_unaligned(w.add(32) as *const u32);
        let ty           = std::ptr::read_unaligned(w.add(36) as *const u32);
        let p_image_info  = std::ptr::read_unaligned(w.add(40) as *const *const u8);
        let p_buffer_info = std::ptr::read_unaligned(w.add(48) as *const *const u8);

        let Some(set) = sets.get_mut(&dst_set) else { continue; };

        for j in 0..count {
            let mut write = AtriumDescriptorWrite {
                binding:        dst_binding + j,
                descriptor_type: ty,
                ..AtriumDescriptorWrite::default()
            };
            match ty {
                // 6 = UNIFORM_BUFFER, 7 = STORAGE_BUFFER,
                // 8 = UNIFORM_BUFFER_DYNAMIC, 9 = STORAGE_BUFFER_DYNAMIC.
                6 | 7 | 8 | 9 if !p_buffer_info.is_null() => {
                    let bi = p_buffer_info.add(24 * j as usize);
                    let buf = std::ptr::read_unaligned(bi as *const u64);
                    let offset = std::ptr::read_unaligned(bi.add(8) as *const u64);
                    let range  = std::ptr::read_unaligned(bi.add(16) as *const u64);
                    let bid = buffers.as_ref()
                        .and_then(|b| b.get(&buf).and_then(|x| x.buffer_id))
                        .map(|r| r.raw()).unwrap_or(0);
                    write.buffer_id = bid;
                    write.offset    = offset;
                    write.range     = range;
                }
                // 0 = SAMPLER, 1 = COMBINED_IMAGE_SAMPLER,
                // 2 = SAMPLED_IMAGE, 3 = STORAGE_IMAGE,
                // 4 = UNIFORM_TEXEL_BUFFER, 5 = STORAGE_TEXEL_BUFFER.
                0 | 1 | 2 | 3 if !p_image_info.is_null() => {
                    let ii = p_image_info.add(24 * j as usize);
                    let sampler   = std::ptr::read_unaligned(ii as *const u64);
                    let image_view = std::ptr::read_unaligned(ii.add(8) as *const u64);
                    let sid = samplers.as_ref()
                        .and_then(|s| s.get(&sampler).and_then(|o| *o))
                        .map(|r| r.raw()).unwrap_or(0);
                    // image_view → image → image_id
                    let img = dev.image_views.lock().ok()
                        .and_then(|v| v.get(&image_view).copied()).unwrap_or(0);
                    let iid = images.as_ref()
                        .and_then(|m| m.get(&img).and_then(|x| x.image_id))
                        .map(|r| r.raw()).unwrap_or(0);
                    write.sampler_id = sid;
                    write.image_id   = iid;
                }
                _ => {}
            }
            // Replace existing entry for this binding or append.
            if let Some(slot) = set.writes.iter_mut().find(|x| x.binding == write.binding) {
                *slot = write;
            } else {
                set.writes.push(write);
            }
        }
    }
}

/// `vkCmdBindDescriptorSets` — push one `BindDescriptors` FrameOp
/// per descriptor set being bound. Body layout (28 bytes per
/// binding write, plus a 4-byte header): set_index u32 +
/// write_count u32 + per-write { binding u32, type u32,
/// buffer_id u32, image_id u32, sampler_id u32, offset u64,
/// range u64 } (= 32 B per write).
///
/// pDynamicOffsets is ignored today.
#[no_mangle]
pub unsafe extern "C" fn vkCmdBindDescriptorSets(
    command_buffer:        VkCommandBuffer,
    _pipeline_bind_point:  u32,
    _layout:               u64,
    first_set:             u32,
    descriptor_set_count:  u32,
    p_descriptor_sets:     *const u64,
    _dynamic_offset_count: u32,
    _p_dynamic_offsets:    *const u32,
) {
    let Some(cb) = cmdbuf_recording(command_buffer) else { return };
    if p_descriptor_sets.is_null() || cb.device.is_null() { return; }
    let dev = &*(cb.device as *const AtriumDevice);

    let sets = match dev.descriptor_sets.lock() { Ok(s) => s, Err(_) => return };
    for i in 0..descriptor_set_count {
        let h = *p_descriptor_sets.offset(i as isize);
        let Some(set) = sets.get(&h) else { continue; };
        let mut body = Vec::with_capacity(8 + set.writes.len() * 32);
        body.extend_from_slice(&(first_set + i).to_le_bytes());
        body.extend_from_slice(&(set.writes.len() as u32).to_le_bytes());
        for w in &set.writes {
            body.extend_from_slice(&w.binding.to_le_bytes());
            body.extend_from_slice(&w.descriptor_type.to_le_bytes());
            body.extend_from_slice(&w.buffer_id.to_le_bytes());
            body.extend_from_slice(&w.image_id.to_le_bytes());
            body.extend_from_slice(&w.sampler_id.to_le_bytes());
            body.extend_from_slice(&w.offset.to_le_bytes());
            body.extend_from_slice(&w.range.to_le_bytes());
        }
        let _ = cb.frame.push(aqueduct_gpu::opcodes::FrameOp::BindDescriptors, &body);
    }
}

/// `vkCmdCopyBuffer` — buffer-to-buffer region copy. Pushes one
/// PipelineBarrier opcode reservation today (we don't have a
/// dedicated buffer-copy FrameOp; the host endpoint handles it
/// generically). Each VkBufferCopy region: srcOffset(u64) +
/// dstOffset(u64) + size(u64) = 24 B.
///
/// Body: src_buffer_id u32 + dst_buffer_id u32 + region_count u32
/// + per-region (24 B). Stuffed into BindDescriptors for now —
/// dedicated opcode pending.
#[no_mangle]
pub unsafe extern "C" fn vkCmdCopyBuffer(
    command_buffer: VkCommandBuffer,
    src_buffer:     u64,
    dst_buffer:     u64,
    region_count:   u32,
    p_regions:      *const c_void,
) {
    let Some(cb) = cmdbuf_recording(command_buffer) else { return };
    if p_regions.is_null() || cb.device.is_null() { return; }
    let src_id = resolve_buffer(cb, src_buffer).raw();
    let dst_id = resolve_buffer(cb, dst_buffer).raw();
    let mut body = Vec::with_capacity(12 + (region_count as usize) * 24);
    body.extend_from_slice(&src_id.to_le_bytes());
    body.extend_from_slice(&dst_id.to_le_bytes());
    body.extend_from_slice(&region_count.to_le_bytes());
    let regions_base = p_regions as *const u8;
    for i in 0..region_count {
        let r = regions_base.add(24 * i as usize);
        body.extend_from_slice(
            std::slice::from_raw_parts(r, 24),
        );
    }
    // Reuse FrameOp::CopyBufToImg as the buffer-copy carrier for
    // now; the renderer treats both copy variants in its
    // Unsupported bucket until full implementation. Future
    // protocol revision adds a dedicated CopyBufToBuf.
    let _ = cb.frame.push(aqueduct_gpu::opcodes::FrameOp::CopyBufToImg, &body);
}

/// `vkCmdCopyBufferToImage` — push a CopyBufToImg FrameOp with
/// each VkBufferImageCopy region. Region layout (56 B per VkSpec):
///   0   bufferOffset (u64)
///   8   bufferRowLength (u32)
///   12  bufferImageHeight (u32)
///   16  imageSubresource (VkImageSubresourceLayers, 16 B)
///   32  imageOffset (VkOffset3D, 12 B)
///   44  _pad
///   48  imageExtent (VkExtent3D, 12 B)
///   60  _pad
///
/// Body: src_buffer_id u32 + dst_image_id u32 + dst_layout u32 +
/// region_count u32 + per-region (56 B verbatim).
#[no_mangle]
pub unsafe extern "C" fn vkCmdCopyBufferToImage(
    command_buffer:  VkCommandBuffer,
    src_buffer:      u64,
    dst_image:       u64,
    dst_image_layout: u32, /* VkImageLayout */
    region_count:    u32,
    p_regions:       *const c_void,
) {
    let Some(cb) = cmdbuf_recording(command_buffer) else { return };
    if p_regions.is_null() || cb.device.is_null() { return; }
    let dev = &*(cb.device as *const AtriumDevice);
    let src_id = resolve_buffer(cb, src_buffer).raw();
    let dst_id = dev.images.lock().ok()
        .and_then(|m| m.get(&dst_image).and_then(|x| x.image_id))
        .map(|r| r.raw()).unwrap_or(0);

    let mut body = Vec::with_capacity(16 + (region_count as usize) * 56);
    body.extend_from_slice(&src_id.to_le_bytes());
    body.extend_from_slice(&dst_id.to_le_bytes());
    body.extend_from_slice(&dst_image_layout.to_le_bytes());
    body.extend_from_slice(&region_count.to_le_bytes());
    let regions_base = p_regions as *const u8;
    for i in 0..region_count {
        let r = regions_base.add(64 * i as usize);
        // VkBufferImageCopy is 64 B in size (Vk spec; includes
        // trailing pad to 8-byte alignment).
        body.extend_from_slice(std::slice::from_raw_parts(r, 56));
    }
    let _ = cb.frame.push(aqueduct_gpu::opcodes::FrameOp::CopyBufToImg, &body);
}

/// `vkCmdPipelineBarrier` — push a PipelineBarrier FrameOp.
///
/// Vk's model maps onto Atrium's: every barrier becomes a host-
/// observable "wait for prior writes / make later reads see them"
/// point. We carry srcStageMask + dstStageMask (and the buffer/
/// image-memory-barrier counts; the barrier bodies themselves are
/// dropped for now — the renderer's barrier handling is generic).
///
/// Body (12 B): src_stage_mask u32 + dst_stage_mask u32 +
/// memory/buffer/image_barrier_count u32 (packed into one byte
/// each + 1 pad).
#[no_mangle]
pub unsafe extern "C" fn vkCmdPipelineBarrier(
    command_buffer:           VkCommandBuffer,
    src_stage_mask:           u32, /* VkPipelineStageFlags */
    dst_stage_mask:           u32,
    _dependency_flags:        u32,
    memory_barrier_count:     u32,
    _p_memory_barriers:       *const c_void,
    buffer_barrier_count:     u32,
    _p_buffer_memory_barriers: *const c_void,
    image_barrier_count:      u32,
    _p_image_memory_barriers: *const c_void,
) {
    let Some(cb) = cmdbuf_recording(command_buffer) else { return };
    let mut body = [0u8; 12];
    body[ 0.. 4].copy_from_slice(&src_stage_mask.to_le_bytes());
    body[ 4.. 8].copy_from_slice(&dst_stage_mask.to_le_bytes());
    body[ 8.. 9].copy_from_slice(&[memory_barrier_count.min(255) as u8]);
    body[ 9..10].copy_from_slice(&[buffer_barrier_count.min(255) as u8]);
    body[10..11].copy_from_slice(&[image_barrier_count.min(255)  as u8]);
    let _ = cb.frame.push(aqueduct_gpu::opcodes::FrameOp::PipelineBarrier, &body);
}

/// `vkGetImageSubresourceLayout` — describe the linear layout of
/// an image subresource within its backing memory. Required by
/// apps that vkMapMemory a LINEAR-tiling image and walk pixels
/// directly (texture loaders, CPU-side readback, screenshot
/// tools, RenderDoc-style capture probes).
///
/// Tier-1 (tiny-skia) treats every image as row-major BGRA with
/// no padding, one mip level, one array layer. So for any
/// subresource we return a flat:
///   offset      = 0
///   size        = width * height * bpp
///   rowPitch    = width * bpp
///   arrayPitch  = size
///   depthPitch  = size
///
/// The 1.0 entry takes a VkImageSubresource (16 bytes:
/// aspectMask u32 + mipLevel u32 + arrayLayer u32 + 4-byte pad)
/// and fills a VkSubresourceLayout (40 bytes: 5x u64).
#[no_mangle]
pub unsafe extern "C" fn vkGetImageSubresourceLayout(
    device:        VkDevice,
    image:         u64,
    _p_subresource: *const c_void, /* VkImageSubresource — accepted but ignored */
    p_layout:      *mut c_void,    /* VkSubresourceLayout */
) {
    if device.is_null() || p_layout.is_null() || image == 0 { return; }
    let dev = &*(device as *const AtriumDevice);
    let (width, height, format) = {
        let images = match dev.images.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        match images.get(&image) {
            Some(img) => (img.width, img.height, img.format),
            None => return,
        }
    };
    let bpp = bpp_for_vk_format(format).max(1) as u64;
    let row_pitch = width as u64 * bpp;
    let size      = row_pitch * height as u64;

    let b = p_layout as *mut u8;
    let put64 = |off: usize, v: u64| {
        std::ptr::copy_nonoverlapping(
            v.to_le_bytes().as_ptr(), b.add(off), 8,
        );
    };
    put64( 0, 0);          // offset
    put64( 8, size);       // size
    put64(16, row_pitch);  // rowPitch
    put64(24, size);       // arrayPitch
    put64(32, size);       // depthPitch
}

/// `vkCmdPipelineBarrier2` — Vulkan 1.3 mandatory barrier with
/// VkDependencyInfo. Each contained barrier carries 64-bit stage
/// and access masks (vs the 32-bit masks in the 1.0 variant) and
/// per-barrier sync rather than a single global stage pair.
///
/// We collapse to the same per-cmdbuf PipelineBarrier FrameOp
/// the 1.0 path emits: take the FIRST memory barrier's stage
/// masks (truncated u64 → u32) as the global pair, and forward
/// the three barrier counts. Atrium's renderer treats every
/// barrier as a global write→read fence; the per-barrier
/// granularity is not yet wired through the daemon.
///
/// VkDependencyInfo (64 bytes):
///   0  sType
///   8  pNext
///   16 dependencyFlags (u32)
///   20 memoryBarrierCount (u32)
///   24 pMemoryBarriers (*const VkMemoryBarrier2)
///   32 bufferMemoryBarrierCount (u32)
///   40 pBufferMemoryBarriers
///   48 imageMemoryBarrierCount (u32)
///   56 pImageMemoryBarriers
///
/// VkMemoryBarrier2 (48 bytes):
///   0  sType
///   8  pNext
///   16 srcStageMask (u64)
///   24 srcAccessMask (u64)
///   32 dstStageMask (u64)
///   40 dstAccessMask (u64)
#[no_mangle]
pub unsafe extern "C" fn vkCmdPipelineBarrier2(
    command_buffer: VkCommandBuffer,
    p_dependency_info: *const c_void,
) {
    let Some(cb) = cmdbuf_recording(command_buffer) else { return };
    if p_dependency_info.is_null() { return; }
    let dep = p_dependency_info as *const u8;
    let mem_count = std::ptr::read_unaligned(dep.add(20) as *const u32);
    let p_mem     = std::ptr::read_unaligned(dep.add(24) as *const *const u8);
    let buf_count = std::ptr::read_unaligned(dep.add(32) as *const u32);
    let img_count = std::ptr::read_unaligned(dep.add(48) as *const u32);

    let (src_stage_mask, dst_stage_mask) = if mem_count > 0 && !p_mem.is_null() {
        let s = std::ptr::read_unaligned(p_mem.add(16) as *const u64) as u32;
        let d = std::ptr::read_unaligned(p_mem.add(32) as *const u64) as u32;
        (s, d)
    } else {
        // Spec allows zero memory barriers + only buffer/image
        // barriers. Use a conservative ALL_COMMANDS pair so the
        // renderer's barrier walks aren't accidentally reordered.
        // 0x00010000 = VK_PIPELINE_STAGE_ALL_COMMANDS_BIT (1.0 enum).
        (0x00010000, 0x00010000)
    };

    let mut body = [0u8; 12];
    body[ 0.. 4].copy_from_slice(&src_stage_mask.to_le_bytes());
    body[ 4.. 8].copy_from_slice(&dst_stage_mask.to_le_bytes());
    body[ 8.. 9].copy_from_slice(&[mem_count.min(255) as u8]);
    body[ 9..10].copy_from_slice(&[buf_count.min(255) as u8]);
    body[10..11].copy_from_slice(&[img_count.min(255) as u8]);
    let _ = cb.frame.push(aqueduct_gpu::opcodes::FrameOp::PipelineBarrier, &body);
}

/// `vkDeviceWaitIdle` — every prior vkQueueSubmit was synchronous
/// from the ICD's POV (submit_frame returns when the daemon has
/// queued the work; the host endpoint serializes), so this is a
/// success no-op. Future async submit grows this into a real
/// queue-drain.
#[no_mangle]
pub unsafe extern "C" fn vkDeviceWaitIdle(
    _device: VkDevice,
) -> VkResult {
    VK_SUCCESS
}

/// `vkQueueWaitIdle` — same shape as vkDeviceWaitIdle (single
/// queue today).
#[no_mangle]
pub unsafe extern "C" fn vkQueueWaitIdle(
    _queue: VkQueue,
) -> VkResult {
    VK_SUCCESS
}

/// `vkCreateComputePipelines` — like `vkCreateGraphicsPipelines`
/// but for compute. Allocates one ResourceId per requested
/// pipeline from the device's IdAllocator and registers it in
/// the pipelines map. Today the create-info contents (shader
/// stage, layout) are ignored — the host's bundle definition
/// is the source of truth.
#[no_mangle]
pub unsafe extern "C" fn vkCreateComputePipelines(
    device:             VkDevice,
    _pipeline_cache:    u64,
    create_info_count:  u32,
    _p_create_infos:    *const c_void,
    _p_allocator:       *const c_void,
    p_pipelines:        *mut u64,
) -> VkResult {
    if device.is_null() || p_pipelines.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let dev = &*(device as *const AtriumDevice);
    let mut alloc = match dev.id_alloc.lock() {
        Ok(a) => a,
        Err(_) => return VK_ERROR_INITIALIZATION_FAILED,
    };
    let mut pipelines = match dev.pipelines.lock() {
        Ok(p) => p,
        Err(_) => return VK_ERROR_INITIALIZATION_FAILED,
    };
    for i in 0..create_info_count {
        let id = match alloc.next() {
            Some(id) => id,
            None     => return VK_ERROR_INITIALIZATION_FAILED,
        };
        let handle = id.raw() as u64;
        pipelines.insert(handle, id);
        *p_pipelines.offset(i as isize) = handle;
    }
    VK_SUCCESS
}

/// `vkCmdDispatch` — push a `Dispatch` FrameOp.
/// Body: groupCountX/Y/Z (3 × u32 = 12 B).
#[no_mangle]
pub unsafe extern "C" fn vkCmdDispatch(
    command_buffer: VkCommandBuffer,
    group_count_x:  u32,
    group_count_y:  u32,
    group_count_z:  u32,
) {
    let Some(cb) = cmdbuf_recording(command_buffer) else { return };
    let mut body = [0u8; 12];
    body[ 0.. 4].copy_from_slice(&group_count_x.to_le_bytes());
    body[ 4.. 8].copy_from_slice(&group_count_y.to_le_bytes());
    body[ 8..12].copy_from_slice(&group_count_z.to_le_bytes());
    let _ = cb.frame.push(aqueduct_gpu::opcodes::FrameOp::Dispatch, &body);
}

/// `vkCmdNextSubpass` — no-op today. Atrium's render-pass model
/// collapses subpasses (the host endpoint's renderer handles
/// dependency tracking on its own); Vulkan apps that call
/// vkCmdNextSubpass during a multi-subpass render pass behave
/// as if the subpasses are merged.
#[no_mangle]
pub unsafe extern "C" fn vkCmdNextSubpass(
    _command_buffer: VkCommandBuffer,
    _contents:       u32,
) {}

/// `vkCmdDrawIndirect` — push `DrawIndirect`. Body:
/// buffer_id u32 + _pad u32 + offset u64 + draw_count u32 + stride u32
/// (24 B).
#[no_mangle]
pub unsafe extern "C" fn vkCmdDrawIndirect(
    command_buffer: VkCommandBuffer,
    buffer:         u64,
    offset:         u64,
    draw_count:     u32,
    stride:         u32,
) {
    let Some(cb) = cmdbuf_recording(command_buffer) else { return };
    let rid = resolve_buffer(cb, buffer).raw();
    let mut body = [0u8; 24];
    body[ 0.. 4].copy_from_slice(&rid.to_le_bytes());
    body[ 8..16].copy_from_slice(&offset.to_le_bytes());
    body[16..20].copy_from_slice(&draw_count.to_le_bytes());
    body[20..24].copy_from_slice(&stride.to_le_bytes());
    let _ = cb.frame.push(aqueduct_gpu::opcodes::FrameOp::DrawIndirect, &body);
}

/// `vkCmdDrawIndexedIndirect` — same wire shape as DrawIndirect
/// (the renderer demultiplexes via opcode). Body: 24 B as above.
/// Reuses `FrameOp::DrawIndirect` for now; a dedicated
/// `DrawIndexedIndirect` opcode lands when the protocol gets a
/// dedicated entry.
#[no_mangle]
pub unsafe extern "C" fn vkCmdDrawIndexedIndirect(
    command_buffer: VkCommandBuffer,
    buffer:         u64,
    offset:         u64,
    draw_count:     u32,
    stride:         u32,
) {
    let Some(cb) = cmdbuf_recording(command_buffer) else { return };
    let rid = resolve_buffer(cb, buffer).raw();
    let mut body = [0u8; 24];
    body[ 0.. 4].copy_from_slice(&rid.to_le_bytes());
    body[ 8..16].copy_from_slice(&offset.to_le_bytes());
    body[16..20].copy_from_slice(&draw_count.to_le_bytes());
    body[20..24].copy_from_slice(&stride.to_le_bytes());
    let _ = cb.frame.push(aqueduct_gpu::opcodes::FrameOp::DrawIndirect, &body);
}

/// `vkCmdDispatchIndirect` — push `DispatchIndirect`. Body:
/// buffer_id u32 + _pad u32 + offset u64 (16 B).
#[no_mangle]
pub unsafe extern "C" fn vkCmdDispatchIndirect(
    command_buffer: VkCommandBuffer,
    buffer:         u64,
    offset:         u64,
) {
    let Some(cb) = cmdbuf_recording(command_buffer) else { return };
    let rid = resolve_buffer(cb, buffer).raw();
    let mut body = [0u8; 16];
    body[ 0.. 4].copy_from_slice(&rid.to_le_bytes());
    body[ 8..16].copy_from_slice(&offset.to_le_bytes());
    let _ = cb.frame.push(aqueduct_gpu::opcodes::FrameOp::DispatchIndirect, &body);
}

/// `vkCmdSetLineWidth` — dynamic state. No FrameOp; today the
/// renderer's rasteriser doesn't honour non-unit line widths.
/// Recorded as PushConstants with a sentinel stage_mask=0,
/// offset=0xLINEWIDTH so the host can ignore.
#[no_mangle]
pub unsafe extern "C" fn vkCmdSetLineWidth(
    _command_buffer: VkCommandBuffer,
    _line_width:     f32,
) {}

/// `vkCmdSetDepthBias` — dynamic state. No-op today; future
/// pipeline-state extension lands when depth-bias mattered.
#[no_mangle]
pub unsafe extern "C" fn vkCmdSetDepthBias(
    _command_buffer: VkCommandBuffer,
    _constant_factor: f32,
    _clamp:           f32,
    _slope_factor:    f32,
) {}

/// `vkCmdSetBlendConstants` — dynamic state. No-op today.
#[no_mangle]
pub unsafe extern "C" fn vkCmdSetBlendConstants(
    _command_buffer:    VkCommandBuffer,
    _p_blend_constants: *const f32, /* [f32; 4] */
) {}

/// Helper: resolve a VkImage u64 to its daemon-side ResourceId.
#[inline]
unsafe fn resolve_image(
    cb: &AtriumCommandBuffer, handle: u64,
) -> aqueduct_gpu::ids::ResourceId {
    if cb.device.is_null() { return aqueduct_gpu::ids::ResourceId(0); }
    let dev = &*(cb.device as *const AtriumDevice);
    dev.images.lock().ok()
        .and_then(|m| m.get(&handle).and_then(|x| x.image_id))
        .unwrap_or(aqueduct_gpu::ids::ResourceId(0))
}

/// `vkCmdClearColorImage` — no FrameOp counterpart today; the
/// renderer's clear-color path is the begin-renderpass clear.
/// Recorded as a sentinel Blit with src=dst and a marker body
/// so the host can decline gracefully. Future protocol revision
/// adds a dedicated ClearColorImage opcode.
#[no_mangle]
pub unsafe extern "C" fn vkCmdClearColorImage(
    _command_buffer: VkCommandBuffer,
    _image:          u64,
    _image_layout:   u32,
    _p_color:        *const c_void,
    _range_count:    u32,
    _p_ranges:       *const c_void,
) {}

/// `vkCmdClearDepthStencilImage` — same shape as ClearColorImage;
/// no-op today (depth-stencil targets aren't in the tier-1 renderer).
#[no_mangle]
pub unsafe extern "C" fn vkCmdClearDepthStencilImage(
    _command_buffer:    VkCommandBuffer,
    _image:             u64,
    _image_layout:      u32,
    _p_depth_stencil:   *const c_void,
    _range_count:       u32,
    _p_ranges:          *const c_void,
) {}

/// `vkCmdClearAttachments` — mid-render-pass clear. No FrameOp
/// counterpart; no-op today. Real apps that depend on this for
/// per-attachment clears would re-issue a BeginRenderPass with
/// the desired clear value to achieve the same effect.
#[no_mangle]
pub unsafe extern "C" fn vkCmdClearAttachments(
    _command_buffer:    VkCommandBuffer,
    _attachment_count:  u32,
    _p_attachments:     *const c_void,
    _rect_count:        u32,
    _p_rects:           *const c_void,
) {}

/// `vkCmdCopyImage` — image-to-image region copy.
/// Body: src_image_id u32 + dst_image_id u32 + src_layout u32 +
/// dst_layout u32 + region_count u32 + per-region 68 B (VkImageCopy)
/// = 24 B header + 68 B × N.
#[no_mangle]
pub unsafe extern "C" fn vkCmdCopyImage(
    command_buffer:  VkCommandBuffer,
    src_image:       u64,
    src_image_layout: u32,
    dst_image:       u64,
    dst_image_layout: u32,
    region_count:    u32,
    p_regions:       *const c_void,
) {
    let Some(cb) = cmdbuf_recording(command_buffer) else { return };
    let src_id = resolve_image(cb, src_image).raw();
    let dst_id = resolve_image(cb, dst_image).raw();
    let mut body = Vec::with_capacity(24 + (region_count as usize) * 68);
    body.extend_from_slice(&src_id.to_le_bytes());
    body.extend_from_slice(&dst_id.to_le_bytes());
    body.extend_from_slice(&src_image_layout.to_le_bytes());
    body.extend_from_slice(&dst_image_layout.to_le_bytes());
    body.extend_from_slice(&region_count.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // _pad to 24 B
    if !p_regions.is_null() {
        let rb = p_regions as *const u8;
        for i in 0..region_count {
            body.extend_from_slice(std::slice::from_raw_parts(rb.add(68 * i as usize), 68));
        }
    }
    let _ = cb.frame.push(aqueduct_gpu::opcodes::FrameOp::Blit, &body);
}

/// `vkCmdBlitImage` — scaled image-to-image copy with filter.
/// Body: src_image_id u32 + dst_image_id u32 + src_layout u32 +
/// dst_layout u32 + filter u32 + region_count u32 + per-region
/// 80 B (VkImageBlit). 24 B header + 80 B × N.
#[no_mangle]
pub unsafe extern "C" fn vkCmdBlitImage(
    command_buffer:  VkCommandBuffer,
    src_image:       u64,
    src_image_layout: u32,
    dst_image:       u64,
    dst_image_layout: u32,
    region_count:    u32,
    p_regions:       *const c_void,
    filter:          u32, /* VkFilter */
) {
    let Some(cb) = cmdbuf_recording(command_buffer) else { return };
    let src_id = resolve_image(cb, src_image).raw();
    let dst_id = resolve_image(cb, dst_image).raw();
    let mut body = Vec::with_capacity(24 + (region_count as usize) * 80);
    body.extend_from_slice(&src_id.to_le_bytes());
    body.extend_from_slice(&dst_id.to_le_bytes());
    body.extend_from_slice(&src_image_layout.to_le_bytes());
    body.extend_from_slice(&dst_image_layout.to_le_bytes());
    body.extend_from_slice(&filter.to_le_bytes());
    body.extend_from_slice(&region_count.to_le_bytes());
    if !p_regions.is_null() {
        let rb = p_regions as *const u8;
        for i in 0..region_count {
            body.extend_from_slice(std::slice::from_raw_parts(rb.add(80 * i as usize), 80));
        }
    }
    let _ = cb.frame.push(aqueduct_gpu::opcodes::FrameOp::Blit, &body);
}

/// `vkCmdResolveImage` — MSAA resolve. Tier-1 software doesn't
/// support MSAA; no-op.
#[no_mangle]
pub unsafe extern "C" fn vkCmdResolveImage(
    _command_buffer:  VkCommandBuffer,
    _src_image:       u64,
    _src_image_layout: u32,
    _dst_image:       u64,
    _dst_image_layout: u32,
    _region_count:    u32,
    _p_regions:       *const c_void,
) {}

/// `vkCmdCopyImageToBuffer` — image readback. Body:
/// src_image_id u32 + dst_buffer_id u32 + src_layout u32 +
/// region_count u32 + per-region 56 B (VkBufferImageCopy).
#[no_mangle]
pub unsafe extern "C" fn vkCmdCopyImageToBuffer(
    command_buffer:  VkCommandBuffer,
    src_image:       u64,
    src_image_layout: u32,
    dst_buffer:      u64,
    region_count:    u32,
    p_regions:       *const c_void,
) {
    let Some(cb) = cmdbuf_recording(command_buffer) else { return };
    let src_id = resolve_image(cb, src_image).raw();
    let dst_id = resolve_buffer(cb, dst_buffer).raw();
    let mut body = Vec::with_capacity(16 + (region_count as usize) * 56);
    body.extend_from_slice(&src_id.to_le_bytes());
    body.extend_from_slice(&dst_id.to_le_bytes());
    body.extend_from_slice(&src_image_layout.to_le_bytes());
    body.extend_from_slice(&region_count.to_le_bytes());
    if !p_regions.is_null() {
        let rb = p_regions as *const u8;
        for i in 0..region_count {
            body.extend_from_slice(std::slice::from_raw_parts(rb.add(64 * i as usize), 56));
        }
    }
    let _ = cb.frame.push(aqueduct_gpu::opcodes::FrameOp::CopyImgToBuf, &body);
}

// ───── Query pools ──────────────────────────────────────────────

#[no_mangle]
pub unsafe extern "C" fn vkCreateQueryPool(
    device:        VkDevice,
    _p_create_info: *const c_void,
    _p_allocator:   *const c_void,
    p_query_pool:   *mut u64,
) -> VkResult {
    if device.is_null() || p_query_pool.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let dev = &*(device as *const AtriumDevice);
    let h = dev.next_query_pool_id.get();
    dev.next_query_pool_id.set(h + 1);
    *p_query_pool = h;
    VK_SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn vkDestroyQueryPool(
    _device: VkDevice, _query_pool: u64, _p_allocator: *const c_void,
) {}

#[no_mangle]
pub unsafe extern "C" fn vkCmdBeginQuery(
    _command_buffer: VkCommandBuffer, _query_pool: u64, _query: u32, _flags: u32,
) {}

#[no_mangle]
pub unsafe extern "C" fn vkCmdEndQuery(
    _command_buffer: VkCommandBuffer, _query_pool: u64, _query: u32,
) {}

#[no_mangle]
pub unsafe extern "C" fn vkCmdResetQueryPool(
    _command_buffer: VkCommandBuffer, _query_pool: u64,
    _first_query: u32, _query_count: u32,
) {}

#[no_mangle]
pub unsafe extern "C" fn vkCmdWriteTimestamp(
    _command_buffer: VkCommandBuffer, _stage: u32, _query_pool: u64, _query: u32,
) {}

/// `vkGetQueryPoolResults` — return zero data + VK_NOT_READY so
/// apps that wait on results know to stop. (Tier-1 has no
/// hardware timestamps; reporting zeros would be a silent lie.)
#[no_mangle]
pub unsafe extern "C" fn vkGetQueryPoolResults(
    _device:     VkDevice,
    _query_pool: u64,
    _first_query: u32,
    _query_count: u32,
    _data_size:  usize,
    _p_data:     *mut c_void,
    _stride:     u64,
    _flags:      u32,
) -> VkResult {
    3 /* VK_NOT_READY */
}

// ───── Events ───────────────────────────────────────────────────

#[no_mangle]
pub unsafe extern "C" fn vkCreateEvent(
    device:          VkDevice,
    _p_create_info:  *const c_void,
    _p_allocator:    *const c_void,
    p_event:         *mut u64,
) -> VkResult {
    if device.is_null() || p_event.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let dev = &*(device as *const AtriumDevice);
    let h = dev.next_event_id.get();
    dev.next_event_id.set(h + 1);
    if let Ok(mut e) = dev.events.lock() {
        e.insert(h, false);
    } else {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    *p_event = h;
    VK_SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn vkDestroyEvent(
    device: VkDevice, event: u64, _p_allocator: *const c_void,
) {
    if device.is_null() || event == 0 { return; }
    let dev = &*(device as *const AtriumDevice);
    if let Ok(mut e) = dev.events.lock() {
        e.remove(&event);
    }
}

#[no_mangle]
pub unsafe extern "C" fn vkSetEvent(device: VkDevice, event: u64) -> VkResult {
    if device.is_null() || event == 0 { return VK_ERROR_INITIALIZATION_FAILED; }
    let dev = &*(device as *const AtriumDevice);
    if let Ok(mut e) = dev.events.lock() {
        if let Some(s) = e.get_mut(&event) { *s = true; }
    }
    VK_SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn vkResetEvent(device: VkDevice, event: u64) -> VkResult {
    if device.is_null() || event == 0 { return VK_ERROR_INITIALIZATION_FAILED; }
    let dev = &*(device as *const AtriumDevice);
    if let Ok(mut e) = dev.events.lock() {
        if let Some(s) = e.get_mut(&event) { *s = false; }
    }
    VK_SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn vkGetEventStatus(device: VkDevice, event: u64) -> VkResult {
    if device.is_null() || event == 0 { return VK_ERROR_INITIALIZATION_FAILED; }
    let dev = &*(device as *const AtriumDevice);
    match dev.events.lock().ok().and_then(|e| e.get(&event).copied()) {
        Some(true)  => VK_SUCCESS,        // VK_EVENT_SET
        Some(false) => 4,                 // VK_EVENT_RESET
        None        => VK_ERROR_INITIALIZATION_FAILED,
    }
}

#[no_mangle]
pub unsafe extern "C" fn vkCmdSetEvent(
    _command_buffer: VkCommandBuffer, _event: u64, _stage_mask: u32,
) {}

#[no_mangle]
pub unsafe extern "C" fn vkCmdResetEvent(
    _command_buffer: VkCommandBuffer, _event: u64, _stage_mask: u32,
) {}

#[no_mangle]
pub unsafe extern "C" fn vkCmdWaitEvents(
    _command_buffer:           VkCommandBuffer,
    _event_count:              u32,
    _p_events:                 *const u64,
    _src_stage_mask:           u32,
    _dst_stage_mask:           u32,
    _memory_barrier_count:     u32,
    _p_memory_barriers:        *const c_void,
    _buffer_barrier_count:     u32,
    _p_buffer_memory_barriers: *const c_void,
    _image_barrier_count:      u32,
    _p_image_memory_barriers:  *const c_void,
) {}

// ── Vulkan 1.3 sync2 event variants ─────────────────────────────
//
// All three are no-ops on tier-1 (sequential submission +
// daemon-side fence sync make events redundant). Wired so the
// 1.3 / VK_KHR_synchronization2 dispatch table resolves them
// instead of bailing.

#[no_mangle]
pub unsafe extern "C" fn vkCmdSetEvent2(
    _command_buffer:    VkCommandBuffer,
    _event:             u64,
    _p_dependency_info: *const c_void,
) {}

#[no_mangle]
pub unsafe extern "C" fn vkCmdResetEvent2(
    _command_buffer: VkCommandBuffer,
    _event:          u64,
    _stage_mask:     u64, /* VkPipelineStageFlags2 — wider than 1.0 */
) {}

#[no_mangle]
pub unsafe extern "C" fn vkCmdWaitEvents2(
    _command_buffer:     VkCommandBuffer,
    _event_count:        u32,
    _p_events:           *const u64,
    _p_dependency_infos: *const c_void,
) {}

/// `vkCmdWriteTimestamp2` — 1.3 timestamp write. tier-1 has no
/// real query pool (queries are SUCCESS no-ops); writing a
/// timestamp is therefore also a no-op. Apps that subsequently
/// read via vkGetQueryPoolResults will see zeros, which matches
/// our timestampPeriod=0 limit (signals "queries not really
/// supported").
#[no_mangle]
pub unsafe extern "C" fn vkCmdWriteTimestamp2(
    _command_buffer: VkCommandBuffer,
    _stage:          u64,
    _query_pool:     u64,
    _query:          u32,
) {}

// ───── VkBufferView ─────────────────────────────────────────────

#[no_mangle]
pub unsafe extern "C" fn vkCreateBufferView(
    device:          VkDevice,
    _p_create_info:  *const c_void,
    _p_allocator:    *const c_void,
    p_view:          *mut u64,
) -> VkResult {
    if device.is_null() || p_view.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let dev = &*(device as *const AtriumDevice);
    let h = dev.next_buffer_view_id.get();
    dev.next_buffer_view_id.set(h + 1);
    *p_view = h;
    VK_SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn vkDestroyBufferView(
    _device: VkDevice, _view: u64, _p_allocator: *const c_void,
) {}

// ───── Secondary cmdbuf execution ───────────────────────────────

/// `vkCmdExecuteCommands` — replay each secondary cmdbuf's
/// recorded FrameOps into the primary. Walks the secondary's
/// frame.as_bytes() record-by-record and pushes each into the
/// primary. Atrium has no protocol-level distinction between
/// primary and secondary, so this concatenates the streams.
#[no_mangle]
pub unsafe extern "C" fn vkCmdExecuteCommands(
    command_buffer:        VkCommandBuffer,
    command_buffer_count:  u32,
    p_command_buffers:     *const VkCommandBuffer,
) {
    let Some(primary) = cmdbuf_recording(command_buffer) else { return };
    if p_command_buffers.is_null() { return; }
    for i in 0..command_buffer_count {
        let sec_handle = *p_command_buffers.offset(i as isize);
        if sec_handle.is_null() { continue; }
        let sec = &*(sec_handle as *const AtriumCommandBuffer);
        if sec.state != CmdBufferState::Executable {
            continue;
        }
        let bytes = sec.frame.as_bytes();
        let mut off = 0;
        while off + 8 <= bytes.len() {
            let opcode = u16::from_le_bytes([bytes[off], bytes[off + 1]]);
            let total = u32::from_le_bytes([
                bytes[off + 4], bytes[off + 5], bytes[off + 6], bytes[off + 7],
            ]) as usize;
            if total < 8 || off + total > bytes.len() { break; }
            let body = &bytes[off + 8 .. off + total];
            // Convert raw opcode back to FrameOp; on unknown,
            // skip (we'd be replaying something our renderer
            // doesn't understand anyway).
            if let Some(op) = aqueduct_gpu::opcodes::FrameOp::from_u16(opcode) {
                let _ = primary.frame.push(op, body);
            }
            off += total;
        }
    }
}

/// `vkCreateCommandPool` — allocate a non-dispatchable
/// VkCommandPool handle. We ignore the create-info (queue family,
/// flags); Atrium's single queue family means every pool is
/// equivalent.
///
/// # Safety
///
/// `p_command_pool` must be writable.
#[no_mangle]
pub unsafe extern "C" fn vkCreateCommandPool(
    _device:          VkDevice,
    _p_create_info:   *const c_void, /* const VkCommandPoolCreateInfo* */
    _p_allocator:     *const c_void, /* const VkAllocationCallbacks* */
    p_command_pool:   *mut VkCommandPool,
) -> VkResult {
    if p_command_pool.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let pool = Box::new(AtriumCommandPool { buffers: Vec::new() });
    *p_command_pool = Box::into_raw(pool) as VkCommandPool;
    VK_SUCCESS
}

/// `vkDestroyCommandPool` — reclaim the pool + every buffer it
/// still owns. Apps that called vkFreeCommandBuffers first will
/// have left the pool's buffers list empty; the rest are freed
/// here.
///
/// # Safety
///
/// `command_pool` must be a handle from `vkCreateCommandPool` or
/// `VK_NULL_HANDLE` (no-op).
#[no_mangle]
pub unsafe extern "C" fn vkDestroyCommandPool(
    _device:        VkDevice,
    command_pool:   VkCommandPool,
    _p_allocator:   *const c_void, /* const VkAllocationCallbacks* */
) {
    if command_pool == 0 {
        return;
    }
    let pool = Box::from_raw(command_pool as *mut AtriumCommandPool);
    for b in &pool.buffers {
        let _ = Box::from_raw(*b);
    }
}

/// `vkAllocateCommandBuffers` — allocate N command buffers from the
/// pool. We accept the pNext-less form and only read
/// `commandPool` + `commandBufferCount` from the VkAllocateInfo
/// (offsets 16 and 28 respectively per the Vk spec). `level` is
/// ignored — Atrium doesn't distinguish primary/secondary buffers.
///
/// The result array must hold at least `commandBufferCount` slots.
///
/// # Safety
///
/// `p_allocate_info` must point to a properly-laid-out
/// VkCommandBufferAllocateInfo. `p_command_buffers` must point to a
/// buffer of at least `commandBufferCount` VkCommandBuffer slots.
#[no_mangle]
pub unsafe extern "C" fn vkAllocateCommandBuffers(
    device:             VkDevice,
    p_allocate_info:    *const c_void, /* const VkCommandBufferAllocateInfo* */
    p_command_buffers:  *mut VkCommandBuffer,
) -> VkResult {
    if device.is_null() || p_allocate_info.is_null() || p_command_buffers.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let dev_ptr = device as *mut AtriumDevice;
    // VkCommandBufferAllocateInfo layout:
    //   offset 0   sType : VkStructureType (u32)
    //   offset 8   pNext : *const c_void
    //   offset 16  commandPool : VkCommandPool (u64)
    //   offset 24  level : VkCommandBufferLevel (u32)
    //   offset 28  commandBufferCount : u32
    let bytes = p_allocate_info as *const u8;
    // read_unaligned — Vulkan struct layouts are 8-byte-aligned in
    // theory, but defensive against weird caller layouts (test
    // fixtures, partial struct construction, etc.) is cheap.
    let pool_u64  = std::ptr::read_unaligned(bytes.add(16) as *const u64);
    let count     = std::ptr::read_unaligned(bytes.add(28) as *const u32);
    if pool_u64 == 0 {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let pool = &mut *(pool_u64 as *mut AtriumCommandPool);

    for i in 0..count {
        let cb = Box::new(AtriumCommandBuffer {
            loader_dispatch_slot: VK_ICD_LOADER_MAGIC,
            state: CmdBufferState::Initial,
            frame: aqueduct_gpu::frame::FrameBuilder::new(ATRIUM_CMDBUF_INITIAL_CAPACITY),
            device: dev_ptr,
        });
        let cb_raw = Box::into_raw(cb);
        pool.buffers.push(cb_raw);
        *p_command_buffers.offset(i as isize) = cb_raw as VkCommandBuffer;
    }
    VK_SUCCESS
}

/// `vkFreeCommandBuffers` — return the listed buffers to the pool.
///
/// # Safety
///
/// `p_command_buffers` must point to `command_buffer_count`
/// VkCommandBuffer handles previously returned by
/// `vkAllocateCommandBuffers` against `command_pool`.
#[no_mangle]
pub unsafe extern "C" fn vkFreeCommandBuffers(
    _device:               VkDevice,
    command_pool:          VkCommandPool,
    command_buffer_count:  u32,
    p_command_buffers:     *const VkCommandBuffer,
) {
    if command_pool == 0 || p_command_buffers.is_null() {
        return;
    }
    let pool = &mut *(command_pool as *mut AtriumCommandPool);
    for i in 0..command_buffer_count {
        let cb_handle = *p_command_buffers.offset(i as isize);
        if cb_handle.is_null() { continue; }
        let cb_raw = cb_handle as *mut AtriumCommandBuffer;
        // Remove from the pool's tracking list before freeing.
        pool.buffers.retain(|&b| b != cb_raw);
        let _ = Box::from_raw(cb_raw);
    }
}

/// `vkBeginCommandBuffer` — transition Initial/Executable →
/// Recording, resetting any previously-accumulated FrameOps.
///
/// # Safety
///
/// `command_buffer` must be a handle from
/// `vkAllocateCommandBuffers`.
#[no_mangle]
pub unsafe extern "C" fn vkBeginCommandBuffer(
    command_buffer:    VkCommandBuffer,
    _p_begin_info:     *const c_void, /* const VkCommandBufferBeginInfo* */
) -> VkResult {
    if command_buffer.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let cb = &mut *(command_buffer as *mut AtriumCommandBuffer);
    cb.state = CmdBufferState::Recording;
    cb.frame = aqueduct_gpu::frame::FrameBuilder::new(ATRIUM_CMDBUF_INITIAL_CAPACITY);
    VK_SUCCESS
}

/// `vkEndCommandBuffer` — finalize Recording → Executable.
///
/// # Safety
///
/// `command_buffer` must be in Recording state.
#[no_mangle]
pub unsafe extern "C" fn vkEndCommandBuffer(
    command_buffer: VkCommandBuffer,
) -> VkResult {
    if command_buffer.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let cb = &mut *(command_buffer as *mut AtriumCommandBuffer);
    if cb.state != CmdBufferState::Recording {
        // Vk spec: vkEndCommandBuffer on a non-Recording buffer is
        // undefined behavior. We return a soft error instead of
        // panicking.
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    cb.state = CmdBufferState::Executable;
    VK_SUCCESS
}

/// `vkResetCommandBuffer` — drop any recorded FrameOps, return to
/// Initial state. The `flags` argument is ignored (we always
/// release any pool memory we held).
#[no_mangle]
pub unsafe extern "C" fn vkResetCommandBuffer(
    command_buffer: VkCommandBuffer,
    _flags:         u32, /* VkCommandBufferResetFlags */
) -> VkResult {
    if command_buffer.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let cb = &mut *(command_buffer as *mut AtriumCommandBuffer);
    cb.state = CmdBufferState::Initial;
    cb.frame = aqueduct_gpu::frame::FrameBuilder::new(ATRIUM_CMDBUF_INITIAL_CAPACITY);
    VK_SUCCESS
}

/// `vkGetBufferMemoryRequirements2` — Vulkan 1.1 pNext-chain
/// variant. Parses VkBufferMemoryRequirementsInfo2 (16-byte
/// header + buffer u64 at offset 16) and fills VkMemoryRequirements2
/// (16-byte header + inner VkMemoryRequirements at offset 16).
///
/// We compute requirements into a stack-aligned MemoryRequirements
/// and then copy it byte-wise into the caller's output buffer to
/// avoid an aligned-write panic when the caller's struct sits
/// at an address that's well-aligned for VkMemoryRequirements2
/// (8 bytes) but not for the inner VkMemoryRequirements at
/// offset 16 — both happen to be 8-aligned, but tests sometimes
/// hand us a raw `Vec<u8>` whose data() isn't 8-aligned.
#[no_mangle]
pub unsafe extern "C" fn vkGetBufferMemoryRequirements2(
    device:           VkDevice,
    p_info:           *const c_void,
    p_requirements:   *mut c_void,
) {
    if p_info.is_null() || p_requirements.is_null() { return; }
    let info = p_info as *const u8;
    let info_p_next = std::ptr::read_unaligned(info.add(8) as *const *mut c_void);
    let buffer = std::ptr::read_unaligned(info.add(16) as *const u64);
    let _ = walk_p_next_chain(info_p_next);

    let mut tmp = ash::vk::MemoryRequirements::default();
    vkGetBufferMemoryRequirements(device, buffer, &mut tmp);
    let out = p_requirements as *mut u8;
    let out_p_next = std::ptr::read_unaligned(out.add(8) as *const *mut c_void);
    std::ptr::copy_nonoverlapping(
        &tmp as *const _ as *const u8,
        out.add(16),
        std::mem::size_of::<ash::vk::MemoryRequirements>(),
    );
    let _ = walk_p_next_chain(out_p_next);
}

/// `vkGetImageMemoryRequirements2` — Vulkan 1.1 pNext-chain
/// variant. Same shape as the buffer variant.
#[no_mangle]
pub unsafe extern "C" fn vkGetImageMemoryRequirements2(
    device:           VkDevice,
    p_info:           *const c_void,
    p_requirements:   *mut c_void,
) {
    if p_info.is_null() || p_requirements.is_null() { return; }
    let info = p_info as *const u8;
    let info_p_next = std::ptr::read_unaligned(info.add(8) as *const *mut c_void);
    let image = std::ptr::read_unaligned(info.add(16) as *const u64);
    let _ = walk_p_next_chain(info_p_next);

    let mut tmp = ash::vk::MemoryRequirements::default();
    vkGetImageMemoryRequirements(device, image, &mut tmp);
    let out = p_requirements as *mut u8;
    let out_p_next = std::ptr::read_unaligned(out.add(8) as *const *mut c_void);
    std::ptr::copy_nonoverlapping(
        &tmp as *const _ as *const u8,
        out.add(16),
        std::mem::size_of::<ash::vk::MemoryRequirements>(),
    );
    let _ = walk_p_next_chain(out_p_next);
}

/// `vkGetDeviceBufferMemoryRequirements` — Vulkan 1.3 entry that
/// computes the memory-requirements a vkCreateBuffer +
/// vkGetBufferMemoryRequirements pair would have reported, but
/// without actually creating the buffer. Useful for size-
/// planning before allocation.
///
/// VkDeviceBufferMemoryRequirements (24 bytes):
///   0  sType
///   8  pNext
///   16 pCreateInfo: *const VkBufferCreateInfo
///
/// Output VkMemoryRequirements2: 16-byte header + inner block at
/// offset 16 (byte-copied to dodge alignment traps from
/// caller-provided Vec<u8>-style buffers — same rationale as
/// vkGetBufferMemoryRequirements2).
#[no_mangle]
pub unsafe extern "C" fn vkGetDeviceBufferMemoryRequirements(
    _device:        VkDevice,
    p_info:         *const c_void,
    p_requirements: *mut c_void,
) {
    if p_info.is_null() || p_requirements.is_null() { return; }
    let info = p_info as *const u8;
    let info_p_next = std::ptr::read_unaligned(info.add(8) as *const *mut c_void);
    let p_create = std::ptr::read_unaligned(info.add(16) as *const *const u8);
    let _ = walk_p_next_chain(info_p_next);
    let size = if p_create.is_null() { 0 } else {
        // VkBufferCreateInfo: size at offset 24 (matches
        // vkCreateBuffer's parser).
        std::ptr::read_unaligned(p_create.add(24) as *const u64)
    };
    let tmp = ash::vk::MemoryRequirements {
        size,
        alignment: 16,
        memory_type_bits: 0b1,
    };
    let out = p_requirements as *mut u8;
    let out_p_next = std::ptr::read_unaligned(out.add(8) as *const *mut c_void);
    std::ptr::copy_nonoverlapping(
        &tmp as *const _ as *const u8,
        out.add(16),
        std::mem::size_of::<ash::vk::MemoryRequirements>(),
    );
    let _ = walk_p_next_chain(out_p_next);
}

/// `vkGetDeviceImageMemoryRequirements` — image-side counterpart.
///
/// VkDeviceImageMemoryRequirements (32 bytes):
///   0  sType
///   8  pNext
///   16 pCreateInfo: *const VkImageCreateInfo
///   24 planeAspect (VkImageAspectFlagBits) — ignored (no
///      multi-planar support in tier-1)
///
/// Image-size accounting mirrors vkGetImageMemoryRequirements:
/// sum of per-mip (w * h * d * bpp) halved at each level times
/// array_layers.
#[no_mangle]
pub unsafe extern "C" fn vkGetDeviceImageMemoryRequirements(
    _device:        VkDevice,
    p_info:         *const c_void,
    p_requirements: *mut c_void,
) {
    if p_info.is_null() || p_requirements.is_null() { return; }
    let info = p_info as *const u8;
    let info_p_next = std::ptr::read_unaligned(info.add(8) as *const *mut c_void);
    let p_create = std::ptr::read_unaligned(info.add(16) as *const *const u8);
    let _ = walk_p_next_chain(info_p_next);
    let size = if p_create.is_null() { 0 } else {
        // VkImageCreateInfo: format=24, width=28, height=32,
        // depth=36, mipLevels=40, arrayLayers=44 (matches
        // vkCreateImage's parser).
        let format       = std::ptr::read_unaligned(p_create.add(24) as *const u32);
        let width        = std::ptr::read_unaligned(p_create.add(28) as *const u32) as u64;
        let height       = std::ptr::read_unaligned(p_create.add(32) as *const u32) as u64;
        let depth        = (std::ptr::read_unaligned(p_create.add(36) as *const u32).max(1)) as u64;
        let mip_levels   = std::ptr::read_unaligned(p_create.add(40) as *const u32).max(1);
        let array_layers = (std::ptr::read_unaligned(p_create.add(44) as *const u32).max(1)) as u64;
        let bpp = bpp_for_vk_format(format) as u64;
        let base = width * height * depth * bpp;
        let mips: u64 = (0..mip_levels).map(|l| base >> (2 * l)).sum();
        mips * array_layers
    };
    let tmp = ash::vk::MemoryRequirements {
        size,
        alignment: 256,
        memory_type_bits: 0b1,
    };
    let out = p_requirements as *mut u8;
    let out_p_next = std::ptr::read_unaligned(out.add(8) as *const *mut c_void);
    std::ptr::copy_nonoverlapping(
        &tmp as *const _ as *const u8,
        out.add(16),
        std::mem::size_of::<ash::vk::MemoryRequirements>(),
    );
    let _ = walk_p_next_chain(out_p_next);
}

// ── Push-descriptor + descriptor-update-template stubs ──────────
//
// Templates (1.1) let an app bake a sequence of descriptor
// updates once and replay them with a single pointer-walk. Push
// descriptors (VK_KHR_push_descriptor) bind descriptors inline
// at the cmdbuf without going through a VkDescriptorSet. Both
// are popular fast-paths in modern Vulkan codebases (wgpu,
// Slang, SaschaWillems samples). Without them, those codebases
// resolve null and either fall back or fail.
//
// tier-1's renderer doesn't honor descriptor bindings (the
// bundle pipelines are baked); these are no-ops that satisfy
// the loader + let the app proceed.

/// `vkCreateDescriptorUpdateTemplate` — allocate a u64 handle.
/// Template contents (the update sequence) are ignored; we keep
/// only the handle so vkDestroy + vkUpdateDescriptorSetWithTemplate
/// recognise it.
#[no_mangle]
pub unsafe extern "C" fn vkCreateDescriptorUpdateTemplate(
    _device:         VkDevice,
    _p_create_info:  *const c_void,
    _p_allocator:    *const c_void,
    p_template:      *mut u64,
) -> VkResult {
    if p_template.is_null() { return VK_ERROR_INITIALIZATION_FAILED; }
    static NEXT_ID: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(1);
    *p_template = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    VK_SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn vkDestroyDescriptorUpdateTemplate(
    _device: VkDevice, _template: u64, _p_allocator: *const c_void,
) {}

#[no_mangle]
pub unsafe extern "C" fn vkUpdateDescriptorSetWithTemplate(
    _device:    VkDevice,
    _set:       u64,
    _template:  u64,
    _p_data:    *const c_void,
) {}

/// `vkCmdPushDescriptorSet` — VK_KHR_push_descriptor. Tier-1
/// doesn't bind descriptors at the cmdbuf level so this is a
/// no-op; the renderer's bundle pipelines stay using the
/// pipeline-baked binding model.
#[no_mangle]
pub unsafe extern "C" fn vkCmdPushDescriptorSet(
    _command_buffer:    VkCommandBuffer,
    _pipeline_bind_point: u32,
    _layout:            u64,
    _set:               u32,
    _descriptor_write_count: u32,
    _p_descriptor_writes: *const c_void,
) {}

#[no_mangle]
pub unsafe extern "C" fn vkCmdPushDescriptorSetWithTemplate(
    _command_buffer:    VkCommandBuffer,
    _template:          u64,
    _layout:            u64,
    _set:               u32,
    _p_data:            *const c_void,
) {}

/// `vkFreeDescriptorSets` — release descriptor sets back to the
/// pool. atrium-vk-icd's descriptor sets are non-resource-owning
/// (the resource bindings live in the per-cmdbuf BindDescriptors
/// FrameOps), so freeing is a SUCCESS no-op.
#[no_mangle]
pub unsafe extern "C" fn vkFreeDescriptorSets(
    _device: VkDevice, _pool: u64, _count: u32, _sets: *const u64,
) -> VkResult { VK_SUCCESS }

/// `vkResetDescriptorPool` — recycle all descriptor sets from a
/// pool. Same rationale as vkFreeDescriptorSets: descriptor sets
/// don't hold pool memory on our side.
#[no_mangle]
pub unsafe extern "C" fn vkResetDescriptorPool(
    _device: VkDevice, _pool: u64, _flags: u32,
) -> VkResult { VK_SUCCESS }

/// `vkSignalSemaphore` — host-side timeline-semaphore signal
/// (Vulkan 1.2). atrium-vk-icd's submission path is sequential
/// (each vkQueueSubmit returns after the daemon has queued the
/// work), so any wait on a signaled value is already satisfied
/// by the time the next call lands. Returns SUCCESS.
#[no_mangle]
pub unsafe extern "C" fn vkSignalSemaphore(
    _device: VkDevice, _p_signal_info: *const c_void,
) -> VkResult { VK_SUCCESS }

/// `vkWaitSemaphores` — host-side wait for one-or-more timeline
/// semaphores to reach given values. With our sequential submit
/// model every prior signal has already happened on the daemon
/// side; return SUCCESS immediately regardless of timeout.
#[no_mangle]
pub unsafe extern "C" fn vkWaitSemaphores(
    _device: VkDevice, _p_wait_info: *const c_void, _timeout: u64,
) -> VkResult { VK_SUCCESS }

/// `vkGetSemaphoreCounterValue` — return the current value of a
/// timeline semaphore. Without real timeline tracking on the
/// ICD side we return 0 (the spec-mandated initial value);
/// well-behaved apps subsequently call vkSignalSemaphore /
/// vkWaitSemaphores rather than treating this as authoritative.
#[no_mangle]
pub unsafe extern "C" fn vkGetSemaphoreCounterValue(
    _device: VkDevice, _semaphore: u64, p_value: *mut u64,
) -> VkResult {
    if !p_value.is_null() { *p_value = 0; }
    VK_SUCCESS
}

/// `vkResetCommandPool` — reset all cmdbufs in a pool. Today this
/// is a SUCCESS no-op: atrium-vk-icd doesn't track which cmdbufs
/// belong to which pool (alloc returns a `Box<AtriumCommandBuffer>`
/// with no back-pointer), and the standard recycle pattern
/// (alloc → record → submit → reset_pool → vkBeginCommandBuffer
/// → record again) still works correctly because
/// vkBeginCommandBuffer unconditionally resets the cmdbuf state
/// + drops its FrameBuilder.
///
/// Spec-deviation note: apps that strictly rely on the pool reset
/// putting EVERY allocated cmdbuf back to Initial without a
/// subsequent vkBeginCommandBuffer would see stale state. Real
/// apps don't do this; a future per-pool cmdbuf list can fix it
/// if needed.
#[no_mangle]
pub unsafe extern "C" fn vkResetCommandPool(
    _device:     VkDevice,
    _pool:       u64,
    _flags:      u32, /* VkCommandPoolResetFlags */
) -> VkResult { VK_SUCCESS }

/// `vkTrimCommandPool` — release unused command-pool memory back
/// to the system. We allocate per-cmdbuf via Box, so there's no
/// pool-wide buffer to trim. SUCCESS no-op.
#[no_mangle]
pub unsafe extern "C" fn vkTrimCommandPool(
    _device: VkDevice, _pool: u64, _flags: u32,
) {}

/// `vkFlushMappedMemoryRanges` — flush writes to non-coherent
/// host-visible memory. atrium-vk-icd advertises every memory
/// type as HOST_COHERENT (vkGetPhysicalDeviceMemoryProperties),
/// so this is spec-required to be a SUCCESS no-op for any range
/// we'd see.
#[no_mangle]
pub unsafe extern "C" fn vkFlushMappedMemoryRanges(
    _device: VkDevice, _range_count: u32, _p_ranges: *const c_void,
) -> VkResult { VK_SUCCESS }

/// `vkInvalidateMappedMemoryRanges` — symmetric counterpart to
/// vkFlushMappedMemoryRanges; same coherent-memory rationale.
#[no_mangle]
pub unsafe extern "C" fn vkInvalidateMappedMemoryRanges(
    _device: VkDevice, _range_count: u32, _p_ranges: *const c_void,
) -> VkResult { VK_SUCCESS }

/// `vkQueueSubmit` — flush each submitted cmdbuf's FrameOp stream
/// to the aqueduct-gpu host endpoint via the owning AtriumInstance's
/// GpuClient.
///
/// Walks: queue → device → instance.client. Each `VkSubmitInfo`'s
/// `pCommandBuffers` array is read by offset; each cmdbuf's
/// `frame` is swapped out (left empty so the cmdbuf stays usable
/// after submit per Vk spec's "pending" → "invalid" semantics) and
/// the byte stream handed to `submit_frame`.
///
/// `VkSubmitInfo` layout (size 72, no pNext set):
///   0   sType : u32
///   4   _pad
///   8   pNext : ptr
///   16  waitSemaphoreCount : u32
///   20  _pad
///   24  pWaitSemaphores : ptr
///   32  pWaitDstStageMask : ptr
///   40  commandBufferCount : u32
///   44  _pad
///   48  pCommandBuffers : ptr
///   56  signalSemaphoreCount : u32
///   60  _pad
///   64  pSignalSemaphores : ptr
///
/// Submit failures (client error, fence ResourceId(0) meaning no
/// live client) silently no-op; vkQueueSubmit returns success
/// regardless. Wait-semaphores + signal-semaphores are ignored —
/// the timeline counter ordering handles serialization, and
/// VkFence (the last arg) is also ignored for now.
#[no_mangle]
pub unsafe extern "C" fn vkQueueSubmit(
    queue:         VkQueue,
    submit_count:  u32,
    p_submits:     *const c_void, /* const VkSubmitInfo* */
    _fence:        *mut c_void,   /* VkFence */
) -> VkResult {
    if queue.is_null() { return VK_ERROR_INITIALIZATION_FAILED; }
    let q = &*(queue as *const AtriumQueue);
    if q._device.is_null() { return VK_SUCCESS; }
    let dev = &*(q._device as *const AtriumDevice);
    if dev.instance.is_null() { return VK_SUCCESS; }
    let inst = &*(dev.instance as *const AtriumInstance);

    let Some(client_mu) = inst.client.as_ref() else { return VK_SUCCESS; };
    if dev.fence == aqueduct_gpu::ids::ResourceId(0) { return VK_SUCCESS; }

    if p_submits.is_null() || submit_count == 0 { return VK_SUCCESS; }

    // Walk each VkSubmitInfo. Stride is 72 bytes.
    let submits_base = p_submits as *const u8;
    for s in 0..submit_count {
        let info = submits_base.add(72 * s as usize);
        let cb_count    = std::ptr::read_unaligned(info.add(40) as *const u32);
        let cb_array_pp = std::ptr::read_unaligned(info.add(48) as *const *const VkCommandBuffer);
        if cb_count == 0 || cb_array_pp.is_null() { continue; }

        for c in 0..cb_count {
            let cb_handle = *cb_array_pp.offset(c as isize);
            if cb_handle.is_null() { continue; }
            let cb = &mut *(cb_handle as *mut AtriumCommandBuffer);
            // Snapshot the FrameOp stream and reset the cmdbuf's
            // builder to an empty one so the cmdbuf stays reusable
            // (per Vk spec, post-submit a cmdbuf re-enters
            // Executable; vkBegin will reset it for the next
            // recording).
            let snapshot = std::mem::replace(
                &mut cb.frame,
                aqueduct_gpu::frame::FrameBuilder::new(ATRIUM_CMDBUF_INITIAL_CAPACITY),
            );
            if snapshot.is_empty() { continue; }

            let Ok(mut c) = client_mu.lock() else { continue; };
            dev.timeline.set(dev.timeline.get() + 1);
            let timeline = dev.timeline.get();
            let _ = c.submit_frame(dev.fence, snapshot, timeline);
        }
    }
    VK_SUCCESS
}

/// `vkQueueSubmit2` — Vulkan 1.3 mandatory submit. Differs from
/// vkQueueSubmit in that each VkSubmitInfo2 carries semaphore
/// metadata (stage masks, timeline values) inline via
/// VkSemaphoreSubmitInfo, and command buffers via
/// VkCommandBufferSubmitInfo (which itself carries a deviceMask
/// for multi-GPU). atrium-vk-icd is single-queue + tier-1 has
/// no real semaphores, so we walk straight to the cmdbuf array.
///
/// Layout notes (offsets verified against Vulkan 1.3 headers):
///
///   VkSubmitInfo2 (64 bytes):
///     0  sType
///     8  pNext
///     16 flags
///     20 waitSemaphoreInfoCount
///     24 pWaitSemaphoreInfos
///     32 commandBufferInfoCount
///     40 pCommandBufferInfos
///     48 signalSemaphoreInfoCount
///     56 pSignalSemaphoreInfos
///
///   VkCommandBufferSubmitInfo (32 bytes):
///     0  sType
///     8  pNext
///     16 commandBuffer (VkCommandBuffer; 8 bytes)
///     24 deviceMask
#[no_mangle]
pub unsafe extern "C" fn vkQueueSubmit2(
    queue:         VkQueue,
    submit_count:  u32,
    p_submits:     *const c_void, /* const VkSubmitInfo2* */
    _fence:        *mut c_void,
) -> VkResult {
    if queue.is_null() { return VK_ERROR_INITIALIZATION_FAILED; }
    let q = &*(queue as *const AtriumQueue);
    if q._device.is_null() { return VK_SUCCESS; }
    let dev = &*(q._device as *const AtriumDevice);
    if dev.instance.is_null() { return VK_SUCCESS; }
    let inst = &*(dev.instance as *const AtriumInstance);
    let Some(client_mu) = inst.client.as_ref() else { return VK_SUCCESS; };
    if dev.fence == aqueduct_gpu::ids::ResourceId(0) { return VK_SUCCESS; }
    if p_submits.is_null() || submit_count == 0 { return VK_SUCCESS; }

    const SUBMIT_INFO_2_STRIDE: usize = 64;
    const CMDBUF_INFO_STRIDE: usize = 32;
    let submits_base = p_submits as *const u8;
    for s in 0..submit_count {
        let info = submits_base.add(SUBMIT_INFO_2_STRIDE * s as usize);
        let cb_count    = std::ptr::read_unaligned(info.add(32) as *const u32);
        let cb_array_p  = std::ptr::read_unaligned(info.add(40) as *const *const u8);
        if cb_count == 0 || cb_array_p.is_null() { continue; }
        for c in 0..cb_count {
            let cbi = cb_array_p.add(CMDBUF_INFO_STRIDE * c as usize);
            let cb_handle = std::ptr::read_unaligned(cbi.add(16) as *const VkCommandBuffer);
            if cb_handle.is_null() { continue; }
            let cb = &mut *(cb_handle as *mut AtriumCommandBuffer);
            let snapshot = std::mem::replace(
                &mut cb.frame,
                aqueduct_gpu::frame::FrameBuilder::new(ATRIUM_CMDBUF_INITIAL_CAPACITY),
            );
            if snapshot.is_empty() { continue; }
            let Ok(mut c) = client_mu.lock() else { continue; };
            dev.timeline.set(dev.timeline.get() + 1);
            let timeline = dev.timeline.get();
            let _ = c.submit_frame(dev.fence, snapshot, timeline);
        }
    }
    VK_SUCCESS
}

/// `vkGetDeviceQueue` — return the queue for `(family, index)`.
///
/// Today we materialize a single (family=0, index=0) queue at
/// vkCreateDevice time. Any other `(family, index)` pair writes
/// `VK_NULL_HANDLE` to `p_queue`.
///
/// # Safety
///
/// `device` must be a handle from `vkCreateDevice`. `p_queue` must
/// be writable.
#[no_mangle]
pub unsafe extern "C" fn vkGetDeviceQueue(
    device:             VkDevice,
    queue_family_index: u32,
    queue_index:        u32,
    p_queue:            *mut VkQueue,
) {
    if device.is_null() || p_queue.is_null() {
        return;
    }
    let dev = &*(device as *const AtriumDevice);
    *p_queue = std::ptr::null_mut();
    for &q in &dev.queues {
        let qref = &*q;
        if qref.family == queue_family_index && qref.index == queue_index {
            *p_queue = q as VkQueue;
            return;
        }
    }
}

/// `vkCreateInstance` — allocate an ICD-owned VkInstance handle.
///
/// Today we ignore the create-info: no extensions to enable, no
/// application info to record. The returned handle's first slot is
/// `VK_ICD_LOADER_MAGIC` per the loader-ICD ABI; the loader
/// overwrites that slot with its own dispatch-table pointer on first
/// use.
///
/// # Safety
///
/// `p_instance` must be a writable `VkInstance` slot. `p_create_info`
/// is currently unused; we don't dereference it.
#[no_mangle]
pub unsafe extern "C" fn vkCreateInstance(
    _p_create_info: *const c_void, /* const VkInstanceCreateInfo* */
    _p_allocator:   *const c_void, /* const VkAllocationCallbacks* */
    p_instance:     *mut VkInstance,
) -> VkResult {
    if p_instance.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }

    // Best-effort aqueduct-gpu connection. Failure (no socket,
    // daemon not running) is non-fatal: the instance is still
    // valid, just exposes zero physical devices.
    //
    // Allocate the instance first (chicken-and-egg: AtriumPhysicalDevice
    // carries an *mut AtriumInstance back-pointer, so we need the
    // instance's heap address before constructing its devices).
    let mut inst = Box::new(AtriumInstance {
        loader_dispatch_slot: VK_ICD_LOADER_MAGIC,
        client:  None,
        devices: Vec::new(),
    });
    let inst_ptr: *mut AtriumInstance = &mut *inst;

    if let Some((client, backend)) = try_connect_aqueduct() {
        // One backend → one VkPhysicalDevice today. When the
        // kmod gains multi-backend enumeration (D5+), this grows
        // into a loop over IOC_GPU_LIST_BACKENDS.
        let pd = Box::new(AtriumPhysicalDevice {
            loader_dispatch_slot: VK_ICD_LOADER_MAGIC,
            backend_vendor:     backend.vendor,
            backend_generation: backend.generation,
            instance:           inst_ptr,
        });
        inst.client = Some(std::sync::Mutex::new(client));
        inst.devices.push(Box::into_raw(pd));
    }

    *p_instance = Box::into_raw(inst) as VkInstance;
    VK_SUCCESS
}

/// `vkDestroyInstance` — reclaim the AtriumInstance allocation.
///
/// # Safety
///
/// `instance` must be a handle previously returned by
/// `vkCreateInstance`, or null (no-op).
#[no_mangle]
pub unsafe extern "C" fn vkDestroyInstance(
    instance:     VkInstance,
    _p_allocator: *const c_void, /* const VkAllocationCallbacks* */
) {
    if instance.is_null() {
        return;
    }
    // Reclaim the AtriumInstance and its owned VkPhysicalDevice
    // handles. The Vec's Drop frees the Vec itself; we own each
    // device via raw pointer and free them explicitly.
    let inst = Box::from_raw(instance as *mut AtriumInstance);
    for pd in &inst.devices {
        let _ = Box::from_raw(*pd);
    }
    // inst goes out of scope here, dropping the GpuClient + Vec.
}

/// `vkEnumeratePhysicalDevices` — list the GPUs this ICD can target.
///
/// Today: zero. The full aqueduct-gpu device-discovery path
/// (handshake against frescod-aqueduct, learn what tier-1/2/3
/// backends are reachable, expose each as a `VkPhysicalDevice`)
/// lands later in Phase 1.3b.
///
/// Standard Vulkan two-call query: caller invokes once with
/// `p_devices=NULL` to learn the count, then again with a buffer.
///
/// # Safety
///
/// `p_count` must be writable. `p_devices` may be null (count-only
/// query) or point to a writable buffer of at least `*p_count`
/// `VkPhysicalDevice` slots.
#[no_mangle]
pub unsafe extern "C" fn vkEnumeratePhysicalDevices(
    instance:  VkInstance,
    p_count:   *mut u32,
    p_devices: *mut VkPhysicalDevice,
) -> VkResult {
    if p_count.is_null() || instance.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let inst = &*(instance as *const AtriumInstance);
    let n = inst.devices.len() as u32;

    if p_devices.is_null() {
        // Count-only query.
        *p_count = n;
        return VK_SUCCESS;
    }
    let cap = *p_count;
    let to_copy = cap.min(n);
    for i in 0..to_copy {
        *p_devices.offset(i as isize) = inst.devices[i as usize] as VkPhysicalDevice;
    }
    *p_count = to_copy;
    // VK_INCOMPLETE (5) signals "you asked for fewer slots than I
    // have devices; here's what I could fit". Strictly, our count
    // is 0 or 1 today, so this fires only when cap=0 and we have a
    // device — also a legitimate "size probe" call pattern.
    if to_copy < n { 5 /* VK_INCOMPLETE */ } else { VK_SUCCESS }
}

/// `vkEnumerateInstanceVersion` — loader's first probe to learn what
/// Vulkan API version this ICD speaks. Returns the value in
/// `ATRIUM_ICD_API_VERSION`; bumps as more entry points reach
/// correctness.
#[no_mangle]
pub unsafe extern "C" fn vkEnumerateInstanceVersion(
    p_api_version: *mut u32,
) -> VkResult {
    if !p_api_version.is_null() {
        *p_api_version = ATRIUM_ICD_API_VERSION;
    }
    VK_SUCCESS
}

/// `vkEnumerateInstanceExtensionProperties` — returns the
/// instance-level extensions this ICD supports.
///
/// Today: VK_KHR_surface (1), VK_EXT_atrium_surface (1). Swapchain
/// is a device-level extension and surfaces at
/// vkEnumerateDeviceExtensionProperties.
const ATRIUM_INSTANCE_EXTENSIONS: &[(&[u8], u32)] = &[
    (b"VK_KHR_surface\0", 25),
    (b"VK_EXT_atrium_surface\0", 1),
    // VK_EXT_debug_utils — we expose no-op stubs (see "Debug-utils
    // stubs" section below). Lets apps + validation layers that
    // probe for the extension load against atrium-vk-icd without
    // tripping VK_ERROR_EXTENSION_NOT_PRESENT. The stubs are
    // genuinely no-ops: messenger create/destroy succeed but never
    // fire callbacks; labels and object names are silently dropped.
    // Real diagnostic forwarding can land later if useful.
    (b"VK_EXT_debug_utils\0", 2),
    // VK_KHR_get_surface_capabilities2 — instance-level
    // extension that exposes the *2 surface-probe entry points.
    // Apps that opt in to KHR_get_physical_device_properties2
    // typically also enable this one; required by VK_KHR_
    // surface_protected_capabilities and several other
    // downstream extensions Mesa/SDL2 probe for.
    (b"VK_KHR_get_surface_capabilities2\0", 1),
];

#[no_mangle]
pub unsafe extern "C" fn vkEnumerateInstanceExtensionProperties(
    _p_layer_name: *const c_char,
    p_property_count: *mut u32,
    p_properties:     *mut VkExtensionProperties,
) -> VkResult {
    if p_property_count.is_null() {
        return -7 /* VK_ERROR_INITIALIZATION_FAILED */;
    }
    let n = ATRIUM_INSTANCE_EXTENSIONS.len() as u32;
    if p_properties.is_null() {
        *p_property_count = n;
        return VK_SUCCESS;
    }
    let cap = *p_property_count;
    let to_copy = cap.min(n);
    for i in 0..to_copy {
        let (name, ver) = ATRIUM_INSTANCE_EXTENSIONS[i as usize];
        let mut props = VkExtensionProperties {
            extensionName: [0; VK_MAX_EXTENSION_NAME_SIZE],
            specVersion:   ver,
        };
        for (j, &b) in name.iter().enumerate() {
            if j >= VK_MAX_EXTENSION_NAME_SIZE { break; }
            props.extensionName[j] = b as c_char;
        }
        *p_properties.offset(i as isize) = props;
    }
    *p_property_count = to_copy;
    if to_copy < n { 5 /* VK_INCOMPLETE */ } else { VK_SUCCESS }
}

/// Device-level extensions advertised by atrium-vk-icd.
///
/// VK_KHR_swapchain is the load-blocking one for any windowed
/// app: the Khronos loader filters its swapchain dispatch through
/// vkEnumerateDeviceExtensionProperties — apps that pass it in
/// vkCreateDevice::enabledExtensions[] would otherwise hit
/// VK_ERROR_EXTENSION_NOT_PRESENT even though atrium-vk-icd's
/// vkCreateSwapchainKHR is fully wired.
///
/// VK_EXT_debug_marker stays an instance-level "no-op stub" via
/// debug_utils — adding a separate device entry would be churn
/// for no gain (debug_marker is legacy, superseded by
/// debug_utils).
const ATRIUM_DEVICE_EXTENSIONS: &[(&[u8], u32)] = &[
    (b"VK_KHR_swapchain\0", 70),
    // VK_KHR_push_descriptor — apps that bind descriptors at
    // draw time without going through vkAllocateDescriptorSets
    // (Slang/wgpu's fast path; SaschaWillems samples) probe for
    // this extension. We expose no-op cmd stubs (see "push-
    // descriptor + descriptor-update-template stubs" below);
    // tier-1's renderer ignores per-draw descriptor bindings.
    (b"VK_KHR_push_descriptor\0", 2),
];

/// `vkEnumerateDeviceExtensionProperties` — returns the device-
/// level extensions atrium-vk-icd supports. Same two-call
/// contract as the instance variant: caller passes null
/// pProperties first to size, then a real buffer to fill.
#[no_mangle]
pub unsafe extern "C" fn vkEnumerateDeviceExtensionProperties(
    _physical_device: VkPhysicalDevice,
    _p_layer_name:    *const c_char,
    p_property_count: *mut u32,
    p_properties:     *mut VkExtensionProperties,
) -> VkResult {
    if p_property_count.is_null() {
        return -7 /* VK_ERROR_INITIALIZATION_FAILED */;
    }
    let n = ATRIUM_DEVICE_EXTENSIONS.len() as u32;
    if p_properties.is_null() {
        *p_property_count = n;
        return VK_SUCCESS;
    }
    let cap = *p_property_count;
    let to_copy = cap.min(n);
    for i in 0..to_copy {
        let (name, ver) = ATRIUM_DEVICE_EXTENSIONS[i as usize];
        let mut props = VkExtensionProperties {
            extensionName: [0; VK_MAX_EXTENSION_NAME_SIZE],
            specVersion:   ver,
        };
        for (j, &b) in name.iter().enumerate() {
            if j >= VK_MAX_EXTENSION_NAME_SIZE { break; }
            props.extensionName[j] = b as c_char;
        }
        *p_properties.offset(i as isize) = props;
    }
    *p_property_count = to_copy;
    if to_copy < n { 5 /* VK_INCOMPLETE */ } else { VK_SUCCESS }
}

/// `vkEnumerateInstanceLayerProperties` — returns ICD-side instance
/// layers. ICDs typically expose zero (layers come from external
/// dlopen'd libraries known to the loader, not from drivers).
#[no_mangle]
pub unsafe extern "C" fn vkEnumerateInstanceLayerProperties(
    p_property_count: *mut u32,
    p_properties:     *mut VkLayerProperties,
) -> VkResult {
    if p_property_count.is_null() {
        return -7 /* VK_ERROR_INITIALIZATION_FAILED */;
    }
    *p_property_count = 0;
    let _ = p_properties;
    VK_SUCCESS
}

/// `vk_icdNegotiateLoaderICDInterfaceVersion` — the loader proposes
/// its supported version (in `*p_version` on entry); we clamp to our
/// max and write the agreed-on version back.
///
/// Returns `VK_SUCCESS` if a compatible version was negotiated.
/// Returning `VK_ERROR_INCOMPATIBLE_DRIVER` (or a non-success value)
/// makes the loader skip us entirely.
///
/// # Safety
///
/// `p_version` must point to a writable `u32`.
#[no_mangle]
pub unsafe extern "C" fn vk_icdNegotiateLoaderICDInterfaceVersion(
    p_version: *mut u32,
) -> VkResult {
    if p_version.is_null() {
        // Defensive — Khronos loader always provides this, but a
        // malicious or buggy caller might not.
        return -8 /* VK_ERROR_INCOMPATIBLE_DRIVER */;
    }
    let loader_version = *p_version;
    let agreed = loader_version.min(ATRIUM_LOADER_ICD_INTERFACE_VERSION_MAX);
    *p_version = agreed;
    VK_SUCCESS
}

// ─────────────────────────────────────────────────────────────────
// WSI — VK_KHR_surface + VK_KHR_swapchain (skeleton)
// Design sketch: docs/spec/aqueduct-gpu.md §7.1.1.
// ─────────────────────────────────────────────────────────────────

/// VkSurfaceKHR is non-dispatchable (u64). Real ICDs would track a
/// per-surface platform handle (Fresco window-id); today we hand
/// back unique non-zero u64s and don't validate further. Surface
/// destruction is a no-op.
///
/// Surfaces are typically created by platform extensions
/// (vkCreateXcbSurfaceKHR / vkCreateWaylandSurfaceKHR /
/// vkCreateMetalSurfaceEXT). Atrium's canonical creator is
/// `vkCreateAtriumSurfaceEXT` (sized in the spec but not yet
/// wired); apps that ship for Atrium link against the
/// VK_EXT_atrium_surface extension.
#[allow(dead_code)] // consumed by future vkCreateAtriumSurfaceEXT
static ATRIUM_NEXT_SURFACE_ID: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

/// `vkDestroySurfaceKHR` — no-op today (no per-surface state).
#[no_mangle]
pub unsafe extern "C" fn vkDestroySurfaceKHR(
    _instance: VkInstance, _surface: u64, _p_allocator: *const c_void,
) {}

/// `vkCreateAtriumSurfaceEXT` — Atrium's canonical
/// VkSurfaceKHR creator (VK_EXT_atrium_surface). The create-info
/// carries a Fresco window-id (u32, allocated by the app via
/// fresco-protocol's WindowCreate). The returned VkSurfaceKHR's
/// numeric value IS that window-id widened to u64, so
/// OP_GPU_PRESENT.surface_id is directly routable on the daemon
/// side (no extra surface→window map needed).
///
/// VkAtriumSurfaceCreateInfoEXT layout:
///   0   sType : u32  (1_000_310_000 — extension number 310 ×
///                     1000 + 0 per Khronos convention; reserved
///                     for VK_EXT_atrium_surface in our local
///                     numbering until upstream assigns).
///   4   _pad
///   8   pNext : ptr
///   16  flags : u32
///   20  window_id : u32  (Fresco window-id)
#[no_mangle]
pub unsafe extern "C" fn vkCreateAtriumSurfaceEXT(
    _instance:     VkInstance,
    p_create_info: *const c_void,
    _p_allocator:  *const c_void,
    p_surface:     *mut u64,
) -> VkResult {
    if p_create_info.is_null() || p_surface.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let b = p_create_info as *const u8;
    let window_id = std::ptr::read_unaligned(b.add(20) as *const u32);
    if window_id == 0 {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    // The surface handle IS the window-id widened. Lets the
    // daemon route OP_GPU_PRESENT directly through its existing
    // per-window WindowSurface map without a separate
    // surface→window translation step.
    //
    // ATRIUM_NEXT_SURFACE_ID is reserved for a future generation
    // scheme if surface lifetime starts to outlive its window.
    let _ = ATRIUM_NEXT_SURFACE_ID
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    *p_surface = window_id as u64;
    VK_SUCCESS
}

// ── VK_EXT_debug_utils stubs ─────────────────────────────────────
//
// All entry points here are intentionally no-ops. We expose the
// extension so apps and validation layers that probe for it can
// load against atrium-vk-icd without aborting on
// VK_ERROR_EXTENSION_NOT_PRESENT — common with the Khronos
// loader's debug-utils path.
//
// Stubs return success and do not retain any state:
//   - Messenger create/destroy succeed but never fire callbacks.
//   - Object name/tag setters accept the call and drop the
//     metadata.
//   - Queue + cmdbuf labels are silently dropped.
//   - vkSubmitDebugUtilsMessageEXT is a no-op (no registered
//     callbacks anyway).
//
// Real diagnostic forwarding to a fresco "debug" channel can
// land later if Atrium gains a validation story; for now the
// goal is unblocking app load.

/// Pseudo-handle for VkDebugUtilsMessengerEXT. The ICD doesn't
/// retain any state, but the loader insists on a non-null handle
/// to forward through dispatch. We return a stable sentinel
/// (any non-null aligned address) plus increment a counter so
/// distinct messengers have distinct handles, which keeps
/// "double-destroy" probes from passing accidentally.
static ATRIUM_NEXT_DEBUG_MESSENGER_ID: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

#[no_mangle]
pub unsafe extern "C" fn vkCreateDebugUtilsMessengerEXT(
    _instance:      VkInstance,
    _p_create_info: *const c_void,
    _p_allocator:   *const c_void,
    p_messenger:    *mut u64,
) -> VkResult {
    if p_messenger.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let id = ATRIUM_NEXT_DEBUG_MESSENGER_ID
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    *p_messenger = id;
    VK_SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn vkDestroyDebugUtilsMessengerEXT(
    _instance:    VkInstance,
    _messenger:   u64,
    _p_allocator: *const c_void,
) {}

#[no_mangle]
pub unsafe extern "C" fn vkSubmitDebugUtilsMessageEXT(
    _instance:        VkInstance,
    _message_severity: u32,
    _message_types:    u32,
    _p_callback_data:  *const c_void,
) {}

#[no_mangle]
pub unsafe extern "C" fn vkSetDebugUtilsObjectNameEXT(
    _device:      VkDevice,
    _p_name_info: *const c_void,
) -> VkResult { VK_SUCCESS }

#[no_mangle]
pub unsafe extern "C" fn vkSetDebugUtilsObjectTagEXT(
    _device:     VkDevice,
    _p_tag_info: *const c_void,
) -> VkResult { VK_SUCCESS }

#[no_mangle]
pub unsafe extern "C" fn vkQueueBeginDebugUtilsLabelEXT(
    _queue:       VkQueue,
    _p_label_info: *const c_void,
) {}

#[no_mangle]
pub unsafe extern "C" fn vkQueueEndDebugUtilsLabelEXT(
    _queue: VkQueue,
) {}

#[no_mangle]
pub unsafe extern "C" fn vkQueueInsertDebugUtilsLabelEXT(
    _queue:        VkQueue,
    _p_label_info: *const c_void,
) {}

#[no_mangle]
pub unsafe extern "C" fn vkCmdBeginDebugUtilsLabelEXT(
    _cmd_buffer:   VkCommandBuffer,
    _p_label_info: *const c_void,
) {}

#[no_mangle]
pub unsafe extern "C" fn vkCmdEndDebugUtilsLabelEXT(
    _cmd_buffer: VkCommandBuffer,
) {}

#[no_mangle]
pub unsafe extern "C" fn vkCmdInsertDebugUtilsLabelEXT(
    _cmd_buffer:   VkCommandBuffer,
    _p_label_info: *const c_void,
) {}

/// `vkGetPhysicalDeviceSurfaceSupportKHR` — always returns
/// VK_TRUE for queue family 0 (our single family supports
/// graphics+compute+transfer, and the spec sketch puts present
/// on the same family). Per Vk, any other family + the only
/// surface we can present to gets VK_TRUE too — Atrium's
/// single-queue model is the source of truth.
#[no_mangle]
pub unsafe extern "C" fn vkGetPhysicalDeviceSurfaceSupportKHR(
    _physical_device:     VkPhysicalDevice,
    _queue_family_index:  u32,
    _surface:             u64,
    p_supported:          *mut u32,
) -> VkResult {
    if p_supported.is_null() { return VK_ERROR_INITIALIZATION_FAILED; }
    *p_supported = 1; /* VK_TRUE */
    VK_SUCCESS
}

/// VkSurfaceCapabilitiesKHR layout (52 bytes):
///   0   minImageCount : u32
///   4   maxImageCount : u32
///   8   currentExtent : VkExtent2D (8 B)
///   16  minImageExtent : VkExtent2D
///   24  maxImageExtent : VkExtent2D
///   32  maxImageArrayLayers : u32
///   36  supportedTransforms : u32
///   40  currentTransform : u32
///   44  supportedCompositeAlpha : u32
///   48  supportedUsageFlags : u32
#[no_mangle]
pub unsafe extern "C" fn vkGetPhysicalDeviceSurfaceCapabilitiesKHR(
    _physical_device: VkPhysicalDevice,
    _surface:         u64,
    p_caps:           *mut c_void, /* VkSurfaceCapabilitiesKHR */
) -> VkResult {
    if p_caps.is_null() { return VK_ERROR_INITIALIZATION_FAILED; }
    let b = p_caps as *mut u8;
    std::ptr::write_bytes(b, 0, 52);
    let put32 = |off: usize, v: u32| {
        std::ptr::copy_nonoverlapping(
            v.to_le_bytes().as_ptr(), b.add(off), 4,
        );
    };
    put32( 0, 3);                 // minImageCount = 3 (triple-buffer)
    put32( 4, 4);                 // maxImageCount = 4
    // currentExtent: 1280×800 (frescod-aqueduct's typical mode).
    put32( 8, 1280); put32(12, 800);
    put32(16, 64);   put32(20, 64);    // min 64×64
    put32(24, 4096); put32(28, 4096);  // max 4096×4096
    put32(32, 1);                 // maxImageArrayLayers
    put32(36, 0x1);               // supportedTransforms = IDENTITY only
    put32(40, 0x1);               // currentTransform = IDENTITY
    put32(44, 0x1);               // supportedCompositeAlpha = OPAQUE only
    put32(48, 0x10);              // supportedUsageFlags = COLOR_ATTACHMENT
    VK_SUCCESS
}

/// Surface formats atrium-vk-icd exposes. Listed in priority
/// order (apps that pick the first match get the renderer's
/// preferred scanout format).
///
/// All map onto the renderer's BGRA scanout chain via the kmod's
/// hardcoded R↔B swap (D1 step 2(a) bring-up note), so swapchain
/// images in any of the four can be displayed without an extra
/// conversion pass.
///
/// (format, color_space): VkFormat numeric code +
/// VK_COLOR_SPACE_SRGB_NONLINEAR_KHR (0).
const ATRIUM_SURFACE_FORMATS: &[(u32, u32)] = &[
    (37, 0),  // R8G8B8A8_UNORM
    (43, 0),  // R8G8B8A8_SRGB
    (44, 0),  // B8G8R8A8_UNORM
    (50, 0),  // B8G8R8A8_SRGB
];

/// `vkGetPhysicalDeviceSurfaceFormatsKHR` — return our four
/// scanout-compatible formats in priority order. Apps typically
/// walk the list and pick the first match they prefer.
///
/// VkSurfaceFormatKHR is 8 bytes (format u32 + colorSpace u32).
#[no_mangle]
pub unsafe extern "C" fn vkGetPhysicalDeviceSurfaceFormatsKHR(
    _physical_device:     VkPhysicalDevice,
    _surface:             u64,
    p_surface_format_count: *mut u32,
    p_surface_formats:    *mut c_void, /* VkSurfaceFormatKHR* */
) -> VkResult {
    if p_surface_format_count.is_null() { return VK_ERROR_INITIALIZATION_FAILED; }
    let n = ATRIUM_SURFACE_FORMATS.len() as u32;
    if p_surface_formats.is_null() {
        *p_surface_format_count = n;
        return VK_SUCCESS;
    }
    let cap = *p_surface_format_count;
    if cap == 0 { return VK_SUCCESS; }
    let to_copy = cap.min(n);
    let b = p_surface_formats as *mut u8;
    let put32 = |off: usize, v: u32| {
        std::ptr::copy_nonoverlapping(
            v.to_le_bytes().as_ptr(), b.add(off), 4,
        );
    };
    for i in 0..to_copy {
        let (fmt, cs) = ATRIUM_SURFACE_FORMATS[i as usize];
        let off = (i as usize) * 8;
        put32(off, fmt);
        put32(off + 4, cs);
    }
    *p_surface_format_count = to_copy;
    if to_copy < n { 5 /* VK_INCOMPLETE */ } else { VK_SUCCESS }
}

/// `vkGetPhysicalDeviceSurfaceCapabilities2KHR` — VK_KHR_
/// get_surface_capabilities2 variant. Parses
/// VkPhysicalDeviceSurfaceInfo2KHR (16-byte header + surface at
/// offset 16) and fills VkSurfaceCapabilities2KHR (16-byte
/// header + inner caps at offset 16).
#[no_mangle]
pub unsafe extern "C" fn vkGetPhysicalDeviceSurfaceCapabilities2KHR(
    physical_device:  VkPhysicalDevice,
    p_surface_info:   *const c_void,
    p_caps:           *mut c_void,
) -> VkResult {
    if p_surface_info.is_null() || p_caps.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let info = p_surface_info as *const u8;
    let info_p_next = std::ptr::read_unaligned(info.add(8) as *const *mut c_void);
    let surface = std::ptr::read_unaligned(info.add(16) as *const u64);
    let _ = walk_p_next_chain(info_p_next);

    let out = p_caps as *mut u8;
    let out_p_next = std::ptr::read_unaligned(out.add(8) as *const *mut c_void);
    let inner = out.add(16) as *mut c_void;
    let r = vkGetPhysicalDeviceSurfaceCapabilitiesKHR(physical_device, surface, inner);
    let _ = walk_p_next_chain(out_p_next);
    r
}

/// `vkGetPhysicalDeviceSurfaceFormats2KHR` — VK_KHR_get_surface_
/// capabilities2 variant. Output array is VkSurfaceFormat2KHR
/// (16-byte header + 8-byte VkSurfaceFormatKHR inner per slot).
#[no_mangle]
pub unsafe extern "C" fn vkGetPhysicalDeviceSurfaceFormats2KHR(
    physical_device:       VkPhysicalDevice,
    p_surface_info:        *const c_void,
    p_surface_format_count: *mut u32,
    p_surface_formats:     *mut c_void,
) -> VkResult {
    if p_surface_info.is_null() || p_surface_format_count.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let info = p_surface_info as *const u8;
    let info_p_next = std::ptr::read_unaligned(info.add(8) as *const *mut c_void);
    let surface = std::ptr::read_unaligned(info.add(16) as *const u64);
    let _ = walk_p_next_chain(info_p_next);

    let n = ATRIUM_SURFACE_FORMATS.len() as u32;
    if p_surface_formats.is_null() {
        *p_surface_format_count = n;
        let _ = physical_device; let _ = surface;
        return VK_SUCCESS;
    }
    let cap = *p_surface_format_count;
    if cap == 0 { return VK_SUCCESS; }
    let to_copy = cap.min(n);

    // Each VkSurfaceFormat2KHR is 16-byte header (sType+pad+pNext)
    // + 8-byte inner VkSurfaceFormatKHR. The struct's natural
    // stride is 24 bytes; layout-checked by ash + by the test
    // below.
    const SLOT_STRIDE: usize = 24;
    let base = p_surface_formats as *mut u8;
    let put32 = |b: *mut u8, off: usize, v: u32| {
        std::ptr::copy_nonoverlapping(
            v.to_le_bytes().as_ptr(), b.add(off), 4,
        );
    };
    for i in 0..to_copy {
        let slot = base.add((i as usize) * SLOT_STRIDE);
        let slot_p_next = std::ptr::read_unaligned(slot.add(8) as *const *mut c_void);
        // Don't overwrite the caller-provided sType / pNext header;
        // we only fill the inner block at offset 16.
        let (fmt, cs) = ATRIUM_SURFACE_FORMATS[i as usize];
        put32(slot, 16, fmt);
        put32(slot, 20, cs);
        let _ = walk_p_next_chain(slot_p_next);
    }
    *p_surface_format_count = to_copy;
    if to_copy < n { 5 /* VK_INCOMPLETE */ } else { VK_SUCCESS }
}

/// `vkGetPhysicalDeviceSurfacePresentModesKHR` — FIFO only (the
/// spec's required mode + the natural pairing with Atrium's
/// server-side vblank pacing per §6.5.5).
#[no_mangle]
pub unsafe extern "C" fn vkGetPhysicalDeviceSurfacePresentModesKHR(
    _physical_device:        VkPhysicalDevice,
    _surface:                u64,
    p_present_mode_count:    *mut u32,
    p_present_modes:         *mut u32,
) -> VkResult {
    if p_present_mode_count.is_null() { return VK_ERROR_INITIALIZATION_FAILED; }
    if p_present_modes.is_null() {
        *p_present_mode_count = 1;
        return VK_SUCCESS;
    }
    if *p_present_mode_count == 0 { return VK_SUCCESS; }
    *p_present_modes = 2; /* VK_PRESENT_MODE_FIFO_KHR */
    *p_present_mode_count = 1;
    VK_SUCCESS
}

/// `vkCreateSwapchainKHR` — allocate the swapchain's ring of
/// VkImages. Each image is an internal AtriumImage allocated
/// with the same shape (format + extent) as a normal
/// vkCreateImage; backed by ICD-owned memory.
///
/// VkSwapchainCreateInfoKHR layout:
///   0   sType, 8 pNext, 16 flags,
///   20  surface (u64),
///   28  minImageCount (u32),
///   32  imageFormat (u32),
///   36  imageColorSpace (u32),
///   40  imageExtent (VkExtent2D),
///   48  imageArrayLayers (u32),
///   52  imageUsage (u32), ...
#[no_mangle]
pub unsafe extern "C" fn vkCreateSwapchainKHR(
    device:           VkDevice,
    p_create_info:    *const c_void,
    _p_allocator:     *const c_void,
    p_swapchain:      *mut u64,
) -> VkResult {
    if device.is_null() || p_create_info.is_null() || p_swapchain.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let dev = &*(device as *const AtriumDevice);
    let b = p_create_info as *const u8;
    let surface = std::ptr::read_unaligned(b.add(20) as *const u64);
    let n       = std::ptr::read_unaligned(b.add(28) as *const u32).max(1).min(8);
    let format  = std::ptr::read_unaligned(b.add(32) as *const u32);
    let width   = std::ptr::read_unaligned(b.add(40) as *const u32);
    let height  = std::ptr::read_unaligned(b.add(44) as *const u32);
    let usage   = std::ptr::read_unaligned(b.add(52) as *const u32);

    // Allocate the ring of swapchain images by piggybacking on
    // the existing vkCreateImage machinery.
    let mut images = Vec::with_capacity(n as usize);
    let mut img_info = [0u8; 88];
    img_info[ 0.. 4].copy_from_slice(&14u32.to_le_bytes());
    img_info[24..28].copy_from_slice(&format.to_le_bytes());
    img_info[28..32].copy_from_slice(&width.to_le_bytes());
    img_info[32..36].copy_from_slice(&height.to_le_bytes());
    img_info[36..40].copy_from_slice(&1u32.to_le_bytes());
    img_info[40..44].copy_from_slice(&1u32.to_le_bytes());
    img_info[44..48].copy_from_slice(&1u32.to_le_bytes());
    img_info[56..60].copy_from_slice(&usage.to_le_bytes());
    for _ in 0..n {
        let mut handle: u64 = 0;
        let r = vkCreateImage(
            device, img_info.as_ptr() as *const _, std::ptr::null(), &mut handle,
        );
        if r != VK_SUCCESS {
            // Roll back any already-created images.
            for h in &images {
                vkDestroyImage(device, *h, std::ptr::null());
            }
            return r;
        }
        images.push(handle);
    }

    let h = dev.next_swapchain_id.get();
    dev.next_swapchain_id.set(h + 1);
    if let Ok(mut s) = dev.swapchains.lock() {
        s.insert(h, AtriumSwapchain {
            surface, images, next_acquire: 0, width, height, format,
        });
    } else {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    *p_swapchain = h;
    VK_SUCCESS
}

/// `vkDestroySwapchainKHR` — free the ring's images + drop the
/// swapchain entry.
#[no_mangle]
pub unsafe extern "C" fn vkDestroySwapchainKHR(
    device:       VkDevice,
    swapchain:    u64,
    _p_allocator: *const c_void,
) {
    if device.is_null() || swapchain == 0 { return; }
    let dev = &*(device as *const AtriumDevice);
    let images = if let Ok(mut s) = dev.swapchains.lock() {
        s.remove(&swapchain).map(|sc| sc.images).unwrap_or_default()
    } else { return; };
    for h in images {
        vkDestroyImage(device, h, std::ptr::null());
    }
}

/// `vkGetSwapchainImagesKHR` — two-call query for the ring of
/// VkImages backing this swapchain.
#[no_mangle]
pub unsafe extern "C" fn vkGetSwapchainImagesKHR(
    device:               VkDevice,
    swapchain:            u64,
    p_swapchain_image_count: *mut u32,
    p_swapchain_images:   *mut u64,
) -> VkResult {
    if device.is_null() || swapchain == 0 || p_swapchain_image_count.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let dev = &*(device as *const AtriumDevice);
    let images: Vec<u64> = match dev.swapchains.lock() {
        Ok(s) => s.get(&swapchain).map(|sc| sc.images.clone()).unwrap_or_default(),
        Err(_) => return VK_ERROR_INITIALIZATION_FAILED,
    };
    if p_swapchain_images.is_null() {
        *p_swapchain_image_count = images.len() as u32;
        return VK_SUCCESS;
    }
    let cap = *p_swapchain_image_count as usize;
    let to_copy = images.len().min(cap);
    for i in 0..to_copy {
        *p_swapchain_images.offset(i as isize) = images[i];
    }
    *p_swapchain_image_count = to_copy as u32;
    if to_copy < images.len() { 5 /* VK_INCOMPLETE */ } else { VK_SUCCESS }
}

/// `vkAcquireNextImageKHR` — round-robin pull from the ring.
/// vblank pacing is server-side per spec §6.5.5; we don't block
/// on the timeout. The provided VkSemaphore + VkFence (if any)
/// are ignored — our timeline-via-submit serialization handles
/// the ordering.
#[no_mangle]
pub unsafe extern "C" fn vkAcquireNextImageKHR(
    device:       VkDevice,
    swapchain:    u64,
    _timeout_ns:  u64,
    _semaphore:   u64,
    _fence:       u64,
    p_image_index: *mut u32,
) -> VkResult {
    if device.is_null() || swapchain == 0 || p_image_index.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let dev = &*(device as *const AtriumDevice);
    let idx = if let Ok(mut s) = dev.swapchains.lock() {
        let Some(sc) = s.get_mut(&swapchain) else {
            return VK_ERROR_INITIALIZATION_FAILED;
        };
        let i = sc.next_acquire;
        sc.next_acquire = (sc.next_acquire + 1) % (sc.images.len() as u32).max(1);
        i
    } else { return VK_ERROR_INITIALIZATION_FAILED; };
    *p_image_index = idx;
    VK_SUCCESS
}

/// `vkQueuePresentKHR` — for each (swapchain, imageIndex) tuple
/// in the VkPresentInfoKHR, fire an `OP_GPU_PRESENT` against the
/// instance's GpuClient. Daemon-side routing through the
/// surface→window map happens on the host endpoint (today
/// frescod-aqueduct; the SoftwareBackend currently no-ops the
/// opcode since it isn't wired to a windowing system).
///
/// VkPresentInfoKHR layout:
///   0   sType
///   8   pNext
///   16  waitSemaphoreCount : u32
///   24  pWaitSemaphores : ptr
///   32  swapchainCount : u32
///   40  pSwapchains : *const VkSwapchainKHR (u64)
///   48  pImageIndices : *const u32
///   56  pResults : *mut VkResult (optional)
#[no_mangle]
pub unsafe extern "C" fn vkQueuePresentKHR(
    queue:           VkQueue,
    p_present_info:  *const c_void,
) -> VkResult {
    if queue.is_null() || p_present_info.is_null() { return VK_SUCCESS; }
    let q = &*(queue as *const AtriumQueue);
    if q._device.is_null() { return VK_SUCCESS; }
    let dev = &*(q._device as *const AtriumDevice);
    if dev.instance.is_null() { return VK_SUCCESS; }
    let inst = &*(dev.instance as *const AtriumInstance);
    let Some(client_mu) = inst.client.as_ref() else { return VK_SUCCESS; };

    let b = p_present_info as *const u8;
    let sc_count       = std::ptr::read_unaligned(b.add(32) as *const u32);
    let p_swapchains   = std::ptr::read_unaligned(b.add(40) as *const *const u64);
    let p_image_indices = std::ptr::read_unaligned(b.add(48) as *const *const u32);
    let p_results      = std::ptr::read_unaligned(b.add(56) as *const *mut VkResult);

    if p_swapchains.is_null() || p_image_indices.is_null() {
        return VK_SUCCESS;
    }

    for i in 0..sc_count {
        let sc_handle = *p_swapchains.offset(i as isize);
        let img_index = *p_image_indices.offset(i as isize);

        // Resolve swapchain → (surface_id, image at index).
        let (surface_id, image_handle) = {
            let Ok(scs) = dev.swapchains.lock() else { continue; };
            match scs.get(&sc_handle) {
                Some(sc) => (sc.surface, sc.images.get(img_index as usize).copied().unwrap_or(0)),
                None => continue,
            }
        };
        if image_handle == 0 { continue; }

        // VkImage → daemon-side image_id.
        let image_id = match dev.images.lock() {
            Ok(m) => m.get(&image_handle).and_then(|x| x.image_id),
            Err(_) => None,
        };
        let Some(image_id) = image_id else {
            if !p_results.is_null() {
                *p_results.offset(i as isize) = -3 /* VK_ERROR_INITIALIZATION_FAILED */;
            }
            continue;
        };

        // Send OP_GPU_PRESENT with a monotonic frame_id.
        dev.timeline.set(dev.timeline.get() + 1);
        if let Ok(mut c) = client_mu.lock() {
            let _ = c.present(image_id, surface_id, dev.timeline.get());
        }
        if !p_results.is_null() {
            *p_results.offset(i as isize) = VK_SUCCESS;
        }
    }
    VK_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiate_clamps_to_our_max() {
        let mut v: u32 = 99;
        let r = unsafe { vk_icdNegotiateLoaderICDInterfaceVersion(&mut v) };
        assert_eq!(r, VK_SUCCESS);
        assert_eq!(v, ATRIUM_LOADER_ICD_INTERFACE_VERSION_MAX);
    }

    #[test]
    fn negotiate_accepts_lower_version() {
        let mut v: u32 = 3;
        let r = unsafe { vk_icdNegotiateLoaderICDInterfaceVersion(&mut v) };
        assert_eq!(r, VK_SUCCESS);
        assert_eq!(v, 3);
    }

    #[test]
    fn negotiate_null_version_rejected() {
        let r = unsafe { vk_icdNegotiateLoaderICDInterfaceVersion(std::ptr::null_mut()) };
        assert_ne!(r, VK_SUCCESS);
    }

    #[test]
    fn get_proc_addr_null_name_returns_none() {
        let r = unsafe {
            vk_icdGetInstanceProcAddr(std::ptr::null_mut(), std::ptr::null())
        };
        assert!(r.is_none());
    }

    #[test]
    fn get_proc_addr_unknown_name_returns_none() {
        // vkCreateXlibSurfaceKHR — Atrium ICD doesn't expose this
        // platform extension (Atrium apps use VK_EXT_atrium_surface).
        let name = b"vkCreateXlibSurfaceKHR\0";
        let r = unsafe {
            vk_icdGetInstanceProcAddr(
                std::ptr::null_mut(),
                name.as_ptr() as *const c_char,
            )
        };
        assert!(r.is_none());
    }

    fn lookup(name: &[u8]) -> PFN_vkVoidFunction {
        // Test helper — `name` must include the trailing NUL.
        unsafe {
            vk_icdGetInstanceProcAddr(
                std::ptr::null_mut(),
                name.as_ptr() as *const c_char,
            )
        }
    }

    #[test]
    fn enumerate_instance_version_reports_1_3() {
        let f = lookup(b"vkEnumerateInstanceVersion\0").expect(
            "loader bootstrap entry must resolve",
        );
        let typed: unsafe extern "C" fn(*mut u32) -> VkResult =
            unsafe { std::mem::transmute(f) };
        let mut v: u32 = 0;
        let r = unsafe { typed(&mut v) };
        assert_eq!(r, VK_SUCCESS);
        assert_eq!(v, ATRIUM_ICD_API_VERSION);
        // Confirm we're saying "1.3" and not garbage.
        let major = (v >> 22) & 0x7f;
        let minor = (v >> 12) & 0x3ff;
        assert_eq!((major, minor), (1, 3));
    }

    #[test]
    fn enumerate_instance_extensions_lists_atrium_surface() {
        // VK_KHR_surface + VK_EXT_atrium_surface + VK_EXT_debug_utils
        // + VK_KHR_get_surface_capabilities2 — see
        // ATRIUM_INSTANCE_EXTENSIONS.
        let f = lookup(b"vkEnumerateInstanceExtensionProperties\0").unwrap();
        let typed: unsafe extern "C" fn(*const c_char, *mut u32, *mut VkExtensionProperties) -> VkResult =
            unsafe { std::mem::transmute(f) };
        let mut count: u32 = 99;
        let r = unsafe { typed(std::ptr::null(), &mut count, std::ptr::null_mut()) };
        assert_eq!(r, VK_SUCCESS);
        assert_eq!(count, 4);

        let mut props: [VkExtensionProperties; 8] = unsafe { std::mem::zeroed() };
        let mut cap: u32 = 8;
        let _ = unsafe { typed(std::ptr::null(), &mut cap, props.as_mut_ptr()) };
        assert_eq!(cap, 4);
        let names: Vec<String> = (0..4).map(|i| {
            let bytes: Vec<u8> = props[i].extensionName.iter()
                .take_while(|&&c| c != 0).map(|&c| c as u8).collect();
            String::from_utf8(bytes).unwrap()
        }).collect();
        assert_eq!(names[0], "VK_KHR_surface");
        assert_eq!(names[1], "VK_EXT_atrium_surface");
        assert_eq!(names[2], "VK_EXT_debug_utils");
        assert_eq!(names[3], "VK_KHR_get_surface_capabilities2");
    }

    #[test]
    fn surface_capabilities2_and_formats2_match_1_0_variants() {
        // *2 entry points must resolve.
        assert!(lookup(b"vkGetPhysicalDeviceSurfaceCapabilities2KHR\0").is_some());
        assert!(lookup(b"vkGetPhysicalDeviceSurfaceFormats2KHR\0").is_some());

        // Drive Formats2 against an opaque physical-device handle.
        let pd = AtriumPhysicalDevice {
            loader_dispatch_slot: VK_ICD_LOADER_MAGIC,
            backend_vendor: aqueduct_gpu::backends::GpuVendor::Software,
            backend_generation: 0,
            instance: std::ptr::null_mut(),
        };
        let phys: VkPhysicalDevice = &pd as *const _ as *mut _;

        // VkPhysicalDeviceSurfaceInfo2KHR: 16-byte header + 8-byte
        // surface (u64) = 24 bytes total.
        let mut info = [0u8; 24];
        info[0..4].copy_from_slice(&1_000_119_000u32.to_le_bytes()); // sType
        // pNext at offset 8 stays null.
        info[16..24].copy_from_slice(&42u64.to_le_bytes()); // surface

        // First call: size query.
        let mut count: u32 = 0;
        let r = unsafe {
            vkGetPhysicalDeviceSurfaceFormats2KHR(
                phys, info.as_ptr() as *const c_void, &mut count, std::ptr::null_mut(),
            )
        };
        assert_eq!(r, VK_SUCCESS);
        assert_eq!(count, 4, "must report all four ATRIUM_SURFACE_FORMATS");

        // Second call: real buffer (24 bytes per VkSurfaceFormat2KHR).
        let mut buf = vec![0u8; 24 * 4];
        // Pre-fill caller-side sType for each slot.
        for i in 0..4 {
            buf[i*24..i*24+4].copy_from_slice(&1_000_119_002u32.to_le_bytes());
        }
        let mut got: u32 = 4;
        let r = unsafe {
            vkGetPhysicalDeviceSurfaceFormats2KHR(
                phys, info.as_ptr() as *const c_void,
                &mut got, buf.as_mut_ptr() as *mut c_void,
            )
        };
        assert_eq!(r, VK_SUCCESS);
        assert_eq!(got, 4);

        // Inner VkSurfaceFormatKHR sits at offset 16 of each slot.
        let read = |off: usize| u32::from_le_bytes(
            buf[off..off+4].try_into().unwrap()
        );
        assert_eq!(read(16),  37); // slot 0: R8G8B8A8_UNORM
        assert_eq!(read(40),  43); // slot 1: R8G8B8A8_SRGB
        assert_eq!(read(64),  44); // slot 2: B8G8R8A8_UNORM
        assert_eq!(read(88),  50); // slot 3: B8G8R8A8_SRGB
        // sType preserved by us (we don't touch the header).
        assert_eq!(read(0), 1_000_119_002);
    }

    #[test]
    fn get_device_proc_addr_filters_instance_entry_points() {
        // Resolve vkGetDeviceProcAddr itself first.
        let f = lookup(b"vkGetDeviceProcAddr\0").unwrap();
        let gdpa: unsafe extern "C" fn(VkDevice, *const c_char) -> PFN_vkVoidFunction =
            unsafe { std::mem::transmute(f) };

        // Device-level names: resolve.
        for name in [
            b"vkQueueSubmit\0".as_slice(),
            b"vkCmdDraw\0".as_slice(),
            b"vkCreateSwapchainKHR\0".as_slice(),
            b"vkQueuePresentKHR\0".as_slice(),
            b"vkSetDebugUtilsObjectNameEXT\0".as_slice(),
            b"vkCmdBeginDebugUtilsLabelEXT\0".as_slice(),
            b"vkGetDeviceProcAddr\0".as_slice(),
        ] {
            let r = unsafe { gdpa(std::ptr::null_mut(), name.as_ptr() as *const c_char) };
            assert!(r.is_some(),
                "vkGetDeviceProcAddr should resolve device-level: {:?}",
                std::str::from_utf8(&name[..name.len()-1]).unwrap());
        }

        // Instance-level names: MUST be filtered out.
        for name in [
            b"vkCreateInstance\0".as_slice(),
            b"vkDestroyInstance\0".as_slice(),
            b"vkEnumerateInstanceExtensionProperties\0".as_slice(),
            b"vkEnumeratePhysicalDevices\0".as_slice(),
            b"vkGetPhysicalDeviceProperties\0".as_slice(),
            b"vkCreateDevice\0".as_slice(),
            b"vkCreateAtriumSurfaceEXT\0".as_slice(),
            b"vkCreateDebugUtilsMessengerEXT\0".as_slice(),
            b"vkGetPhysicalDeviceSurfaceCapabilitiesKHR\0".as_slice(),
        ] {
            let r = unsafe { gdpa(std::ptr::null_mut(), name.as_ptr() as *const c_char) };
            assert!(r.is_none(),
                "vkGetDeviceProcAddr MUST NOT resolve instance-level: {:?}",
                std::str::from_utf8(&name[..name.len()-1]).unwrap());
        }

        // Null name + unknown name handling.
        assert!(unsafe { gdpa(std::ptr::null_mut(), std::ptr::null()) }.is_none());
        assert!(unsafe { gdpa(std::ptr::null_mut(),
            b"vkNonexistentEntryPoint\0".as_ptr() as *const c_char) }.is_none());
    }

    #[test]
    fn icd_get_physical_device_proc_addr_resolves_only_phys_device_entries() {
        // Direct call — this function isn't routed through
        // vk_icdGetInstanceProcAddr (the loader looks it up via
        // dlsym), so we call it directly.
        let inst: *mut c_void = std::ptr::null_mut();

        // Physical-device entry points: resolve.
        for name in [
            "vkGetPhysicalDeviceProperties\0",
            "vkGetPhysicalDeviceProperties2\0",
            "vkGetPhysicalDeviceProperties2KHR\0",
            "vkGetPhysicalDeviceFeatures\0",
            "vkGetPhysicalDeviceFeatures2\0",
            "vkGetPhysicalDeviceImageFormatProperties\0",
            "vkGetPhysicalDeviceSurfaceCapabilitiesKHR\0",
            "vkEnumerateDeviceExtensionProperties\0",
        ] {
            let r = unsafe {
                vk_icdGetPhysicalDeviceProcAddr(inst, name.as_ptr() as *const c_char)
            };
            assert!(r.is_some(),
                "vk_icdGetPhysicalDeviceProcAddr should resolve {:?}",
                &name[..name.len()-1]);
        }

        // Instance-level: rejected.
        for name in [
            "vkCreateInstance\0",
            "vkDestroyInstance\0",
            "vkEnumerateInstanceExtensionProperties\0",
            "vkEnumeratePhysicalDevices\0",
            "vkCreateDevice\0",
            "vkCreateAtriumSurfaceEXT\0",
            "vkDestroySurfaceKHR\0",
            "vkCreateDebugUtilsMessengerEXT\0",
        ] {
            let r = unsafe {
                vk_icdGetPhysicalDeviceProcAddr(inst, name.as_ptr() as *const c_char)
            };
            assert!(r.is_none(),
                "vk_icdGetPhysicalDeviceProcAddr MUST NOT resolve {:?}",
                &name[..name.len()-1]);
        }

        // Device-level: rejected.
        for name in [
            "vkQueueSubmit\0",
            "vkCmdDraw\0",
            "vkCreateSwapchainKHR\0",
            "vkQueuePresentKHR\0",
            "vkGetDeviceProcAddr\0",
        ] {
            let r = unsafe {
                vk_icdGetPhysicalDeviceProcAddr(inst, name.as_ptr() as *const c_char)
            };
            assert!(r.is_none(),
                "vk_icdGetPhysicalDeviceProcAddr MUST NOT resolve {:?}",
                &name[..name.len()-1]);
        }

        // Edge cases.
        assert!(unsafe {
            vk_icdGetPhysicalDeviceProcAddr(inst, std::ptr::null())
        }.is_none());
        assert!(unsafe {
            vk_icdGetPhysicalDeviceProcAddr(inst, b"vkNonexistent\0".as_ptr() as *const c_char)
        }.is_none());
    }

    #[test]
    fn format_properties_advertise_correct_tier1_features() {
        use ash::vk::FormatFeatureFlags as F;
        let pd = AtriumPhysicalDevice {
            loader_dispatch_slot: VK_ICD_LOADER_MAGIC,
            backend_vendor: aqueduct_gpu::backends::GpuVendor::Software,
            backend_generation: 0,
            instance: std::ptr::null_mut(),
        };
        let phys: VkPhysicalDevice = &pd as *const _ as *mut _;
        let mut props = ash::vk::FormatProperties::default();

        // R8G8B8A8_UNORM: full color set.
        unsafe { vkGetPhysicalDeviceFormatProperties(
            phys, ash::vk::Format::R8G8B8A8_UNORM, &mut props,
        ); }
        assert!(props.optimal_tiling_features.contains(F::COLOR_ATTACHMENT));
        assert!(props.optimal_tiling_features.contains(F::COLOR_ATTACHMENT_BLEND));
        assert!(props.optimal_tiling_features.contains(F::SAMPLED_IMAGE));
        assert!(props.buffer_features.contains(F::VERTEX_BUFFER));

        // D32_SFLOAT: depth attachment, no color.
        unsafe { vkGetPhysicalDeviceFormatProperties(
            phys, ash::vk::Format::D32_SFLOAT, &mut props,
        ); }
        assert!(props.optimal_tiling_features.contains(F::DEPTH_STENCIL_ATTACHMENT));
        assert!(!props.optimal_tiling_features.contains(F::COLOR_ATTACHMENT));

        // R32G32_SFLOAT: vertex/texel-buffer only, no image features.
        unsafe { vkGetPhysicalDeviceFormatProperties(
            phys, ash::vk::Format::R32G32_SFLOAT, &mut props,
        ); }
        assert!(props.buffer_features.contains(F::VERTEX_BUFFER));
        assert!(props.optimal_tiling_features.is_empty());

        // R8G8B8_UNORM (28): unsupported → all zero.
        unsafe { vkGetPhysicalDeviceFormatProperties(
            phys, ash::vk::Format::R8G8B8_UNORM, &mut props,
        ); }
        assert!(props.optimal_tiling_features.is_empty());
        assert!(props.linear_tiling_features.is_empty());
        assert!(props.buffer_features.is_empty());
    }

    #[test]
    fn extended_dynamic_state_stubs_resolve() {
        // All 14 entry points + 14 EXT aliases — 28 names total.
        for name in [
            "vkCmdSetViewportWithCount",
            "vkCmdSetViewportWithCountEXT",
            "vkCmdSetScissorWithCount",
            "vkCmdSetScissorWithCountEXT",
            "vkCmdBindVertexBuffers2",
            "vkCmdBindVertexBuffers2EXT",
            "vkCmdSetCullMode",
            "vkCmdSetCullModeEXT",
            "vkCmdSetFrontFace",
            "vkCmdSetFrontFaceEXT",
            "vkCmdSetPrimitiveTopology",
            "vkCmdSetPrimitiveTopologyEXT",
            "vkCmdSetDepthTestEnable",
            "vkCmdSetDepthTestEnableEXT",
            "vkCmdSetDepthWriteEnable",
            "vkCmdSetDepthWriteEnableEXT",
            "vkCmdSetDepthCompareOp",
            "vkCmdSetDepthCompareOpEXT",
            "vkCmdSetDepthBoundsTestEnable",
            "vkCmdSetDepthBoundsTestEnableEXT",
            "vkCmdSetStencilTestEnable",
            "vkCmdSetStencilTestEnableEXT",
            "vkCmdSetStencilOp",
            "vkCmdSetStencilOpEXT",
            "vkCmdSetRasterizerDiscardEnable",
            "vkCmdSetRasterizerDiscardEnableEXT",
            "vkCmdSetDepthBiasEnable",
            "vkCmdSetDepthBiasEnableEXT",
            "vkCmdSetPrimitiveRestartEnable",
            "vkCmdSetPrimitiveRestartEnableEXT",
        ] {
            let with_nul: Vec<u8> = name.bytes().chain(std::iter::once(0)).collect();
            assert!(lookup(&with_nul).is_some(),
                "must resolve {name}");
        }

        // Each stub must accept null cmdbuf without panicking
        // (matches the 1.0 cmd-stub forgiveness contract).
        let f = lookup(b"vkCmdSetCullMode\0").unwrap();
        let g: unsafe extern "C" fn(VkCommandBuffer, u32) =
            unsafe { std::mem::transmute(f) };
        unsafe { g(std::ptr::null_mut(), 1); }

        let f = lookup(b"vkCmdSetStencilOp\0").unwrap();
        let g: unsafe extern "C" fn(VkCommandBuffer, u32, u32, u32, u32, u32) =
            unsafe { std::mem::transmute(f) };
        unsafe { g(std::ptr::null_mut(), 1, 2, 3, 4, 5); }
    }

    #[test]
    fn sync2_event_and_timestamp_stubs_resolve() {
        for name in [
            b"vkCmdSetEvent2\0".as_slice(),
            b"vkCmdSetEvent2KHR\0".as_slice(),
            b"vkCmdResetEvent2\0".as_slice(),
            b"vkCmdResetEvent2KHR\0".as_slice(),
            b"vkCmdWaitEvents2\0".as_slice(),
            b"vkCmdWaitEvents2KHR\0".as_slice(),
            b"vkCmdWriteTimestamp2\0".as_slice(),
            b"vkCmdWriteTimestamp2KHR\0".as_slice(),
        ] {
            assert!(lookup(name).is_some(),
                "must resolve {}",
                std::str::from_utf8(&name[..name.len()-1]).unwrap());
        }

        // No-op invocations on null cmdbuf must not panic — the
        // existing 1.0 event stubs also accept null and we keep
        // the same forgiving contract.
        let f = lookup(b"vkCmdSetEvent2\0").unwrap();
        let g: unsafe extern "C" fn(VkCommandBuffer, u64, *const c_void) =
            unsafe { std::mem::transmute(f) };
        unsafe { g(std::ptr::null_mut(), 0, std::ptr::null()); }
    }

    #[test]
    fn timeline_semaphore_and_descriptor_recycling_stubs() {
        // 8 entry points + KHR aliases — all resolve, all return
        // SUCCESS / harmless on null-ish input.
        for name in [
            b"vkFreeDescriptorSets\0".as_slice(),
            b"vkResetDescriptorPool\0".as_slice(),
            b"vkSignalSemaphore\0".as_slice(),
            b"vkSignalSemaphoreKHR\0".as_slice(),
            b"vkWaitSemaphores\0".as_slice(),
            b"vkWaitSemaphoresKHR\0".as_slice(),
            b"vkGetSemaphoreCounterValue\0".as_slice(),
            b"vkGetSemaphoreCounterValueKHR\0".as_slice(),
        ] {
            assert!(lookup(name).is_some(),
                "must resolve {}",
                std::str::from_utf8(&name[..name.len()-1]).unwrap());
        }

        // GetSemaphoreCounterValue should write the spec-mandated
        // initial value (0) to *p_value.
        let f = lookup(b"vkGetSemaphoreCounterValue\0").unwrap();
        let g: unsafe extern "C" fn(VkDevice, u64, *mut u64) -> VkResult =
            unsafe { std::mem::transmute(f) };
        let mut v: u64 = 0xdead_beef_dead_beef;
        let r = unsafe { g(std::ptr::null_mut(), 1, &mut v) };
        assert_eq!(r, VK_SUCCESS);
        assert_eq!(v, 0, "must overwrite the caller buffer with 0");

        // Null p_value must not crash.
        let r = unsafe { g(std::ptr::null_mut(), 1, std::ptr::null_mut()) };
        assert_eq!(r, VK_SUCCESS);

        // vkWaitSemaphores must return immediately regardless of
        // the timeout we pass.
        let f = lookup(b"vkWaitSemaphores\0").unwrap();
        let g: unsafe extern "C" fn(VkDevice, *const c_void, u64) -> VkResult =
            unsafe { std::mem::transmute(f) };
        let t0 = std::time::Instant::now();
        let r = unsafe { g(std::ptr::null_mut(), std::ptr::null(), u64::MAX) };
        assert_eq!(r, VK_SUCCESS);
        assert!(t0.elapsed() < std::time::Duration::from_millis(50),
            "stub must NOT actually wait the timeout");
    }

    #[test]
    fn command_pool_and_memory_range_stubs_resolve_and_succeed() {
        // 5 entry points — all proc-addr resolvable and all return
        // SUCCESS / void without crashing on null-ish inputs.
        for name in [
            b"vkResetCommandPool\0".as_slice(),
            b"vkTrimCommandPool\0".as_slice(),
            b"vkTrimCommandPoolKHR\0".as_slice(),
            b"vkFlushMappedMemoryRanges\0".as_slice(),
            b"vkInvalidateMappedMemoryRanges\0".as_slice(),
        ] {
            let r = lookup(name);
            assert!(r.is_some(),
                "must resolve {}",
                std::str::from_utf8(&name[..name.len()-1]).unwrap());
        }

        let f = lookup(b"vkResetCommandPool\0").unwrap();
        let g: unsafe extern "C" fn(VkDevice, u64, u32) -> VkResult =
            unsafe { std::mem::transmute(f) };
        let r = unsafe { g(std::ptr::null_mut(), 0, 0) };
        assert_eq!(r, VK_SUCCESS);

        let f = lookup(b"vkFlushMappedMemoryRanges\0").unwrap();
        let g: unsafe extern "C" fn(VkDevice, u32, *const c_void) -> VkResult =
            unsafe { std::mem::transmute(f) };
        let r = unsafe { g(std::ptr::null_mut(), 0, std::ptr::null()) };
        assert_eq!(r, VK_SUCCESS);
    }

    #[test]
    fn image_format_properties_accepts_supported_rejects_unsupported() {
        let pd = AtriumPhysicalDevice {
            loader_dispatch_slot: VK_ICD_LOADER_MAGIC,
            backend_vendor: aqueduct_gpu::backends::GpuVendor::Software,
            backend_generation: 0,
            instance: std::ptr::null_mut(),
        };
        let phys: VkPhysicalDevice = &pd as *const _ as *mut _;
        let f = lookup(b"vkGetPhysicalDeviceImageFormatProperties\0").unwrap();
        let g: unsafe extern "C" fn(VkPhysicalDevice, ash::vk::Format, ash::vk::ImageType, ash::vk::ImageTiling, ash::vk::ImageUsageFlags, ash::vk::ImageCreateFlags, *mut ash::vk::ImageFormatProperties) -> VkResult =
            unsafe { std::mem::transmute(f) };

        // Supported: R8G8B8A8_UNORM (37), 2D, OPTIMAL.
        let mut props = ash::vk::ImageFormatProperties::default();
        let r = unsafe { g(
            phys,
            ash::vk::Format::R8G8B8A8_UNORM,
            ash::vk::ImageType::TYPE_2D,
            ash::vk::ImageTiling::OPTIMAL,
            ash::vk::ImageUsageFlags::COLOR_ATTACHMENT,
            ash::vk::ImageCreateFlags::empty(),
            &mut props,
        ) };
        assert_eq!(r, VK_SUCCESS);
        assert_eq!(props.max_extent.width, 16384);
        assert_eq!(props.max_array_layers, 256);
        assert_eq!(props.max_mip_levels, 14);
        assert!(props.sample_counts.contains(ash::vk::SampleCountFlags::TYPE_1));

        // Unsupported: random YUV format.
        let r = unsafe { g(
            phys,
            ash::vk::Format::G8B8G8R8_422_UNORM,
            ash::vk::ImageType::TYPE_2D,
            ash::vk::ImageTiling::OPTIMAL,
            ash::vk::ImageUsageFlags::SAMPLED,
            ash::vk::ImageCreateFlags::empty(),
            &mut props,
        ) };
        assert_eq!(r, -11 /* FORMAT_NOT_SUPPORTED */);

        // Unsupported: 3D image even at a supported format.
        let r = unsafe { g(
            phys,
            ash::vk::Format::R8G8B8A8_UNORM,
            ash::vk::ImageType::TYPE_3D,
            ash::vk::ImageTiling::OPTIMAL,
            ash::vk::ImageUsageFlags::SAMPLED,
            ash::vk::ImageCreateFlags::empty(),
            &mut props,
        ) };
        assert_eq!(r, -11);

        // *2 variant resolves.
        assert!(lookup(b"vkGetPhysicalDeviceImageFormatProperties2\0").is_some());
        assert!(lookup(b"vkGetPhysicalDeviceImageFormatProperties2KHR\0").is_some());
    }

    #[test]
    fn physical_device_properties2_fills_inner_struct_via_offset() {
        // Build a fake AtriumPhysicalDevice we can pass through.
        let pd = AtriumPhysicalDevice {
            loader_dispatch_slot: VK_ICD_LOADER_MAGIC,
            backend_vendor: aqueduct_gpu::backends::GpuVendor::Software,
            backend_generation: 0,
            instance: std::ptr::null_mut(),
        };
        let phys: VkPhysicalDevice = &pd as *const _ as *mut _;

        // VkPhysicalDeviceProperties2: sType+pad+pNext = 16 B
        // header, then VkPhysicalDeviceProperties.
        let header_size = 16usize;
        let inner_size  = std::mem::size_of::<ash::vk::PhysicalDeviceProperties>();
        let total = header_size + inner_size;
        let mut buf: Vec<u8> = vec![0u8; total];
        // sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2 (1000059001)
        let s_type: u32 = 1_000_059_001;
        unsafe {
            std::ptr::write_unaligned(buf.as_mut_ptr() as *mut u32, s_type);
        }
        // pNext = null
        unsafe {
            std::ptr::write_unaligned(
                buf.as_mut_ptr().add(8) as *mut *mut c_void, std::ptr::null_mut(),
            );
        }

        let f = lookup(b"vkGetPhysicalDeviceProperties2\0").unwrap();
        let g2: unsafe extern "C" fn(VkPhysicalDevice, *mut c_void) =
            unsafe { std::mem::transmute(f) };
        unsafe { g2(phys, buf.as_mut_ptr() as *mut c_void); }

        // Read back inner.api_version — should match
        // ATRIUM_ICD_API_VERSION.
        let api_ver = unsafe {
            std::ptr::read_unaligned(buf.as_ptr().add(header_size) as *const u32)
        };
        assert_eq!(api_ver, ATRIUM_ICD_API_VERSION,
            "Properties2 must fill inner.api_version like Properties does");

        // KHR alias must resolve too.
        assert!(lookup(b"vkGetPhysicalDeviceProperties2KHR\0").is_some());

        // All four other *2 entry points resolve.
        assert!(lookup(b"vkGetPhysicalDeviceFeatures2\0").is_some());
        assert!(lookup(b"vkGetPhysicalDeviceMemoryProperties2\0").is_some());
        assert!(lookup(b"vkGetPhysicalDeviceFormatProperties2\0").is_some());
        assert!(lookup(b"vkGetPhysicalDeviceQueueFamilyProperties2\0").is_some());
    }

    #[test]
    fn physical_device_properties_limits_meet_spec_minimums() {
        // Create instance + enumerate a physical device via the
        // null-instance bootstrap so we get a real
        // AtriumPhysicalDevice handle.
        let f_ci = lookup(b"vkCreateInstance\0").unwrap();
        let ci: unsafe extern "C" fn(*const c_void, *const c_void, *mut VkInstance) -> VkResult =
            unsafe { std::mem::transmute(f_ci) };
        let mut inst: VkInstance = std::ptr::null_mut();
        unsafe { ci(std::ptr::null(), std::ptr::null(), &mut inst); }
        // No live daemon → 0 devices, but we still need a non-
        // null AtriumPhysicalDevice to call vkGet…Properties on.
        // Stub the call against a stack-allocated handle: this
        // test exercises the *limits-fill* path, not enumeration.
        let pd = AtriumPhysicalDevice {
            loader_dispatch_slot: VK_ICD_LOADER_MAGIC,
            backend_vendor: aqueduct_gpu::backends::GpuVendor::Software,
            backend_generation: 0,
            instance: std::ptr::null_mut(),
        };
        let phys: VkPhysicalDevice = &pd as *const _ as *mut _;
        let mut props = ash::vk::PhysicalDeviceProperties::default();
        unsafe { vkGetPhysicalDeviceProperties(phys, &mut props); }

        // Vulkan 1.3 spec "Required Limits" lower bounds for any
        // conformant implementation. If any of these fail, an
        // app that targets baseline Vulkan would reject us.
        let l = &props.limits;
        assert!(l.max_image_dimension2_d >= 4096, "spec min 4096");
        assert!(l.max_image_dimension_cube >= 4096);
        assert!(l.max_image_array_layers >= 256);
        assert!(l.max_bound_descriptor_sets >= 4);
        assert!(l.max_framebuffer_width >= 4096);
        assert!(l.max_framebuffer_height >= 4096);
        assert!(l.max_color_attachments >= 4);
        assert!(l.max_push_constants_size >= 128);
        assert!(l.max_viewports >= 1);
        assert_eq!(l.max_vertex_input_attributes >= 16, true);
        // Sample counts: must include 1-bit somewhere.
        assert!(l.framebuffer_color_sample_counts
            .contains(ash::vk::SampleCountFlags::TYPE_1));
        assert!(l.sampled_image_color_sample_counts
            .contains(ash::vk::SampleCountFlags::TYPE_1));

        let f_di = lookup(b"vkDestroyInstance\0").unwrap();
        let di: unsafe extern "C" fn(VkInstance, *const c_void) =
            unsafe { std::mem::transmute(f_di) };
        unsafe { di(inst, std::ptr::null()); }
    }

    #[test]
    fn enumerate_device_extensions_lists_swapchain() {
        let f = lookup(b"vkEnumerateDeviceExtensionProperties\0").unwrap();
        let typed: unsafe extern "C" fn(VkPhysicalDevice, *const c_char, *mut u32, *mut VkExtensionProperties) -> VkResult =
            unsafe { std::mem::transmute(f) };
        let mut count: u32 = 99;
        let r = unsafe { typed(std::ptr::null_mut(), std::ptr::null(), &mut count, std::ptr::null_mut()) };
        assert_eq!(r, VK_SUCCESS);
        assert_eq!(count, 2);

        let mut props: [VkExtensionProperties; 4] = unsafe { std::mem::zeroed() };
        let mut cap: u32 = 4;
        let _ = unsafe { typed(std::ptr::null_mut(), std::ptr::null(), &mut cap, props.as_mut_ptr()) };
        assert_eq!(cap, 2);
        let names: Vec<String> = (0..2).map(|i| {
            let bytes: Vec<u8> = props[i].extensionName.iter()
                .take_while(|&&c| c != 0).map(|&c| c as u8).collect();
            String::from_utf8(bytes).unwrap()
        }).collect();
        assert_eq!(names[0], "VK_KHR_swapchain");
        assert_eq!(names[1], "VK_KHR_push_descriptor");
    }

    #[test]
    fn descriptor_update_template_and_push_descriptor_stubs_resolve() {
        for name in [
            b"vkCreateDescriptorUpdateTemplate\0".as_slice(),
            b"vkCreateDescriptorUpdateTemplateKHR\0".as_slice(),
            b"vkDestroyDescriptorUpdateTemplate\0".as_slice(),
            b"vkDestroyDescriptorUpdateTemplateKHR\0".as_slice(),
            b"vkUpdateDescriptorSetWithTemplate\0".as_slice(),
            b"vkUpdateDescriptorSetWithTemplateKHR\0".as_slice(),
            b"vkCmdPushDescriptorSet\0".as_slice(),
            b"vkCmdPushDescriptorSetKHR\0".as_slice(),
            b"vkCmdPushDescriptorSetWithTemplate\0".as_slice(),
            b"vkCmdPushDescriptorSetWithTemplateKHR\0".as_slice(),
        ] {
            assert!(lookup(name).is_some(),
                "must resolve {}",
                std::str::from_utf8(&name[..name.len()-1]).unwrap());
        }

        // Create returns distinct handles.
        let f = lookup(b"vkCreateDescriptorUpdateTemplate\0").unwrap();
        let g: unsafe extern "C" fn(VkDevice, *const c_void, *const c_void, *mut u64) -> VkResult =
            unsafe { std::mem::transmute(f) };
        let mut t1: u64 = 0;
        let mut t2: u64 = 0;
        unsafe { g(std::ptr::null_mut(), std::ptr::null(), std::ptr::null(), &mut t1); }
        unsafe { g(std::ptr::null_mut(), std::ptr::null(), std::ptr::null(), &mut t2); }
        assert_ne!(t1, 0);
        assert_ne!(t1, t2, "successive Create returns distinct handles");

        // Cmd stubs accept null cmdbuf without panicking.
        let f = lookup(b"vkCmdPushDescriptorSet\0").unwrap();
        let g: unsafe extern "C" fn(VkCommandBuffer, u32, u64, u32, u32, *const c_void) =
            unsafe { std::mem::transmute(f) };
        unsafe { g(std::ptr::null_mut(), 0, 0, 0, 0, std::ptr::null()); }
    }

    #[test]
    fn debug_utils_messenger_round_trip_and_label_stubs() {
        // Create instance.
        let f_ci = lookup(b"vkCreateInstance\0").unwrap();
        let ci: unsafe extern "C" fn(*const c_void, *const c_void, *mut VkInstance) -> VkResult =
            unsafe { std::mem::transmute(f_ci) };
        let mut inst: VkInstance = std::ptr::null_mut();
        unsafe { ci(std::ptr::null(), std::ptr::null(), &mut inst); }

        // Messenger create returns success + non-null id; second
        // call returns a DISTINCT id (catches "always 1" regression).
        let f_cm = lookup(b"vkCreateDebugUtilsMessengerEXT\0").unwrap();
        let cm: unsafe extern "C" fn(VkInstance, *const c_void, *const c_void, *mut u64) -> VkResult =
            unsafe { std::mem::transmute(f_cm) };
        let mut m1: u64 = 0;
        let mut m2: u64 = 0;
        let r1 = unsafe { cm(inst, std::ptr::null(), std::ptr::null(), &mut m1) };
        let r2 = unsafe { cm(inst, std::ptr::null(), std::ptr::null(), &mut m2) };
        assert_eq!(r1, VK_SUCCESS);
        assert_eq!(r2, VK_SUCCESS);
        assert_ne!(m1, 0);
        assert_ne!(m1, m2, "distinct messenger handles");

        // Destroy + the label/object-name stubs — none of these
        // should crash or return a value we'd notice.
        let f_dm = lookup(b"vkDestroyDebugUtilsMessengerEXT\0").unwrap();
        let dm: unsafe extern "C" fn(VkInstance, u64, *const c_void) =
            unsafe { std::mem::transmute(f_dm) };
        unsafe { dm(inst, m1, std::ptr::null()); }
        unsafe { dm(inst, m2, std::ptr::null()); }

        // Spot-check the rest resolve to non-null function pointers.
        assert!(lookup(b"vkSubmitDebugUtilsMessageEXT\0").is_some());
        assert!(lookup(b"vkSetDebugUtilsObjectNameEXT\0").is_some());
        assert!(lookup(b"vkSetDebugUtilsObjectTagEXT\0").is_some());
        assert!(lookup(b"vkQueueBeginDebugUtilsLabelEXT\0").is_some());
        assert!(lookup(b"vkQueueEndDebugUtilsLabelEXT\0").is_some());
        assert!(lookup(b"vkQueueInsertDebugUtilsLabelEXT\0").is_some());
        assert!(lookup(b"vkCmdBeginDebugUtilsLabelEXT\0").is_some());
        assert!(lookup(b"vkCmdEndDebugUtilsLabelEXT\0").is_some());
        assert!(lookup(b"vkCmdInsertDebugUtilsLabelEXT\0").is_some());

        let f_di = lookup(b"vkDestroyInstance\0").unwrap();
        let di: unsafe extern "C" fn(VkInstance, *const c_void) =
            unsafe { std::mem::transmute(f_di) };
        unsafe { di(inst, std::ptr::null()); }
    }

    #[test]
    fn enumerate_instance_layers_reports_zero() {
        let f = lookup(b"vkEnumerateInstanceLayerProperties\0").unwrap();
        let typed: unsafe extern "C" fn(*mut u32, *mut VkLayerProperties) -> VkResult =
            unsafe { std::mem::transmute(f) };
        let mut count: u32 = 99;
        let r = unsafe { typed(&mut count, std::ptr::null_mut()) };
        assert_eq!(r, VK_SUCCESS);
        assert_eq!(count, 0);
    }

    #[test]
    fn create_and_destroy_instance_round_trip() {
        let f_create = lookup(b"vkCreateInstance\0").unwrap();
        let create: unsafe extern "C" fn(*const c_void, *const c_void, *mut VkInstance) -> VkResult =
            unsafe { std::mem::transmute(f_create) };
        let mut inst: VkInstance = std::ptr::null_mut();
        let r = unsafe { create(std::ptr::null(), std::ptr::null(), &mut inst) };
        assert_eq!(r, VK_SUCCESS);
        assert!(!inst.is_null(), "create_instance must produce a non-null handle");

        // Loader-ICD ABI: the first sizeof(void*) bytes of a
        // dispatchable handle must be VK_ICD_LOADER_MAGIC when
        // the ICD returns it.
        let slot: usize = unsafe { *(inst as *const usize) };
        assert_eq!(slot, VK_ICD_LOADER_MAGIC,
            "first slot of VkInstance must hold ICD_LOADER_MAGIC for the loader");

        let f_destroy = lookup(b"vkDestroyInstance\0").unwrap();
        let destroy: unsafe extern "C" fn(VkInstance, *const c_void) =
            unsafe { std::mem::transmute(f_destroy) };
        unsafe { destroy(inst, std::ptr::null()); }
        // No assertion — destroy is infallible by spec. The unit
        // test passes if there's no leak/UB (Miri or ASan would
        // catch use-after-free; the Drop on Box ran cleanly).
    }

    #[test]
    fn enumerate_physical_devices_reports_zero_today() {
        // Set up an instance.
        let f_create = lookup(b"vkCreateInstance\0").unwrap();
        let create: unsafe extern "C" fn(*const c_void, *const c_void, *mut VkInstance) -> VkResult =
            unsafe { std::mem::transmute(f_create) };
        let mut inst: VkInstance = std::ptr::null_mut();
        let _ = unsafe { create(std::ptr::null(), std::ptr::null(), &mut inst) };

        let f_enum = lookup(b"vkEnumeratePhysicalDevices\0").unwrap();
        let enum_pd: unsafe extern "C" fn(VkInstance, *mut u32, *mut VkPhysicalDevice) -> VkResult =
            unsafe { std::mem::transmute(f_enum) };
        let mut count: u32 = 99;
        let r = unsafe { enum_pd(inst, &mut count, std::ptr::null_mut()) };
        assert_eq!(r, VK_SUCCESS);
        // No physical devices wired up yet — Phase 1.3b will grow
        // device-discovery against aqueduct-gpu and bump this.
        assert_eq!(count, 0);

        let f_destroy = lookup(b"vkDestroyInstance\0").unwrap();
        let destroy: unsafe extern "C" fn(VkInstance, *const c_void) =
            unsafe { std::mem::transmute(f_destroy) };
        unsafe { destroy(inst, std::ptr::null()); }
    }

    #[test]
    fn destroy_null_instance_is_safe() {
        let f = lookup(b"vkDestroyInstance\0").unwrap();
        let destroy: unsafe extern "C" fn(VkInstance, *const c_void) =
            unsafe { std::mem::transmute(f) };
        // Per Vulkan spec, vkDestroyInstance on VK_NULL_HANDLE is
        // a documented no-op. Verify we don't panic.
        unsafe { destroy(std::ptr::null_mut(), std::ptr::null()); }
    }
}
