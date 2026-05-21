//! WSI present round-trip for tier-2.
//!
//! Renders a hello-triangle into a swapchain ring image and
//! calls `vkQueuePresentKHR`. The Tier2Backend's present hook
//! snapshots the source image's pixels into a per-surface
//! "last presented frame" buffer; the test then reads that
//! buffer back and asserts:
//!
//!   * The presented frame's dimensions match the swapchain
//!     image (8x8).
//!   * The interior pixels of the rendered triangle made it
//!     onto the presented frame (proving render-then-present
//!     is bit-faithful, not just count-only).
//!   * The presented frame is keyed by the surface_id the ICD
//!     resolved from the swapchain.
//!
//! Together with `tier2_render_pixels`, this closes the loop:
//! tier-2 can render to an image AND deliver that image
//! through the WSI present wire. A real Fresco hook-up replaces
//! Tier2Backend's snapshot with a forwarded blit to the
//! compositor.

mod common;

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use aqueduct_gpu_host::{Backend, Listener, Tier2Backend, Tier2Registry};
use atrium_spv_loader::LoaderConfig;
use atrium_vk_icd::{
    vkAllocateCommandBuffers, vkAllocateMemory, vkBeginCommandBuffer,
    vkBindBufferMemory, vkBindImageMemory, vkCmdBeginRenderPass,
    vkCmdBindPipeline, vkCmdBindVertexBuffers, vkCmdDraw,
    vkCmdEndRenderPass, vkCmdSetScissor, vkCmdSetViewport,
    vkCreateAtriumSurfaceEXT, vkCreateBuffer, vkCreateCommandPool,
    vkCreateDevice, vkCreateFramebuffer, vkCreateGraphicsPipelines,
    vkCreateImageView, vkCreateInstance, vkCreateRenderPass,
    vkCreateSwapchainKHR, vkDestroyInstance,
    vkEndCommandBuffer, vkEnumeratePhysicalDevices, vkGetDeviceQueue,
    vkGetImageMemoryRequirements, vkGetSwapchainImagesKHR,
    vkMapMemory, vkQueuePresentKHR, vkQueueSubmit, vkUnmapMemory,
};
use tempfile::TempDir;

use common::{
    EnvLock, VkCommandBuffer, VkDevice, VkInstance, VkPhysicalDevice, VkQueue,
    build_constant_color_fs, build_passthrough_vs, locate_compile_binary,
    make_shader_module, tmp_socket,
};

const WINDOW_ID: u32 = 4242;

