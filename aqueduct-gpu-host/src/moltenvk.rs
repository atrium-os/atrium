//! Tier-3 MoltenVK backend — host-side Vulkan via MoltenVK on macOS.
//!
//! Architecture per `docs/spec/aqueduct-gpu.md` §6.5: tier-3 is the
//! hardware-accelerated path. On macOS-HVF dev hosts it's MoltenVK
//! sitting on top of Metal. On real FreeBSD hardware (D5+) the same
//! aqueduct-gpu wire reaches an in-kernel atrium-gpu driver — this
//! file is the **dev/CI on macOS** half of that story.
//!
//! ## Phase 1.3b scope (this file)
//!
//! - **Construction**: load the Vulkan loader via [`ash::Entry`],
//!   create a `VkInstance`, pick a physical device (preferring an
//!   Apple integrated/discrete GPU when MoltenVK is in use), create
//!   a `VkDevice` with one graphics + transfer queue.
//! - **Identity/caps reporting**: handshake reports
//!   [`GpuVendor::Apple`] (when MoltenVK is the implementation;
//!   actual vendor for non-Apple Vulkan loaders) and `CAPS_COMPUTE
//!   | CAPS_COMPOSITION | CAPS_SHARE_SURFACE | CAPS_SPIRV_UPLOAD`.
//! - **submit_frame**: protocol-correct stub — signals fences
//!   immediately like [`StubBackend`](crate::StubBackend) does. Real
//!   `VkCommandBuffer` recording lands in a follow-on commit.
//!
//! ## What this file deliberately does NOT do yet
//!
//! - Recording draws into a real `VkCommandBuffer`
//! - SPIR-V → `MTLLibrary` compile (via SPIRV-Cross or direct)
//! - Frame command stream → vkCmd* translation
//! - Surface creation / WSI (this is a HEADLESS host — pixels flow
//!   back via OP_GPU_SHARE_SURFACE, not vkSwapchain)
//!
//! Each of these is its own commit in the 1.3b rollout.
//!
//! ## Fail-soft construction
//!
//! [`MoltenVkBackend::new`] returns `Err` if Vulkan isn't installed
//! on the host (no MoltenVK, no Vulkan loader). The daemon falls
//! back to [`SoftwareBackend`](crate::SoftwareBackend) in that case;
//! no other host code knows or cares which tier is active.

use std::collections::HashMap;
use std::ffi::CString;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use ash::{vk, Entry};
use ash::khr;

use aqueduct_gpu::backends::{BackendId, GpuVendor};
use aqueduct_gpu::ids::ResourceId;
use aqueduct_gpu::frame::FrameDecoder;
use aqueduct_gpu::opcodes::FrameOp;

use crate::backend::Backend;

/// A guest colour image, materialised lazily as a real `VkImage` the
/// first time a frame references it (so `image_created` — which arrives
/// before `set_image_format` — doesn't have to guess the format).
struct MvkImage {
    width:  u32,
    height: u32,
    format: vk::Format,
    image:  Option<vk::Image>,
    memory: Option<vk::DeviceMemory>,
}

/// A guest buffer, backed by a host-visible + coherent `VkBuffer` so
/// readback (`buffer_read_bytes`) sees device writes without an
/// explicit invalidate.
struct MvkBuffer {
    size:   u64,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    mapped: *mut u8,
}

// SAFETY: the raw `mapped` pointer is only dereferenced under the
// backend's `submit_lock`/map lifetime; the VkBuffer + memory are owned
// for the backend's lifetime. The Backend trait requires Send+Sync.
unsafe impl Send for MvkBuffer {}
unsafe impl Sync for MvkBuffer {}

/// A built scene acceleration structure: one BLAS (a baked prefab mesh) +
/// a TLAS instancing it. The shader binds `tlas` as a
/// VK_DESCRIPTOR_TYPE_ACCELERATION_STRUCTURE_KHR descriptor and ray-queries
/// it. `owned` holds every backing buffer (vertices, BLAS/TLAS storage,
/// instances, scratch) so they live as long as the structure is bound.
struct MvkAccel {
    tlas:  vk::AccelerationStructureKHR,
    /// The BLASes this TLAS instances (kept alive with the TLAS).
    #[allow(dead_code)] blases: Vec<vk::AccelerationStructureKHR>,
    /// Their device addresses (the TLAS instance references), so the TLAS
    /// alone can be rebuilt over a new instance set (`rebuild_scene_tlas`).
    blas_addrs: Vec<u64>,
    /// Buffers/memory backing the BLASes (live for the scene's lifetime).
    #[allow(dead_code)] owned: Vec<(vk::Buffer, vk::DeviceMemory)>,
    /// In-flight BLAS builds (`add_scene_blases_async`). Drained by
    /// `poll_scene_blases`, which also frees the build inputs.
    pending: Vec<PendingBuild>,
    /// Buffers backing the current TLAS (instances, storage, scratch).
    tlas_owned: Option<TlasBufs>,
    /// The previous TLAS's buffers, kept for the next rebuild (a rebuild
    /// that fits reuses them instead of three allocations + a map).
    tlas_spare: Option<TlasBufs>,
    /// An async TLAS rebuild in flight (`rebuild_scene_tlas_async`); swapped
    /// in by `poll_scene_tlas`.
    tlas_pending: Option<PendingTlas>,
    /// The scene's instance array (64 B Vulkan instance records, one per
    /// slot): the source of truth every TLAS build reads. Patched per slot
    /// by `update_scene_instance`; a build copies only the slots changed
    /// since its buffer set was last synced.
    inst_shadow: Vec<u8>,
    /// Slots changed since the oldest live buffer set was synced.
    inst_log: Vec<u32>,
    /// Bumped by `set_scene_instances` (a buffer set with another
    /// generation re-copies the whole array).
    inst_gen: u64,
}

/// The three buffers of one TLAS build: the mapped instance array, the
/// structure's storage and the build scratch, each with its capacity so a
/// later build can reuse the set.
struct TlasBufs {
    inst: vk::Buffer, inst_mem: vk::DeviceMemory, inst_map: *mut u8, inst_cap: u64,
    store: vk::Buffer, store_mem: vk::DeviceMemory, store_cap: u64,
    scratch: vk::Buffer, scratch_mem: vk::DeviceMemory, scratch_cap: u64,
    /// Instance-array generation this set's `inst` mirrors (0 = never).
    gen: u64,
    /// Position in `inst_log` up to which `inst` has been patched.
    synced: usize,
}
unsafe impl Send for TlasBufs {}
impl TlasBufs {
    fn fits(&self, inst: u64, store: u64, scratch: u64) -> bool {
        inst <= self.inst_cap && store <= self.store_cap && scratch <= self.scratch_cap
    }
    fn pairs(&self) -> [(vk::Buffer, vk::DeviceMemory); 3] {
        [(self.inst, self.inst_mem), (self.store, self.store_mem), (self.scratch, self.scratch_mem)]
    }
}

/// A TLAS being built while frames trace the current one.
struct PendingTlas {
    fence: vk::Fence,
    cb: vk::CommandBuffer,
    tlas: vk::AccelerationStructureKHR,
    owned: TlasBufs,
    size: u64,
    instances: u32,
}

/// One in-flight batch of BLAS builds: its fence + command buffer, the
/// index range it produces, and the buffers only the build reads
/// (vertex input, scratch), freed once the fence signals.
struct PendingBuild {
    fence: vk::Fence,
    cb: vk::CommandBuffer,
    first: u32,
    #[allow(dead_code)] count: u32,
    transient: Vec<(vk::Buffer, vk::DeviceMemory)>,
}

/// One TLAS instance for [`MoltenVkBackend::build_scene_tlas`]: which BLAS
/// it instances, its 24-bit custom index (readable in the kernel as the
/// ray query's committed instance ID — e.g. an attribute-table base), and a
/// row-major 3×4 object-to-world transform.
#[derive(Clone, Copy, Debug)]
pub struct SceneInstance {
    /// Index into the `blases` slice passed to `build_scene_tlas`.
    pub blas: u32,
    /// 24-bit instance custom index (`RayQuery::CommittedInstanceID()` in
    /// the kernel); the mask is always 0xFF.
    pub custom_index: u32,
    /// Row-major 3×4 object-to-world transform.
    pub transform: [f32; 12],
}
unsafe impl Send for MvkAccel {}
unsafe impl Sync for MvkAccel {}

/// A guest graphics pipeline. The VS+FS SPIR-V is stashed at create
/// time, but the real `VkPipeline` is materialised **lazily** on first
/// draw — the colour-attachment format (needed for the pipeline's
/// render pass, and for Vulkan render-pass compatibility with the
/// per-frame render pass) isn't known until `BeginRenderPass` picks a
/// target. `materialized` caches it for the format last drawn with.
struct MvkPipeline {
    vs_spirv:     Vec<u8>,
    fs_spirv:     Vec<u8>,
    materialized: Option<MvkPipelineVk>,
}

/// A compute pipeline: realised eagerly at create time (no
/// render-pass/format dependency, unlike graphics pipelines).
struct MvkComputePipeline {
    pipeline:    vk::Pipeline,
    layout:      vk::PipelineLayout,
    dset_layout: vk::DescriptorSetLayout,
    module:      vk::ShaderModule,
    push_size:   u32,
    /// Storage-buffer bindings in the dset layout. Descriptor
    /// writes clip to this: in-stream bindings PERSIST across
    /// dispatches by design, so a stale binding 7 from an
    /// 8-binding pass must not be written into a 4-binding
    /// pipeline's set (invalid update -> poisoned descriptors).
    ssbo_count:  u32,
    /// Binding indices whose descriptor type is
    /// VK_DESCRIPTOR_TYPE_ACCELERATION_STRUCTURE_KHR (the rest are storage
    /// buffers). Written from the accel map, not the buffer map, at dispatch.
    as_bindings: Vec<u32>,
}

/// The realised Vulkan objects for an `MvkPipeline` at a specific
/// colour format.
struct MvkPipelineVk {
    format:      vk::Format,
    pipeline:    vk::Pipeline,
    layout:      vk::PipelineLayout,
    render_pass: vk::RenderPass,
    vs:          vk::ShaderModule,
    fs:          vk::ShaderModule,
}

/// Tier-3 Vulkan backend. Wraps a loaded `VkInstance` + `VkDevice`.
///
/// One instance per host endpoint; `submit_frame` is internally
/// serialised by tiny-skia in the SW path and by a shared graphics
/// queue here. Multiple guest connections share the same VkDevice;
/// per-session isolation is the [session
/// layer](crate::session)'s job.
///
/// **Owned resources** (drop order matters):
///   1. `device`  — must be destroyed before instance
///   2. `instance`
///   3. `entry`   — last (owns the dlopen handle on the loader)
pub struct MoltenVkBackend {
    /// Submission counter for telemetry.
    submissions: AtomicU64,

    /// Selected physical device. Cached so handshake can synthesise
    /// a stable [`BackendId`] without re-querying.
    physical: vk::PhysicalDevice,
    /// Vendor reported by the physical device (Apple under MoltenVK,
    /// AMD/Intel/NVIDIA on Linux dev hosts).
    vendor: GpuVendor,
    /// Driver / device generation. We pack the major-API number.
    generation: u16,

    /// Logical device.
    device: ash::Device,
    /// One graphics+transfer queue (`VK_QUEUE_GRAPHICS_BIT |
    /// VK_QUEUE_TRANSFER_BIT`). Held for later `vkQueueSubmit` calls.
    _queue: vk::Queue,
    _queue_family: u32,

    /// VkInstance. Stays alive until `Drop`.
    instance: ash::Instance,
    /// The Vulkan loader. Stays alive until `Drop`. Box keeps it
    /// pointer-stable for ash's internal references.
    _entry: Box<Entry>,

    /// Command pool (transient, resettable) for per-submit command
    /// buffers. Guarded by `submit_lock`.
    cmd_pool: vk::CommandPool,
    /// Queue + pool for acceleration-structure builds: a SECOND queue
    /// family when the device offers one (MoltenVK exposes several, each
    /// its own Metal command queue), so builds overlap frames on the GPU
    /// instead of sitting in front of the next frame on the one queue. The
    /// host orders everything through fences (a build's outputs are used
    /// only after its fence was observed), so no cross-queue semaphores.
    /// Same as the main queue/pool when there is no second family or
    /// AQUEDUCT_GPU_SINGLE_QUEUE is set.
    build_queue: vk::Queue,
    build_pool: vk::CommandPool,
    /// Physical-device memory properties, cached for memory-type
    /// selection.
    mem_props: vk::PhysicalDeviceMemoryProperties,
    /// Guest image id → materialised `VkImage`.
    images: Mutex<HashMap<u32, MvkImage>>,
    /// Guest buffer id → host-visible `VkBuffer`.
    buffers: Mutex<HashMap<u32, MvkBuffer>>,
    /// Guest pipeline id → materialised graphics pipeline.
    pipelines: Mutex<HashMap<u32, MvkPipeline>>,
    /// Guest pipeline id → compute pipeline (the engine-bundle
    /// path: SPIR-V compute kernels dispatched via FrameOp::Dispatch).
    compute_pipelines: Mutex<HashMap<u32, MvkComputePipeline>>,
    /// Storage-buffer bindings staged for the next Dispatch
    /// (binding → guest buffer id), like Tier-2's
    /// `bind_compute_storage_buffer`. Drained per dispatch.
    compute_binds: Mutex<HashMap<u32, u32>>,
    /// Descriptor pool for per-dispatch sets; reset at the top of
    /// every `record_and_submit`.
    desc_pool: std::sync::Mutex<(vk::DescriptorPool, u32)>,
    /// Serialises command-buffer record + submit (one graphics queue).
    submit_lock: Mutex<()>,

    /// 2-slot TIMESTAMP query pool for measured GPU exec time (null if the
    /// device/queue doesn't support timestamps). Reused per submit —
    /// `submit_lock` serialises access. The measured-truth half of the
    /// device-model calibration (D-M6): real silicon time to ground the
    /// analytic roofline against.
    query_pool: vk::QueryPool,
    /// Nanoseconds per timestamp tick (`VkPhysicalDeviceLimits::
    /// timestampPeriod`).
    timestamp_period_ns: f32,
    /// Last measured GPU exec time, nanoseconds (0 until first timed frame).
    last_gpu_ns: AtomicU64,
    /// Per-dispatch GPU time of the last timed frame, ns, in dispatch order
    /// (a timestamp after every compute dispatch, up to `DISPATCH_STAMPS`).
    last_dispatch_ns: Mutex<Vec<u64>>,
    /// Cumulative measured GPU exec time across all timed frames, ns.
    total_gpu_ns: AtomicU64,
    /// Whether VK_KHR_acceleration_structure + VK_KHR_ray_query were enabled at
    /// device creation (HW ray-tracing for the instanced-mesh path available).
    ray_query: bool,
    /// VK_KHR_acceleration_structure device function loader (BLAS/TLAS build +
    /// device-address queries). `Some` iff `ray_query`.
    as_device: Option<khr::acceleration_structure::Device>,
    /// VK_EXT_external_memory_host loader (`Some` when the device offers
    /// it): page-aligned host memory bound as a buffer WITHOUT a copy —
    /// MoltenVK wraps it with `newBufferWithBytesNoCopy`, which on unified
    /// memory means the GPU reads the caller's own pages.
    host_import: Option<ash::ext::external_memory_host::Device>,
    /// `minImportedHostPointerAlignment` (the host page size; 0 = no import).
    host_page: u64,
    /// Built scene acceleration structures, keyed by ResourceId. A dispatch
    /// binding that resolves here is written as an AS descriptor, not a buffer.
    accels: Mutex<HashMap<u32, MvkAccel>>,
    /// Per-binding acceleration-structure stash for the next Dispatch (mirror
    /// of `compute_binds` for the AS descriptor type).
    compute_accel_binds: Mutex<HashMap<u32, u32>>,
    /// Set once a submission timed out: the queue may be wedged, so the
    /// owner should stop using this backend (and must not wait on it —
    /// `Drop` skips device_wait_idle when set).
    stalled: std::sync::atomic::AtomicBool,
}

/// Construction errors for [`MoltenVkBackend::new`]. Each variant
/// indicates the host environment can't support the tier-3 path;
/// the caller should fall back to tier-1 SW.
#[derive(Debug)]
pub enum MoltenVkError {
    /// Couldn't load the Vulkan loader (MoltenVK / libvulkan
    /// not installed).
    LoaderUnavailable(ash::LoadingError),
    /// Vulkan call returned an error code.
    Vulkan(vk::Result),
    /// No physical device was acceptable (no graphics queue family,
    /// etc.). Diagnostic message included.
    NoSuitableDevice(String),
    /// Internal text-conversion error during instance creation.
    BadCString,
}

impl std::fmt::Display for MoltenVkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MoltenVkError::LoaderUnavailable(e) =>
                write!(f, "Vulkan loader unavailable (install MoltenVK / Vulkan SDK): {e}"),
            MoltenVkError::Vulkan(e) => write!(f, "Vulkan call failed: {e:?}"),
            MoltenVkError::NoSuitableDevice(s) =>
                write!(f, "no suitable Vulkan device: {s}"),
            MoltenVkError::BadCString =>
                write!(f, "internal: failed to build CString for Vulkan init"),
        }
    }
}
impl std::error::Error for MoltenVkError {}

impl From<vk::Result> for MoltenVkError {
    fn from(e: vk::Result) -> Self { MoltenVkError::Vulkan(e) }
}

/// A sampled RGBA8 texture to bind for [`MoltenVkBackend::draw_and_copy_full`]
/// (combined image sampler at set 0, binding 0). Row-major texels, clamped.
#[derive(Debug, Clone, Copy)]
pub struct TexBind<'a> {
    /// RGBA8 texel bytes, row-major (`len >= width*height*4`).
    pub data: &'a [u8],
    /// Width in texels.
    pub width: u32,
    /// Height in texels.
    pub height: u32,
    /// Linear (bilinear) filtering when true, else nearest.
    pub linear: bool,
}

impl MoltenVkBackend {
    /// Construct a fresh tier-3 backend. Loads Vulkan, creates an
    /// instance, picks a graphics-capable physical device, creates a
    /// logical device with one graphics+transfer queue.
    ///
    /// Returns `Err(LoaderUnavailable)` if no Vulkan loader can be
    /// dlopened. Callers should treat this as "fall back to tier-1
    /// SW", not as a hard failure.
    pub fn new() -> Result<Self, MoltenVkError> {
        // ── Load Vulkan loader ────────────────────────────────────
        // SAFETY: ash's Entry::load is unsafe because the loader is
        // dynamically resolved. We trust the system Vulkan ICD.
        // dlopen's default search misses Homebrew on Apple Silicon
        // (/opt/homebrew/lib isn't in the fallback path), so try the
        // standard load first and the brew loader explicitly second.
        let entry = unsafe { Entry::load() }.or_else(|first_err| {
            const BREW: &str = "/opt/homebrew/lib/libvulkan.dylib";
            if std::path::Path::new(BREW).exists() {
                unsafe { Entry::load_from(BREW) }.map_err(|_| first_err)
            } else {
                Err(first_err)
            }
        }).map_err(MoltenVkError::LoaderUnavailable)?;
        let entry = Box::new(entry);

        // ── Create instance ───────────────────────────────────────
        let app_name = CString::new("aqueduct-gpu-host")
            .map_err(|_| MoltenVkError::BadCString)?;
        let engine_name = CString::new("aqueduct-gpu")
            .map_err(|_| MoltenVkError::BadCString)?;

        let app_info = vk::ApplicationInfo::default()
            .application_name(&app_name)
            .application_version(0)
            .engine_name(&engine_name)
            .engine_version(0)
            .api_version(vk::API_VERSION_1_2);

        // MoltenVK requires the portability-enumeration extension &
        // flag to be advertised. On non-Apple hosts this is harmless.
        let portability_ext_name = khr::portability_enumeration::NAME;
        let extension_ptrs = [portability_ext_name.as_ptr()];

        let mut create_flags = vk::InstanceCreateFlags::empty();
        // The constant only exists when the portability extension is
        // present; ash exposes it unconditionally so we set it.
        create_flags |= vk::InstanceCreateFlags::ENUMERATE_PORTABILITY_KHR;

        let create_info = vk::InstanceCreateInfo::default()
            .application_info(&app_info)
            .flags(create_flags)
            .enabled_extension_names(&extension_ptrs);

        // SAFETY: structs are all stack-built with correct lifetimes.
        let instance = unsafe { entry.create_instance(&create_info, None)? };

        // ── Pick a physical device ────────────────────────────────
        let physicals = unsafe { instance.enumerate_physical_devices()? };
        if physicals.is_empty() {
            unsafe { instance.destroy_instance(None) };
            return Err(MoltenVkError::NoSuitableDevice(
                "enumerate_physical_devices returned 0".into(),
            ));
        }

        let mut chosen: Option<(vk::PhysicalDevice, u32, GpuVendor, u16)> = None;
        for pd in physicals {
            let props = unsafe { instance.get_physical_device_properties(pd) };
            let vendor = vendor_from_pci_id(props.vendor_id);

            let q_families = unsafe {
                instance.get_physical_device_queue_family_properties(pd)
            };
            for (i, fam) in q_families.iter().enumerate() {
                if fam.queue_flags.contains(
                    vk::QueueFlags::GRAPHICS | vk::QueueFlags::TRANSFER,
                ) {
                    // major version of the device's reported API.
                    let generation = vk::api_version_major(props.api_version) as u16;
                    chosen = Some((pd, i as u32, vendor, generation));
                    break;
                }
            }
            if chosen.is_some() { break; }
        }

        let (physical, queue_family, vendor, generation): (vk::PhysicalDevice, u32, GpuVendor, u16) = chosen
            .ok_or_else(|| {
                unsafe { instance.destroy_instance(None) };
                MoltenVkError::NoSuitableDevice(
                    "no graphics+transfer queue family on any device".into(),
                )
            })?;

        // ── Create logical device ─────────────────────────────────
        let priorities = [1.0_f32];
        let q_create = vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&priorities);
        let mut q_creates = vec![q_create];
        // A second family with compute for acceleration-structure builds.
        let build_family: Option<u32> = if std::env::var_os("AQUEDUCT_GPU_SINGLE_QUEUE").is_some() { None } else {
            unsafe { instance.get_physical_device_queue_family_properties(physical) }
                .iter().enumerate()
                .find(|(i, f)| *i as u32 != queue_family && f.queue_count > 0 && f.queue_flags.contains(vk::QueueFlags::COMPUTE))
                .map(|(i, _)| i as u32)
        };
        if let Some(bf) = build_family {
            q_creates.push(vk::DeviceQueueCreateInfo::default()
                .queue_family_index(bf)
                .queue_priorities(&priorities));
        }

