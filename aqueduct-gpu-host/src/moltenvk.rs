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
    desc_pool: vk::DescriptorPool,
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
    /// Cumulative measured GPU exec time across all timed frames, ns.
    total_gpu_ns: AtomicU64,
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
        let q_creates = [q_create];

        // MoltenVK requires VK_KHR_portability_subset on the device
        // (when present in the physical-device extensions). Querying
        // for it up front would be the production path; for the
        // skeleton we just attempt with no extensions and let the
        // VK_ERROR_EXTENSION_NOT_PRESENT bubble up if the host needs
        // it. The portability-subset device extension is documented
        // in the spec as "MUST be enabled if reported."
        let portability_subset = khr::portability_subset::NAME;
        let device_exts = match
            host_supports_portability_subset(&instance, physical)
        {
            true  => vec![portability_subset.as_ptr()],
            false => vec![],
        };

        let device_create = vk::DeviceCreateInfo::default()
            .queue_create_infos(&q_creates)
            .enabled_extension_names(&device_exts);

        let device = unsafe { instance.create_device(physical, &device_create, None) };
        let device = match device {
            Ok(d) => d,
            Err(e) => {
                unsafe { instance.destroy_instance(None) };
                return Err(MoltenVkError::Vulkan(e));
            }
        };
        let queue = unsafe { device.get_device_queue(queue_family, 0) };

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
        let timestamps_ok = timestamp_period_ns > 0.0
            && qf_props.get(queue_family as usize)
                .map(|q| q.timestamp_valid_bits > 0).unwrap_or(false);
        let query_pool = if timestamps_ok {
            let qpi = vk::QueryPoolCreateInfo::default()
                .query_type(vk::QueryType::TIMESTAMP).query_count(2);
            unsafe { device.create_query_pool(&qpi, None) }
                .unwrap_or(vk::QueryPool::null())
        } else {
            vk::QueryPool::null()
        };
        if query_pool == vk::QueryPool::null() {
            log::info!("MoltenVk: GPU timestamps unavailable; measured exec time disabled");
        }

        // Descriptor pool for per-dispatch sets (compute path).
        // Reset wholesale at each record_and_submit.
        let pool_sizes = [vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::STORAGE_BUFFER)
            .descriptor_count(256)];
        let dp_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(64)
            .pool_sizes(&pool_sizes);
        let desc_pool = unsafe { device.create_descriptor_pool(&dp_info, None) }
            .unwrap_or(vk::DescriptorPool::null());

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
            total_gpu_ns: AtomicU64::new(0),
        })
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
                    vk::DescriptorSetLayoutBinding::default()
                        .binding(b)
                        .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
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

impl Drop for MoltenVkBackend {
    fn drop(&mut self) {
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
            if self.desc_pool != vk::DescriptorPool::null() {
                self.device.destroy_descriptor_pool(self.desc_pool, None);
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
        if timing {
            unsafe {
                dev.cmd_reset_query_pool(cb, self.query_pool, 0, 2);
                dev.cmd_write_timestamp(
                    cb, vk::PipelineStageFlags::TOP_OF_PIPE, self.query_pool, 0);
            }
        }

        let mut images = self.images.lock().unwrap();
        let buffers = self.buffers.lock().unwrap();
        let mut pipelines = self.pipelines.lock().unwrap();
        let compute_pipelines = self.compute_pipelines.lock().unwrap();
        if self.desc_pool != vk::DescriptorPool::null() {
            unsafe {
                let _ = dev.reset_descriptor_pool(
                    self.desc_pool, vk::DescriptorPoolResetFlags::empty());
            }
        }
        // Compute state for this frame stream.
        let mut cur_compute: Option<u32> = None;
        let mut push_bytes: Vec<u8> = Vec::new();

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
                FrameOp::Dispatch => {
                    end_rp!();
                    if body.len() < 12 { continue; }
                    let gx = u32::from_le_bytes(body[0..4].try_into().unwrap());
                    let gy = u32::from_le_bytes(body[4..8].try_into().unwrap());
                    let gz = u32::from_le_bytes(body[8..12].try_into().unwrap());
                    let Some(pid) = cur_compute else { continue; };
                    let Some(cp) = compute_pipelines.get(&pid) else { continue; };
                    if self.desc_pool == vk::DescriptorPool::null() { continue; }
                    // Allocate + write the descriptor set from the
                    // staged buffer bindings.
                    let binds: Vec<(u32, u32)> = {
                        let mut m = self.compute_binds.lock().unwrap();
                        let mut v: Vec<(u32, u32)> = m.drain().collect();
                        v.sort_unstable();
                        v
                    };
                    let set_layouts = [cp.dset_layout];
                    let dset = match unsafe { dev.allocate_descriptor_sets(
                        &vk::DescriptorSetAllocateInfo::default()
                            .descriptor_pool(self.desc_pool)
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
                        // Make the writes visible to host readback
                        // (HOST_COHERENT memory + queue-wait below).
                        let barrier = vk::MemoryBarrier::default()
                            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                            .dst_access_mask(vk::AccessFlags::HOST_READ);
                        dev.cmd_pipeline_barrier(cb,
                            vk::PipelineStageFlags::COMPUTE_SHADER,
                            vk::PipelineStageFlags::HOST,
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
        let res = unsafe {
            dev.queue_submit(self._queue, &[submit], fence)
                .and_then(|_| dev.wait_for_fences(&[fence], true, u64::MAX))
        };
        // Read the two timestamps back (the fence guarantees completion)
        // and record the modeled-vs-measured ground-truth exec time.
        if timing && res.is_ok() {
            let mut ts = [0u64; 2];
            let got = unsafe {
                dev.get_query_pool_results(
                    self.query_pool, 0, &mut ts, vk::QueryResultFlags::TYPE_64)
            };
            if got.is_ok() {
                let delta = ts[1].saturating_sub(ts[0]);
                let ns = (delta as f64 * self.timestamp_period_ns as f64) as u64;
                self.last_gpu_ns.store(ns, Ordering::Relaxed);
                self.total_gpu_ns.fetch_add(ns, Ordering::Relaxed);
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
    let exts = match unsafe { instance.enumerate_device_extension_properties(physical) } {
        Ok(v)  => v,
        Err(_) => return false,
    };
    let want = khr::portability_subset::NAME;
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
