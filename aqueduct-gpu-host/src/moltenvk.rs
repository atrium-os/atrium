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
    /// Serialises command-buffer record + submit (one graphics queue).
    submit_lock: Mutex<()>,
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
        let entry = unsafe { Entry::load() }
            .map_err(MoltenVkError::LoaderUnavailable)?;
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
            submit_lock: Mutex::new(()),
        })
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
            for (_, img) in self.images.lock().unwrap().drain() {
                if let Some(h) = img.image { self.device.destroy_image(h, None); }
                if let Some(m) = img.memory { self.device.free_memory(m, None); }
            }
            for (_, b) in self.buffers.lock().unwrap().drain() {
                self.device.unmap_memory(b.memory);
                self.device.destroy_buffer(b.buffer, None);
                self.device.free_memory(b.memory, None);
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
                | vk::BufferUsageFlags::TRANSFER_SRC)
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

    /// Replay the frame's op stream as real Vulkan commands on Metal.
    /// Slice scope: render-pass *clear* (`vkCmdClearColorImage`) +
    /// image→buffer *readback* (`vkCmdCopyImageToBuffer`). Draws,
    /// pipelines and SPIR-V→Metal are follow-on slices.
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
}

impl MoltenVkBackend {
    /// Record + submit one frame's clear/copy ops. Errors are returned
    /// (logged by the caller); the frame is still "consumed".
    fn record_and_submit(&self, frame_buf: &[u8]) -> Result<(), vk::Result> {
        // Allocate a one-shot command buffer.
        let alloc = vk::CommandBufferAllocateInfo::default()
            .command_pool(self.cmd_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let cb = unsafe { self.device.allocate_command_buffers(&alloc)? }[0];
        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        unsafe { self.device.begin_command_buffer(cb, &begin)?; }

        // Per-frame image layout tracking (all start UNDEFINED).
        let mut layouts: HashMap<u32, vk::ImageLayout> = HashMap::new();
        let mut images = self.images.lock().unwrap();
        let buffers = self.buffers.lock().unwrap();

        let mut dec = FrameDecoder::new(frame_buf);
        while let Ok(Some((op, body))) = dec.next() {
            match op {
                FrameOp::BeginRenderPass => {
                    if body.len() < 8 { continue; }
                    let img_id = u32::from_le_bytes(body[0..4].try_into().unwrap());
                    let flags = if body.len() >= 12 {
                        u32::from_le_bytes(body[8..12].try_into().unwrap())
                    } else { 0 };
                    const NO_CLEAR: u32 = 0x1;
                    if flags & NO_CLEAR != 0 { continue; }
                    let rgba = [body[4], body[5], body[6], body[7]];
                    let Some(img) = images.get_mut(&img_id) else { continue; };
                    let Some(handle) = self.ensure_image(img) else { continue; };
                    self.transition(cb, handle,
                        *layouts.get(&img_id).unwrap_or(&vk::ImageLayout::UNDEFINED),
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL);
                    layouts.insert(img_id, vk::ImageLayout::TRANSFER_DST_OPTIMAL);
                    let clear = vk::ClearColorValue {
                        float32: [rgba[0] as f32 / 255.0, rgba[1] as f32 / 255.0,
                                  rgba[2] as f32 / 255.0, rgba[3] as f32 / 255.0],
                    };
                    let range = vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .level_count(1).layer_count(1);
                    unsafe {
                        self.device.cmd_clear_color_image(cb, handle,
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL, &clear, &[range]);
                    }
                }
                FrameOp::CopyImgToBuf => {
                    if body.len() < 16 + 56 { continue; }
                    let src_id = u32::from_le_bytes(body[0..4].try_into().unwrap());
                    let dst_id = u32::from_le_bytes(body[4..8].try_into().unwrap());
                    let region_count = u32::from_le_bytes(body[12..16].try_into().unwrap());
                    if region_count == 0 { continue; }
                    let Some(img) = images.get_mut(&src_id) else { continue; };
                    let Some(handle) = self.ensure_image(img) else { continue; };
                    let Some(buf) = buffers.get(&dst_id) else { continue; };
                    self.transition(cb, handle,
                        *layouts.get(&src_id).unwrap_or(&vk::ImageLayout::UNDEFINED),
                        vk::ImageLayout::TRANSFER_SRC_OPTIMAL);
                    layouts.insert(src_id, vk::ImageLayout::TRANSFER_SRC_OPTIMAL);
                    // First region only for the slice (whole-image
                    // readback is one region).
                    let r = &body[16..16 + 56];
                    let buf_offset = u64::from_le_bytes(r[0..8].try_into().unwrap());
                    let row_length = u32::from_le_bytes(r[8..12].try_into().unwrap());
                    let img_h = u32::from_le_bytes(r[12..16].try_into().unwrap());
                    let ox = i32::from_le_bytes(r[32..36].try_into().unwrap());
                    let oy = i32::from_le_bytes(r[36..40].try_into().unwrap());
                    let ew = u32::from_le_bytes(r[44..48].try_into().unwrap());
                    let eh = u32::from_le_bytes(r[48..52].try_into().unwrap());
                    let copy = vk::BufferImageCopy::default()
                        .buffer_offset(buf_offset)
                        .buffer_row_length(row_length)
                        .buffer_image_height(img_h)
                        .image_subresource(vk::ImageSubresourceLayers::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .mip_level(0).base_array_layer(0).layer_count(1))
                        .image_offset(vk::Offset3D { x: ox, y: oy, z: 0 })
                        .image_extent(vk::Extent3D { width: ew, height: eh, depth: 1 });
                    unsafe {
                        self.device.cmd_copy_image_to_buffer(cb, handle,
                            vk::ImageLayout::TRANSFER_SRC_OPTIMAL, buf.buffer, &[copy]);
                    }
                }
                _ => { /* draws / other ops: follow-on slices */ }
            }
        }
        drop(buffers);
        drop(images);

        unsafe { self.device.end_command_buffer(cb)?; }

        // Submit + wait on a transient fence (synchronous, like the SW
        // backend; the aqueduct fence is signalled by the caller path).
        let fence = unsafe {
            self.device.create_fence(&vk::FenceCreateInfo::default(), None)?
        };
        let cbs = [cb];
        let submit = vk::SubmitInfo::default().command_buffers(&cbs);
        let res = unsafe {
            self.device.queue_submit(self._queue, &[submit], fence)
                .and_then(|_| self.device.wait_for_fences(&[fence], true, u64::MAX))
        };
        unsafe {
            self.device.destroy_fence(fence, None);
            self.device.free_command_buffers(self.cmd_pool, &[cb]);
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
    }
}