        // MoltenVK requires VK_KHR_portability_subset on the device
        // (when present in the physical-device extensions). Querying
        // for it up front would be the production path; for the
        // skeleton we just attempt with no extensions and let the
        // VK_ERROR_EXTENSION_NOT_PRESENT bubble up if the host needs
        // it. The portability-subset device extension is documented
        // in the spec as "MUST be enabled if reported."
        let portability_subset = khr::portability_subset::NAME;
        let mut device_exts = match
            host_supports_portability_subset(&instance, physical)
        {
            true  => vec![portability_subset.as_ptr()],
            false => vec![],
        };

        // Hardware ray-query (VK_KHR_acceleration_structure + VK_KHR_ray_query)
        // for the instanced-mesh path. Detect support via the FEATURE query, not
        // device-extension enumeration: the Vulkan loader omits AS/ray_query from
        // enumeration for the MoltenVK portability driver (a cosmetic loader
        // filter — see external/MoltenVK/MoltenVK/ray_query_test/README.md §5),
        // yet rayQuery is reported correctly through GetPhysicalDeviceFeatures2 and
        // the extensions are accepted at device creation. So we ask the feature
        // chain and, when present, request the extensions unconditionally.
        let mut as_query = vk::PhysicalDeviceAccelerationStructureFeaturesKHR::default();
        let mut rq_query = vk::PhysicalDeviceRayQueryFeaturesKHR::default();
        let mut feat_query = vk::PhysicalDeviceFeatures2::default()
            .push_next(&mut as_query)
            .push_next(&mut rq_query);
        unsafe { instance.get_physical_device_features2(physical, &mut feat_query) };
        let ray_query = as_query.acceleration_structure == vk::TRUE
            && rq_query.ray_query == vk::TRUE;

        if ray_query {
            device_exts.push(khr::acceleration_structure::NAME.as_ptr());
            device_exts.push(khr::ray_query::NAME.as_ptr());
            device_exts.push(khr::deferred_host_operations::NAME.as_ptr());
        }
        // VK_EXT_external_memory_host (enumerated normally — only the AS /
        // ray-query pair is hidden by the loader): lets a BLAS build read
        // page-aligned host memory in place instead of a copied upload.
        let host_import_ok = device_ext_present(&instance, physical, ash::ext::external_memory_host::NAME);
        if host_import_ok {
            device_exts.push(ash::ext::external_memory_host::NAME.as_ptr());
        }

        // Feature chain enabled at device creation. AS requires bufferDeviceAddress
        // + descriptorIndexing (core 1.2). Kept alive until create_device returns.
        let mut v12 = vk::PhysicalDeviceVulkan12Features::default()
            .buffer_device_address(ray_query)
            .descriptor_indexing(ray_query);
        let mut as_feat = vk::PhysicalDeviceAccelerationStructureFeaturesKHR::default()
            .acceleration_structure(ray_query);
        let mut rq_feat = vk::PhysicalDeviceRayQueryFeaturesKHR::default()
            .ray_query(ray_query);

        let mut device_create = vk::DeviceCreateInfo::default()
            .queue_create_infos(&q_creates)
            .enabled_extension_names(&device_exts);
        if ray_query {
            device_create = device_create
                .push_next(&mut v12)
                .push_next(&mut as_feat)
                .push_next(&mut rq_feat);
        }

        let device = unsafe { instance.create_device(physical, &device_create, None) };
        let device = match device {
            Ok(d) => d,
            Err(e) => {
                unsafe { instance.destroy_instance(None) };
                return Err(MoltenVkError::Vulkan(e));
            }
        };
        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        let build_queue = build_family.map_or(queue, |bf| unsafe { device.get_device_queue(bf, 0) });

