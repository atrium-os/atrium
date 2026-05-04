//! Host-side GPU resource table.
//!
//! Per `docs/spec/fresco-rendering-stack.md` §3.7's CAS story: when
//! a client `OP_SLOT_SET`s a hash to a slot, the host shim allocates
//! a vkImage / vkBuffer of the appropriate shape, stages the bytes
//! from atrium-rpc's CAS into it, and registers the resource in a
//! per-slot table. Scene ops then reference the resource by slot ID
//! (4 bytes) instead of carrying the bytes inline.
//!
//! Threading: requests are queued from the dispatcher thread into a
//! `Vec<UploadRequest>` in SceneState. The renderer drains them at
//! the start of each render() on the main thread (where it owns the
//! Vulkan device + command queue). One frame of latency between
//! "client said upload" and "GPU has bytes" — fine for the POC.
//!
//! Step 6 scope: vkImage allocation, staging-buffer upload,
//! pipeline barriers, registration. SLOT_CLEAR frees by reverse
//! sequence (waitidle, destroy view, destroy image, free memory).
//! No refcounting yet — last-write-wins on slot reuse, like the
//! per-connection table semantics already imply.

use anyhow::{anyhow, Result};
use ash::vk;

/// One uploaded GPU resource. Currently only Texture; mesh / buffer
/// variants land when those scene ops do.
pub enum Resource {
    Texture(Texture),
}

pub struct Texture {
    pub image:        vk::Image,
    pub view:         vk::ImageView,
    pub memory:       vk::DeviceMemory,
    pub width:        u32,
    pub height:       u32,
    pub format:       vk::Format,
}

impl Texture {
    pub unsafe fn destroy(&self, device: &ash::Device) {
        device.destroy_image_view(self.view, None);
        device.destroy_image(self.image, None);
        device.free_memory(self.memory, None);
    }
}

/// What the dispatcher hands the renderer per SLOT_SET.
pub enum UploadRequest {
    Texture {
        slot_id: u32,
        bytes:   Vec<u8>,
        width:   u32,
        height:  u32,
        format:  vk::Format,
    },
}

/// Allocate + upload a single texture. Synchronous — submits a
/// one-off command buffer and waits on a fence. The cost is one
/// queue submit per upload, which is fine for the POC's expected
/// upload volume (a handful of textures per scene). Real production
/// would batch multiple uploads into one command buffer.
pub fn upload_texture(
    device:          &ash::Device,
    queue:           vk::Queue,
    cmd_pool:        vk::CommandPool,
    physical_device: vk::PhysicalDevice,
    instance:        &ash::Instance,
    bytes:           &[u8],
    width:           u32,
    height:          u32,
    format:          vk::Format,
) -> Result<Texture> {
    let device_mem_props = unsafe {
        instance.get_physical_device_memory_properties(physical_device)
    };

    /* 1. Staging buffer (host-visible) — we'll memcpy bytes here,
     *    then vkCmdCopyBufferToImage from it into the device-local
     *    image. */
    let staging_size = bytes.len() as vk::DeviceSize;
    let (staging_buf, staging_mem) = create_buffer(
        device, &device_mem_props, staging_size,
        vk::BufferUsageFlags::TRANSFER_SRC,
        vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
    )?;
    unsafe {
        let p = device.map_memory(staging_mem, 0, staging_size,
            vk::MemoryMapFlags::empty())?;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len());
        device.unmap_memory(staging_mem);
    }

    /* 2. Image (device-local) — TRANSFER_DST | SAMPLED. */
    let image_info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(format)
        .extent(vk::Extent3D { width, height, depth: 1 })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED);
    let image = unsafe { device.create_image(&image_info, None) }?;

    let req = unsafe { device.get_image_memory_requirements(image) };
    let mem_type = find_memory_type(
        &device_mem_props, req.memory_type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
    )?;
    let alloc = vk::MemoryAllocateInfo::default()
        .allocation_size(req.size).memory_type_index(mem_type);
    let memory = unsafe { device.allocate_memory(&alloc, None) }?;
    unsafe { device.bind_image_memory(image, memory, 0) }?;

    /* 3. Record + submit transfer + transition. */
    let cb_alloc = vk::CommandBufferAllocateInfo::default()
        .command_pool(cmd_pool)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);
    let cb = unsafe { device.allocate_command_buffers(&cb_alloc) }?[0];
    unsafe {
        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        device.begin_command_buffer(cb, &begin)?;

        /* UNDEFINED → TRANSFER_DST_OPTIMAL */
        let to_dst = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(image)
            .subresource_range(image_subresource());
        device.cmd_pipeline_barrier(
            cb,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[], &[], &[to_dst]);

        /* Copy staging → image */
        let region = vk::BufferImageCopy::default()
            .buffer_offset(0)
            .buffer_row_length(0).buffer_image_height(0)
            .image_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0, base_array_layer: 0, layer_count: 1,
            })
            .image_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
            .image_extent(vk::Extent3D { width, height, depth: 1 });
        device.cmd_copy_buffer_to_image(
            cb, staging_buf, image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL, &[region]);

        /* TRANSFER_DST_OPTIMAL → SHADER_READ_ONLY_OPTIMAL */
        let to_shader = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ)
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(image)
            .subresource_range(image_subresource());
        device.cmd_pipeline_barrier(
            cb,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::DependencyFlags::empty(),
            &[], &[], &[to_shader]);

        device.end_command_buffer(cb)?;

        /* Submit + wait inline. A more elaborate uploader would use
         * its own transfer queue + a fence pool; for the POC the
         * simplicity is worth the latency hit. */
        let cb_arr = [cb];
        let submit = vk::SubmitInfo::default().command_buffers(&cb_arr);
        device.queue_submit(queue, &[submit], vk::Fence::null())?;
        device.queue_wait_idle(queue)?;

        device.free_command_buffers(cmd_pool, &[cb]);
        device.destroy_buffer(staging_buf, None);
        device.free_memory(staging_mem, None);
    }

    /* 4. Image view, for sampling. */
    let view_info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(format)
        .subresource_range(image_subresource());
    let view = unsafe { device.create_image_view(&view_info, None) }?;

    Ok(Texture { image, view, memory, width, height, format })
}

// ── helpers ─────────────────────────────────────────────────────────

fn image_subresource() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0, level_count: 1,
        base_array_layer: 0, layer_count: 1,
    }
}

fn create_buffer(
    device:          &ash::Device,
    mem_props:       &vk::PhysicalDeviceMemoryProperties,
    size:            vk::DeviceSize,
    usage:           vk::BufferUsageFlags,
    props:           vk::MemoryPropertyFlags,
) -> Result<(vk::Buffer, vk::DeviceMemory)> {
    let info = vk::BufferCreateInfo::default()
        .size(size)
        .usage(usage)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let buf = unsafe { device.create_buffer(&info, None) }?;
    let req = unsafe { device.get_buffer_memory_requirements(buf) };
    let mem_type = find_memory_type(mem_props, req.memory_type_bits, props)?;
    let alloc = vk::MemoryAllocateInfo::default()
        .allocation_size(req.size).memory_type_index(mem_type);
    let mem = unsafe { device.allocate_memory(&alloc, None) }?;
    unsafe { device.bind_buffer_memory(buf, mem, 0) }?;
    Ok((buf, mem))
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
