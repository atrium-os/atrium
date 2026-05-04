//! Headless Vulkan renderer — no swapchain, no surface, no window.
//!
//! Renders into an off-screen `vk::Image` at a configurable size,
//! then copies the rendered pixels into a host-visible buffer that
//! the consumer (frescod, conformance tests, lavapipe-in-QEMU CI)
//! can read back.
//!
//! This is the **production-target** Vulkan path for Atrium:
//! - In QEMU + lavapipe, there's no display surface; we render to a
//!   buffer and dump to PNG (CI lane) or hand to frescod's
//!   atrium-gpu-rs scanout BO (production path).
//! - On real FreeBSD HW (D5+), frescod owns the display via the
//!   Atrium GPU ABI; the rendered image gets memcpy'd into the
//!   scanout BO before page-flip.
//! - On macOS dev with venus passthrough, headless still works —
//!   the guest's lavapipe or venus-host drivers don't need a WSI
//!   surface to produce pixels.
//!
//! The windowed `Renderer` (in `renderer.rs`) is kept for the
//! archived POC reference and stays useful for the macOS-host
//! dev workflow when running a Vulkan demo directly. Production
//! frescod uses this headless path exclusively.

use std::ffi::{c_char, CStr};

use anyhow::{anyhow, Context, Result};
use ash::vk;

const COLOR_FORMAT: vk::Format = vk::Format::B8G8R8A8_UNORM;

/// A headless Vulkan device that renders into a single off-screen
/// color image. The image is **device-local** (fast for GPU writes);
/// pixel readback goes through a host-visible staging buffer.
///
/// Single-frame-in-flight: each `clear_and_readback` does a full
/// submit + queue-wait + buffer copy. Production rendering will use
/// the same instance/device but call into the existing render-pass
/// machinery in `renderer.rs` (extracted to shared helpers in a
/// follow-up).
pub struct HeadlessRenderer {
    /* Lifetime: dropped LIFO. */
    _entry:    ash::Entry,
    instance:  ash::Instance,

    physical_device: vk::PhysicalDevice,
    #[allow(dead_code)]
    queue_family:    u32,
    device:          ash::Device,
    queue:           vk::Queue,

    extent:          vk::Extent2D,

    /* Off-screen render target: device-local image we render into. */
    color_image:     vk::Image,
    color_memory:    vk::DeviceMemory,
    #[allow(dead_code)]
    color_view:      vk::ImageView,

    /* Host-visible staging buffer for readback. */
    readback_buffer: vk::Buffer,
    readback_memory: vk::DeviceMemory,
    readback_size:   vk::DeviceSize,

    cmd_pool:   vk::CommandPool,
    cmd_buffer: vk::CommandBuffer,
    fence:      vk::Fence,
}

impl HeadlessRenderer {
    /// Create a headless renderer that targets an `extent.width ×
    /// extent.height` BGRA8 image. Picks the first physical device
    /// that exposes a graphics queue. No surface / present support.
    pub fn new(width: u32, height: u32) -> Result<Self> {
        let entry = unsafe { ash::Entry::load() }
            .context("ash::Entry::load — install vulkan-loader (lavapipe/venus/vendor)")?;

        let instance = create_instance(&entry)?;
        let (physical_device, queue_family) = pick_physical_device(&instance)?;
        log_physical_device(&instance, physical_device);
        let device = create_device(&instance, physical_device, queue_family)?;
        let queue = unsafe { device.get_device_queue(queue_family, 0) };

        let extent = vk::Extent2D { width, height };

        /* Allocate render target image. */
        let mem_props = unsafe {
            instance.get_physical_device_memory_properties(physical_device)
        };
        let (color_image, color_memory) = create_color_image(
            &device, &mem_props, extent, COLOR_FORMAT)?;
        let color_view = create_image_view(&device, color_image, COLOR_FORMAT)?;

        /* Allocate readback buffer. 4 bytes/pixel for BGRA8. */
        let readback_size = (width as vk::DeviceSize) * (height as vk::DeviceSize) * 4;
        let (readback_buffer, readback_memory) = create_buffer(
            &device, &mem_props, readback_size,
            vk::BufferUsageFlags::TRANSFER_DST,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )?;

        let (cmd_pool, cmd_buffer) = create_command_pool(&device, queue_family)?;

        let fence_info = vk::FenceCreateInfo::default();
        let fence = unsafe { device.create_fence(&fence_info, None) }
            .context("create_fence")?;

        Ok(Self {
            _entry: entry, instance,
            physical_device, queue_family,
            device, queue,
            extent,
            color_image, color_memory, color_view,
            readback_buffer, readback_memory, readback_size,
            cmd_pool, cmd_buffer, fence,
        })
    }