        // Command pool for per-submit command buffers (transient +
        // individually resettable).
        let pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(queue_family)
            .flags(vk::CommandPoolCreateFlags::TRANSIENT
                | vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        let cmd_pool = match unsafe { device.create_command_pool(&pool_info, None) } {
            Ok(p) => p,
            Err(e) => {
                unsafe { device.destroy_device(None); instance.destroy_instance(None); }
                return Err(MoltenVkError::Vulkan(e));
            }
        };
        // Command pools are per queue family: the build queue needs its own.
        let build_pool = match build_family {
            None => cmd_pool,
            Some(bf) => {
                let bpi = vk::CommandPoolCreateInfo::default()
                    .queue_family_index(bf)
                    .flags(vk::CommandPoolCreateFlags::TRANSIENT | vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
                match unsafe { device.create_command_pool(&bpi, None) } {
                    Ok(p) => {
                        log::info!("acceleration-structure builds on queue family {bf} (main {queue_family})");
                        if std::env::var_os("AQUEDUCT_GPU_LOG").is_some() {
                            eprintln!("aqueduct-gpu-host: acceleration-structure builds on queue family {bf} (main {queue_family})");
                        }
                        p
                    }
                    Err(e) => {
                        unsafe { device.destroy_command_pool(cmd_pool, None); device.destroy_device(None); instance.destroy_instance(None); }
                        return Err(MoltenVkError::Vulkan(e));
                    }
                }
            }
        };

        let mem_props = unsafe {
            instance.get_physical_device_memory_properties(physical)
        };

        // Measured-GPU-time support (D-M6): the device must report a
        // non-zero timestampPeriod and the chosen queue family must have
        // timestampValidBits > 0. MoltenVK on Apple Silicon satisfies both.
        let props = unsafe { instance.get_physical_device_properties(physical) };
        let timestamp_period_ns = props.limits.timestamp_period;
        let qf_props =
            unsafe { instance.get_physical_device_queue_family_properties(physical) };
        let valid_bits = qf_props.get(queue_family as usize)
            .map(|q| q.timestamp_valid_bits).unwrap_or(0);
        let timestamps_ok = timestamp_period_ns > 0.0 && valid_bits > 0;
        let query_pool = if timestamps_ok {
            let qpi = vk::QueryPoolCreateInfo::default()
                .query_type(vk::QueryType::TIMESTAMP).query_count(2 + DISPATCH_STAMPS);
            unsafe { device.create_query_pool(&qpi, None) }
                .unwrap_or(vk::QueryPool::null())
        } else {
            vk::QueryPool::null()
        };
        if query_pool == vk::QueryPool::null() {
            // log::* is swallowed without a logger (the frescod gotcha) — use
            // eprintln so the reason is visible to benches/diagnostics.
            eprintln!("MoltenVk: GPU timestamps unavailable (timestamp_period_ns={timestamp_period_ns}, \
                       queue_family={queue_family} timestamp_valid_bits={valid_bits}); measured exec time disabled");
        }

        // Descriptor pool for per-dispatch sets (compute path).
        // Reset wholesale at each record_and_submit.
        let pool_sizes = [vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::STORAGE_BUFFER)
            .descriptor_count(256)];
        let dp_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(64)
            .pool_sizes(&pool_sizes);
        let desc_pool = std::sync::Mutex::new((
            unsafe { device.create_descriptor_pool(&dp_info, None) }
                .unwrap_or(vk::DescriptorPool::null()),
            64,
        ));

        // VK_KHR_acceleration_structure device functions (build/query). Only
        // meaningful when ray-query was enabled above.
        let as_device = if ray_query {
            Some(khr::acceleration_structure::Device::new(&instance, &device))
        } else {
            None
        };
        let (host_import, host_page) = if host_import_ok {
            let mut hp = vk::PhysicalDeviceExternalMemoryHostPropertiesEXT::default();
            let mut p2 = vk::PhysicalDeviceProperties2::default().push_next(&mut hp);
            unsafe { instance.get_physical_device_properties2(physical, &mut p2) };
            (Some(ash::ext::external_memory_host::Device::new(&instance, &device)),
             hp.min_imported_host_pointer_alignment.max(4096))
        } else {
            (None, 0)
        };

        Ok(Self {
            submissions: AtomicU64::new(0),
            physical,
            vendor,
            generation,
            device,
            _queue: queue,
            _queue_family: queue_family,
            instance,
            _entry: entry,
            cmd_pool,
            build_queue,
            build_pool,
            mem_props,
            images: Mutex::new(HashMap::new()),
            buffers: Mutex::new(HashMap::new()),
            pipelines: Mutex::new(HashMap::new()),
            compute_pipelines: Mutex::new(HashMap::new()),
            compute_binds: Mutex::new(HashMap::new()),
            desc_pool,
            submit_lock: Mutex::new(()),
            query_pool,
            timestamp_period_ns,
            last_gpu_ns: AtomicU64::new(0),
            last_dispatch_ns: Mutex::new(Vec::new()),
            total_gpu_ns: AtomicU64::new(0),
            ray_query,
            as_device,
            host_import,
            host_page,
            accels: Mutex::new(HashMap::new()),
            compute_accel_binds: Mutex::new(HashMap::new()),
            stalled: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// True once any submission has timed out (queue possibly wedged).
    pub fn is_stalled(&self) -> bool { self.stalled.load(Ordering::Relaxed) }

    /// Whether HW ray-query (VK_KHR_acceleration_structure + VK_KHR_ray_query)
    /// is available on this device. The instanced-mesh path checks this and
    /// falls back (no objects) when false — e.g. against a stock MoltenVK that
    /// lacks the acceleration-structure port.
    pub fn has_ray_query(&self) -> bool { self.ray_query }

    /// Pick a memory type index satisfying `want` (and, when `exclude` is set,
    /// lacking those flags — used to force device-local *private* AS storage).
    fn find_mem_type(&self, type_bits: u32, want: vk::MemoryPropertyFlags,
                     exclude: vk::MemoryPropertyFlags) -> Option<u32> {
        (0..self.mem_props.memory_type_count).find(|&i| {
            let f = self.mem_props.memory_types[i as usize].property_flags;
            (type_bits & (1 << i)) != 0 && f.contains(want) && !f.intersects(exclude)
        })
    }

    /// Create a device-address-enabled buffer + memory. `host_visible` ⇒
    /// mappable/coherent (geometry, instances, scratch); otherwise device-local
    /// PRIVATE (acceleration-structure storage must be Private on Metal).
    /// Returns (buffer, memory, mapped-ptr-or-null).
    unsafe fn make_as_buffer(&self, size: u64, usage: vk::BufferUsageFlags,
                             host_visible: bool)
        -> Result<(vk::Buffer, vk::DeviceMemory, *mut u8), String>
    {
        let dev = &self.device;
        let bci = vk::BufferCreateInfo::default()
            .size(size.max(4))
            .usage(usage | vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buf = dev.create_buffer(&bci, None)
            .map_err(|e| format!("AS buffer create: {e:?}"))?;
        let req = dev.get_buffer_memory_requirements(buf);
        let mt = if host_visible {
            self.find_mem_type(req.memory_type_bits,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
                vk::MemoryPropertyFlags::empty())
        } else {
            self.find_mem_type(req.memory_type_bits,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
                vk::MemoryPropertyFlags::HOST_VISIBLE)
        }.ok_or_else(|| "AS buffer: no suitable memory type".to_string())?;
        let mut flags = vk::MemoryAllocateFlagsInfo::default()
            .flags(vk::MemoryAllocateFlags::DEVICE_ADDRESS);
        let ai = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(mt)
            .push_next(&mut flags);
        let mem = dev.allocate_memory(&ai, None)
            .map_err(|e| format!("AS memory alloc: {e:?}"))?;
        dev.bind_buffer_memory(buf, mem, 0)
            .map_err(|e| format!("AS bind: {e:?}"))?;
        let mapped = if host_visible {
            dev.map_memory(mem, 0, req.size, vk::MemoryMapFlags::empty())
                .map_err(|e| format!("AS map: {e:?}"))? as *mut u8
        } else {
            std::ptr::null_mut()
        };
        Ok((buf, mem, mapped))
    }

    unsafe fn free_buffers(&self, bufs: Vec<(vk::Buffer, vk::DeviceMemory)>) {
        for (b, m) in bufs {
            self.device.destroy_buffer(b, None);
            self.device.free_memory(m, None);
        }
    }

    /// Whether a block at `ptr` can be bound in place: the extension is
    /// present and the pointer is page-aligned. The import covers whole
    /// pages, so the block's last page must be mapped to its end (true of
    /// any page-aligned mmap / posix_memalign allocation of ≥ `len`).
    pub fn can_import_host(&self, ptr: *const u8, len: u64) -> bool {
        self.host_import.is_some() && self.host_page > 0
            && (ptr as u64) % self.host_page == 0 && len > 0
    }

    /// Bind a page-aligned host block as a device-address buffer WITHOUT
    /// copying (VK_EXT_external_memory_host; MoltenVK →
    /// `newBufferWithBytesNoCopy`). The import is `len` rounded up to whole
    /// pages. The caller's memory must stay mapped and unchanged until every
    /// GPU use of the buffer has completed; `free_memory` releases only the
    /// Vulkan side.
    unsafe fn make_imported_buffer(&self, ptr: *const u8, len: u64, usage: vk::BufferUsageFlags)
        -> Result<(vk::Buffer, vk::DeviceMemory), String>
    {
        let emh = self.host_import.as_ref().ok_or_else(|| "host import unsupported".to_string())?;
        if !self.can_import_host(ptr, len) {
            return Err(format!("host import: {ptr:?} is not page-aligned ({} B pages)", self.host_page));
        }
        let len = len.div_ceil(self.host_page) * self.host_page;
        let dev = &self.device;
        let ht = vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT;
        let mut props = vk::MemoryHostPointerPropertiesEXT::default();
        (emh.fp().get_memory_host_pointer_properties_ext)(dev.handle(), ht, ptr as *const _, &mut props)
            .result().map_err(|e| format!("host pointer properties: {e:?}"))?;
        let mut ext = vk::ExternalMemoryBufferCreateInfo::default().handle_types(ht);
        let bci = vk::BufferCreateInfo::default()
            .size(len)
            .usage(usage | vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .push_next(&mut ext);
        let buf = dev.create_buffer(&bci, None).map_err(|e| format!("imported buffer create: {e:?}"))?;
        let req = dev.get_buffer_memory_requirements(buf);
        if req.size > len {
            dev.destroy_buffer(buf, None);
            return Err(format!("imported buffer needs {} B, host block is {len}", req.size));
        }
        let mt = self.find_mem_type(req.memory_type_bits & props.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            vk::MemoryPropertyFlags::empty())
            .ok_or_else(|| "imported buffer: no host-visible memory type".to_string())?;
        let mut flags = vk::MemoryAllocateFlagsInfo::default().flags(vk::MemoryAllocateFlags::DEVICE_ADDRESS);
        let mut imp = vk::ImportMemoryHostPointerInfoEXT::default().handle_type(ht).host_pointer(ptr as *mut _);
        let ai = vk::MemoryAllocateInfo::default()
            .allocation_size(len)
            .memory_type_index(mt)
            .push_next(&mut flags)
            .push_next(&mut imp);
        let mem = match dev.allocate_memory(&ai, None) {
            Ok(m) => m,
            Err(e) => { dev.destroy_buffer(buf, None); return Err(format!("host memory import: {e:?}")); }
        };
        if let Err(e) = dev.bind_buffer_memory(buf, mem, 0) {
            dev.destroy_buffer(buf, None); dev.free_memory(mem, None);
            return Err(format!("imported buffer bind: {e:?}"));
        }
        Ok((buf, mem))
    }

    unsafe fn buffer_address(&self, buf: vk::Buffer) -> u64 {
        self.device.get_buffer_device_address(
            &vk::BufferDeviceAddressInfo::default().buffer(buf))
    }

    /// Run a one-time command buffer to completion on the build queue.
    unsafe fn one_time_submit<F: FnOnce(vk::CommandBuffer)>(&self, rec: F)
        -> Result<(), String>
    {
        let dev = &self.device;
        let cb = dev.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default()
                .command_pool(self.build_pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1))
            .map_err(|e| format!("AS cmd alloc: {e:?}"))?[0];
        dev.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT))
            .map_err(|e| format!("AS cmd begin: {e:?}"))?;
        rec(cb);
        dev.end_command_buffer(cb).map_err(|e| format!("AS cmd end: {e:?}"))?;
        let cbs = [cb];
        let si = vk::SubmitInfo::default().command_buffers(&cbs);
        let _guard = self.submit_lock.lock().unwrap();
        // Bounded wait: a fence with a timeout, never queue_wait_idle. A GPU
        // stall (an enqueued-but-never-committed Metal command buffer wedges
        // the whole queue with no watchdog and no error) must surface as an
        // error the caller can act on, not an infinite block on the main
        // thread. AQUEDUCT_GPU_WAIT_MS overrides the default 30 s.
        let fence = dev.create_fence(&vk::FenceCreateInfo::default(), None)
            .map_err(|e| format!("AS fence: {e:?}"))?;
        dev.queue_submit(self.build_queue, &[si], fence)
            .map_err(|e| format!("AS submit: {e:?}"))?;
        let res = dev.wait_for_fences(&[fence], true, Self::wait_timeout_ns(30_000));
        match res {
            Ok(()) => {}
            Err(vk::Result::TIMEOUT) => {
                // Still in flight (or wedged): neither the fence nor the
                // command buffer may be freed under it — leak both.
                self.stalled.store(true, Ordering::Relaxed);
                return Err(format!(
                    "GPU stall: acceleration-structure build did not complete within {} ms \
                     (queue wedged or GPU contended); the backend must be replaced",
                    Self::wait_timeout_ns(30_000) / 1_000_000));
            }
            Err(e) => return Err(format!("AS wait: {e:?}")),
        }
        dev.destroy_fence(fence, None);
        dev.free_command_buffers(self.build_pool, &cbs);
        Ok(())
    }

    /// Record + submit one command buffer and return its fence WITHOUT
    /// waiting (streaming builds that run while frames render). The caller
    /// polls the fence and then frees both via `finish_submit`.
    unsafe fn submit_no_wait<F: FnOnce(vk::CommandBuffer)>(&self, rec: F)
        -> Result<(vk::Fence, vk::CommandBuffer), String>
    {
        let dev = &self.device;
        let cb = dev.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default()
                .command_pool(self.build_pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1))
            .map_err(|e| format!("AS cmd alloc: {e:?}"))?[0];
        dev.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT))
            .map_err(|e| format!("AS cmd begin: {e:?}"))?;
        rec(cb);
        dev.end_command_buffer(cb).map_err(|e| format!("AS cmd end: {e:?}"))?;
        let cbs = [cb];
        let si = vk::SubmitInfo::default().command_buffers(&cbs);
        let _guard = self.submit_lock.lock().unwrap();
        let fence = dev.create_fence(&vk::FenceCreateInfo::default(), None)
            .map_err(|e| format!("AS fence: {e:?}"))?;
        dev.queue_submit(self.build_queue, &[si], fence)
            .map_err(|e| format!("AS submit: {e:?}"))?;
        Ok((fence, cb))
    }

    /// Wait budget for GPU work (ns): `AQUEDUCT_GPU_WAIT_MS` or `default_ms`.
    fn wait_timeout_ns(default_ms: u64) -> u64 {
        std::env::var("AQUEDUCT_GPU_WAIT_MS").ok().and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(default_ms) * 1_000_000
    }

    /// Build a scene acceleration structure: one BLAS from a triangle-list
    /// prefab (`vertices` = flat xyz, 3 verts per triangle, non-indexed) and a
    /// TLAS instancing it once per entry in `instances` (each a row-major 3×4
    /// transform). Stored under `tlas_id`; bind it to a dispatch with
    /// [`bind_compute_accel`]. Mirrors the verified ray_query_test/host.c flow.
    pub fn build_prefab_tlas(&self, tlas_id: ResourceId, vertices: &[f32],
                             instances: &[[f32; 12]]) -> Result<(), String> {
        // Single prefab: every instance shares one attribute table, so the
        // custom index (attribute base) is 0 for all of them.
        let scene: Vec<SceneInstance> = instances.iter()
            .map(|m| SceneInstance { blas: 0, custom_index: 0, transform: *m })
            .collect();
        self.build_scene_tlas(tlas_id, &[vertices], &scene)
    }

    /// Build a scene acceleration structure: one BLAS per entry of `blases`
    /// (each a flat xyz triangle list, 3 verts/tri, non-indexed; built
    /// PREFER_FAST_TRACE — static geometry, traversal-bound) and a TLAS over
    /// `instances`, each referencing a BLAS by index and carrying a 24-bit
    /// custom index the kernel reads back as the committed instance ID.
    /// Stored under `tlas_id`; bind with [`bind_compute_accel`].
    ///
    /// Through the khr-ray-query MoltenVK fork an AS "device address" is a
    /// base constant + the AS's slot in the device's list, and the TLAS
    /// build resolves instances by that slot; BLASes created once and never
    /// freed (this API) map exactly. (Freeing an AS leaves a hole the list
    /// skips — a fork follow-up before any AS-churning use.)
    pub fn build_scene_tlas(&self, tlas_id: ResourceId, blases: &[&[f32]],
                            instances: &[SceneInstance]) -> Result<(), String> {
        self.build_scene_tlas_from(tlas_id, blases, instances, false)
    }

    /// `build_scene_tlas` with the vertex upload policy explicit. With
    /// `import_host` every page-aligned slice (`can_import_host`; its last
    /// page mapped to the end) is bound in place — no copy, the GPU reads
    /// the caller's pages through unified memory — and only has to outlive
    /// this call (the builds are waited on). Other slices are copied.
    pub fn build_scene_tlas_from(&self, tlas_id: ResourceId, blases: &[&[f32]],
                                 instances: &[SceneInstance], import_host: bool) -> Result<(), String> {
        let asd = self.as_device.as_ref()
            .ok_or_else(|| "ray-query not available on this device".to_string())?;
        if blases.is_empty() {
            return Err("build_scene_tlas: need at least one BLAS".into());
        }
        for (i, v) in blases.iter().enumerate() {
            if v.len() % 9 != 0 || v.is_empty() {
                return Err(format!("BLAS {i}: vertices must be a non-empty multiple of 9 floats (3 verts/tri)"));
            }
        }
        for (i, inst) in instances.iter().enumerate() {
            if inst.blas as usize >= blases.len() {
                return Err(format!("instance {i} references BLAS {} of {}", inst.blas, blases.len()));
            }
        }
        let owned: Vec<(vk::Buffer, vk::DeviceMemory)> = Vec::new();

        // Scratch alignment from the AS properties.
        let mut as_props = vk::PhysicalDeviceAccelerationStructurePropertiesKHR::default();
        let mut p2 = vk::PhysicalDeviceProperties2::default().push_next(&mut as_props);
        unsafe { self.instance.get_physical_device_properties2(self.physical, &mut p2) };
        let scratch_align =
            as_props.min_acceleration_structure_scratch_offset_alignment.max(256) as u64;

        let mut owned_all = owned;
        let mut transient = Vec::new();
        let mut blas_handles: Vec<vk::AccelerationStructureKHR> = Vec::with_capacity(blases.len());
        let mut blas_addrs: Vec<u64> = Vec::with_capacity(blases.len());
        let stats = unsafe { self.build_blases(asd, blases, scratch_align, &mut owned_all, &mut transient, &mut blas_handles, &mut blas_addrs, None, import_host)? };
        // Synchronous builds: the inputs are done with once build_blases returns.
        unsafe { self.free_buffers(transient) };
        let accel = MvkAccel { tlas: vk::AccelerationStructureKHR::null(), blases: blas_handles, blas_addrs, owned: owned_all, tlas_owned: None, tlas_spare: None, pending: Vec::new(), tlas_pending: None, inst_shadow: Vec::new(), inst_log: Vec::new(), inst_gen: 0 };
        self.accels.lock().unwrap().insert(tlas_id.raw(), accel);
        eprintln!("build_scene_tlas: {stats}");
        self.rebuild_scene_tlas(tlas_id, instances)
    }

    /// Rebuild ONLY the top-level structure of scene `tlas_id` over a new
    /// instance set (the BLASes stay): distance-LOD re-instancing as the
    /// camera moves. The previous TLAS and its buffers are destroyed first —
    /// frames are synchronous (the previous frame's fence was waited on), so
    /// nothing in flight references it. Through the MoltenVK fork the freed
    /// TLAS slot is reused by the new one (free-list), so BLAS slot indices
    /// stay valid.
    /// Build BLASes for `blases` (batched submits), appending handles,
    /// addresses and owned buffers. Returns a stats line.
    /// `owned` receives the BLAS storage (lives with the scene); `transient`
    /// receives the build-only buffers (vertex input, scratch), which the
    /// caller frees once the builds have completed. With `import_host` a
    /// page-aligned vertex slice is bound in place (no copy); anything else
    /// is copied into a fresh host-visible buffer.
    unsafe fn build_blases(&self, asd: &ash::khr::acceleration_structure::Device, blases: &[&[f32]], scratch_align: u64,
                           owned: &mut Vec<(vk::Buffer, vk::DeviceMemory)>,
                           transient: &mut Vec<(vk::Buffer, vk::DeviceMemory)>,
                           blas_handles: &mut Vec<vk::AccelerationStructureKHR>, blas_addrs: &mut Vec<u64>,
                           fences: Option<&mut Vec<(vk::Fence, vk::CommandBuffer)>>,
                           import_host: bool) -> Result<String, String> {
        let mut fences = fences;
        let mut imported = 0usize;
        let align_up = |a: u64, al: u64| (a + al - 1) & !(al - 1);
        let mut blas_bytes_total = 0u64;
        let mut vbytes_total = 0u64;
        let mut tri_total = 0u64;
        // Builds are recorded in BATCHES of one command buffer + one fence
        // wait each: a per-BLAS submit costs ~6 ms of round trip, which at
        // a thousand BLASes (per-cell building LOD) was 8 s of startup.
        // The geometry structs the build infos point into live in `geos`
        // (pre-sized: no reallocation moves them under the pointers).
        const BATCH: usize = 32;
        let mut geos: Vec<[vk::AccelerationStructureGeometryKHR<'_>; 1]> = Vec::with_capacity(blases.len());
        let mut pending: Vec<(vk::AccelerationStructureBuildGeometryInfoKHR<'_>, vk::AccelerationStructureBuildRangeInfoKHR)> = Vec::with_capacity(BATCH);
        for vertices in blases {
            let tri_count = (vertices.len() / 9) as u32;
            let vert_count = (vertices.len() / 3) as u32;
            let vbytes = (vertices.len() * 4) as u64;
            vbytes_total += vbytes;
            tri_total += tri_count as u64;
            let vusage = vk::BufferUsageFlags::ACCELERATION_STRUCTURE_BUILD_INPUT_READ_ONLY_KHR;
            let vptr = vertices.as_ptr() as *const u8;
            let vbuf = if import_host && self.can_import_host(vptr, vbytes) {
                let (vbuf, vmem) = self.make_imported_buffer(vptr, vbytes, vusage)?;
                transient.push((vbuf, vmem));
                imported += 1;
                vbuf
            } else {
                let (vbuf, vmem, vmap) = self.make_as_buffer(vbytes, vusage, true)?;
                transient.push((vbuf, vmem));
                std::ptr::copy_nonoverlapping(vptr, vmap, vbytes as usize);
                vbuf
            };
            let vaddr = self.buffer_address(vbuf);

            let tri = vk::AccelerationStructureGeometryTrianglesDataKHR::default()
                .vertex_format(vk::Format::R32G32B32_SFLOAT)
                .vertex_data(vk::DeviceOrHostAddressConstKHR { device_address: vaddr })
                .vertex_stride(12)
                .max_vertex(vert_count - 1)
                .index_type(vk::IndexType::NONE_KHR);
            let bgeo = vk::AccelerationStructureGeometryKHR::default()
                .geometry_type(vk::GeometryTypeKHR::TRIANGLES)
                .flags(vk::GeometryFlagsKHR::OPAQUE)
                .geometry(vk::AccelerationStructureGeometryDataKHR { triangles: tri });
            geos.push([bgeo]);
            // Detached reference into the pre-sized Vec (its storage never
            // moves) so later pushes don't conflict with the borrow the
            // build info holds.
            let bgeos: &[vk::AccelerationStructureGeometryKHR<'_>; 1] = &*geos.as_ptr().add(geos.len() - 1);
            let mut bbi = vk::AccelerationStructureBuildGeometryInfoKHR::default()
                .ty(vk::AccelerationStructureTypeKHR::BOTTOM_LEVEL)
                .flags(vk::BuildAccelerationStructureFlagsKHR::PREFER_FAST_TRACE)
                .mode(vk::BuildAccelerationStructureModeKHR::BUILD)
                .geometries(bgeos);
            let mut bsz = vk::AccelerationStructureBuildSizesInfoKHR::default();
            asd.get_acceleration_structure_build_sizes(
                vk::AccelerationStructureBuildTypeKHR::DEVICE, &bbi, &[tri_count], &mut bsz);
            blas_bytes_total += bsz.acceleration_structure_size;

            let (blas_buf, blas_mem, _) = self.make_as_buffer(bsz.acceleration_structure_size,
                vk::BufferUsageFlags::ACCELERATION_STRUCTURE_STORAGE_KHR, false)?;
            owned.push((blas_buf, blas_mem));
            let (bscratch, bscratch_mem, _) = self.make_as_buffer(
                bsz.build_scratch_size + scratch_align,
                vk::BufferUsageFlags::STORAGE_BUFFER, true)?;
            transient.push((bscratch, bscratch_mem));
            let blas = asd.create_acceleration_structure(
                &vk::AccelerationStructureCreateInfoKHR::default()
                    .buffer(blas_buf).size(bsz.acceleration_structure_size)
                    .ty(vk::AccelerationStructureTypeKHR::BOTTOM_LEVEL), None)
                .map_err(|e| format!("create BLAS: {e:?}"))?;
            bbi = bbi.dst_acceleration_structure(blas)
                .scratch_data(vk::DeviceOrHostAddressKHR {
                    device_address: align_up(self.buffer_address(bscratch), scratch_align),
                });
            let brange = vk::AccelerationStructureBuildRangeInfoKHR::default()
                .primitive_count(tri_count);
            pending.push((bbi, brange));
            if pending.len() >= BATCH {
                let rec = |cb: vk::CommandBuffer| {
                    for (b, r) in &pending {
                        asd.cmd_build_acceleration_structures(cb, &[*b], &[&[*r]]);
                    }
                };
                match fences.as_deref_mut() {
                    Some(f) => f.push(self.submit_no_wait(rec)?),
                    None => self.one_time_submit(rec)?,
                }
                pending.clear();
            }
            blas_handles.push(blas);
            blas_addrs.push(asd.get_acceleration_structure_device_address(
                &vk::AccelerationStructureDeviceAddressInfoKHR::default()
                    .acceleration_structure(blas)));
        }
        if !pending.is_empty() {
            let rec = |cb: vk::CommandBuffer| {
                for (b, r) in &pending {
                    asd.cmd_build_acceleration_structures(cb, &[*b], &[&[*r]]);
                }
            };
            match fences.as_deref_mut() {
                Some(f) => f.push(self.submit_no_wait(rec)?),
                None => self.one_time_submit(rec)?,
            }
            pending.clear();
        }
        drop(geos);
        Ok(format!("{} BLAS ({} tris), {:.1} MB verts ({imported} bound in place) (BLAS {:.2} MB)", blases.len(), tri_total, vbytes_total as f64 / 1e6, blas_bytes_total as f64 / 1e6))
    }

    /// Append BLASes to an existing scene (streaming / lazy detail): builds
    /// them and returns the index of the first new BLAS. The TLAS is not
    /// rebuilt here — call `rebuild_scene_tlas` with instances that reference
    /// the new indices.
    pub fn add_scene_blases(&self, tlas_id: ResourceId, blases: &[&[f32]]) -> Result<u32, String> {
        let asd = self.as_device.as_ref()
            .ok_or_else(|| "ray-query not available on this device".to_string())?;
        for (i, v) in blases.iter().enumerate() {
            if v.len() % 9 != 0 || v.is_empty() {
                return Err(format!("add_scene_blases: BLAS {i} vertex data is not whole triangles"));
            }
        }
        let mut as_props = vk::PhysicalDeviceAccelerationStructurePropertiesKHR::default();
        let mut p2 = vk::PhysicalDeviceProperties2::default().push_next(&mut as_props);
        unsafe { self.instance.get_physical_device_properties2(self.physical, &mut p2) };
        let scratch_align = as_props.min_acceleration_structure_scratch_offset_alignment.max(256) as u64;
        let mut accels = self.accels.lock().unwrap();
        let acc = accels.get_mut(&tlas_id.raw()).ok_or_else(|| "add_scene_blases: unknown scene".to_string())?;
        let first = acc.blases.len() as u32;
        let (mut owned, mut handles, mut addrs) = (std::mem::take(&mut acc.owned), std::mem::take(&mut acc.blases), std::mem::take(&mut acc.blas_addrs));
        let mut transient = Vec::new();
        let res = unsafe { self.build_blases(asd, blases, scratch_align, &mut owned, &mut transient, &mut handles, &mut addrs, None, false) };
        unsafe { self.free_buffers(transient) };
        acc.owned = owned; acc.blases = handles; acc.blas_addrs = addrs;
        res.map(|_| first)
    }

    /// Like `add_scene_blases` but returns as soon as the builds are
    /// submitted: the GPU builds them while frames render. The new indices
    /// must not be instanced until `poll_scene_blases` reports them done.
    pub fn add_scene_blases_async(&self, tlas_id: ResourceId, blases: &[&[f32]]) -> Result<u32, String> {
        self.add_scene_blases_async_from(tlas_id, blases, false)
    }

    /// `add_scene_blases_async` with the upload policy explicit. With
    /// `import_host`, a page-aligned slice (last page mapped to its end) is
    /// bound in place and MUST stay valid and unchanged until
    /// `poll_scene_blases` has reported this call's first index done (the
    /// GPU reads it meanwhile).
    pub fn add_scene_blases_async_from(&self, tlas_id: ResourceId, blases: &[&[f32]], import_host: bool) -> Result<u32, String> {
        let asd = self.as_device.as_ref()
            .ok_or_else(|| "ray-query not available on this device".to_string())?;
        for (i, v) in blases.iter().enumerate() {
            if v.len() % 9 != 0 || v.is_empty() {
                return Err(format!("add_scene_blases_async: BLAS {i} vertex data is not whole triangles"));
            }
        }
        let mut as_props = vk::PhysicalDeviceAccelerationStructurePropertiesKHR::default();
        let mut p2 = vk::PhysicalDeviceProperties2::default().push_next(&mut as_props);
        unsafe { self.instance.get_physical_device_properties2(self.physical, &mut p2) };
        let scratch_align = as_props.min_acceleration_structure_scratch_offset_alignment.max(256) as u64;
        let mut accels = self.accels.lock().unwrap();
        let acc = accels.get_mut(&tlas_id.raw()).ok_or_else(|| "add_scene_blases_async: unknown scene".to_string())?;
        let first = acc.blases.len() as u32;
        let (mut owned, mut handles, mut addrs) = (std::mem::take(&mut acc.owned), std::mem::take(&mut acc.blases), std::mem::take(&mut acc.blas_addrs));
        let mut fences = Vec::new();
        let mut transient = Vec::new();
        let res = unsafe { self.build_blases(asd, blases, scratch_align, &mut owned, &mut transient, &mut handles, &mut addrs, Some(&mut fences), import_host) };
        acc.owned = owned; acc.blases = handles; acc.blas_addrs = addrs;
        // The inputs go with the LAST batch: one queue, in-order fences, so
        // when it signals every earlier batch of this call is done too.
        let n = fences.len();
        for (i, (f, cb)) in fences.into_iter().enumerate() {
            let t = if i + 1 == n { std::mem::take(&mut transient) } else { Vec::new() };
            acc.pending.push(PendingBuild { fence: f, cb, first, count: blases.len() as u32, transient: t });
        }
        if !transient.is_empty() { unsafe { self.free_buffers(transient) }; }
        res.map(|_| first)
    }

    /// Poll in-flight async BLAS builds; returns the first index of every
    /// batch that has completed (its BLASes may now be instanced).
    pub fn poll_scene_blases(&self, tlas_id: ResourceId) -> Result<Vec<u32>, String> {
        let mut accels = self.accels.lock().unwrap();
        let acc = accels.get_mut(&tlas_id.raw()).ok_or_else(|| "poll_scene_blases: unknown scene".to_string())?;
        let dev = &self.device;
        let mut done = Vec::new();
        let mut keep = Vec::with_capacity(acc.pending.len());
        for pb in acc.pending.drain(..) {
            let signalled = unsafe { dev.get_fence_status(pb.fence) }.map_err(|e| format!("fence status: {e:?}"))?;
            if signalled {
                unsafe {
                    dev.destroy_fence(pb.fence, None);
                    dev.free_command_buffers(self.build_pool, &[pb.cb]);
                    self.free_buffers(pb.transient);
                }
                done.push(pb.first);
            } else {
                keep.push(pb);
            }
        }
        acc.pending = keep;
        Ok(done)
    }

    /// Rebuild the scene TLAS over the stored BLASes for a new instance
    /// list (the BLASes persist; only the TLAS and its instance buffer are
    /// replaced) and WAIT for it. Used at startup; frames use
    /// `rebuild_scene_tlas_async` + `poll_scene_tlas`.
    pub fn rebuild_scene_tlas(&self, tlas_id: ResourceId, instances: &[SceneInstance]) -> Result<(), String> {
        self.set_scene_instances(tlas_id, instances)?;
        self.rebuild_scene_tlas_slots(tlas_id)
    }

    /// Replace the scene's whole instance array (slot i = `instances[i]`).
    /// Nothing is built: follow with `rebuild_scene_tlas_slots[_async]`.
    pub fn set_scene_instances(&self, tlas_id: ResourceId, instances: &[SceneInstance]) -> Result<(), String> {
        let mut accels = self.accels.lock().unwrap();
        let a = accels.get_mut(&tlas_id.raw()).ok_or_else(|| format!("scene {tlas_id} not built"))?;
        Self::check_instances(a, instances)?;
        a.inst_shadow.clear();
        a.inst_shadow.resize(instances.len() * 64, 0);
        for (i, inst) in instances.iter().enumerate() {
            Self::encode_instance(&mut a.inst_shadow[i * 64..i * 64 + 64], inst, &a.blas_addrs);
        }
        a.inst_log.clear();
        a.inst_gen += 1;
        Ok(())
    }

    /// Patch one slot of the scene's instance array (LOD switch, streamed
    /// cell): only this slot is re-copied at the next build. Nothing is
    /// built until `rebuild_scene_tlas_slots[_async]`.
    pub fn update_scene_instance(&self, tlas_id: ResourceId, slot: u32, inst: &SceneInstance) -> Result<(), String> {
        let mut accels = self.accels.lock().unwrap();
        let a = accels.get_mut(&tlas_id.raw()).ok_or_else(|| format!("scene {tlas_id} not built"))?;
        Self::check_instances(a, std::slice::from_ref(inst))?;
        let n = a.inst_shadow.len() / 64;
        if slot as usize >= n {
            return Err(format!("update_scene_instance: slot {slot} of {n}"));
        }
        let o = slot as usize * 64;
        Self::encode_instance(&mut a.inst_shadow[o..o + 64], inst, &a.blas_addrs);
        a.inst_log.push(slot);
        Ok(())
    }

    /// Number of instance slots in the scene's array.
    pub fn scene_instance_count(&self, tlas_id: ResourceId) -> usize {
        self.accels.lock().unwrap().get(&tlas_id.raw()).map_or(0, |a| a.inst_shadow.len() / 64)
    }

    /// One Vulkan instance record (VkAccelerationStructureInstanceKHR).
    fn encode_instance(dst: &mut [u8], inst: &SceneInstance, blas_addrs: &[u64]) {
        // transform: 12 f32 row-major 3x4
        for (k, v) in inst.transform.iter().enumerate() { dst[k * 4..k * 4 + 4].copy_from_slice(&v.to_le_bytes()); }
        // instanceCustomIndex(24) | mask(8=0xFF)
        dst[48..52].copy_from_slice(&((inst.custom_index & 0xFFFFFF) | (0xFFu32 << 24)).to_le_bytes());
        // sbtOffset(24)=0 | flags(8)=0
        dst[52..56].copy_from_slice(&0u32.to_le_bytes());
        // accelerationStructureReference = the instanced BLAS
        dst[56..64].copy_from_slice(&blas_addrs[inst.blas as usize].to_le_bytes());
    }

    /// Drop the head of the change log every live buffer set has applied.
    fn compact_inst_log(a: &mut MvkAccel) {
        let mut sets: Vec<&mut TlasBufs> = Vec::new();
        if let Some(b) = a.tlas_owned.as_mut() { sets.push(b); }
        if let Some(b) = a.tlas_spare.as_mut() { sets.push(b); }
        if let Some(p) = a.tlas_pending.as_mut() { sets.push(&mut p.owned); }
        let gen = a.inst_gen;
        let min = sets.iter().filter(|b| b.gen == gen).map(|b| b.synced).min().unwrap_or(a.inst_log.len());
        let min = min.min(a.inst_log.len());
        if min == 0 { return; }
        a.inst_log.drain(..min);
        for b in sets { if b.gen == gen { b.synced -= min; } }
    }

    /// Rebuild the scene TLAS over the instance array as it stands
    /// (`set_scene_instances` / `update_scene_instance`) and WAIT for it.
    pub fn rebuild_scene_tlas_slots(&self, tlas_id: ResourceId) -> Result<(), String> {
        let asd = self.as_device.as_ref()
            .ok_or_else(|| "ray-query not available on this device".to_string())?;
        let (old_tlas, old_owned) = {
            let mut accels = self.accels.lock().unwrap();
            let a = accels.get_mut(&tlas_id.raw()).ok_or_else(|| format!("scene {tlas_id} not built"))?;
            if let Some(p) = a.tlas_pending.take() {
                // An async rebuild is in flight: let it finish, then discard it.
                unsafe {
                    let _ = self.device.wait_for_fences(&[p.fence], true, Self::wait_timeout_ns(30_000));
                    self.device.destroy_fence(p.fence, None);
                    self.device.free_command_buffers(self.build_pool, &[p.cb]);
                    asd.destroy_acceleration_structure(p.tlas, None);
                    self.free_buffers(p.owned.pairs().to_vec());
                }
            }
            let old = std::mem::replace(&mut a.tlas, vk::AccelerationStructureKHR::null());
            (old, a.tlas_owned.take())
        };
        unsafe {
            // Frames are synchronous (the previous frame's fence was waited
            // on), so nothing in flight references the old TLAS. Its
            // buffers become the spare set for this build.
            if old_tlas != vk::AccelerationStructureKHR::null() {
                asd.destroy_acceleration_structure(old_tlas, None);
            }
            // The lock is held through the (waited) submit: nothing else
            // touches the scene meanwhile, and the instance array is not
            // cloned.
            let mut accels = self.accels.lock().unwrap();
            let a = accels.get_mut(&tlas_id.raw()).unwrap();
            if let Some(o) = old_owned {
                if let Some(s) = a.tlas_spare.replace(o) { self.free_buffers(s.pairs().to_vec()); }
            }
            let spare = a.tlas_spare.take();
            let (tlas, owned, fence_cb, size) = self.submit_tlas_build(asd, &a.inst_shadow, a.inst_gen, &a.inst_log, spare, true)?;
            debug_assert!(fence_cb.is_none());
            a.tlas = tlas;
            a.tlas_owned = Some(owned);
            Self::compact_inst_log(a);
            log::debug!("rebuild_scene_tlas: {} instances, TLAS {:.2} MB", a.inst_shadow.len() / 64, size as f64 / 1e6);
        }
        Ok(())
    }

    /// Submit a TLAS rebuild WITHOUT waiting: the new TLAS is built while
    /// frames keep tracing the current one, and `poll_scene_tlas` swaps it
    /// in once the fence signals. Returns `Ok(false)` (nothing submitted)
    /// while a previous async rebuild is still in flight — the caller keeps
    /// its instance list dirty and retries after a poll.
    pub fn rebuild_scene_tlas_async(&self, tlas_id: ResourceId, instances: &[SceneInstance]) -> Result<bool, String> {
        if self.accels.lock().unwrap().get(&tlas_id.raw()).map_or(false, |a| a.tlas_pending.is_some()) {
            return Ok(false);
        }
        self.set_scene_instances(tlas_id, instances)?;
        self.rebuild_scene_tlas_slots_async(tlas_id)
    }

    /// `rebuild_scene_tlas_slots` without waiting (see
    /// `rebuild_scene_tlas_async` for the protocol): the build reads the
    /// instance array as patched so far; later patches go to the next
    /// build.
    pub fn rebuild_scene_tlas_slots_async(&self, tlas_id: ResourceId) -> Result<bool, String> {
        let asd = self.as_device.as_ref()
            .ok_or_else(|| "ray-query not available on this device".to_string())?;
        let mut accels = self.accels.lock().unwrap();
        let a = accels.get_mut(&tlas_id.raw()).ok_or_else(|| format!("scene {tlas_id} not built"))?;
        if a.tlas_pending.is_some() { return Ok(false); }
        if a.inst_shadow.is_empty() { return Err("rebuild_scene_tlas_slots_async: no instances set".into()); }
        let spare = a.tlas_spare.take();
        let (tlas, owned, fence_cb, size) = unsafe { self.submit_tlas_build(asd, &a.inst_shadow, a.inst_gen, &a.inst_log, spare, false)? };
        let (fence, cb) = fence_cb.expect("async build returns its fence");
        a.tlas_pending = Some(PendingTlas { fence, cb, tlas, owned, size, instances: (a.inst_shadow.len() / 64) as u32 });
        Self::compact_inst_log(a);
        Ok(true)
    }

    /// Poll the in-flight async TLAS rebuild of scene `tlas_id`: when its
    /// fence has signalled, the new TLAS replaces the current one (which is
    /// destroyed — frames are synchronous, nothing references it) and
    /// `Ok(true)` is returned. `Ok(false)` = nothing swapped (none in
    /// flight, or still building).
    pub fn poll_scene_tlas(&self, tlas_id: ResourceId) -> Result<bool, String> {
        let asd = self.as_device.as_ref()
            .ok_or_else(|| "ray-query not available on this device".to_string())?;
        let mut accels = self.accels.lock().unwrap();
        let a = accels.get_mut(&tlas_id.raw()).ok_or_else(|| format!("scene {tlas_id} not built"))?;
        let Some(p) = a.tlas_pending.as_ref() else { return Ok(false) };
        let signalled = unsafe { self.device.get_fence_status(p.fence) }.map_err(|e| format!("TLAS fence status: {e:?}"))?;
        if !signalled { return Ok(false); }
        let p = a.tlas_pending.take().unwrap();
        let old = std::mem::replace(&mut a.tlas, p.tlas);
        let old_owned = std::mem::replace(&mut a.tlas_owned, Some(p.owned));
        unsafe {
            self.device.destroy_fence(p.fence, None);
            self.device.free_command_buffers(self.build_pool, &[p.cb]);
            if old != vk::AccelerationStructureKHR::null() {
                asd.destroy_acceleration_structure(old, None);
            }
            if let Some(o) = old_owned {
                if let Some(s) = a.tlas_spare.replace(o) { self.free_buffers(s.pairs().to_vec()); }
            }
        }
        Self::compact_inst_log(a);
        log::debug!("poll_scene_tlas: swapped in {} instances, TLAS {:.2} MB", p.instances, p.size as f64 / 1e6);
        Ok(true)
    }

    fn check_instances(a: &MvkAccel, instances: &[SceneInstance]) -> Result<(), String> {
        for inst in instances {
            if inst.blas as usize >= a.blas_addrs.len() {
                return Err(format!("instance references BLAS {} of {}", inst.blas, a.blas_addrs.len()));
            }
        }
        Ok(())
    }

    /// Encode + submit one TLAS build over `instances` (BLAS references
    /// resolved through `blas_addrs`). `spare` = a retired buffer set to
    /// reuse when it fits (else it is freed and a new set allocated).
    /// `wait` = block on the fence (nothing returned to free); otherwise
    /// the fence + command buffer come back for the caller to poll.
    /// Returns the new TLAS, the buffer set backing it and its storage size.
    unsafe fn submit_tlas_build(&self, asd: &ash::khr::acceleration_structure::Device,
                                shadow: &[u8], gen: u64, log: &[u32], spare: Option<TlasBufs>, wait: bool)
        -> Result<(vk::AccelerationStructureKHR, TlasBufs, Option<(vk::Fence, vk::CommandBuffer)>, u64), String>
    {
        let mut as_props = vk::PhysicalDeviceAccelerationStructurePropertiesKHR::default();
        let mut p2 = vk::PhysicalDeviceProperties2::default().push_next(&mut as_props);
        self.instance.get_physical_device_properties2(self.physical, &mut p2);
        let scratch_align =
            as_props.min_acceleration_structure_scratch_offset_alignment.max(256) as u64;
        let align_up = |a: u64, al: u64| (a + al - 1) & !(al - 1);
        let inst_count = (shadow.len() / 64) as u32;
        let inst_bytes = (shadow.len() as u64).max(64);

        // ---- sizes (the instance address only affects the build, not the
        // sizes, so query with a null address first) ----
        // PREFER_FAST_BUILD for the TLAS (not FAST_TRACE). Two distinct hang
        // causes were found and separated: (1) the deterministic ~32k-65k band
        // hang was a buffer-overflow BUG (UserID instance-descriptor stride,
        // fixed in MoltenVK MVKCmdAccelerationStructure) — flag-independent;
        // (2) an INTERMITTENT hang at extreme counts (1M ~1/3) is the FAST_TRACE
        // optimization itself — its heavier SAH build (~50 ms vs ~30 ms at 1M)
        // stays marginal against the GPU watchdog. FAST_BUILD is reliable across
        // 33k..1M (5/5 at 1M) and, since instance traversal is already near-free,
        // costs nothing at render time. So FAST_BUILD is the default.
        let geo_with = |addr: u64| vk::AccelerationStructureGeometryKHR::default()
            .geometry_type(vk::GeometryTypeKHR::INSTANCES)
            .flags(vk::GeometryFlagsKHR::OPAQUE)
            .geometry(vk::AccelerationStructureGeometryDataKHR {
                instances: vk::AccelerationStructureGeometryInstancesDataKHR::default()
                    .data(vk::DeviceOrHostAddressConstKHR { device_address: addr }),
            });
        let flags = vk::BuildAccelerationStructureFlagsKHR::PREFER_FAST_BUILD;
        let tgeos0 = [geo_with(0)];
        let tbi0 = vk::AccelerationStructureBuildGeometryInfoKHR::default()
            .ty(vk::AccelerationStructureTypeKHR::TOP_LEVEL)
            .flags(flags)
            .mode(vk::BuildAccelerationStructureModeKHR::BUILD)
            .geometries(&tgeos0);
        let mut tsz = vk::AccelerationStructureBuildSizesInfoKHR::default();
        asd.get_acceleration_structure_build_sizes(
            vk::AccelerationStructureBuildTypeKHR::DEVICE, &tbi0, &[inst_count], &mut tsz);
        let store_bytes = tsz.acceleration_structure_size;
        let scratch_bytes = tsz.build_scratch_size + scratch_align;

        // ---- buffers: the spare set when it fits, else a fresh one ----
        let bufs = match spare {
            Some(sp) if sp.fits(inst_bytes, store_bytes, scratch_bytes) => sp,
            other => {
                if let Some(sp) = other { self.free_buffers(sp.pairs().to_vec()); }
                let (inst, inst_mem, inst_map) = self.make_as_buffer(inst_bytes,
                    vk::BufferUsageFlags::ACCELERATION_STRUCTURE_BUILD_INPUT_READ_ONLY_KHR, true)?;
                let (store, store_mem, _) = self.make_as_buffer(store_bytes,
                    vk::BufferUsageFlags::ACCELERATION_STRUCTURE_STORAGE_KHR, false)?;
                let (scratch, scratch_mem, _) = self.make_as_buffer(scratch_bytes,
                    vk::BufferUsageFlags::STORAGE_BUFFER, true)?;
                TlasBufs { inst, inst_mem, inst_map, inst_cap: inst_bytes,
                           store, store_mem, store_cap: store_bytes,
                           scratch, scratch_mem, scratch_cap: scratch_bytes, gen: 0, synced: 0 }
            }
        };
        let mut bufs = bufs;

        // ---- instance array: bring this set's mapped copy up to date ----
        // (host-coherent: visible to the build on submit). Another
        // generation or a fresh set = the whole array; otherwise only the
        // slots logged since this set was last synced.
        if bufs.gen != gen {
            std::ptr::copy_nonoverlapping(shadow.as_ptr(), bufs.inst_map, shadow.len());
        } else {
            for &slot in &log[bufs.synced.min(log.len())..] {
                let o = slot as usize * 64;
                std::ptr::copy_nonoverlapping(shadow.as_ptr().add(o), bufs.inst_map.add(o), 64);
            }
        }
        bufs.gen = gen;
        bufs.synced = log.len();
        let iaddr = self.buffer_address(bufs.inst);

        // ---- TLAS + build ----
        let tlas = asd.create_acceleration_structure(
            &vk::AccelerationStructureCreateInfoKHR::default()
                .buffer(bufs.store).size(store_bytes)
                .ty(vk::AccelerationStructureTypeKHR::TOP_LEVEL), None)
            .map_err(|e| format!("create TLAS: {e:?}"))?;
        let tgeos = [geo_with(iaddr)];
        let tbi = vk::AccelerationStructureBuildGeometryInfoKHR::default()
            .ty(vk::AccelerationStructureTypeKHR::TOP_LEVEL)
            .flags(flags)
            .mode(vk::BuildAccelerationStructureModeKHR::BUILD)
            .geometries(&tgeos)
            .dst_acceleration_structure(tlas)
            .scratch_data(vk::DeviceOrHostAddressKHR {
                device_address: align_up(self.buffer_address(bufs.scratch), scratch_align),
            });
        let trange = vk::AccelerationStructureBuildRangeInfoKHR::default()
            .primitive_count(inst_count);
        let rec = |cb: vk::CommandBuffer| {
            asd.cmd_build_acceleration_structures(cb, &[tbi], &[&[trange]]);
        };
        let fence_cb = if wait { self.one_time_submit(rec)?; None } else { Some(self.submit_no_wait(rec)?) };
        Ok((tlas, bufs, fence_cb, store_bytes))
    }

    /// Stage an acceleration-structure binding for the next `Dispatch`.
    pub fn bind_compute_accel(&self, binding: u32, accel_id: ResourceId) {
        self.compute_accel_binds.lock().unwrap().insert(binding, accel_id.raw());
    }

    /// Create a compute pipeline from SPIR-V (the engine-bundle
    /// path). Descriptor interface: `ssbo_count` storage buffers at
    /// set 0, bindings 0..N; `push_size` bytes of push constants.
    /// Eager (no format dependency, unlike graphics pipelines).
    pub fn create_compute_pipeline(
        &self,
        pipeline_id: ResourceId,
        cs_spirv: &[u8],
        ssbo_count: u32,
        push_size: u32,
    ) -> Result<(), String> {
        self.create_compute_pipeline_rt(pipeline_id, cs_spirv, ssbo_count, push_size, &[])
    }

    /// Like [`create_compute_pipeline`] but `as_bindings` lists the binding
    /// indices (within `0..ssbo_count`) that are acceleration structures
    /// (VK_DESCRIPTOR_TYPE_ACCELERATION_STRUCTURE_KHR) for an inline-ray_query
    /// kernel; the rest stay storage buffers.
    pub fn create_compute_pipeline_rt(
        &self,
        pipeline_id: ResourceId,
        cs_spirv: &[u8],
        ssbo_count: u32,
        push_size: u32,
        as_bindings: &[u32],
    ) -> Result<(), String> {
        if cs_spirv.len() % 4 != 0 {
            return Err("SPIR-V length not word-aligned".into());
        }
        let words: Vec<u32> = cs_spirv.chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        let dev = &self.device;
        unsafe {
            let module = dev.create_shader_module(
                &vk::ShaderModuleCreateInfo::default().code(&words), None)
                .map_err(|e| format!("shader module: {e:?}"))?;
            let bindings: Vec<vk::DescriptorSetLayoutBinding> =
                (0..ssbo_count).map(|b| {
                    let ty = if as_bindings.contains(&b) {
                        vk::DescriptorType::ACCELERATION_STRUCTURE_KHR
                    } else {
                        vk::DescriptorType::STORAGE_BUFFER
                    };
                    vk::DescriptorSetLayoutBinding::default()
                        .binding(b)
                        .descriptor_type(ty)
                        .descriptor_count(1)
                        .stage_flags(vk::ShaderStageFlags::COMPUTE)
                }).collect();
            let dset_layout = dev.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default()
                    .bindings(&bindings), None)
                .map_err(|e| format!("dset layout: {e:?}"))?;
            let pc_ranges = [vk::PushConstantRange::default()
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
                .offset(0)
                .size(push_size.max(4))];
            let set_layouts = [dset_layout];
            let mut li = vk::PipelineLayoutCreateInfo::default()
                .set_layouts(&set_layouts);
            if push_size > 0 {
                li = li.push_constant_ranges(&pc_ranges);
            }
            let layout = dev.create_pipeline_layout(&li, None)
                .map_err(|e| format!("pipeline layout: {e:?}"))?;
            // slangc names the SPIR-V entry "main" regardless of
            // the source function name (the -entry flag selects
            // WHICH function, not its exported name).
            let entry = std::ffi::CStr::from_bytes_with_nul(b"main\0")
                .unwrap();
            let stage = vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::COMPUTE)
                .module(module)
                .name(entry);
            let ci = vk::ComputePipelineCreateInfo::default()
                .stage(stage)
                .layout(layout);
            let pipeline = dev.create_compute_pipelines(
                    vk::PipelineCache::null(), &[ci], None)
                .map_err(|(_, e)| format!("compute pipeline: {e:?}"))?[0];
            self.compute_pipelines.lock().unwrap().insert(
                pipeline_id.raw(),
                MvkComputePipeline {
                    pipeline, layout, dset_layout, module, push_size,
                    ssbo_count,
                    as_bindings: as_bindings.to_vec(),
                });
        }
        Ok(())
    }

    /// Stage a storage-buffer binding for the next `Dispatch`
    /// (mirror of Tier-2's `bind_compute_storage_image` shape).
    pub fn bind_compute_buffer(&self, binding: u32, buffer_id: ResourceId) {
        self.compute_binds.lock().unwrap()
            .insert(binding, buffer_id.raw());
    }

    /// Write bytes into a (host-visible, coherent) guest buffer.
    pub fn buffer_write(&self, buffer_id: ResourceId, offset: u64, data: &[u8])
        -> Result<(), String>
    {
        let buffers = self.buffers.lock().unwrap();
        let b = buffers.get(&buffer_id.raw())
            .ok_or_else(|| format!("buffer {buffer_id} not registered"))?;
        let end = offset + data.len() as u64;
        if end > b.size {
            return Err(format!("write end {end} exceeds size {}", b.size));
        }
        if b.mapped.is_null() {
            return Err("buffer memory not mapped".to_string());
        }
        unsafe {
            std::ptr::copy_nonoverlapping(
                data.as_ptr(), b.mapped.add(offset as usize), data.len());
        }
        Ok(())
    }

    /// Last measured GPU exec time in seconds (0.0 if timestamps are
    /// unsupported or no frame has been timed yet). The measured-truth
    /// input for calibrating the device cost model against real silicon.
    pub fn measured_gpu_time_s(&self) -> f64 {
        self.last_gpu_ns.load(Ordering::Relaxed) as f64 * 1e-9
    }

    /// Per-dispatch GPU time of the last timed frame, seconds, in dispatch
    /// order (empty without timestamps). Dispatch k's span runs from the
    /// previous stamp (frame top for k = 0) to the stamp after it, so the
    /// values sum to the frame's exec time up to the last stamped dispatch.
    pub fn measured_dispatch_times_s(&self) -> Vec<f64> {
        self.last_dispatch_ns.lock().unwrap().iter().map(|&ns| ns as f64 * 1e-9).collect()
    }

    /// Cumulative measured GPU exec time across all timed frames, seconds.
    pub fn total_gpu_time_s(&self) -> f64 {
        self.total_gpu_ns.load(Ordering::Relaxed) as f64 * 1e-9
    }

    /// Find a memory type index satisfying `type_bits` (the
    /// `memoryTypeBits` from a resource's memory requirements) with all
    /// of `flags` set. Returns `None` if no type matches.
    fn mem_type(&self, type_bits: u32, flags: vk::MemoryPropertyFlags) -> Option<u32> {
        (0..self.mem_props.memory_type_count).find(|&i| {
            let supported = type_bits & (1 << i) != 0;
            let has_flags = self.mem_props.memory_types[i as usize]
                .property_flags.contains(flags);
            supported && has_flags
        })
    }

    /// Map a guest `TextureFormat`-as-`VkFormat`-numeric (the value the
    /// daemon passes to `set_image_format`) to an `ash` format. Falls
    /// back to RGBA8_UNORM for the clear+readback slice.
    fn vk_format(numeric: u32) -> vk::Format {
        match numeric {
            37 => vk::Format::R8G8B8A8_UNORM,
            43 => vk::Format::R8G8B8A8_SRGB,
            44 => vk::Format::B8G8R8A8_UNORM,
            50 => vk::Format::B8G8R8A8_SRGB,
            _  => vk::Format::R8G8B8A8_UNORM,
        }
    }

    /// Lazily materialise an image's `VkImage` + backing memory, in
    /// `COLOR_ATTACHMENT | TRANSFER_SRC | TRANSFER_DST` usage. Returns
    /// the `VkImage`, or `None` if allocation failed. Caller holds the
    /// `images` lock.
    fn ensure_image(&self, img: &mut MvkImage) -> Option<vk::Image> {
        if let Some(h) = img.image { return Some(h); }
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(img.format)
            .extent(vk::Extent3D { width: img.width, height: img.height, depth: 1 })
            .mip_levels(1).array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT
                | vk::ImageUsageFlags::TRANSFER_SRC
                | vk::ImageUsageFlags::TRANSFER_DST)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { self.device.create_image(&info, None) }.ok()?;
        let req = unsafe { self.device.get_image_memory_requirements(image) };
        let mt = self.mem_type(req.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size).memory_type_index(mt);
        let memory = unsafe { self.device.allocate_memory(&alloc, None) }.ok()?;
        if unsafe { self.device.bind_image_memory(image, memory, 0) }.is_err() {
            unsafe { self.device.free_memory(memory, None);
                     self.device.destroy_image(image, None); }
            return None;
        }
        img.image = Some(image);
        img.memory = Some(memory);
        Some(image)
    }

    /// How many frames have been submitted to this backend. Diagnostic.
    pub fn submission_count(&self) -> u64 {
        self.submissions.load(Ordering::Relaxed)
    }

    /// Returns the Vulkan device properties (vendor name, device name,
    /// driver version). Diagnostic / smoke-test helper.
    pub fn device_summary(&self) -> String {
        let props = unsafe { self.instance.get_physical_device_properties(self.physical) };
        let name: String = props.device_name_as_c_str()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "<unparseable>".into());
        format!(
            "vendor={:?} api={}.{}.{} device={}",
            self.vendor,
            vk::api_version_major(props.api_version),
            vk::api_version_minor(props.api_version),
            vk::api_version_patch(props.api_version),
            name,
        )
    }
}

