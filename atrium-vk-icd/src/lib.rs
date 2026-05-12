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
    /// The aqueduct-gpu client connection this instance owns.
    /// `None` if connect/handshake failed at create time — the
    /// instance is still valid, just exposes zero physical devices.
    client: Option<aqueduct_gpu_client::GpuClient>,
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
        // Phase 1.3b+ continued: vkCmdBindPipeline (needs VkPipeline
        // → ResourceId mapping), vkCmdBindVertexBuffers,
        // vkCmdBindIndexBuffer, vkCmdDrawIndexed, vkCmdCopyBuffer*,
        // vkCmdPipelineBarrier, …
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
    _format:             ash::vk::Format,
    p_format_properties: *mut ash::vk::FormatProperties,
) {
    if p_format_properties.is_null() { return; }
    *p_format_properties = ash::vk::FormatProperties::default();
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
    _physical_device: VkPhysicalDevice,
    _p_create_info:   *const c_void, /* const VkDeviceCreateInfo* */
    _p_allocator:     *const c_void, /* const VkAllocationCallbacks* */
    p_device:         *mut VkDevice,
) -> VkResult {
    if p_device.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    // Allocate the device first (we need its address for the queue
    // back-pointer), then build a single queue and link it in.
    let mut dev = Box::new(AtriumDevice {
        loader_dispatch_slot: VK_ICD_LOADER_MAGIC,
        queues: Vec::with_capacity(1),
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
    _device:            VkDevice,
    p_allocate_info:    *const c_void, /* const VkCommandBufferAllocateInfo* */
    p_command_buffers:  *mut VkCommandBuffer,
) -> VkResult {
    if p_allocate_info.is_null() || p_command_buffers.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
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

/// `vkQueueSubmit` — flush recorded FrameOps from each submitted
/// command buffer to the aqueduct-gpu host endpoint via the
/// owning AtriumInstance's GpuClient.
///
/// Today this is a no-op success: vkCmd* recording isn't wired yet,
/// so every cmdbuf's FrameOp stream is empty. The signature + state
/// transitions are in place so the Phase 1.3b+ vkCmd* chunk just
/// has to push into `_frame`.
#[no_mangle]
pub unsafe extern "C" fn vkQueueSubmit(
    _queue:         VkQueue,
    _submit_count:  u32,
    _p_submits:     *const c_void, /* const VkSubmitInfo* */
    _fence:         *mut c_void,   /* VkFence */
) -> VkResult {
    // TODO Phase 1.3b+: walk *p_submits, for each VkSubmitInfo's
    // commandBuffers, transmute back to AtriumCommandBuffer, hand
    // the FrameBuilder.into_buf() to GpuClient::submit_frame.
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
    // valid, just exposes zero physical devices. A Vulkan app
    // then sees "Atrium ICD present, 0 GPUs available" — the
    // same surface as if a real GPU were missing.
    let (client, devices) = match try_connect_aqueduct() {
        Some((client, backend)) => {
            // One backend → one VkPhysicalDevice today. When the
            // kmod gains multi-backend enumeration (D5+), this
            // grows into a loop over IOC_GPU_LIST_BACKENDS.
            let pd = Box::new(AtriumPhysicalDevice {
                loader_dispatch_slot: VK_ICD_LOADER_MAGIC,
                backend_vendor:     backend.vendor,
                backend_generation: backend.generation,
            });
            (Some(client), vec![Box::into_raw(pd)])
        }
        None => (None, Vec::new()),
    };

    let inst = Box::new(AtriumInstance {
        loader_dispatch_slot: VK_ICD_LOADER_MAGIC,
        client,
        devices,
    });
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

/// `vkEnumerateInstanceExtensionProperties` — returns the list of
/// instance-level extensions this ICD supports. Skeleton: zero
/// extensions. Standard two-call query: caller invokes once with
/// `p_props=NULL` to learn the count, then again with a buffer.
#[no_mangle]
pub unsafe extern "C" fn vkEnumerateInstanceExtensionProperties(
    _p_layer_name: *const c_char,
    p_property_count: *mut u32,
    p_properties:     *mut VkExtensionProperties,
) -> VkResult {
    if p_property_count.is_null() {
        return -7 /* VK_ERROR_INITIALIZATION_FAILED */;
    }
    // Zero extensions supported today. p_properties is ignored
    // because count is zero; future implementations would fill it.
    let cap_in = *p_property_count;
    *p_property_count = 0;
    let _ = p_properties;
    let _ = cap_in;
    VK_SUCCESS
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
        // vkCmdBindPipeline isn't wired yet.
        let name = b"vkCmdBindPipeline\0";
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
    fn enumerate_instance_extensions_reports_zero() {
        let f = lookup(b"vkEnumerateInstanceExtensionProperties\0").unwrap();
        let typed: unsafe extern "C" fn(*const c_char, *mut u32, *mut VkExtensionProperties) -> VkResult =
            unsafe { std::mem::transmute(f) };
        let mut count: u32 = 99;
        let r = unsafe { typed(std::ptr::null(), &mut count, std::ptr::null_mut()) };
        assert_eq!(r, VK_SUCCESS);
        assert_eq!(count, 0);
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