    /// Width × height of the off-screen render target.
    pub fn extent(&self) -> (u32, u32) { (self.extent.width, self.extent.height) }

    /// Clear the render target to `color` and copy the pixels back
    /// into the host staging buffer. Returns once the GPU is idle and
    /// `read_pixels` can be called. The simplest possible "make sure
    /// the Vulkan path is alive" check.
    ///
    /// `color` is a BGRA8 quadruple (each component 0..=255).
    pub fn clear_and_readback(&mut self, color: [u8; 4]) -> Result<()> {
        unsafe {
            self.device.reset_command_buffer(
                self.cmd_buffer, vk::CommandBufferResetFlags::empty())?;

            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            self.device.begin_command_buffer(self.cmd_buffer, &begin)?;

            /* UNDEFINED → TRANSFER_DST_OPTIMAL */
            transition_image(
                &self.device, self.cmd_buffer, self.color_image,
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::AccessFlags::empty(),
                vk::AccessFlags::TRANSFER_WRITE,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER);

            /* Clear via vkCmdClearColorImage. */
            let clear = vk::ClearColorValue {
                float32: [
                    color[2] as f32 / 255.0,  /* R */
                    color[1] as f32 / 255.0,  /* G */
                    color[0] as f32 / 255.0,  /* B */
                    color[3] as f32 / 255.0,  /* A */
                ],
            };
            let range = vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0, level_count: 1,
                base_array_layer: 0, layer_count: 1,
            };
            self.device.cmd_clear_color_image(
                self.cmd_buffer, self.color_image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &clear, &[range]);

            /* TRANSFER_DST_OPTIMAL → TRANSFER_SRC_OPTIMAL  (for the
             * upcoming copy-to-buffer). */
            transition_image(
                &self.device, self.cmd_buffer, self.color_image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                vk::AccessFlags::TRANSFER_WRITE,
                vk::AccessFlags::TRANSFER_READ,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::TRANSFER);

            /* Copy image → readback buffer. */
            let region = vk::BufferImageCopy::default()
                .buffer_offset(0)
                .buffer_row_length(0).buffer_image_height(0)
                .image_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0, base_array_layer: 0, layer_count: 1,
                })
                .image_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
                .image_extent(vk::Extent3D {
                    width: self.extent.width, height: self.extent.height, depth: 1,
                });
            self.device.cmd_copy_image_to_buffer(
                self.cmd_buffer, self.color_image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                self.readback_buffer, &[region]);

            self.device.end_command_buffer(self.cmd_buffer)?;

            /* Reset + submit + wait. Simple synchronous pattern; enough
             * for the smoke-test path. */
            self.device.reset_fences(&[self.fence])?;
            let submit = vk::SubmitInfo::default()
                .command_buffers(std::slice::from_ref(&self.cmd_buffer));
            self.device.queue_submit(self.queue, &[submit], self.fence)?;
            self.device.wait_for_fences(&[self.fence], true, u64::MAX)?;
        }
        Ok(())
    }

    /// Read the most-recently-written pixels into a borrowed slice.
    /// `dst.len()` must equal `width * height * 4` (BGRA8).
    pub fn read_pixels(&self, dst: &mut [u8]) -> Result<()> {
        if dst.len() as vk::DeviceSize != self.readback_size {
            return Err(anyhow!(
                "read_pixels: dst.len()={} but readback buffer is {} bytes",
                dst.len(), self.readback_size));
        }
        unsafe {
            let p = self.device.map_memory(
                self.readback_memory, 0, self.readback_size,
                vk::MemoryMapFlags::empty())?;
            std::ptr::copy_nonoverlapping(
                p as *const u8, dst.as_mut_ptr(), dst.len());
            self.device.unmap_memory(self.readback_memory);
        }
        Ok(())
    }

    /// Convenience: allocate a fresh `Vec<u8>` and read pixels into it.
    pub fn read_pixels_vec(&self) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; self.readback_size as usize];
        self.read_pixels(&mut buf)?;
        Ok(buf)
    }
}

impl Drop for HeadlessRenderer {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.cmd_pool, None);
            self.device.destroy_image_view(self.color_view, None);
            self.device.destroy_image(self.color_image, None);
            self.device.free_memory(self.color_memory, None);
            self.device.destroy_buffer(self.readback_buffer, None);
            self.device.free_memory(self.readback_memory, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
        let _ = self.physical_device;  // pacify dead_code on some configs
    }
}