/// Timestamp slots after every compute dispatch of a frame (per-pass GPU time).
const DISPATCH_STAMPS: u32 = 64;

impl Drop for MoltenVkBackend {
    fn drop(&mut self) {
        // A wedged queue never goes idle: skip the wait (and the resource
        // teardown that needs it) and let the process reclaim on exit.
        if self.stalled.load(Ordering::Relaxed) {
            log::warn!("MoltenVk drop: backend stalled — skipping device_wait_idle and teardown");
            return;
        }
        // SAFETY: all handles were created via ash; destroy resources
        // before the device, and the device before the instance.
        unsafe {
            let _ = self.device.device_wait_idle();
            for (_, cp) in self.compute_pipelines.lock().unwrap().drain() {
                self.device.destroy_pipeline(cp.pipeline, None);
                self.device.destroy_pipeline_layout(cp.layout, None);
                self.device.destroy_descriptor_set_layout(cp.dset_layout, None);
                self.device.destroy_shader_module(cp.module, None);
            }
            let pool = self.desc_pool.lock().unwrap().0;
            if pool != vk::DescriptorPool::null() {
                self.device.destroy_descriptor_pool(pool, None);
            }
            for (_, img) in self.images.lock().unwrap().drain() {
                if let Some(h) = img.image { self.device.destroy_image(h, None); }
                if let Some(m) = img.memory { self.device.free_memory(m, None); }
            }
            for (_, b) in self.buffers.lock().unwrap().drain() {
                self.device.unmap_memory(b.memory);
                self.device.destroy_buffer(b.buffer, None);
                self.device.free_memory(b.memory, None);
            }
            for (_, p) in self.pipelines.lock().unwrap().drain() {
                if let Some(vk) = p.materialized { self.destroy_pipeline_vk(vk); }
            }
            if self.query_pool != vk::QueryPool::null() {
                self.device.destroy_query_pool(self.query_pool, None);
            }
            if self.build_pool != self.cmd_pool { self.device.destroy_command_pool(self.build_pool, None); }
            self.device.destroy_command_pool(self.cmd_pool, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

impl Backend for MoltenVkBackend {
    fn identity(&self) -> BackendId {
        BackendId::new(self.vendor, self.generation)
    }

    fn caps(&self) -> u64 {
        use aqueduct_gpu::payloads::HandshakeResponse as H;
        // Tier-3 advertises the full grown-up surface: compute,
        // composition, SPIR-V upload (cold path), share-surface.
        // Bundle load lands when the bundle materialisation pipeline
        // ships (Phase 2.x).
        H::CAPS_COMPUTE
            | H::CAPS_COMPOSITION
            | H::CAPS_SHARE_SURFACE
            | H::CAPS_SPIRV_UPLOAD
    }

    fn max_frame_bytes(&self) -> u32 {
        // GPU paths can chew much larger frames than tier-1. Cap at
        // 16 MiB — Vulkan's typical maxPushConstantsSize and
        // maxCommandBuffer constraints aren't hit until well past
        // this.
        16 * (1 << 20)
    }

    fn max_fences_inflight(&self) -> u32 {
        128
    }

    fn allocate_memory(&self, _size: u64, _usage: u8) -> [u8; 32] {
        // Real impl: vkAllocateMemory of a host-visible region, return
        // a token the guest kmod imports. Stub here so handshake-level
        // wiring works.
        let n = self.submissions.fetch_add(0, Ordering::Relaxed);
        let mut tok = [0u8; 32];
        tok[..8].copy_from_slice(&n.to_le_bytes());
        tok[31] = 0xCE; // tier-3 sentinel
        tok
    }

    fn image_created(&self, image_id: ResourceId, width: u32, height: u32) {
        self.images.lock().unwrap().insert(image_id.raw(), MvkImage {
            width, height,
            format: vk::Format::R8G8B8A8_UNORM, // until set_image_format
            image: None, memory: None,
        });
    }

    fn set_image_format(&self, image_id: ResourceId, vk_format: u32) {
        if let Some(img) = self.images.lock().unwrap().get_mut(&image_id.raw()) {
            // Safe to change while the VkImage hasn't been materialised
            // yet (the common order: image_created → set_image_format →
            // first submit). If already created, leave it — re-creation
            // mid-life isn't needed for the clear+readback slice.
            if img.image.is_none() {
                img.format = Self::vk_format(vk_format);
            }
        }
    }

    fn image_destroyed(&self, image_id: ResourceId) {
        if let Some(img) = self.images.lock().unwrap().remove(&image_id.raw()) {
            unsafe {
                if let Some(h) = img.image { self.device.destroy_image(h, None); }
                if let Some(m) = img.memory { self.device.free_memory(m, None); }
            }
        }
    }

    fn buffer_created(&self, buffer_id: ResourceId, size: u64) {
        let size = size.max(1);
        let info = vk::BufferCreateInfo::default()
            .size(size)
            .usage(vk::BufferUsageFlags::TRANSFER_DST
                | vk::BufferUsageFlags::TRANSFER_SRC
                | vk::BufferUsageFlags::STORAGE_BUFFER)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buffer = match unsafe { self.device.create_buffer(&info, None) } {
            Ok(b) => b,
            Err(e) => { log::warn!("MoltenVk buffer_created: create {e:?}"); return; }
        };
        let req = unsafe { self.device.get_buffer_memory_requirements(buffer) };
        let Some(mt) = self.mem_type(req.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT)
        else {
            log::warn!("MoltenVk buffer_created: no host-visible memory type");
            unsafe { self.device.destroy_buffer(buffer, None); }
            return;
        };
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size).memory_type_index(mt);
        let memory = match unsafe { self.device.allocate_memory(&alloc, None) } {
            Ok(m) => m,
            Err(e) => {
                log::warn!("MoltenVk buffer_created: alloc {e:?}");
                unsafe { self.device.destroy_buffer(buffer, None); }
                return;
            }
        };
        unsafe { let _ = self.device.bind_buffer_memory(buffer, memory, 0); }
        let mapped = unsafe {
            self.device.map_memory(memory, 0, req.size, vk::MemoryMapFlags::empty())
        }.map(|p| p as *mut u8).unwrap_or(std::ptr::null_mut());
        self.buffers.lock().unwrap().insert(buffer_id.raw(), MvkBuffer {
            size, buffer, memory, mapped,
        });
    }

    fn buffer_destroyed(&self, buffer_id: ResourceId) {
        if let Some(b) = self.buffers.lock().unwrap().remove(&buffer_id.raw()) {
            unsafe {
                self.device.unmap_memory(b.memory);
                self.device.destroy_buffer(b.buffer, None);
                self.device.free_memory(b.memory, None);
            }
        }
    }

    fn buffer_read_bytes(&self, buffer_id: ResourceId, offset: u64, size: u64)
        -> Result<Vec<u8>, String>
    {
        let buffers = self.buffers.lock().unwrap();
        let b = buffers.get(&buffer_id.raw())
            .ok_or_else(|| format!("buffer {buffer_id} not registered"))?;
        let end = offset.checked_add(size)
            .ok_or_else(|| "offset+size overflow".to_string())?;
        if end > b.size {
            return Err(format!("read end {end} exceeds buffer size {}", b.size));
        }
        if b.mapped.is_null() {
            return Err("buffer memory not mapped".to_string());
        }
        // HOST_COHERENT: device writes are visible post-fence without
        // an explicit invalidate.
        let mut out = vec![0u8; size as usize];
        unsafe {
            std::ptr::copy_nonoverlapping(
                b.mapped.add(offset as usize), out.as_mut_ptr(), size as usize);
        }
        Ok(out)
    }

    /// Tier-3 pipeline-create hook: stash the VS+FS SPIR-V (the real
    /// VkPipeline is built lazily at first draw — see
    /// `create_graphics_pipeline`).
    fn pipeline_created(&self, pipeline_id: ResourceId,
                        vs_spirv: &[u8], fs_spirv: &[u8]) {
        self.create_graphics_pipeline(pipeline_id, vs_spirv, fs_spirv);
    }

    /// Replay the frame's op stream as real Vulkan commands on Metal:
    /// render-pass clear + draws (`vkCmdDraw` via registered pipelines)
    /// + image→buffer readback.
    fn submit_frame(
        &self,
        _fence_id: ResourceId,
        _timeline: u64,
        frame_buf: &[u8],
    ) -> bool {
        self.submissions.fetch_add(1, Ordering::Relaxed);
        let _guard = self.submit_lock.lock().unwrap();
        match self.record_and_submit(frame_buf) {
            Ok(()) => true,
            // A stall is the one failure the caller must see: the frame never
            // completed and the backend should be replaced.
            Err(vk::Result::TIMEOUT) => false,
            Err(e) => { log::warn!("MoltenVk submit_frame: {e:?}"); true }
        }
    }

    fn measured_gpu_time_s(&self) -> Option<f64> {
        match self.last_gpu_ns.load(Ordering::Relaxed) {
            0 => None,
            ns => Some(ns as f64 * 1e-9),
        }
    }
}

impl MoltenVkBackend {
    /// Record + submit one frame's clear/copy ops. Errors are returned
    /// (logged by the caller); the frame is still "consumed".
    fn record_and_submit(&self, frame_buf: &[u8]) -> Result<(), vk::Result> {
        let dev = &self.device;
        let alloc = vk::CommandBufferAllocateInfo::default()
            .command_pool(self.cmd_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let cb = unsafe { dev.allocate_command_buffers(&alloc)? }[0];
        unsafe {
            dev.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT))?;
        }

        // Measured GPU exec time (D-M6): reset the pool + stamp the top of
        // the pipe before any work; stamp the bottom just before close.
        let timing = self.query_pool != vk::QueryPool::null();
        let mut n_stamped = 0u32;
        if timing {
            unsafe {
                dev.cmd_reset_query_pool(cb, self.query_pool, 0, 2 + DISPATCH_STAMPS);
                dev.cmd_write_timestamp(
                    cb, vk::PipelineStageFlags::TOP_OF_PIPE, self.query_pool, 0);
            }
        }

        let mut images = self.images.lock().unwrap();
        let buffers = self.buffers.lock().unwrap();
        let mut pipelines = self.pipelines.lock().unwrap();
        let compute_pipelines = self.compute_pipelines.lock().unwrap();
        // Per-dispatch descriptor sets: size the pool to THIS
        // frame's dispatch count (a multi-step compute frame can
        // record thousands of dispatches; the old fixed 64-set
        // pool silently skipped every dispatch past it). The
        // previous submission has completed by the time we are
        // re-entered, so recreating the pool is safe.
        let n_dispatch = {
            let mut dec = FrameDecoder::new(frame_buf);
            let mut n = 0u32;
            while let Ok(Some((op, _))) = dec.next() {
                if matches!(op, FrameOp::Dispatch) {
                    n += 1;
                }
            }
            n.max(64)
        };
        let desc_pool = {
            let mut pool = self.desc_pool.lock().unwrap();
            if n_dispatch > pool.1 && pool.0 != vk::DescriptorPool::null() {
                unsafe { dev.destroy_descriptor_pool(pool.0, None) };
                // Storage buffers for every binding; plus acceleration-structure
                // descriptors (one per dispatch is plenty for the mesh path) when
                // the device supports ray-query.
                let mut pool_sizes = vec![vk::DescriptorPoolSize::default()
                    .ty(vk::DescriptorType::STORAGE_BUFFER)
                    .descriptor_count(n_dispatch * 8)];
                if self.ray_query {
                    pool_sizes.push(vk::DescriptorPoolSize::default()
                        .ty(vk::DescriptorType::ACCELERATION_STRUCTURE_KHR)
                        .descriptor_count(n_dispatch));
                }
                let dp_info = vk::DescriptorPoolCreateInfo::default()
                    .max_sets(n_dispatch)
                    .pool_sizes(&pool_sizes);
                pool.0 = unsafe { dev.create_descriptor_pool(&dp_info, None) }
                    .unwrap_or(vk::DescriptorPool::null());
                pool.1 = n_dispatch;
            }
            if pool.0 != vk::DescriptorPool::null() {
                unsafe {
                    let _ = dev.reset_descriptor_pool(
                        pool.0, vk::DescriptorPoolResetFlags::empty());
                }
            }
            pool.0
        };
        // Compute state for this frame stream.
        let mut cur_compute: Option<u32> = None;
        let mut push_bytes: Vec<u8> = Vec::new();
        // In-stream storage-buffer bindings (FrameOp::BindDescriptors,
        // dtype 7): binding -> buffer id. Persist across dispatches
        // within the frame, Vulkan-style, until rebound. The legacy
        // out-of-band bind_compute_buffer stash is merged in at each
        // Dispatch (it cannot express per-dispatch sets: a HashMap
        // staged before submit collapses multi-pass frames).
        let mut stream_binds: std::collections::BTreeMap<u32, u32> =
            std::collections::BTreeMap::new();
        // Acceleration-structure bindings persist across dispatches in the frame,
        // mirroring stream_binds (the inline-ray_query TLAS for the mesh path).
        let mut stream_accel_binds: std::collections::BTreeMap<u32, u32> =
            std::collections::BTreeMap::new();

        // Open render pass + the transient objects to destroy post-submit.
        struct Active { rp: vk::RenderPass, fb: vk::Framebuffer,
                        view: vk::ImageView, w: u32, h: u32, format: vk::Format }
        let mut active: Option<Active> = None;
        let mut trash: Vec<(vk::RenderPass, vk::Framebuffer, vk::ImageView)> = Vec::new();
        let mut bound_pipeline: Option<u32> = None;
        // Close the open render pass (if any) and queue its objects.
        macro_rules! end_rp { () => {
            if let Some(a) = active.take() {
                unsafe { dev.cmd_end_render_pass(cb); }
                trash.push((a.rp, a.fb, a.view));
            }
        }}

        let mut dec = FrameDecoder::new(frame_buf);
        while let Ok(Some((op, body))) = dec.next() {
            match op {
                FrameOp::BeginRenderPass => {
                    if body.len() < 8 { continue; }
                    end_rp!();
                    let img_id = u32::from_le_bytes(body[0..4].try_into().unwrap());
                    let flags = if body.len() >= 12 {
                        u32::from_le_bytes(body[8..12].try_into().unwrap())
                    } else { 0 };
                    const NO_CLEAR: u32 = 0x1;
                    let no_clear = flags & NO_CLEAR != 0;
                    let rgba = [body[4], body[5], body[6], body[7]];
                    let Some(img) = images.get_mut(&img_id) else { continue; };
                    let (w, h, format) = (img.width, img.height, img.format);
                    let Some(handle) = self.ensure_image(img) else { continue; };
                    unsafe {
                        let view = dev.create_image_view(&vk::ImageViewCreateInfo::default()
                            .image(handle).view_type(vk::ImageViewType::TYPE_2D).format(format)
                            .subresource_range(vk::ImageSubresourceRange::default()
                                .aspect_mask(vk::ImageAspectFlags::COLOR)
                                .level_count(1).layer_count(1)), None)?;
                        // loadOp CLEAR (initial UNDEFINED) or LOAD-preserve
                        // (initial TRANSFER_SRC, the prior frame's final).
                        let (load_op, initial) = if no_clear {
                            (vk::AttachmentLoadOp::LOAD, vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                        } else {
                            (vk::AttachmentLoadOp::CLEAR, vk::ImageLayout::UNDEFINED)
                        };
                        let attach = [vk::AttachmentDescription::default()
                            .format(format).samples(vk::SampleCountFlags::TYPE_1)
                            .load_op(load_op).store_op(vk::AttachmentStoreOp::STORE)
                            .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
                            .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
                            .initial_layout(initial)
                            .final_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)];
                        let color_ref = [vk::AttachmentReference::default()
                            .attachment(0).layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)];
                        let subpass = [vk::SubpassDescription::default()
                            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
                            .color_attachments(&color_ref)];
                        let rp = dev.create_render_pass(&vk::RenderPassCreateInfo::default()
                            .attachments(&attach).subpasses(&subpass), None)?;
                        let views = [view];
                        let fb = dev.create_framebuffer(&vk::FramebufferCreateInfo::default()
                            .render_pass(rp).attachments(&views)
                            .width(w).height(h).layers(1), None)?;
                        let clear = [vk::ClearValue { color: vk::ClearColorValue {
                            float32: [rgba[0] as f32 / 255.0, rgba[1] as f32 / 255.0,
                                      rgba[2] as f32 / 255.0, rgba[3] as f32 / 255.0] }}];
                        dev.cmd_begin_render_pass(cb, &vk::RenderPassBeginInfo::default()
                            .render_pass(rp).framebuffer(fb)
                            .render_area(vk::Rect2D { offset: vk::Offset2D { x: 0, y: 0 },
                                extent: vk::Extent2D { width: w, height: h } })
                            .clear_values(&clear), vk::SubpassContents::INLINE);
                        active = Some(Active { rp, fb, view, w, h, format });
                    }
                }
                FrameOp::BindPipeline => {
                    if body.len() >= 4 {
                        let pid = u32::from_le_bytes(
                            body[0..4].try_into().unwrap());
                        if compute_pipelines.contains_key(&pid) {
                            cur_compute = Some(pid);
                            continue;
                        }
                    }
                    if body.len() < 4 { continue; }
                    let pid = u32::from_le_bytes(body[0..4].try_into().unwrap());
                    bound_pipeline = Some(pid);
                    let Some(a) = active.as_ref() else { continue; };
                    let (aw, ah, afmt) = (a.w, a.h, a.format);
                    let Some(p) = pipelines.get_mut(&pid) else { continue; };
                    // Lazily materialise the VkPipeline for this render
                    // target's format (rebuild if the format changed).
                    let need = p.materialized.as_ref().map(|m| m.format != afmt).unwrap_or(true);
                    if need {
                        if let Some(old) = p.materialized.take() {
                            unsafe { let _ = dev.device_wait_idle();
                                     self.destroy_pipeline_vk(old); }
                        }
                        match self.materialize_pipeline(&p.vs_spirv, &p.fs_spirv, afmt) {
                            Ok(vk) => p.materialized = Some(vk),
                            Err(e) => { log::warn!("MoltenVk materialize pipeline: {e:?}"); }
                        }
                    }
                    if let Some(m) = p.materialized.as_ref() {
                        unsafe {
                            dev.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, m.pipeline);
                            dev.cmd_set_viewport(cb, 0, &[vk::Viewport {
                                x: 0.0, y: 0.0, width: aw as f32, height: ah as f32,
                                min_depth: 0.0, max_depth: 1.0 }]);
                            dev.cmd_set_scissor(cb, 0, &[vk::Rect2D {
                                offset: vk::Offset2D { x: 0, y: 0 },
                                extent: vk::Extent2D { width: aw, height: ah } }]);
                        }
                    }
                }
                FrameOp::Draw => {
                    if body.len() < 16 || active.is_none() { continue; }
                    let bound_ok = bound_pipeline
                        .map(|p| pipelines.contains_key(&p)).unwrap_or(false);
                    if !bound_ok { continue; }
                    let vcount = u32::from_le_bytes(body[0..4].try_into().unwrap());
                    let icount = u32::from_le_bytes(body[4..8].try_into().unwrap()).max(1);
                    let fvert  = u32::from_le_bytes(body[8..12].try_into().unwrap());
                    let finst  = u32::from_le_bytes(body[12..16].try_into().unwrap());
                    unsafe { dev.cmd_draw(cb, vcount, icount, fvert, finst); }
                }
                FrameOp::EndRenderPass => { end_rp!(); }
                FrameOp::PushConstants => {
                    // 4-byte header (stage/offset/reserved) + payload.
                    if body.len() >= 4 {
                        push_bytes.clear();
                        push_bytes.extend_from_slice(&body[4..]);
                    }
                }
                FrameOp::BindDescriptors => {
                    // Storage-buffer descriptor writes carried in
                    // the frame stream (the multi-dispatch path:
                    // each pass binds before its Dispatch). Body:
                    // { set u32, count u32 } + count x 36-byte
                    // writes { binding, dtype, buffer, image,
                    // sampler, offset u64, range u64 }.
                    if body.len() < 8 { continue; }
                    let count = u32::from_le_bytes(
                        body[4..8].try_into().unwrap()) as usize;
                    for w in 0..count {
                        let off = 8 + w * 36;
                        if off + 36 > body.len() { break; }
                        let at = |o: usize| u32::from_le_bytes(
                            body[off + o..off + o + 4].try_into().unwrap());
                        let binding = at(0);
                        let dtype = at(4);
                        let buffer_id = at(8);
                        // 7 = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER.
                        if dtype == 7 && buffer_id != 0 {
                            stream_binds.insert(binding, buffer_id);
                        }
                    }
                }
                FrameOp::Dispatch => {
                    end_rp!();
                    if body.len() < 12 { continue; }
                    let gx = u32::from_le_bytes(body[0..4].try_into().unwrap());
                    let gy = u32::from_le_bytes(body[4..8].try_into().unwrap());
                    let gz = u32::from_le_bytes(body[8..12].try_into().unwrap());
                    let Some(pid) = cur_compute else { continue; };
                    let Some(cp) = compute_pipelines.get(&pid) else { continue; };
                    if desc_pool == vk::DescriptorPool::null() { continue; }
                    // Merge the legacy out-of-band stash, then build
                    // the descriptor set from the stream bindings.
                    {
                        let mut m = self.compute_binds.lock().unwrap();
                        for (b, id) in m.drain() {
                            stream_binds.insert(b, id);
                        }
                    }
                    let binds: Vec<(u32, u32)> = stream_binds
                        .iter()
                        .map(|(b, id)| (*b, *id))
                        .filter(|(b, _)| *b < cp.ssbo_count)
                        .collect();
                    let set_layouts = [cp.dset_layout];
                    let dset = match unsafe { dev.allocate_descriptor_sets(
                        &vk::DescriptorSetAllocateInfo::default()
                            .descriptor_pool(desc_pool)
                            .set_layouts(&set_layouts)) }
                    {
                        Ok(d) => d[0],
                        Err(e) => {
                            log::warn!("MoltenVk Dispatch: dset alloc {e:?}");
                            continue;
                        }
                    };
                    // Resolve (binding, VkBuffer) pairs first so the
                    // info/write arrays stay aligned even when a
                    // binding references an unknown buffer.
                    let resolved: Vec<(u32, vk::Buffer, u64)> = binds
                        .iter()
                        .filter_map(|(binding, buf_id)| {
                            buffers.get(buf_id).map(|b|
                                (*binding, b.buffer, b.size))
                        })
                        .collect();
                    let infos: Vec<vk::DescriptorBufferInfo> = resolved
                        .iter()
                        .map(|(_, buf, size)|
                            vk::DescriptorBufferInfo::default()
                                .buffer(*buf).offset(0).range(*size))
                        .collect();
                    let writes: Vec<vk::WriteDescriptorSet> = resolved
                        .iter()
                        .zip(infos.iter())
                        .map(|((binding, _, _), info)|
                            vk::WriteDescriptorSet::default()
                                .dst_set(dset)
                                .dst_binding(*binding)
                                .descriptor_type(
                                    vk::DescriptorType::STORAGE_BUFFER)
                                .buffer_info(std::slice::from_ref(info)))
                        .collect();
                    unsafe {
                        dev.update_descriptor_sets(&writes, &[]);
                    }
                    // Acceleration-structure descriptors (inline ray_query). The
                    // accel-bind stash is merged + drained like the buffer binds;
                    // each is written with a chained
                    // VkWriteDescriptorSetAccelerationStructureKHR. N is tiny
                    // (one TLAS per kernel), so one update per binding is fine and
                    // sidesteps push_next aliasing across a write batch.
                    {
                        {
                            let mut am = self.compute_accel_binds.lock().unwrap();
                            for (b, id) in am.drain() { stream_accel_binds.insert(b, id); }
                        }
                        let accels = self.accels.lock().unwrap();
                        for (binding, accel_id) in stream_accel_binds.iter() {
                            if *binding >= cp.ssbo_count
                                || !cp.as_bindings.contains(binding) { continue; }
                            if let Some(acc) = accels.get(accel_id) {
                                let as_handles = [acc.tlas];
                                let mut was =
                                    vk::WriteDescriptorSetAccelerationStructureKHR::default()
                                        .acceleration_structures(&as_handles);
                                let mut w = vk::WriteDescriptorSet::default()
                                    .dst_set(dset)
                                    .dst_binding(*binding)
                                    .descriptor_type(
                                        vk::DescriptorType::ACCELERATION_STRUCTURE_KHR)
                                    .push_next(&mut was);
                                w.descriptor_count = 1;
                                unsafe { dev.update_descriptor_sets(&[w], &[]); }
                            }
                        }
                    }
                    unsafe {
                        dev.cmd_bind_pipeline(cb,
                            vk::PipelineBindPoint::COMPUTE, cp.pipeline);
                        dev.cmd_bind_descriptor_sets(cb,
                            vk::PipelineBindPoint::COMPUTE, cp.layout,
                            0, &[dset], &[]);
                        if cp.push_size > 0 && !push_bytes.is_empty() {
                            let n = push_bytes.len()
                                .min(cp.push_size as usize);
                            dev.cmd_push_constants(cb, cp.layout,
                                vk::ShaderStageFlags::COMPUTE, 0,
                                &push_bytes[..n]);
                        }
                        dev.cmd_dispatch(cb, gx, gy, gz);
                        // Per-dispatch GPU time: a stamp after each dispatch.
                        if timing && n_stamped < DISPATCH_STAMPS {
                            dev.cmd_write_timestamp(cb, vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                                self.query_pool, 2 + n_stamped);
                            n_stamped += 1;
                        }
                        // Make the writes visible to host readback
                        // (HOST_COHERENT memory + queue-wait below)
                        // AND to any chained compute dispatch in
                        // the same frame (multi-kernel pipelines:
                        // G-buffer pass -> resolve pass).
                        let barrier = vk::MemoryBarrier::default()
                            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                            .dst_access_mask(
                                vk::AccessFlags::HOST_READ
                                    | vk::AccessFlags::SHADER_READ
                                    | vk::AccessFlags::SHADER_WRITE,
                            );
                        dev.cmd_pipeline_barrier(cb,
                            vk::PipelineStageFlags::COMPUTE_SHADER,
                            vk::PipelineStageFlags::HOST
                                | vk::PipelineStageFlags::COMPUTE_SHADER,
                            vk::DependencyFlags::empty(),
                            &[barrier], &[], &[]);
                    }
                }
                FrameOp::CopyImgToBuf => {
                    end_rp!(); // image left in TRANSFER_SRC by the render pass
                    if body.len() < 16 + 56 { continue; }
                    let src_id = u32::from_le_bytes(body[0..4].try_into().unwrap());
                    let dst_id = u32::from_le_bytes(body[4..8].try_into().unwrap());
                    let region_count = u32::from_le_bytes(body[12..16].try_into().unwrap());
                    if region_count == 0 { continue; }
                    let Some(img) = images.get_mut(&src_id) else { continue; };
                    let Some(handle) = self.ensure_image(img) else { continue; };
                    let Some(buf) = buffers.get(&dst_id) else { continue; };
                    // Sync barrier: make the copy wait on the render pass's
                    // colour writes (image is already TRANSFER_SRC).
                    self.transition(cb, handle,
                        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                        vk::ImageLayout::TRANSFER_SRC_OPTIMAL);
                    let r = &body[16..16 + 56];
                    let buf_offset = u64::from_le_bytes(r[0..8].try_into().unwrap());
                    let row_length = u32::from_le_bytes(r[8..12].try_into().unwrap());
                    let img_h = u32::from_le_bytes(r[12..16].try_into().unwrap());
                    let ox = i32::from_le_bytes(r[32..36].try_into().unwrap());
                    let oy = i32::from_le_bytes(r[36..40].try_into().unwrap());
                    let ew = u32::from_le_bytes(r[44..48].try_into().unwrap());
                    let eh = u32::from_le_bytes(r[48..52].try_into().unwrap());
                    let copy = vk::BufferImageCopy::default()
                        .buffer_offset(buf_offset).buffer_row_length(row_length)
                        .buffer_image_height(img_h)
                        .image_subresource(vk::ImageSubresourceLayers::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .mip_level(0).base_array_layer(0).layer_count(1))
                        .image_offset(vk::Offset3D { x: ox, y: oy, z: 0 })
                        .image_extent(vk::Extent3D { width: ew, height: eh, depth: 1 });
                    unsafe {
                        dev.cmd_copy_image_to_buffer(cb, handle,
                            vk::ImageLayout::TRANSFER_SRC_OPTIMAL, buf.buffer, &[copy]);
                    }
                }
                _ => { /* other ops: not yet modelled on tier-3 */ }
            }
        }
        end_rp!();
        drop(pipelines); drop(buffers); drop(images);

        if timing {
            unsafe {
                dev.cmd_write_timestamp(
                    cb, vk::PipelineStageFlags::BOTTOM_OF_PIPE, self.query_pool, 1);
            }
        }
        unsafe { dev.end_command_buffer(cb)?; }
        let fence = unsafe { dev.create_fence(&vk::FenceCreateInfo::default(), None)? };
        let cbs = [cb];
        let submit = vk::SubmitInfo::default().command_buffers(&cbs);
        // Bounded: a frame that does not complete within the wait budget
        // (default 10 s) returns TIMEOUT instead of freezing the caller; the
        // caller treats it as a lost device and recreates the backend. The
        // command buffer and fence are leaked on timeout (still in flight).
        let res = unsafe {
            dev.queue_submit(self._queue, &[submit], fence)
                .and_then(|_| dev.wait_for_fences(&[fence], true, Self::wait_timeout_ns(10_000)))
        };
        if res == Err(vk::Result::TIMEOUT) {
            log::error!("MoltenVk submit_frame: GPU stall — frame did not complete within the wait budget");
            self.stalled.store(true, Ordering::Relaxed);
            return Err(vk::Result::TIMEOUT);
        }
        // Read the two timestamps back (the fence guarantees completion)
        // and record the modeled-vs-measured ground-truth exec time.
        if timing && res.is_ok() {
            let mut ts = vec![0u64; 2 + n_stamped as usize];
            let got = unsafe {
                dev.get_query_pool_results(
                    self.query_pool, 0, &mut ts, vk::QueryResultFlags::TYPE_64)
            };
            if got.is_ok() {
                let period = self.timestamp_period_ns as f64;
                let delta = ts[1].saturating_sub(ts[0]);
                let ns = (delta as f64 * period) as u64;
                self.last_gpu_ns.store(ns, Ordering::Relaxed);
                self.total_gpu_ns.fetch_add(ns, Ordering::Relaxed);
                let mut per = Vec::with_capacity(n_stamped as usize);
                let mut prev = ts[0];
                for k in 0..n_stamped as usize {
                    let t = ts[2 + k];
                    per.push((t.saturating_sub(prev) as f64 * period) as u64);
                    prev = t;
                }
                *self.last_dispatch_ns.lock().unwrap() = per;
            }
        }
        unsafe {
            for (rp, fb, view) in trash {
                dev.destroy_framebuffer(fb, None);
                dev.destroy_render_pass(rp, None);
                dev.destroy_image_view(view, None);
            }
            dev.destroy_fence(fence, None);
            dev.free_command_buffers(self.cmd_pool, &cbs);
        }
        res
    }