#[test]
fn vk_app_renders_into_swapchain_image_then_present_round_trips_pixels() {
    let sock = tmp_socket("present");
    let cache_dir = TempDir::new().unwrap();
    let registry = Arc::new(Tier2Registry::new(LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    }));
    let tier2_backend = Arc::new(Tier2Backend::new(registry.clone()));
    let backend_for_listener: Arc<dyn Backend> = tier2_backend.clone();
    let listener = Listener::bind(&sock, backend_for_listener).unwrap()
        .with_tier2_registry(registry.clone());
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));

    let _env = EnvLock::set(&sock);

    // ── Bootstrap. ──────────────────────────────────────────────
    let mut instance: VkInstance = std::ptr::null_mut();
    unsafe { vkCreateInstance(std::ptr::null(), std::ptr::null(), &mut instance); }
    let mut pds: [VkPhysicalDevice; 1] = [std::ptr::null_mut(); 1];
    let mut cap: u32 = 1;
    unsafe { vkEnumeratePhysicalDevices(instance, &mut cap, pds.as_mut_ptr()); }
    let mut device: VkDevice = std::ptr::null_mut();
    unsafe { vkCreateDevice(pds[0], std::ptr::null(), std::ptr::null(), &mut device); }
    let mut queue: VkQueue = std::ptr::null_mut();
    unsafe { vkGetDeviceQueue(device, 0, 0, &mut queue); }

    // ── Surface + swapchain (8x8 single-image ring). ────────────
    let mut surf_info = [0u8; 24];
    surf_info[ 0.. 4].copy_from_slice(&1_000_310_000u32.to_le_bytes());
    surf_info[20..24].copy_from_slice(&WINDOW_ID.to_le_bytes());
    let mut surface: u64 = 0;
    unsafe {
        vkCreateAtriumSurfaceEXT(instance, surf_info.as_ptr() as *const _,
                                 std::ptr::null(), &mut surface);
    }
    assert!(surface != 0);

    let mut sc_info = [0u8; 104];
    sc_info[ 0.. 4].copy_from_slice(&1000001000u32.to_le_bytes());
    sc_info[20..28].copy_from_slice(&surface.to_le_bytes());
    sc_info[28..32].copy_from_slice(&1u32.to_le_bytes()); // 1 ring image
    sc_info[32..36].copy_from_slice(&37u32.to_le_bytes()); // R8G8B8A8_UNORM
    sc_info[40..44].copy_from_slice(&8u32.to_le_bytes());
    sc_info[44..48].copy_from_slice(&8u32.to_le_bytes());
    sc_info[48..52].copy_from_slice(&1u32.to_le_bytes());
    sc_info[52..56].copy_from_slice(&0x10u32.to_le_bytes()); // COLOR_ATTACHMENT
    let mut swapchain: u64 = 0;
    unsafe {
        vkCreateSwapchainKHR(device, sc_info.as_ptr() as *const _,
                             std::ptr::null(), &mut swapchain);
    }
    assert!(swapchain != 0);

    let mut ring = [0u64; 1];
    let mut n: u32 = 1;
    unsafe { vkGetSwapchainImagesKHR(device, swapchain, &mut n, ring.as_mut_ptr()); }
    assert_eq!(n, 1);
    let color_img = ring[0];

    // Bind memory to the ring image so it lands on the daemon.
    let mut req = ash::vk::MemoryRequirements::default();
    unsafe { vkGetImageMemoryRequirements(device, color_img, &mut req); }
    let mut img_alloc = [0u8; 32];
    img_alloc[0..4].copy_from_slice(&5u32.to_le_bytes());
    img_alloc[16..24].copy_from_slice(&req.size.to_le_bytes());
    let mut imem: u64 = 0;
    unsafe {
        vkAllocateMemory(device, img_alloc.as_ptr() as *const _,
                         std::ptr::null(), &mut imem);
    }
    unsafe { vkBindImageMemory(device, color_img, imem, 0); }

    // Image view + render pass + framebuffer.
    let mut view_info = [0u8; 80];
    view_info[ 0.. 4].copy_from_slice(&15u32.to_le_bytes());
    view_info[24..32].copy_from_slice(&color_img.to_le_bytes());
    let mut color_view: u64 = 0;
    unsafe {
        vkCreateImageView(device, view_info.as_ptr() as *const _,
                          std::ptr::null(), &mut color_view);
    }

    let mut rp_info = [0u8; 64];
    rp_info[0..4].copy_from_slice(&38u32.to_le_bytes());
    let mut render_pass: u64 = 0;
    unsafe {
        vkCreateRenderPass(device, rp_info.as_ptr() as *const _,
                           std::ptr::null(), &mut render_pass);
    }

    let mut fb_info = [0u8; 64];
    fb_info[ 0.. 4].copy_from_slice(&37u32.to_le_bytes());
    fb_info[24..32].copy_from_slice(&render_pass.to_le_bytes());
    fb_info[32..36].copy_from_slice(&1u32.to_le_bytes());
    let atts = [color_view];
    fb_info[40..48].copy_from_slice(&(atts.as_ptr() as u64).to_le_bytes());
    fb_info[48..52].copy_from_slice(&8u32.to_le_bytes());
    fb_info[52..56].copy_from_slice(&8u32.to_le_bytes());
    let mut framebuffer: u64 = 0;
    unsafe {
        vkCreateFramebuffer(device, fb_info.as_ptr() as *const _,
                            std::ptr::null(), &mut framebuffer);
    }

    // Shader modules + vertex buffer.
    let vs_spv = build_passthrough_vs();
    let fs_spv = build_constant_color_fs([0.2, 0.8, 0.4, 1.0]);
    let vs_mod = make_shader_module(device, &vs_spv);
    let fs_mod = make_shader_module(device, &fs_spv);

    let mut buf_info = [0u8; 56];
    buf_info[ 0.. 4].copy_from_slice(&12u32.to_le_bytes());
    buf_info[24..32].copy_from_slice(&(48u64).to_le_bytes());
    buf_info[32..36].copy_from_slice(&0x80u32.to_le_bytes()); // VERTEX
    let mut vbuf: u64 = 0;
    unsafe { vkCreateBuffer(device, buf_info.as_ptr() as *const _, std::ptr::null(), &mut vbuf); }

    let mut alloc = [0u8; 32];
    alloc[ 0.. 4].copy_from_slice(&5u32.to_le_bytes());
    alloc[16..24].copy_from_slice(&(48u64).to_le_bytes());
    let mut vmem: u64 = 0;
    unsafe { vkAllocateMemory(device, alloc.as_ptr() as *const _, std::ptr::null(), &mut vmem); }
    unsafe { vkBindBufferMemory(device, vbuf, vmem, 0); }

    let mut p: *mut std::ffi::c_void = std::ptr::null_mut();
    unsafe { vkMapMemory(device, vmem, 0, u64::MAX, 0, &mut p); }
    let dst = unsafe { std::slice::from_raw_parts_mut(p as *mut u8, 36) };
    let mut off = 0;
    for v in [[-0.5_f32, -0.5, 0.0], [0.5, -0.5, 0.0], [0.0, 0.5, 0.0]] {
        for f in v {
            dst[off..off + 4].copy_from_slice(&f.to_le_bytes());
            off += 4;
        }
    }
    unsafe { vkUnmapMemory(device, vmem); }

    // Graphics pipeline.
    use ash::vk;
    use ash::vk::Handle;
    let vs_stage = vk::PipelineShaderStageCreateInfo {
        s_type: vk::StructureType::PIPELINE_SHADER_STAGE_CREATE_INFO,
        p_next: std::ptr::null(),
        flags: vk::PipelineShaderStageCreateFlags::empty(),
        stage: vk::ShaderStageFlags::VERTEX,
        module: vk::ShaderModule::from_raw(vs_mod),
        p_name: b"main\0".as_ptr() as *const i8,
        p_specialization_info: std::ptr::null(),
        _marker: std::marker::PhantomData,
    };
    let fs_stage = vk::PipelineShaderStageCreateInfo {
        s_type: vk::StructureType::PIPELINE_SHADER_STAGE_CREATE_INFO,
        p_next: std::ptr::null(),
        flags: vk::PipelineShaderStageCreateFlags::empty(),
        stage: vk::ShaderStageFlags::FRAGMENT,
        module: vk::ShaderModule::from_raw(fs_mod),
        p_name: b"main\0".as_ptr() as *const i8,
        p_specialization_info: std::ptr::null(),
        _marker: std::marker::PhantomData,
    };
    let stages = [vs_stage, fs_stage];
    let bindings = [vk::VertexInputBindingDescription {
        binding: 0, stride: 12, input_rate: vk::VertexInputRate::VERTEX,
    }];
    let attributes = [vk::VertexInputAttributeDescription {
        location: 0, binding: 0,
        format: vk::Format::R32G32B32_SFLOAT, offset: 0,
    }];
    let vi = vk::PipelineVertexInputStateCreateInfo {
        s_type: vk::StructureType::PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO,
        p_next: std::ptr::null(),
        flags: vk::PipelineVertexInputStateCreateFlags::empty(),
        vertex_binding_description_count: bindings.len() as u32,
        p_vertex_binding_descriptions: bindings.as_ptr(),
        vertex_attribute_description_count: attributes.len() as u32,
        p_vertex_attribute_descriptions: attributes.as_ptr(),
        _marker: std::marker::PhantomData,
    };
    let info = vk::GraphicsPipelineCreateInfo {
        s_type: vk::StructureType::GRAPHICS_PIPELINE_CREATE_INFO,
        p_next: std::ptr::null(),
        flags: vk::PipelineCreateFlags::empty(),
        stage_count: stages.len() as u32,
        p_stages: stages.as_ptr(),
        p_vertex_input_state: &vi,
        p_input_assembly_state: std::ptr::null(),
        p_tessellation_state: std::ptr::null(),
        p_viewport_state: std::ptr::null(),
        p_rasterization_state: std::ptr::null(),
        p_multisample_state: std::ptr::null(),
        p_depth_stencil_state: std::ptr::null(),
        p_color_blend_state: std::ptr::null(),
        p_dynamic_state: std::ptr::null(),
        layout: vk::PipelineLayout::null(),
        render_pass: vk::RenderPass::null(),
        subpass: 0,
        base_pipeline_handle: vk::Pipeline::null(),
        base_pipeline_index: 0,
        _marker: std::marker::PhantomData,
    };
    let infos = [info];
    let mut pipeline: u64 = 0;
    unsafe {
        vkCreateGraphicsPipelines(
            device, 0, 1,
            infos.as_ptr() as *const std::ffi::c_void,
            std::ptr::null(), &mut pipeline,
        );
    }

    // Command buffer.
    let mut pool: u64 = 0;
    unsafe { vkCreateCommandPool(device, std::ptr::null(), std::ptr::null(), &mut pool); }
    let mut cb_info = [0u8; 40];
    cb_info[0..4].copy_from_slice(&40u32.to_le_bytes());
    cb_info[16..24].copy_from_slice(&pool.to_le_bytes());
    cb_info[28..32].copy_from_slice(&1u32.to_le_bytes());
    let mut cbs: [VkCommandBuffer; 1] = [std::ptr::null_mut(); 1];
    unsafe { vkAllocateCommandBuffers(device, cb_info.as_ptr() as *const _, cbs.as_mut_ptr()); }
    let cb = cbs[0];

    // Record: render into ring[0].
    unsafe { vkBeginCommandBuffer(cb, std::ptr::null()); }
    let mut rpb = [0u8; 64];
    rpb[ 0.. 4].copy_from_slice(&43u32.to_le_bytes());
    rpb[16..24].copy_from_slice(&render_pass.to_le_bytes());
    rpb[24..32].copy_from_slice(&framebuffer.to_le_bytes());
    rpb[48..52].copy_from_slice(&1u32.to_le_bytes());
    let clear: [f32; 4] = [0.0, 0.0, 0.0, 0.0];
    rpb[56..64].copy_from_slice(&(clear.as_ptr() as u64).to_le_bytes());
    unsafe { vkCmdBeginRenderPass(cb, rpb.as_ptr() as *const _, 0); }
    unsafe { vkCmdBindPipeline(cb, 0, pipeline); }
    let vp = vk::Viewport { x: 0.0, y: 0.0, width: 8.0, height: 8.0,
                            min_depth: 0.0, max_depth: 1.0 };
    unsafe { vkCmdSetViewport(cb, 0, 1, &vp); }
    let sc = vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent: vk::Extent2D { width: 8, height: 8 },
    };
    unsafe { vkCmdSetScissor(cb, 0, 1, &sc); }
    let vbufs = [vbuf];
    let voffs = [0u64];
    unsafe { vkCmdBindVertexBuffers(cb, 0, 1, vbufs.as_ptr(), voffs.as_ptr()); }
    unsafe { vkCmdDraw(cb, 3, 1, 0, 0); }
    unsafe { vkCmdEndRenderPass(cb); }
    unsafe { vkEndCommandBuffer(cb); }

    // Submit.
    let mut submit = [0u8; 72];
    submit[0..4].copy_from_slice(&4u32.to_le_bytes());
    submit[40..44].copy_from_slice(&1u32.to_le_bytes());
    let cb_arr = [cb];
    submit[48..56].copy_from_slice(&(cb_arr.as_ptr() as u64).to_le_bytes());
    unsafe {
        vkQueueSubmit(queue, 1, submit.as_ptr() as *const _, std::ptr::null_mut());
    }

    thread::sleep(Duration::from_millis(300));

    // ── Present ring[0]. ────────────────────────────────────────
    let sc_arr = [swapchain];
    let idx_arr = [0u32];
    let mut present_info = [0u8; 64];
    present_info[ 0.. 4].copy_from_slice(&1000001001u32.to_le_bytes());
    present_info[32..36].copy_from_slice(&1u32.to_le_bytes());
    present_info[40..48].copy_from_slice(&(sc_arr.as_ptr() as u64).to_le_bytes());
    present_info[48..56].copy_from_slice(&(idx_arr.as_ptr() as u64).to_le_bytes());
    let pr = unsafe { vkQueuePresentKHR(queue, present_info.as_ptr() as *const _) };
    assert_eq!(pr, 0);

    thread::sleep(Duration::from_millis(150));

    // ── Verify the present hook snapshotted the rendered frame. ─
    let frame = tier2_backend.last_presented_frame(WINDOW_ID as u64)
        .expect("Tier2Backend should have snapshotted the present");
    assert_eq!(frame.width, 8);
    assert_eq!(frame.height, 8);
    assert_eq!(frame.pixels.len(), 8 * 8 * 4);

    let px = |x: usize, y: usize| -> [u8; 4] {
        let i = (y * 8 + x) * 4;
        [frame.pixels[i], frame.pixels[i+1], frame.pixels[i+2], frame.pixels[i+3]]
    };
    // FS color (0.2, 0.8, 0.4, 1.0) -> (51, 204, 102, 255).
    let green = [51u8, 204, 102, 255];
    assert_eq!(px(2, 2), green, "(2,2) = {:?}", px(2, 2));
    assert_eq!(px(4, 2), green);
    assert_eq!(px(3, 3), green);
    assert_eq!(px(4, 4), green);
    assert_eq!(px(0, 0), [0, 0, 0, 0]);
    assert_eq!(px(7, 7), [0, 0, 0, 0]);

    unsafe { vkDestroyInstance(instance, std::ptr::null()); }
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}