// ── helpers ──────────────────────────────────────────────────────────

fn create_instance(entry: &ash::Entry) -> Result<ash::Instance> {
    let app_info = vk::ApplicationInfo::default()
        .application_name(c"fresco-vulkan-headless")
        .application_version(vk::make_api_version(0, 0, 1, 0))
        .engine_name(c"fresco")
        .engine_version(vk::make_api_version(0, 0, 1, 0))
        .api_version(vk::API_VERSION_1_3);

    /* Always-on extensions: portability_enumeration is needed on macOS
     * (MoltenVK reports as a portable Vulkan implementation). On real
     * Vulkan loaders it's a no-op. */
    let exts: Vec<*const c_char> = vec![
        ash::khr::portability_enumeration::NAME.as_ptr(),
        ash::khr::get_physical_device_properties2::NAME.as_ptr(),
    ];

    let create_flags = vk::InstanceCreateFlags::ENUMERATE_PORTABILITY_KHR;

    let info = vk::InstanceCreateInfo::default()
        .application_info(&app_info)
        .enabled_extension_names(&exts)
        .flags(create_flags);

    Ok(unsafe { entry.create_instance(&info, None) }
        .context("create_instance")?)
}

fn pick_physical_device(instance: &ash::Instance)
    -> Result<(vk::PhysicalDevice, u32)>
{
    let devices = unsafe { instance.enumerate_physical_devices() }?;
    for &pd in &devices {
        let props = unsafe { instance.get_physical_device_queue_family_properties(pd) };
        for (i, p) in props.iter().enumerate() {
            if p.queue_flags.contains(vk::QueueFlags::GRAPHICS) {
                return Ok((pd, i as u32));
            }
        }
    }
    Err(anyhow!("no Vulkan device with a graphics queue"))
}

fn log_physical_device(instance: &ash::Instance, pd: vk::PhysicalDevice) {
    let props = unsafe { instance.get_physical_device_properties(pd) };
    let name = unsafe { CStr::from_ptr(props.device_name.as_ptr()) }
        .to_string_lossy().into_owned();
    let api  = props.api_version;
    log::info!("vulkan headless device: {name} (api {}.{}.{})",
        vk::api_version_major(api),
        vk::api_version_minor(api),
        vk::api_version_patch(api));
}

fn create_device(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    queue_family: u32,
) -> Result<ash::Device> {
    let queue_priorities = [1.0f32];
    let queue_info = vk::DeviceQueueCreateInfo::default()
        .queue_family_index(queue_family)
        .queue_priorities(&queue_priorities);

    /* portability_subset must be enabled when the implementation is
     * non-conformant (MoltenVK on macOS); harmless to probe and skip
     * if the device doesn't expose it (real vendor Vulkan, lavapipe). */
    let avail = unsafe { instance.enumerate_device_extension_properties(physical_device) }?;
    let has_portability = avail.iter().any(|e| {
        let cname = unsafe { CStr::from_ptr(e.extension_name.as_ptr()) };
        cname == ash::khr::portability_subset::NAME
    });
    let mut exts: Vec<*const c_char> = Vec::new();
    if has_portability {
        exts.push(ash::khr::portability_subset::NAME.as_ptr());
    }

    let queues = [queue_info];
    let info = vk::DeviceCreateInfo::default()
        .queue_create_infos(&queues)
        .enabled_extension_names(&exts);

    Ok(unsafe { instance.create_device(physical_device, &info, None) }
        .context("create_device")?)
}

fn create_color_image(
    device:     &ash::Device,
    mem_props:  &vk::PhysicalDeviceMemoryProperties,
    extent:     vk::Extent2D,
    format:     vk::Format,
) -> Result<(vk::Image, vk::DeviceMemory)> {
    let info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(format)
        .extent(vk::Extent3D { width: extent.width, height: extent.height, depth: 1 })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT
             | vk::ImageUsageFlags::TRANSFER_SRC
             | vk::ImageUsageFlags::TRANSFER_DST)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED);
    let image = unsafe { device.create_image(&info, None) }?;

    let req = unsafe { device.get_image_memory_requirements(image) };
    let mem_type = find_memory_type(
        mem_props, req.memory_type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
    let alloc = vk::MemoryAllocateInfo::default()
        .allocation_size(req.size).memory_type_index(mem_type);
    let memory = unsafe { device.allocate_memory(&alloc, None) }?;
    unsafe { device.bind_image_memory(image, memory, 0) }?;

    Ok((image, memory))
}