    /// Pipeline barrier transitioning `image` between layouts with
    /// conservative all-commands scope (correctness over tightness for
    /// the slice).
    fn transition(&self, cb: vk::CommandBuffer, image: vk::Image,
                  old: vk::ImageLayout, new: vk::ImageLayout) {
        let barrier = vk::ImageMemoryBarrier::default()
            .old_layout(old).new_layout(new)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(image)
            .subresource_range(vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1).layer_count(1))
            .src_access_mask(vk::AccessFlags::MEMORY_WRITE)
            .dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE);
        unsafe {
            self.device.cmd_pipeline_barrier(cb,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::DependencyFlags::empty(), &[], &[], &[barrier]);
        }
    }

    /// Tier-3 level-2a bring-up: clear + draw `vertex_count` vertices
    /// (procedural — no vertex buffers; the VS derives positions from
    /// `gl_VertexIndex`) into `image_id` through a **real Vulkan
    /// graphics pipeline** on Metal, then copy the rendered image into
    /// `dst_buffer_id`. Synchronous (one command buffer + fence).
    ///
    /// Proves the full graphics path — SPIR-V shader modules
    /// (MoltenVK compiles SPIR-V→Metal internally), render pass +
    /// framebuffer, pipeline, `vkCmdDraw` — works on this host. The
    /// FrameOp-stream wiring (BindPipeline/Draw + a hardware
    /// pipeline-create hook on the `Backend` trait) is level-2b; the
    /// resource-creation helpers here are its building blocks.
    ///
    /// Transient resources (pipeline / render pass / framebuffer /
    /// shader modules / image view) are created + destroyed per call —
    /// caching is a later optimisation, not needed for bring-up.
    pub fn draw_and_copy(
        &self,
        image_id: ResourceId,
        dst_buffer_id: ResourceId,
        vs_spirv: &[u8],
        fs_spirv: &[u8],
        vertex_count: u32,
        clear_rgba: [u8; 4],
    ) -> Result<(), vk::Result> {
        self.draw_and_copy_pc(image_id, dst_buffer_id, vs_spirv, fs_spirv, vertex_count, clear_rgba, &[])
    }

    /// As [`Self::draw_and_copy`], but binds `push` bytes as push constants
    /// (VERTEX | FRAGMENT, offset 0) — the descriptor-free uniform path.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_and_copy_pc(
        &self,
        image_id: ResourceId,
        dst_buffer_id: ResourceId,
        vs_spirv: &[u8],
        fs_spirv: &[u8],
        vertex_count: u32,
        clear_rgba: [u8; 4],
        push: &[u8],
    ) -> Result<(), vk::Result> {
        self.draw_and_copy_full(image_id, dst_buffer_id, vs_spirv, fs_spirv, vertex_count, clear_rgba, push, None, None, false)
    }

    /// Full draw path: optional push constants + an optional descriptor at
    /// (set 0, binding 0) — a UBO (`ubo`) or a sampled RGBA8 texture (`tex`,
    /// COMBINED_IMAGE_SAMPLER, uploaded via a staging buffer). The descriptor
    /// path is this backend's descriptor support, driven by cross-tier
    /// shaded certification's uniform + texture rungs.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_and_copy_full(
        &self,
        image_id: ResourceId,
        dst_buffer_id: ResourceId,
        vs_spirv: &[u8],
        fs_spirv: &[u8],
        vertex_count: u32,
        clear_rgba: [u8; 4],
        push: &[u8],
        ubo: Option<&[u8]>,
        tex: Option<TexBind>,
        blend_srcover: bool,
    ) -> Result<(), vk::Result> {
        let _guard = self.submit_lock.lock().unwrap();
        let dev = &self.device;

        // Resolve image (materialise its VkImage) + format/dims.
        let (image, format, width, height) = {
            let mut images = self.images.lock().unwrap();
            let img = images.get_mut(&image_id.raw())
                .ok_or(vk::Result::ERROR_UNKNOWN)?;
            let handle = self.ensure_image(img).ok_or(vk::Result::ERROR_UNKNOWN)?;
            (handle, img.format, img.width, img.height)
        };
        let dst_buffer = {
            let buffers = self.buffers.lock().unwrap();
            buffers.get(&dst_buffer_id.raw()).map(|b| b.buffer)
                .ok_or(vk::Result::ERROR_UNKNOWN)?
        };

        unsafe {
            // ── Image view ────────────────────────────────────────
            let view_info = vk::ImageViewCreateInfo::default()
                .image(image).view_type(vk::ImageViewType::TYPE_2D).format(format)
                .subresource_range(vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1).layer_count(1));
            let view = dev.create_image_view(&view_info, None)?;

            // ── Render pass: clear → store, end in TRANSFER_SRC so the
            //    subsequent copy reads it. ───────────────────────────
            let attach = vk::AttachmentDescription::default()
                .format(format).samples(vk::SampleCountFlags::TYPE_1)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
                .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .final_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL);
            let color_ref = [vk::AttachmentReference::default()
                .attachment(0).layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)];
            let subpass = [vk::SubpassDescription::default()
                .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
                .color_attachments(&color_ref)];
            let attachments = [attach];
            let rp_info = vk::RenderPassCreateInfo::default()
                .attachments(&attachments).subpasses(&subpass);
            let render_pass = dev.create_render_pass(&rp_info, None)?;

            // ── Shader modules ────────────────────────────────────
            let vs_code = spirv_words(vs_spirv);
            let fs_code = spirv_words(fs_spirv);
            let vs = dev.create_shader_module(
                &vk::ShaderModuleCreateInfo::default().code(&vs_code), None)?;
            let fs = dev.create_shader_module(
                &vk::ShaderModuleCreateInfo::default().code(&fs_code), None)?;
            let entry = CString::new("main").unwrap();
            let stages = [
                vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::VERTEX).module(vs).name(&entry),
                vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::FRAGMENT).module(fs).name(&entry),
            ];

            // ── Fixed-function state ──────────────────────────────
            let vinput = vk::PipelineVertexInputStateCreateInfo::default();
            let ia = vk::PipelineInputAssemblyStateCreateInfo::default()
                .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
            let viewports = [vk::Viewport {
                x: 0.0, y: 0.0, width: width as f32, height: height as f32,
                min_depth: 0.0, max_depth: 1.0,
            }];
            let scissors = [vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: vk::Extent2D { width, height },
            }];
            let vp = vk::PipelineViewportStateCreateInfo::default()
                .viewports(&viewports).scissors(&scissors);
            let rs = vk::PipelineRasterizationStateCreateInfo::default()
                .polygon_mode(vk::PolygonMode::FILL)
                .cull_mode(vk::CullModeFlags::NONE)
                .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
                .line_width(1.0);
            let ms = vk::PipelineMultisampleStateCreateInfo::default()
                .rasterization_samples(vk::SampleCountFlags::TYPE_1);
            // Blend: when `blend_srcover`, the hardware fixed-function blend
            // unit runs SrcOver with the SAME factors as our Tier-2
            // `apply_blend` (color: SrcAlpha / OneMinusSrcAlpha; alpha:
            // One / OneMinusSrcAlpha; ADD) — so a clear-to-D + draw-S capture
            // reads Metal's exact SrcOver(S, D) for tier-equivalence
            // verification.  Otherwise blend is disabled (shader-output
            // passthrough, the original behaviour).
            let blend_attach = [if blend_srcover {
                vk::PipelineColorBlendAttachmentState::default()
                    .color_write_mask(vk::ColorComponentFlags::RGBA)
                    .blend_enable(true)
                    .src_color_blend_factor(vk::BlendFactor::SRC_ALPHA)
                    .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                    .color_blend_op(vk::BlendOp::ADD)
                    .src_alpha_blend_factor(vk::BlendFactor::ONE)
                    .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                    .alpha_blend_op(vk::BlendOp::ADD)
            } else {
                vk::PipelineColorBlendAttachmentState::default()
                    .color_write_mask(vk::ColorComponentFlags::RGBA)
                    .blend_enable(false)
            }];
            let cb_state = vk::PipelineColorBlendStateCreateInfo::default()
                .attachments(&blend_attach);
            // Push-constant range (VERTEX|FRAGMENT) when push data is given.
            let pc_ranges = if push.is_empty() {
                Vec::new()
            } else {
                vec![vk::PushConstantRange::default()
                    .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
                    .offset(0)
                    .size(push.len() as u32)]
            };
            // ── Optional UBO at (set 0, binding 0) ────────────────────
            // Create the uniform buffer + descriptor set layout/pool/set up
            // front; bound before the draw, torn down after.
            // Optional single descriptor at (set 0, binding 0): a UBO
            // (UNIFORM_BUFFER) or a sampled texture (COMBINED_IMAGE_SAMPLER).
            let mut ubo_res: Option<(vk::Buffer, vk::DeviceMemory)> = None;
            let mut tex_res: Option<(vk::Image, vk::DeviceMemory, vk::ImageView,
                                     vk::Sampler, vk::Buffer, vk::DeviceMemory, u32, u32)> = None;
            let mut desc_res: Option<(vk::DescriptorSetLayout, vk::DescriptorPool,
                                      vk::DescriptorSet)> = None;
            let mut set_layouts: Vec<vk::DescriptorSetLayout> = Vec::new();
            if let Some(bytes) = ubo {
                let size = bytes.len().max(16) as u64;
                let bi = vk::BufferCreateInfo::default().size(size)
                    .usage(vk::BufferUsageFlags::UNIFORM_BUFFER)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE);
                let ubuf = dev.create_buffer(&bi, None)?;
                let req = dev.get_buffer_memory_requirements(ubuf);
                let mt = self.mem_type(req.memory_type_bits,
                    vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT)
                    .ok_or(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY)?;
                let umem = dev.allocate_memory(&vk::MemoryAllocateInfo::default()
                    .allocation_size(req.size).memory_type_index(mt), None)?;
                dev.bind_buffer_memory(ubuf, umem, 0)?;
                let ptr = dev.map_memory(umem, 0, size, vk::MemoryMapFlags::empty())? as *mut u8;
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
                dev.unmap_memory(umem);
                let binding = [vk::DescriptorSetLayoutBinding::default()
                    .binding(0).descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)];
                let dsl = dev.create_descriptor_set_layout(
                    &vk::DescriptorSetLayoutCreateInfo::default().bindings(&binding), None)?;
                let pool_size = [vk::DescriptorPoolSize::default()
                    .ty(vk::DescriptorType::UNIFORM_BUFFER).descriptor_count(1)];
                let pool = dev.create_descriptor_pool(&vk::DescriptorPoolCreateInfo::default()
                    .max_sets(1).pool_sizes(&pool_size), None)?;
                let dsls = [dsl];
                let set = dev.allocate_descriptor_sets(&vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(pool).set_layouts(&dsls))?[0];
                let bufinfo = [vk::DescriptorBufferInfo::default().buffer(ubuf).offset(0).range(size)];
                let write = [vk::WriteDescriptorSet::default().dst_set(set).dst_binding(0)
                    .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER).buffer_info(&bufinfo)];
                dev.update_descriptor_sets(&write, &[]);
                ubo_res = Some((ubuf, umem));
                desc_res = Some((dsl, pool, set));
                set_layouts = vec![dsl];
            } else if let Some(t) = tex {
                // Sampled RGBA8 image (SAMPLED|TRANSFER_DST) + staging buffer.
                let fmt = vk::Format::R8G8B8A8_UNORM;
                let ici = vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(fmt)
                    .extent(vk::Extent3D { width: t.width, height: t.height, depth: 1 })
                    .mip_levels(1).array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
                    .initial_layout(vk::ImageLayout::UNDEFINED);
                let timg = dev.create_image(&ici, None)?;
                let ireq = dev.get_image_memory_requirements(timg);
                let imt = self.mem_type(ireq.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
                    .ok_or(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY)?;
                let tmem = dev.allocate_memory(&vk::MemoryAllocateInfo::default()
                    .allocation_size(ireq.size).memory_type_index(imt), None)?;
                dev.bind_image_memory(timg, tmem, 0)?;
                // Staging buffer with the texel bytes.
                let sbi = vk::BufferCreateInfo::default().size(t.data.len() as u64)
                    .usage(vk::BufferUsageFlags::TRANSFER_SRC)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE);
                let sbuf = dev.create_buffer(&sbi, None)?;
                let sreq = dev.get_buffer_memory_requirements(sbuf);
                let smt = self.mem_type(sreq.memory_type_bits,
                    vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT)
                    .ok_or(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY)?;
                let smem = dev.allocate_memory(&vk::MemoryAllocateInfo::default()
                    .allocation_size(sreq.size).memory_type_index(smt), None)?;
                dev.bind_buffer_memory(sbuf, smem, 0)?;
                let sptr = dev.map_memory(smem, 0, t.data.len() as u64, vk::MemoryMapFlags::empty())? as *mut u8;
                std::ptr::copy_nonoverlapping(t.data.as_ptr(), sptr, t.data.len());
                dev.unmap_memory(smem);
                let view = dev.create_image_view(&vk::ImageViewCreateInfo::default()
                    .image(timg).view_type(vk::ImageViewType::TYPE_2D).format(fmt)
                    .subresource_range(vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR).level_count(1).layer_count(1)),
                    None)?;
                let filt = if t.linear { vk::Filter::LINEAR } else { vk::Filter::NEAREST };
                let sampler = dev.create_sampler(&vk::SamplerCreateInfo::default()
                    .mag_filter(filt).min_filter(filt)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE), None)?;
                let binding = [vk::DescriptorSetLayoutBinding::default()
                    .binding(0).descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .descriptor_count(1).stage_flags(vk::ShaderStageFlags::FRAGMENT)];
                let dsl = dev.create_descriptor_set_layout(
                    &vk::DescriptorSetLayoutCreateInfo::default().bindings(&binding), None)?;
                let pool_size = [vk::DescriptorPoolSize::default()
                    .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER).descriptor_count(1)];
                let pool = dev.create_descriptor_pool(&vk::DescriptorPoolCreateInfo::default()
                    .max_sets(1).pool_sizes(&pool_size), None)?;
                let dsls = [dsl];
                let set = dev.allocate_descriptor_sets(&vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(pool).set_layouts(&dsls))?[0];
                let iinfo = [vk::DescriptorImageInfo::default()
                    .sampler(sampler).image_view(view)
                    .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
                let write = [vk::WriteDescriptorSet::default().dst_set(set).dst_binding(0)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER).image_info(&iinfo)];
                dev.update_descriptor_sets(&write, &[]);
                tex_res = Some((timg, tmem, view, sampler, sbuf, smem, t.width, t.height));
                desc_res = Some((dsl, pool, set));
                set_layouts = vec![dsl];
            }
            let layout = dev.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default()
                    .set_layouts(&set_layouts)
                    .push_constant_ranges(&pc_ranges), None)?;

            let pipe_info = vk::GraphicsPipelineCreateInfo::default()
                .stages(&stages)
                .vertex_input_state(&vinput)
                .input_assembly_state(&ia)
                .viewport_state(&vp)
                .rasterization_state(&rs)
                .multisample_state(&ms)
                .color_blend_state(&cb_state)
                .layout(layout)
                .render_pass(render_pass)
                .subpass(0);
            let pipeline = dev.create_graphics_pipelines(
                vk::PipelineCache::null(), &[pipe_info], None)
                .map_err(|(_, e)| e)?[0];

            // ── Framebuffer ───────────────────────────────────────
            let fb_views = [view];
            let fb_info = vk::FramebufferCreateInfo::default()
                .render_pass(render_pass).attachments(&fb_views)
                .width(width).height(height).layers(1);
            let framebuffer = dev.create_framebuffer(&fb_info, None)?;

            // ── Record: render pass (clear+draw) → copy to buffer ──
            let alloc = vk::CommandBufferAllocateInfo::default()
                .command_pool(self.cmd_pool)
                .level(vk::CommandBufferLevel::PRIMARY).command_buffer_count(1);
            let cb = dev.allocate_command_buffers(&alloc)?[0];
            dev.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT))?;
            // Measured GPU render time: stamp the top of the pipe before any
            // work; the bottom is stamped right after the render pass (below),
            // so the span is clear+draw only — not the readback copy.
            let timing = self.query_pool != vk::QueryPool::null();
            if timing {
                dev.cmd_reset_query_pool(cb, self.query_pool, 0, 2);
                dev.cmd_write_timestamp(
                    cb, vk::PipelineStageFlags::TOP_OF_PIPE, self.query_pool, 0);
            }
            // Texture upload: staging → image, UNDEFINED → TRANSFER_DST →
            // SHADER_READ_ONLY, recorded before the render pass.
            if let Some((timg, _, _, _, sbuf, _, tw, th)) = tex_res {
                let sub = vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR).level_count(1).layer_count(1);
                let to_dst = vk::ImageMemoryBarrier::default()
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(timg).subresource_range(sub)
                    .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE);
                dev.cmd_pipeline_barrier(cb, vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::TRANSFER, vk::DependencyFlags::empty(),
                    &[], &[], &[to_dst]);
                let region = vk::BufferImageCopy::default()
                    .image_subresource(vk::ImageSubresourceLayers::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR).mip_level(0)
                        .base_array_layer(0).layer_count(1))
                    .image_extent(vk::Extent3D { width: tw, height: th, depth: 1 });
                dev.cmd_copy_buffer_to_image(cb, sbuf, timg,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL, &[region]);
                let to_read = vk::ImageMemoryBarrier::default()
                    .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(timg).subresource_range(sub)
                    .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .dst_access_mask(vk::AccessFlags::SHADER_READ);
                dev.cmd_pipeline_barrier(cb, vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::FRAGMENT_SHADER, vk::DependencyFlags::empty(),
                    &[], &[], &[to_read]);
            }
            let clear = [vk::ClearValue { color: vk::ClearColorValue {
                float32: [clear_rgba[0] as f32 / 255.0, clear_rgba[1] as f32 / 255.0,
                          clear_rgba[2] as f32 / 255.0, clear_rgba[3] as f32 / 255.0],
            }}];
            let rp_begin = vk::RenderPassBeginInfo::default()
                .render_pass(render_pass).framebuffer(framebuffer)
                .render_area(vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 },
                    extent: vk::Extent2D { width, height } })
                .clear_values(&clear);
            dev.cmd_begin_render_pass(cb, &rp_begin, vk::SubpassContents::INLINE);
            dev.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pipeline);
            if let Some((_, _, set)) = desc_res {
                dev.cmd_bind_descriptor_sets(cb, vk::PipelineBindPoint::GRAPHICS,
                    layout, 0, &[set], &[]);
            }
            if !push.is_empty() {
                dev.cmd_push_constants(cb, layout,
                    vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT, 0, push);
            }
            dev.cmd_draw(cb, vertex_count, 1, 0, 0);
            dev.cmd_end_render_pass(cb);
            if timing {
                dev.cmd_write_timestamp(
                    cb, vk::PipelineStageFlags::BOTTOM_OF_PIPE, self.query_pool, 1);
            }
            // Image is now TRANSFER_SRC_OPTIMAL (render pass finalLayout).
            let copy = vk::BufferImageCopy::default()
                .image_subresource(vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .mip_level(0).base_array_layer(0).layer_count(1))
                .image_extent(vk::Extent3D { width, height, depth: 1 });
            dev.cmd_copy_image_to_buffer(cb, image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL, dst_buffer, &[copy]);
            dev.end_command_buffer(cb)?;

            // ── Submit + wait ─────────────────────────────────────
            let fence = dev.create_fence(&vk::FenceCreateInfo::default(), None)?;
            let cbs = [cb];
            let submit = vk::SubmitInfo::default().command_buffers(&cbs);
            let res = dev.queue_submit(self._queue, &[submit], fence)
                .and_then(|_| dev.wait_for_fences(&[fence], true, u64::MAX));

            // Read the render-span timestamps back (the fence guarantees the
            // GPU is done) → measured silicon render time for cost calibration.
            if timing && res.is_ok() {
                let mut ts = [0u64; 2];
                if dev.get_query_pool_results(
                    self.query_pool, 0, &mut ts, vk::QueryResultFlags::TYPE_64).is_ok()
                {
                    let delta = ts[1].saturating_sub(ts[0]);
                    let ns = (delta as f64 * self.timestamp_period_ns as f64) as u64;
                    self.last_gpu_ns.store(ns, Ordering::Relaxed);
                    self.total_gpu_ns.fetch_add(ns, Ordering::Relaxed);
                }
            }

            // ── Teardown (transient) ──────────────────────────────
            dev.destroy_fence(fence, None);
            dev.free_command_buffers(self.cmd_pool, &cbs);
            dev.destroy_framebuffer(framebuffer, None);
            dev.destroy_pipeline(pipeline, None);
            dev.destroy_pipeline_layout(layout, None);
            if let Some((dsl, pool, _)) = desc_res {
                dev.destroy_descriptor_pool(pool, None); // frees the set
                dev.destroy_descriptor_set_layout(dsl, None);
            }
            if let Some((ubuf, umem)) = ubo_res {
                dev.destroy_buffer(ubuf, None);
                dev.free_memory(umem, None);
            }
            if let Some((timg, tmem, view, sampler, sbuf, smem, _, _)) = tex_res {
                dev.destroy_sampler(sampler, None);
                dev.destroy_image_view(view, None);
                dev.destroy_image(timg, None);
                dev.free_memory(tmem, None);
                dev.destroy_buffer(sbuf, None);
                dev.free_memory(smem, None);
            }
            dev.destroy_shader_module(vs, None);
            dev.destroy_shader_module(fs, None);
            dev.destroy_render_pass(render_pass, None);
            dev.destroy_image_view(view, None);
            res
        }
    }

    /// Tier-3 level-2b: register a graphics pipeline from VS+FS SPIR-V,
    /// keyed by `pipeline_id`. The real `VkPipeline` is built lazily on
    /// first draw (the colour format isn't known until then — see
    /// `materialize_pipeline`); here we just stash the bytecode. A
    /// later `FrameOp::BindPipeline` in `submit_frame` binds it.
    pub fn create_graphics_pipeline(
        &self, pipeline_id: ResourceId, vs_spirv: &[u8], fs_spirv: &[u8],
    ) {
        let prior = self.pipelines.lock().unwrap().insert(
            pipeline_id.raw(),
            MvkPipeline {
                vs_spirv: vs_spirv.to_vec(),
                fs_spirv: fs_spirv.to_vec(),
                materialized: None,
            });
        if let Some(p) = prior {
            if let Some(vk) = p.materialized {
                unsafe { let _ = self.device.device_wait_idle();
                         self.destroy_pipeline_vk(vk); }
            }
        }
    }

    /// Build the Vulkan objects for a graphics pipeline at `format`.
    /// Dynamic viewport/scissor (target dims set at draw); the render
    /// pass is compatible-by-format with `submit_frame`'s per-frame
    /// render pass (Vulkan compatibility is on formats/samples).
    fn materialize_pipeline(&self, vs_spirv: &[u8], fs_spirv: &[u8],
                            format: vk::Format) -> Result<MvkPipelineVk, vk::Result> {
        let dev = &self.device;
        unsafe {
            let attach = [vk::AttachmentDescription::default()
                .format(format).samples(vk::SampleCountFlags::TYPE_1)
                .load_op(vk::AttachmentLoadOp::CLEAR).store_op(vk::AttachmentStoreOp::STORE)
                .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
                .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .final_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)];
            let color_ref = [vk::AttachmentReference::default()
                .attachment(0).layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)];
            let subpass = [vk::SubpassDescription::default()
                .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
                .color_attachments(&color_ref)];
            let render_pass = dev.create_render_pass(&vk::RenderPassCreateInfo::default()
                .attachments(&attach).subpasses(&subpass), None)?;
            let vs_code = spirv_words(vs_spirv);
            let fs_code = spirv_words(fs_spirv);
            let vs = dev.create_shader_module(
                &vk::ShaderModuleCreateInfo::default().code(&vs_code), None)?;
            let fs = dev.create_shader_module(
                &vk::ShaderModuleCreateInfo::default().code(&fs_code), None)?;
            let entry = CString::new("main").unwrap();
            let stages = [
                vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::VERTEX).module(vs).name(&entry),
                vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::FRAGMENT).module(fs).name(&entry),
            ];
            let vinput = vk::PipelineVertexInputStateCreateInfo::default();
            let ia = vk::PipelineInputAssemblyStateCreateInfo::default()
                .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
            let vp = vk::PipelineViewportStateCreateInfo::default()
                .viewport_count(1).scissor_count(1);
            let dyn_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
            let dyn_info = vk::PipelineDynamicStateCreateInfo::default()
                .dynamic_states(&dyn_states);
            let rs = vk::PipelineRasterizationStateCreateInfo::default()
                .polygon_mode(vk::PolygonMode::FILL)
                .cull_mode(vk::CullModeFlags::NONE)
                .front_face(vk::FrontFace::COUNTER_CLOCKWISE).line_width(1.0);
            let ms = vk::PipelineMultisampleStateCreateInfo::default()
                .rasterization_samples(vk::SampleCountFlags::TYPE_1);
            let blend_attach = [vk::PipelineColorBlendAttachmentState::default()
                .color_write_mask(vk::ColorComponentFlags::RGBA).blend_enable(false)];
            let cb_state = vk::PipelineColorBlendStateCreateInfo::default()
                .attachments(&blend_attach);
            let layout = dev.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default(), None)?;
            let pipe_info = vk::GraphicsPipelineCreateInfo::default()
                .stages(&stages).vertex_input_state(&vinput).input_assembly_state(&ia)
                .viewport_state(&vp).dynamic_state(&dyn_info)
                .rasterization_state(&rs).multisample_state(&ms)
                .color_blend_state(&cb_state)
                .layout(layout).render_pass(render_pass).subpass(0);
            let pipeline = dev.create_graphics_pipelines(
                vk::PipelineCache::null(), &[pipe_info], None).map_err(|(_, e)| e)?[0];
            Ok(MvkPipelineVk { format, pipeline, layout, render_pass, vs, fs })
        }
    }

    /// Destroy a realised pipeline's Vulkan objects.
    unsafe fn destroy_pipeline_vk(&self, p: MvkPipelineVk) {
        self.device.destroy_pipeline(p.pipeline, None);
        self.device.destroy_pipeline_layout(p.layout, None);
        self.device.destroy_render_pass(p.render_pass, None);
        self.device.destroy_shader_module(p.vs, None);
        self.device.destroy_shader_module(p.fs, None);
    }
}