fn create_image_view(
    device: &ash::Device,
    image:  vk::Image,
    format: vk::Format,
) -> Result<vk::ImageView> {
    let info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(format)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0, level_count: 1,
            base_array_layer: 0, layer_count: 1,
        });
    Ok(unsafe { device.create_image_view(&info, None) }?)
}

fn create_buffer(
    device:    &ash::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    size:      vk::DeviceSize,
    usage:     vk::BufferUsageFlags,
    props:     vk::MemoryPropertyFlags,
) -> Result<(vk::Buffer, vk::DeviceMemory)> {
    let info = vk::BufferCreateInfo::default()
        .size(size).usage(usage)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let buf = unsafe { device.create_buffer(&info, None) }?;
    let req = unsafe { device.get_buffer_memory_requirements(buf) };
    let mem_type = find_memory_type(mem_props, req.memory_type_bits, props)?;
    let alloc = vk::MemoryAllocateInfo::default()
        .allocation_size(req.size).memory_type_index(mem_type);
    let memory = unsafe { device.allocate_memory(&alloc, None) }?;
    unsafe { device.bind_buffer_memory(buf, memory, 0) }?;
    Ok((buf, memory))
}

fn find_memory_type(
    props: &vk::PhysicalDeviceMemoryProperties,
    type_bits: u32,
    needed:  vk::MemoryPropertyFlags,
) -> Result<u32> {
    for i in 0..props.memory_type_count {
        let bit = 1u32 << i;
        if (type_bits & bit) != 0
           && props.memory_types[i as usize].property_flags.contains(needed)
        {
            return Ok(i);
        }
    }
    Err(anyhow!("no memory type matches bits={type_bits:#x} props={needed:?}"))
}

fn create_command_pool(device: &ash::Device, queue_family: u32)
    -> Result<(vk::CommandPool, vk::CommandBuffer)>
{
    let info = vk::CommandPoolCreateInfo::default()
        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
        .queue_family_index(queue_family);
    let pool = unsafe { device.create_command_pool(&info, None) }?;
    let alloc = vk::CommandBufferAllocateInfo::default()
        .command_pool(pool)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);
    let bufs = unsafe { device.allocate_command_buffers(&alloc) }?;
    Ok((pool, bufs[0]))
}

fn transition_image(
    device:        &ash::Device,
    cb:            vk::CommandBuffer,
    image:         vk::Image,
    old_layout:    vk::ImageLayout,
    new_layout:    vk::ImageLayout,
    src_access:    vk::AccessFlags,
    dst_access:    vk::AccessFlags,
    src_stage:     vk::PipelineStageFlags,
    dst_stage:     vk::PipelineStageFlags,
) {
    let barrier = vk::ImageMemoryBarrier::default()
        .src_access_mask(src_access)
        .dst_access_mask(dst_access)
        .old_layout(old_layout)
        .new_layout(new_layout)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0, level_count: 1,
            base_array_layer: 0, layer_count: 1,
        });
    unsafe {
        device.cmd_pipeline_barrier(
            cb, src_stage, dst_stage, vk::DependencyFlags::empty(),
            &[], &[], &[barrier]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke-test: create a 64×64 headless renderer, clear to a known
    /// BGRA color, read back, verify every pixel matches.
    ///
    /// This test requires a Vulkan loader to be available
    /// (lavapipe / venus / vendor / MoltenVK on macOS). If none is
    /// available the test is skipped (init returns Err).
    #[test]
    fn clear_64x64_red_round_trip() {
        let mut r = match HeadlessRenderer::new(64, 64) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping: no Vulkan loader available ({e})");
                return;
            }
        };

        let color = [0x00, 0x00, 0xFF, 0xFF];  /* BGRA = pure red */
        r.clear_and_readback(color).expect("clear_and_readback");
        let pixels = r.read_pixels_vec().expect("read_pixels");

        assert_eq!(pixels.len(), 64 * 64 * 4);

        /* Every pixel must match the cleared color. Lavapipe / vendor
         * drivers may swizzle BGRA8 differently — check just the RGB
         * channels with a tolerance for sRGB rounding. */
        let mut nonred = 0;
        for px in pixels.chunks_exact(4) {
            if px[0] < 0xF0 || px[1] > 0x10 || px[2] > 0x10 {
                nonred += 1;
            }
        }
        assert_eq!(nonred, 0, "expected all-red, found {nonred} divergent pixels");
    }
}