/// Reinterpret SPIR-V bytes as the `u32` words ash's
/// `ShaderModuleCreateInfo::code` wants. SPIR-V is little-endian
/// 32-bit words; a non-multiple-of-4 length is truncated (malformed
/// input).
fn spirv_words(bytes: &[u8]) -> Vec<u32> {
    bytes.chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Best-effort vendor mapping from a PCI vendor-id. MoltenVK reports
/// `0x106B` (Apple) on Apple Silicon; on Intel Macs MoltenVK still
/// reports the underlying GPU vendor.
fn vendor_from_pci_id(pci: u32) -> GpuVendor {
    match pci {
        0x106B => GpuVendor::Apple,
        0x1002 => GpuVendor::Amd,
        0x8086 => GpuVendor::Intel,
        0x10DE => GpuVendor::Nvidia,
        // Unknown vendor: report Software (the safe "no special caps"
        // fallback). The enum has no Other / Unknown variant by design
        // — see aqueduct-gpu::backends::GpuVendor. Future hardware
        // additions should add explicit variants.
        _      => GpuVendor::Software,
    }
}

/// Check whether the device exposes `VK_KHR_portability_subset`.
/// Per Vulkan spec, devices that DO advertise it MUST have it
/// enabled at device creation. MoltenVK does; native Vulkan
/// drivers don't.
fn host_supports_portability_subset(
    instance: &ash::Instance,
    physical: vk::PhysicalDevice,
) -> bool {
    device_ext_present(instance, physical, khr::portability_subset::NAME)
}

/// Whether the physical device enumerates device extension `want`.
fn device_ext_present(instance: &ash::Instance, physical: vk::PhysicalDevice, want: &std::ffi::CStr) -> bool {
    let exts = match unsafe { instance.enumerate_device_extension_properties(physical) } {
        Ok(v)  => v,
        Err(_) => return false,
    };
    exts.iter().any(|p| {
        p.extension_name_as_c_str().map(|s| s == want).unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tier-3 tests are gated on a Vulkan loader being present on
    // the host. Use a helper to skip rather than fail when not
    // installed — that's the production daemon's fallback path too.
    fn try_init() -> Option<MoltenVkBackend> {
        match MoltenVkBackend::new() {
            Ok(b) => Some(b),
            Err(MoltenVkError::LoaderUnavailable(_)) => {
                eprintln!("MoltenVK test skipped: no Vulkan loader on this host");
                None
            }
            Err(e) => panic!("MoltenVkBackend::new unexpected failure: {e}"),
        }
    }

    #[test]
    fn loads_and_reports_identity() {
        let Some(b) = try_init() else { return; };
        let id = b.identity();
        // Vendor must be a non-Unknown value; otherwise something is
        // very wrong with the loader.
        assert_ne!(format!("{:?}", id.vendor), "Unknown");
        eprintln!("MoltenVkBackend: {}", b.device_summary());
    }

    #[test]
    fn submit_frame_signals_immediately() {
        let Some(b) = try_init() else { return; };
        let fid = ResourceId::new(aqueduct_gpu::ids::IdNamespace::IcdRuntime, 0x1);
        assert!(b.submit_frame(fid, 1, &[]));
        assert_eq!(b.submission_count(), 1);
    }

    #[test]
    fn caps_advertise_compute_and_spirv_upload() {
        let Some(b) = try_init() else { return; };
        use aqueduct_gpu::payloads::HandshakeResponse as H;
        let c = b.caps();
        assert!(c & H::CAPS_COMPUTE != 0);
        assert!(c & H::CAPS_SPIRV_UPLOAD != 0);
        assert!(c & H::CAPS_COMPOSITION != 0);
        assert!(c & H::CAPS_SHARE_SURFACE != 0);
    }

    /// Inline ray_query through the FULL aqueduct-gpu-host stack: build a
    /// 1-triangle BLAS + 1-instance TLAS, dispatch a ray_query compute kernel
    /// that binds the TLAS as an acceleration-structure descriptor, read back
    /// the committed hit. This is the Rust mirror of the verified
    /// external/MoltenVK ray_query_test/host.c (which links MoltenVK directly):
    /// here the SAME result (hit=1, t≈1) is produced through MoltenVkBackend —
    /// device RT enablement, build_prefab_tlas, the AS descriptor binding, and
    /// the FrameOp::Dispatch path. Requires the khr-ray-query MoltenVK fork; on
    /// stock MoltenVK has_ray_query() is false and the test skips.
    /// (Run with VK_DRIVER_FILES=<fork ICD> DYLD_LIBRARY_PATH=/opt/homebrew/lib.)
    #[test]
    fn ray_query_hit_through_stack() {
        use aqueduct_gpu::frame::FrameBuilder;
        use aqueduct_gpu::ids::IdNamespace;
        let Some(be) = try_init() else { return; };
        if !be.has_ray_query() {
            eprintln!("ray_query test skipped: device lacks VK_KHR_ray_query \
                       (stock MoltenVK — needs the khr-ray-query fork)");
            return;
        }

        // One triangle in the z=0 plane (matches host.c — the kernel fires a ray
        // straight down the -z axis from z=+1 and expects a hit at t=1).
        let verts: [f32; 9] = [-0.5, -0.5, 0.0,  0.5, -0.5, 0.0,  0.0, 0.5, 0.0];
        // One identity instance (row-major 3x4).
        let identity: [f32; 12] = [1.0, 0.0, 0.0, 0.0,
                                   0.0, 1.0, 0.0, 0.0,
                                   0.0, 0.0, 1.0, 0.0];
        let tlas = ResourceId::new(IdNamespace::IcdRuntime, 0xA0);
        be.build_prefab_tlas(tlas, &verts, &[identity]).expect("build TLAS");

        // Pipeline: binding 0 = TLAS (acceleration structure), binding 1 = result
        // SSBO. The SPIR-V is the same kernel host.c validated.
        const RQ_SPV: &[u8] = include_bytes!("test_ray_query.comp.spv");
        let pipe = ResourceId::new(IdNamespace::IcdRuntime, 0xA1);
        be.create_compute_pipeline_rt(pipe, RQ_SPV, 2, 0, &[0]).expect("rt pipeline");

        let res = ResourceId::new(IdNamespace::IcdRuntime, 0xA2);
        be.buffer_created(res, 16);
        be.buffer_write(res, 0, &[0u8; 16]).expect("zero result");

        let mut fb = FrameBuilder::new(4096);
        fb.push(FrameOp::BindPipeline, &pipe.raw().to_le_bytes()).unwrap();
        fb.push_bind_storage_buffers(&[(1, res.raw())]).unwrap();
        be.bind_compute_accel(0, tlas);
        fb.push_dispatch(aqueduct_gpu::frame::DispatchCmd {
            group_count_x: 1, group_count_y: 1, group_count_z: 1,
        }).unwrap();
        let fence = ResourceId::new(IdNamespace::IcdRuntime, 0xA3);
        assert!(be.submit_frame(fence, 1, fb.as_bytes()));

        let out = be.buffer_read_bytes(res, 0, 16).expect("readback");
        let hit = u32::from_le_bytes(out[0..4].try_into().unwrap());
        let t = f32::from_le_bytes(out[4..8].try_into().unwrap());
        eprintln!("ray_query through stack: hit={hit} t={t:.4}");
        assert_eq!(hit, 1, "expected committed hit through the aqueduct stack");
        assert!((t - 1.0).abs() < 0.5, "t={t} not ~1.0");
    }

    /// Multi-BLAS scene: two BLASes, two instances. The test kernel fires
    /// one ray from (0,0,-1) along +z (tMax 100). Scene A: BLAS 0 at z=0
    /// (t=1) and BLAS 1 at z=+2 (t=3) → nearest hit t=1. Scene B: the
    /// BLAS-0 instance moved aside (+10 in x) → the ray must reach BLAS 1
    /// through the second instance: t=3.
    #[test]
    fn multi_blas_scene_hits_the_right_instance() {
        use aqueduct_gpu::frame::FrameBuilder;
        use aqueduct_gpu::ids::IdNamespace;
        let Some(be) = try_init() else { return; };
        if !be.has_ray_query() { return; }
        let tri_a: [f32; 9] = [-0.5, -0.5, 0.0,  0.5, -0.5, 0.0,  0.0, 0.5, 0.0];
        let tri_b: [f32; 9] = [-0.5, -0.5, 2.0,  0.5, -0.5, 2.0,  0.0, 0.5, 2.0];
        let ident = |dx: f32| [1.0, 0.0, 0.0, dx,  0.0, 1.0, 0.0, 0.0,  0.0, 0.0, 1.0, 0.0];
        const RQ_SPV: &[u8] = include_bytes!("test_ray_query.comp.spv");
        let pipe = ResourceId::new(IdNamespace::IcdRuntime, 0xB1);
        be.create_compute_pipeline_rt(pipe, RQ_SPV, 2, 0, &[0]).expect("rt pipeline");
        let res = ResourceId::new(IdNamespace::IcdRuntime, 0xB2);
        be.buffer_created(res, 16);
        // Scene A must report instance 0's custom index (7), scene B instance
        // 1's (9): the Vulkan custom index → Metal user instance ID → the
        // kernel's committed instance ID, which Orbis uses as an attribute base.
        for (k, dx_a, want_t, want_custom) in [(0u32, 0.0f32, 1.0f32, 7u32), (1, 10.0, 3.0, 9)] {
            let tlas = ResourceId::new(IdNamespace::IcdRuntime, 0xB8 + k);
            let inst = [
                SceneInstance { blas: 0, custom_index: 7, transform: ident(dx_a) },
                SceneInstance { blas: 1, custom_index: 9, transform: ident(0.0) },
            ];
            be.build_scene_tlas(tlas, &[&tri_a, &tri_b], &inst).expect("build scene");
            be.buffer_write(res, 0, &[0u8; 16]).expect("zero");
            let mut fb = FrameBuilder::new(4096);
            fb.push(FrameOp::BindPipeline, &pipe.raw().to_le_bytes()).unwrap();
            fb.push_bind_storage_buffers(&[(1, res.raw())]).unwrap();
            be.bind_compute_accel(0, tlas);
            fb.push_dispatch(aqueduct_gpu::frame::DispatchCmd {
                group_count_x: 1, group_count_y: 1, group_count_z: 1 }).unwrap();
            assert!(be.submit_frame(ResourceId::new(IdNamespace::IcdRuntime, 0xB3), 1, fb.as_bytes()));
            let out = be.buffer_read_bytes(res, 0, 16).expect("readback");
            let hit = u32::from_le_bytes(out[0..4].try_into().unwrap());
            let t = f32::from_le_bytes(out[4..8].try_into().unwrap());
            let custom = u32::from_le_bytes(out[12..16].try_into().unwrap());
            eprintln!("multi-BLAS scene {k}: hit={hit} t={t:.3} custom={custom} (want t {want_t}, custom {want_custom})");
            assert_eq!(hit, 1, "scene {k}: expected a hit");
            assert!((t - want_t).abs() < 0.2, "scene {k}: t={t} want {want_t}");
            assert_eq!(custom, want_custom, "scene {k}: committed instance custom index");
        }
    }

    /// A Metal queue wedge, reproduced deliberately: a submission that waits
    /// on a semaphore nobody signals never completes and trips no watchdog
    /// (the shape of the Orbis acceleration-structure hang). The bounded
    /// wait must return TIMEOUT within its budget, the backend must report
    /// itself stalled, and a FRESH backend must keep working while the
    /// wedged one is leaked (never waited on).
    #[test]
    fn wedged_queue_is_bounded_and_recoverable() {
        use aqueduct_gpu::ids::IdNamespace;
        let Some(be) = try_init() else { return; };
        std::env::set_var("AQUEDUCT_GPU_WAIT_MS", "300");
        let dev = &be.device;
        unsafe {
            let sem = dev.create_semaphore(&vk::SemaphoreCreateInfo::default(), None).unwrap();
            let cb = dev.allocate_command_buffers(&vk::CommandBufferAllocateInfo::default()
                .command_pool(be.cmd_pool).level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1)).unwrap()[0];
            dev.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT)).unwrap();
            dev.end_command_buffer(cb).unwrap();
            let fence = dev.create_fence(&vk::FenceCreateInfo::default(), None).unwrap();
            let cbs = [cb];
            let sems = [sem];
            let stages = [vk::PipelineStageFlags::TOP_OF_PIPE];
            let si = vk::SubmitInfo::default()
                .wait_semaphores(&sems).wait_dst_stage_mask(&stages).command_buffers(&cbs);
            dev.queue_submit(be._queue, &[si], fence).unwrap();
            let t0 = std::time::Instant::now();
            let r = dev.wait_for_fences(&[fence], true, MoltenVkBackend::wait_timeout_ns(300));
            assert_eq!(r, Err(vk::Result::TIMEOUT), "a never-signalled wait must time out, not hang");
            assert!(t0.elapsed().as_secs_f64() < 5.0, "timed out late: {:?}", t0.elapsed());
        }
        // Every later submission on the wedged queue also times out — and
        // submit_frame reports it instead of blocking.
        let fence_id = ResourceId::new(IdNamespace::IcdRuntime, 0xC1);
        let t1 = std::time::Instant::now();
        assert!(!be.submit_frame(fence_id, 1, &[]), "submit_frame on a wedged queue must report false");
        assert!(t1.elapsed().as_secs_f64() < 5.0);
        assert!(be.is_stalled());
        // Recovery: a fresh device/queue works while the wedged one is leaked.
        let fresh = MoltenVkBackend::new().expect("fresh backend");
        assert!(fresh.submit_frame(fence_id, 1, &[]), "fresh backend must submit");
        assert!(!fresh.is_stalled());
        std::mem::forget(be);
        std::env::remove_var("AQUEDUCT_GPU_WAIT_MS");
    }

    /// Three BLASes and many instances: the ray (from (0,0,-1) along +z)
    /// must reach the ONE instance of BLAS 2 placed under it (t=1) through
    /// 20 000 decoy instances of BLAS 0 parked far away and one BLAS-1
    /// instance behind it (t=3); then, with BLAS 1 under the ray and BLAS 2
    /// behind, t must be 1 again with custom index 5. Guards the fork's
    /// instance→BLAS slot resolution beyond two BLASes and the
    /// large-instance-count path Orbis's tree inventory uses.
    #[test]
    fn three_blas_many_instances_resolve() {
        use aqueduct_gpu::frame::FrameBuilder;
        use aqueduct_gpu::ids::IdNamespace;
        let Some(be) = try_init() else { return; };
        if !be.has_ray_query() { return; }
        let tri_at = |z: f32| -> [f32; 9] { [-0.5, -0.5, z,  0.5, -0.5, z,  0.0, 0.5, z] };
        let (a, b, c) = (tri_at(0.0), tri_at(0.0), tri_at(0.0));
        let xf = |dx: f32, dz: f32| [1.0, 0.0, 0.0, dx,  0.0, 1.0, 0.0, 0.0,  0.0, 0.0, 1.0, dz];
        const RQ_SPV: &[u8] = include_bytes!("test_ray_query.comp.spv");
        let pipe = ResourceId::new(IdNamespace::IcdRuntime, 0xD1);
        be.create_compute_pipeline_rt(pipe, RQ_SPV, 2, 0, &[0]).expect("rt pipeline");
        let res = ResourceId::new(IdNamespace::IcdRuntime, 0xD2);
        be.buffer_created(res, 16);
        for (k, near_blas, far_blas, want_custom) in [(0u32, 2u32, 1u32, 9u32), (1, 1, 2, 5)] {
            let mut inst: Vec<SceneInstance> = (0..20_000)
                .map(|i| SceneInstance { blas: 0, custom_index: 1, transform: xf(100.0 + i as f32 * 0.01, 0.0) })
                .collect();
            inst.push(SceneInstance { blas: far_blas, custom_index: 7, transform: xf(0.0, 2.0) });
            inst.push(SceneInstance { blas: near_blas, custom_index: want_custom, transform: xf(0.0, 0.0) });
            let tlas = ResourceId::new(IdNamespace::IcdRuntime, 0xD8 + k);
            be.build_scene_tlas(tlas, &[&a, &b, &c], &inst).expect("build scene");
            be.buffer_write(res, 0, &[0u8; 16]).expect("zero");
            let mut fb = FrameBuilder::new(4096);
            fb.push(FrameOp::BindPipeline, &pipe.raw().to_le_bytes()).unwrap();
            fb.push_bind_storage_buffers(&[(1, res.raw())]).unwrap();
            be.bind_compute_accel(0, tlas);
            fb.push_dispatch(aqueduct_gpu::frame::DispatchCmd { group_count_x: 1, group_count_y: 1, group_count_z: 1 }).unwrap();
            assert!(be.submit_frame(ResourceId::new(IdNamespace::IcdRuntime, 0xD3), 1, fb.as_bytes()));
            let out = be.buffer_read_bytes(res, 0, 16).expect("readback");
            let hit = u32::from_le_bytes(out[0..4].try_into().unwrap());
            let t = f32::from_le_bytes(out[4..8].try_into().unwrap());
            let custom = u32::from_le_bytes(out[12..16].try_into().unwrap());
            eprintln!("3-BLAS scene {k}: hit={hit} t={t:.3} custom={custom} (want t 1, custom {want_custom})");
            assert_eq!(hit, 1, "scene {k}");
            assert!((t - 1.0).abs() < 0.2, "scene {k}: t={t}");
            assert_eq!(custom, want_custom, "scene {k}: wrong instance resolved");
        }
    }

    /// Tier-3 level-1: a render-pass clear + image→buffer readback runs
    /// on real Metal (MoltenVK) and reads back the exact clear colour —
    /// the mirror of tier2's level-1, proving the FrameOp→Vulkan replay
    /// path. (Run with `DYLD_LIBRARY_PATH=/opt/homebrew/lib`.)
    #[test]
    fn clear_and_readback_through_metal() {
        use aqueduct_gpu::frame::FrameBuilder;
        use aqueduct_gpu::ids::IdNamespace;
        let Some(be) = try_init() else { return; };

        const W: u32 = 16;
        const H: u32 = 16;
        let img = ResourceId::new(IdNamespace::IcdRuntime, 0x10);
        let buf = ResourceId::new(IdNamespace::IcdRuntime, 0x20);

        be.image_created(img, W, H);
        be.set_image_format(img, 37); // VK_FORMAT_R8G8B8A8_UNORM
        be.buffer_created(buf, (W * H * 4) as u64);

        let mut fb = FrameBuilder::new(4096);
        // BeginRenderPass: image_id u32 + clear_rgba8 + flags u32.
        let mut brp = Vec::new();
        brp.extend_from_slice(&img.raw().to_le_bytes());
        brp.extend_from_slice(&[40u8, 80, 160, 255]);
        brp.extend_from_slice(&0u32.to_le_bytes());
        fb.push(FrameOp::BeginRenderPass, &brp).unwrap();
        // CopyImgToBuf: src u32, dst u32, src_layout u32, region_count u32,
        // then one 56-byte VkBufferImageCopy (extent = full image).
        let mut cib = Vec::new();
        cib.extend_from_slice(&img.raw().to_le_bytes());
        cib.extend_from_slice(&buf.raw().to_le_bytes());
        cib.extend_from_slice(&0u32.to_le_bytes()); // src_layout (ignored)
        cib.extend_from_slice(&1u32.to_le_bytes()); // region_count
        let mut region = vec![0u8; 56];
        region[44..48].copy_from_slice(&W.to_le_bytes()); // extent.width
        region[48..52].copy_from_slice(&H.to_le_bytes()); // extent.height
        region[52..56].copy_from_slice(&1u32.to_le_bytes()); // extent.depth
        cib.extend_from_slice(&region);
        fb.push(FrameOp::CopyImgToBuf, &cib).unwrap();

        let fid = ResourceId::new(IdNamespace::IcdRuntime, 0x99);
        assert!(be.submit_frame(fid, 1, fb.as_bytes()));

        let px = be.buffer_read_bytes(buf, 0, (W * H * 4) as u64)
            .expect("readback");
        let i = ((H as usize / 2) * W as usize + W as usize / 2) * 4;
        assert_eq!(&px[i..i + 4], &[40, 80, 160, 255],
            "clear colour read back through Metal (got {:?})", &px[i..i + 4]);

        // D-M6 measured-truth: the real GPU spent a plausible, non-zero,
        // sub-second slice of time on this frame (timestamps supported on
        // Apple Silicon under MoltenVK). This is the ground truth the
        // analytic cost model calibrates against.
        let t = be.measured_gpu_time_s();
        assert!(t > 0.0 && t < 1.0,
            "measured GPU exec time should be plausible, got {t} s");
        assert!(be.total_gpu_time_s() >= t, "cumulative includes the last frame");
    }

    /// A SPIR-V vertex shader emitting a full-screen triangle from
    /// `gl_VertexIndex` (no vertex buffer). Verts 0/1/2 → NDC
    /// (-1,-1),(3,-1),(-1,3), covering the viewport.
    fn build_fullscreen_tri_vs() -> Vec<u8> {
        use rspirv::binary::Assemble;
        use rspirv::spirv::{
            AddressingModel, BuiltIn, Capability, Decoration, ExecutionModel,
            FunctionControl, MemoryModel, StorageClass,
        };
        use rspirv::dr::Operand;
        let mut b = rspirv::dr::Builder::new();
        b.set_version(1, 0);
        b.capability(Capability::Shader);
        b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
        let void = b.type_void();
        let f32t = b.type_float(32, None);
        let i32t = b.type_int(32, 1);
        let v4 = b.type_vector(f32t, 4);
        let void_fn = b.type_function(void, vec![]);
        let per_vertex = b.type_struct(vec![v4]);
        b.member_decorate(per_vertex, 0, Decoration::BuiltIn,
            vec![Operand::BuiltIn(BuiltIn::Position)]);
        b.member_decorate(per_vertex, 0, Decoration::Offset,
            vec![Operand::LiteralBit32(0)]);
        b.decorate(per_vertex, Decoration::Block, vec![]);
        let ptr_pv = b.type_pointer(None, StorageClass::Output, per_vertex);
        let ptr_out_v4 = b.type_pointer(None, StorageClass::Output, v4);
        let ptr_in_i32 = b.type_pointer(None, StorageClass::Input, i32t);
        let in_idx = b.variable(ptr_in_i32, None, StorageClass::Input, None);
        b.decorate(in_idx, Decoration::BuiltIn, vec![Operand::BuiltIn(BuiltIn::VertexIndex)]);
        let pv_var = b.variable(ptr_pv, None, StorageClass::Output, None);
        let c0i = b.constant_bit32(i32t, 0);
        let c1i = b.constant_bit32(i32t, 1);
        let c2i = b.constant_bit32(i32t, 2);
        let c2f = b.constant_bit32(f32t, 2.0f32.to_bits());
        let c1f = b.constant_bit32(f32t, 1.0f32.to_bits());
        let c0f = b.constant_bit32(f32t, 0.0f32.to_bits());
        let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
        b.begin_block(None).unwrap();
        let idx = b.load(i32t, None, in_idx, None, vec![]).unwrap();
        let sh  = b.shift_left_logical(i32t, None, idx, c1i).unwrap();
        let xb  = b.bitwise_and(i32t, None, sh, c2i).unwrap();
        let yb  = b.bitwise_and(i32t, None, idx, c2i).unwrap();
        let xf  = b.convert_s_to_f(f32t, None, xb).unwrap();
        let yf  = b.convert_s_to_f(f32t, None, yb).unwrap();
        let xm  = b.f_mul(f32t, None, xf, c2f).unwrap();
        let x   = b.f_sub(f32t, None, xm, c1f).unwrap();
        let ym  = b.f_mul(f32t, None, yf, c2f).unwrap();
        let y   = b.f_sub(f32t, None, ym, c1f).unwrap();
        let pos = b.composite_construct(v4, None, vec![x, y, c0f, c1f]).unwrap();
        let dst = b.access_chain(ptr_out_v4, None, pv_var, vec![c0i]).unwrap();
        b.store(dst, pos, None, vec![]).unwrap();
        b.ret().unwrap();
        b.end_function().unwrap();
        b.entry_point(ExecutionModel::Vertex, main, "main", vec![in_idx, pv_var]);
        let words: Vec<u32> = b.module().assemble();
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    /// A SPIR-V fragment shader writing a constant colour to Output 0.
    fn build_const_fs(rgba: [f32; 4]) -> Vec<u8> {
        use rspirv::binary::Assemble;
        use rspirv::spirv::{
            AddressingModel, Capability, Decoration, ExecutionMode, ExecutionModel,
            FunctionControl, MemoryModel, StorageClass,
        };
        use rspirv::dr::Operand;
        let mut b = rspirv::dr::Builder::new();
        b.set_version(1, 0);
        b.capability(Capability::Shader);
        b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
        let void = b.type_void();
        let f32t = b.type_float(32, None);
        let v4 = b.type_vector(f32t, 4);
        let void_fn = b.type_function(void, vec![]);
        let ptr_out = b.type_pointer(None, StorageClass::Output, v4);
        let cs: Vec<_> = rgba.iter().map(|x| b.constant_bit32(f32t, x.to_bits())).collect();
        let color = b.constant_composite(v4, cs);
        let out = b.variable(ptr_out, None, StorageClass::Output, None);
        b.decorate(out, Decoration::Location, vec![Operand::LiteralBit32(0)]);
        let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
        b.begin_block(None).unwrap();
        b.store(out, color, None, vec![]).unwrap();
        b.ret().unwrap();
        b.end_function().unwrap();
        b.entry_point(ExecutionModel::Fragment, main, "main", vec![out]);
        b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
        let words: Vec<u32> = b.module().assemble();
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    /// Tier-3 level-2a: a real graphics-pipeline DRAW (VS+FS → triangle)
    /// runs on Metal and the rendered colour reads back — proving the
    /// SPIR-V → pipeline → vkCmdDraw path works on this host.
    #[test]
    fn draw_triangle_through_metal() {
        use aqueduct_gpu::ids::IdNamespace;
        let Some(be) = try_init() else { return; };
        const W: u32 = 16;
        const H: u32 = 16;
        let img = ResourceId::new(IdNamespace::IcdRuntime, 0x30);
        let buf = ResourceId::new(IdNamespace::IcdRuntime, 0x40);
        be.image_created(img, W, H);
        be.set_image_format(img, 37); // RGBA8_UNORM
        be.buffer_created(buf, (W * H * 4) as u64);

        let vs = build_fullscreen_tri_vs();
        let fs = build_const_fs([0.9, 0.2, 0.2, 1.0]); // red-ish
        be.draw_and_copy(img, buf, &vs, &fs, 3, [10, 10, 10, 255])
            .expect("draw_and_copy");

        let px = be.buffer_read_bytes(buf, 0, (W * H * 4) as u64).expect("readback");
        let i = ((H as usize / 2) * W as usize + W as usize / 2) * 4;
        // Centre is covered by the full-screen triangle → the FS colour
        // (~[230,51,51,255]), NOT the dark clear [10,10,10].
        assert!(px[i] > 200 && px[i + 1] < 90 && px[i + 2] < 90 && px[i + 3] == 255,
            "centre should be the drawn triangle colour, got {:?}", &px[i..i + 4]);
    }

    /// Tier-3 level-2b (-i): the FULL FrameOp draw replay through
    /// `submit_frame` — a registered pipeline + a frame of
    /// BeginRenderPass / BindPipeline / Draw / EndRenderPass /
    /// CopyImgToBuf renders the triangle on Metal. This is the interface
    /// the daemon will drive (level-2b-ii wires the session to it).
    #[test]
    fn frameop_draw_replay_through_metal() {
        use aqueduct_gpu::frame::FrameBuilder;
        use aqueduct_gpu::ids::IdNamespace;
        let Some(be) = try_init() else { return; };
        const W: u32 = 16;
        const H: u32 = 16;
        let img  = ResourceId::new(IdNamespace::IcdRuntime, 0x50);
        let buf  = ResourceId::new(IdNamespace::IcdRuntime, 0x60);
        let pipe = ResourceId::new(IdNamespace::IcdRuntime, 0x70);

        be.image_created(img, W, H);
        be.set_image_format(img, 37); // RGBA8_UNORM
        be.buffer_created(buf, (W * H * 4) as u64);
        // Register the pipeline (SPIR-V stashed; the VkPipeline is
        // materialised lazily on first draw at the target's format).
        be.create_graphics_pipeline(pipe, &build_fullscreen_tri_vs(),
            &build_const_fs([0.2, 0.85, 0.3, 1.0])); // green

        let mut fb = FrameBuilder::new(8192);
        let mut brp = Vec::new();
        brp.extend_from_slice(&img.raw().to_le_bytes());
        brp.extend_from_slice(&[10u8, 10, 10, 255]); // dark clear
        brp.extend_from_slice(&0u32.to_le_bytes());   // flags (CLEAR)
        fb.push(FrameOp::BeginRenderPass, &brp).unwrap();
        fb.push(FrameOp::BindPipeline, &pipe.raw().to_le_bytes()).unwrap();
        let mut draw = Vec::new(); // DrawCmd: vcount, icount, fvert, finst
        draw.extend_from_slice(&3u32.to_le_bytes());
        draw.extend_from_slice(&1u32.to_le_bytes());
        draw.extend_from_slice(&0u32.to_le_bytes());
        draw.extend_from_slice(&0u32.to_le_bytes());
        fb.push(FrameOp::Draw, &draw).unwrap();
        fb.push(FrameOp::EndRenderPass, &[]).unwrap();
        let mut cib = Vec::new();
        cib.extend_from_slice(&img.raw().to_le_bytes());
        cib.extend_from_slice(&buf.raw().to_le_bytes());
        cib.extend_from_slice(&0u32.to_le_bytes());
        cib.extend_from_slice(&1u32.to_le_bytes());
        let mut region = vec![0u8; 56];
        region[44..48].copy_from_slice(&W.to_le_bytes());
        region[48..52].copy_from_slice(&H.to_le_bytes());
        region[52..56].copy_from_slice(&1u32.to_le_bytes());
        cib.extend_from_slice(&region);
        fb.push(FrameOp::CopyImgToBuf, &cib).unwrap();

        let fid = ResourceId::new(IdNamespace::IcdRuntime, 0x71);
        assert!(be.submit_frame(fid, 1, fb.as_bytes()));

        let px = be.buffer_read_bytes(buf, 0, (W * H * 4) as u64).expect("readback");
        let i = ((H as usize / 2) * W as usize + W as usize / 2) * 4;
        // Green triangle (~[51,217,77,255]) over the dark clear.
        assert!(px[i] < 90 && px[i + 1] > 180 && px[i + 2] < 110 && px[i + 3] == 255,
            "centre should be the drawn (green) triangle, got {:?}", &px[i..i + 4]);
    }
}
