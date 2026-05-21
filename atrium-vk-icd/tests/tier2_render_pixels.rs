//! Full vkApp → Tier-2 → pixels end-to-end test.
//!
//! Drives the same hello-triangle the D.5 host-side test
//! verifies, but entirely through the ICD's C ABI: instance →
//! device → shader modules → vertex buffer (mapped + written +
//! unmapped) → color-attachment image + view → render pass +
//! framebuffer → graphics pipeline (real create-info) →
//! command buffer recording → vkQueueSubmit. After the submit
//! returns, we read the daemon-side image storage out of
//! Tier2Backend and assert the triangle's interior pixels
//! match the FS colour and exterior stays cleared.
//!
//! This is the final wire-correctness gate for the ICD→Tier-2
//! migration: every layer (buffer write sync, pipeline state
//! blob, draw walker, frame-walker dispatch, pixel write) is
//! exercised in one pass.

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
    vkCreateBuffer, vkCreateCommandPool, vkCreateDevice,
    vkCreateFramebuffer, vkCreateGraphicsPipelines, vkCreateImage,
    vkCreateImageView, vkCreateInstance, vkCreateRenderPass,
    vkDestroyInstance, vkEndCommandBuffer,
    vkEnumeratePhysicalDevices, vkGetDeviceQueue,
    vkGetImageMemoryRequirements, vkMapMemory, vkQueueSubmit,
    vkUnmapMemory,
};
use tempfile::TempDir;

use common::{
    EnvLock, VkCommandBuffer, VkDevice, VkInstance, VkPhysicalDevice, VkQueue,
    build_constant_color_fs, build_passthrough_vs, locate_compile_binary,
    make_shader_module, tmp_socket,
};

#[test]
fn vk_app_hello_triangle_reaches_tier2_pixels() {
    let sock = tmp_socket("render_pixels");
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

    // ── Bootstrap: instance → device → queue. ───────────────────
    let mut instance: VkInstance = std::ptr::null_mut();
    unsafe { vkCreateInstance(std::ptr::null(), std::ptr::null(), &mut instance); }
    let mut pds: [VkPhysicalDevice; 1] = [std::ptr::null_mut(); 1];
    let mut cap: u32 = 1;
    unsafe { vkEnumeratePhysicalDevices(instance, &mut cap, pds.as_mut_ptr()); }
    let mut device: VkDevice = std::ptr::null_mut();
    unsafe { vkCreateDevice(pds[0], std::ptr::null(), std::ptr::null(), &mut device); }
    let mut queue: VkQueue = std::ptr::null_mut();
    unsafe { vkGetDeviceQueue(device, 0, 0, &mut queue); }

    // ── Shader modules. ─────────────────────────────────────────
    let vs_spv = build_passthrough_vs();
    let fs_spv = build_constant_color_fs([1.0, 0.2, 0.2, 1.0]);
    let vs_mod = make_shader_module(device, &vs_spv);
    let fs_mod = make_shader_module(device, &fs_spv);

    // ── Vertex buffer: 3 NDC vec3s, mapped/written/unmapped. ────
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

    // ── Render target: 8x8 RGBA8 image + view. ──────────────────
    let mut img_info = [0u8; 88];
    img_info[ 0.. 4].copy_from_slice(&14u32.to_le_bytes());
    img_info[24..28].copy_from_slice(&37u32.to_le_bytes()); // R8G8B8A8_UNORM
    img_info[28..32].copy_from_slice(&8u32.to_le_bytes());
    img_info[32..36].copy_from_slice(&8u32.to_le_bytes());
    img_info[36..40].copy_from_slice(&1u32.to_le_bytes());
    img_info[40..44].copy_from_slice(&1u32.to_le_bytes());
    img_info[44..48].copy_from_slice(&1u32.to_le_bytes());
    img_info[56..60].copy_from_slice(&0x10u32.to_le_bytes()); // COLOR_ATTACHMENT
    let mut color_img: u64 = 0;
    unsafe { vkCreateImage(device, img_info.as_ptr() as *const _, std::ptr::null(), &mut color_img); }
    let mut req = ash::vk::MemoryRequirements::default();
    unsafe { vkGetImageMemoryRequirements(device, color_img, &mut req); }
    let mut imem_alloc = [0u8; 32];
    imem_alloc[0..4].copy_from_slice(&5u32.to_le_bytes());
    imem_alloc[16..24].copy_from_slice(&req.size.to_le_bytes());
    let mut imem: u64 = 0;
    unsafe { vkAllocateMemory(device, imem_alloc.as_ptr() as *const _, std::ptr::null(), &mut imem); }
    unsafe { vkBindImageMemory(device, color_img, imem, 0); }
    let mut view_info = [0u8; 80];
    view_info[ 0.. 4].copy_from_slice(&15u32.to_le_bytes());
    view_info[24..32].copy_from_slice(&color_img.to_le_bytes());
    let mut color_view: u64 = 0;
    unsafe { vkCreateImageView(device, view_info.as_ptr() as *const _, std::ptr::null(), &mut color_view); }

    // ── Render pass + framebuffer. ──────────────────────────────
    let mut rp_info = [0u8; 64];
    rp_info[0..4].copy_from_slice(&38u32.to_le_bytes());
    let mut render_pass: u64 = 0;
    unsafe { vkCreateRenderPass(device, rp_info.as_ptr() as *const _, std::ptr::null(), &mut render_pass); }

    let mut fb_info = [0u8; 64];
    fb_info[ 0.. 4].copy_from_slice(&37u32.to_le_bytes());
    fb_info[24..32].copy_from_slice(&render_pass.to_le_bytes());
    fb_info[32..36].copy_from_slice(&1u32.to_le_bytes());
    let atts = [color_view];
    fb_info[40..48].copy_from_slice(&(atts.as_ptr() as u64).to_le_bytes());
    fb_info[48..52].copy_from_slice(&8u32.to_le_bytes());
    fb_info[52..56].copy_from_slice(&8u32.to_le_bytes());
    let mut framebuffer: u64 = 0;
    unsafe { vkCreateFramebuffer(device, fb_info.as_ptr() as *const _, std::ptr::null(), &mut framebuffer); }

    // ── Graphics pipeline with real create-info. ────────────────
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
    assert!(pipeline != 0);

    // ── Command pool + cmdbuf. ──────────────────────────────────
    let mut pool: u64 = 0;
    unsafe { vkCreateCommandPool(device, std::ptr::null(), std::ptr::null(), &mut pool); }
    let mut cb_info = [0u8; 40];
    cb_info[0..4].copy_from_slice(&40u32.to_le_bytes());
    cb_info[16..24].copy_from_slice(&pool.to_le_bytes());
    cb_info[28..32].copy_from_slice(&1u32.to_le_bytes());
    let mut cbs: [VkCommandBuffer; 1] = [std::ptr::null_mut(); 1];
    unsafe { vkAllocateCommandBuffers(device, cb_info.as_ptr() as *const _, cbs.as_mut_ptr()); }
    let cb = cbs[0];

    // ── Record the frame. ───────────────────────────────────────
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

    // ── Submit. ─────────────────────────────────────────────────
    let mut submit = [0u8; 72];
    submit[0..4].copy_from_slice(&4u32.to_le_bytes());
    submit[40..44].copy_from_slice(&1u32.to_le_bytes());
    let cb_arr = [cb];
    submit[48..56].copy_from_slice(&(cb_arr.as_ptr() as u64).to_le_bytes());
    let r = unsafe {
        vkQueueSubmit(queue, 1, submit.as_ptr() as *const _, std::ptr::null_mut())
    };
    assert_eq!(r, 0);

    // Give the daemon time to process upload + frame.
    thread::sleep(Duration::from_millis(300));

    // ── Read back image pixels from Tier2Backend. The daemon-
    // assigned image_id isn't directly known to the test; we
    // ask the backend for its registered images and find the 8x8.
    let mut pixels: Option<Vec<u8>> = None;
    for (id, w, h) in tier2_backend_image_dims(&tier2_backend) {
        if w == 8 && h == 8 {
            pixels = tier2_backend.read_image_pixels(id);
            break;
        }
    }
    let pixels = pixels.expect("Tier2Backend should have registered an 8x8 image");
    assert_eq!(pixels.len(), 8 * 8 * 4);

    let px = |x: usize, y: usize| -> [u8; 4] {
        let i = (y * 8 + x) * 4;
        [pixels[i], pixels[i+1], pixels[i+2], pixels[i+3]]
    };
    // Interior pixels of the same NDC triangle the D.5 test
    // verifies; FS color (1.0, 0.2, 0.2, 1.0) -> (255, 51, 51, 255).
    let red = [255u8, 51, 51, 255];
    assert_eq!(px(2, 2), red, "(2,2) = {:?}", px(2, 2));
    assert_eq!(px(4, 2), red, "(4,2) = {:?}", px(4, 2));
    assert_eq!(px(3, 3), red, "(3,3) = {:?}", px(3, 3));
    assert_eq!(px(4, 4), red, "(4,4) = {:?}", px(4, 4));
    // Exterior pixels stay at the cleared background.
    assert_eq!(px(0, 0), [0, 0, 0, 0]);
    assert_eq!(px(7, 7), [0, 0, 0, 0]);

    unsafe { vkDestroyInstance(instance, std::ptr::null()); }
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

/// Tier2Backend doesn't expose per-image dimensions directly,
/// but the read_image_pixels output length tells us width *
/// height * 4. We don't actually need the dims here -- there's
/// only one 8x8 image in this test -- but we wrap the iteration
/// for readability + so a future caller can pick by size.
fn tier2_backend_image_dims(_backend: &Tier2Backend)
    -> Vec<(aqueduct_gpu::ids::ResourceId, u32, u32)>
{
    // The Tier2Backend.all_buffer_bytes helper doesn't exist for
    // images yet; fall back to scanning every reasonable id in
    // the IcdRuntime namespace and probing. The first registered
    // image in this test gets a low ID, so the scan is short.
    // We try the first 32 ids in IcdRuntime (the only namespace
    // the ICD assigns for tier-2 images), looking for one that
    // reads back as 8*8*4 = 256 bytes.
    let mut out = Vec::new();
    for raw in 1u32..64 {
        let id = aqueduct_gpu::ids::ResourceId::new(
            aqueduct_gpu::ids::IdNamespace::IcdRuntime, raw);
        if let Some(px) = _backend.read_image_pixels(id) {
            // Infer (w,h) -- the test only registers one 8x8 so
            // 256 bytes => 8x8 is unique here.
            let n = (px.len() / 4) as u32;
            if n == 64 { out.push((id, 8, 8)); }
        }
    }
    out
}

/// Fragment shader that reads a `vec4` from push-constants at
/// offset 0 and writes it as `out_color`. Exercises the
/// vkCmdPushConstants -> FrameOp::PushConstants -> tier-2
/// dispatch_draw -> DrawTriangle.push_constants -> Cranelift
/// (Fragment, PushConstant) param mapping path end-to-end.
fn build_pc_color_fs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode, ExecutionModel,
        FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);

    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let f32_ty = b.type_float(32, None);
    let vec4   = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);

    // push_constant block { vec4 color; } at offset 0.
    let pc_block = b.type_struct(vec![vec4]);
    b.decorate(pc_block, Decoration::Block, vec![]);
    b.member_decorate(pc_block, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_pc_block = b.type_pointer(None, StorageClass::PushConstant, pc_block);
    let ptr_pc_vec4  = b.type_pointer(None, StorageClass::PushConstant, vec4);
    let pc_var = b.variable(ptr_pc_block, None, StorageClass::PushConstant, None);

    let ptr_out = b.type_pointer(None, StorageClass::Output, vec4);
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let c_zero = b.constant_bit32(u32_ty, 0);

    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let src = b.access_chain(ptr_pc_vec4, None, pc_var, vec![c_zero]).unwrap();
    let color = b.load(vec4, None, src, None, vec![]).unwrap();
    b.store(out, color, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![out, pc_var]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn vk_app_push_constants_flow_into_fragment_shader() {
    use atrium_vk_icd::{vkCmdPushConstants, vkCreatePipelineLayout};

    let sock = tmp_socket("pc");
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

    let mut instance: VkInstance = std::ptr::null_mut();
    unsafe { vkCreateInstance(std::ptr::null(), std::ptr::null(), &mut instance); }
    let mut pds: [VkPhysicalDevice; 1] = [std::ptr::null_mut(); 1];
    let mut cap: u32 = 1;
    unsafe { vkEnumeratePhysicalDevices(instance, &mut cap, pds.as_mut_ptr()); }
    let mut device: VkDevice = std::ptr::null_mut();
    unsafe { vkCreateDevice(pds[0], std::ptr::null(), std::ptr::null(), &mut device); }
    let mut queue: VkQueue = std::ptr::null_mut();
    unsafe { vkGetDeviceQueue(device, 0, 0, &mut queue); }

    let vs_spv = build_passthrough_vs();
    let fs_spv = build_pc_color_fs();
    let vs_mod = make_shader_module(device, &vs_spv);
    let fs_mod = make_shader_module(device, &fs_spv);

    // Vertex buffer with 3 NDC verts.
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

    // Color attachment image.
    let mut img_info = [0u8; 88];
    img_info[ 0.. 4].copy_from_slice(&14u32.to_le_bytes());
    img_info[24..28].copy_from_slice(&37u32.to_le_bytes());
    img_info[28..32].copy_from_slice(&8u32.to_le_bytes());
    img_info[32..36].copy_from_slice(&8u32.to_le_bytes());
    img_info[36..40].copy_from_slice(&1u32.to_le_bytes());
    img_info[40..44].copy_from_slice(&1u32.to_le_bytes());
    img_info[44..48].copy_from_slice(&1u32.to_le_bytes());
    img_info[56..60].copy_from_slice(&0x10u32.to_le_bytes());
    let mut color_img: u64 = 0;
    unsafe { vkCreateImage(device, img_info.as_ptr() as *const _, std::ptr::null(), &mut color_img); }
    let mut req = ash::vk::MemoryRequirements::default();
    unsafe { vkGetImageMemoryRequirements(device, color_img, &mut req); }
    let mut imem_alloc = [0u8; 32];
    imem_alloc[0..4].copy_from_slice(&5u32.to_le_bytes());
    imem_alloc[16..24].copy_from_slice(&req.size.to_le_bytes());
    let mut imem: u64 = 0;
    unsafe { vkAllocateMemory(device, imem_alloc.as_ptr() as *const _, std::ptr::null(), &mut imem); }
    unsafe { vkBindImageMemory(device, color_img, imem, 0); }
    let mut view_info = [0u8; 80];
    view_info[ 0.. 4].copy_from_slice(&15u32.to_le_bytes());
    view_info[24..32].copy_from_slice(&color_img.to_le_bytes());
    let mut color_view: u64 = 0;
    unsafe { vkCreateImageView(device, view_info.as_ptr() as *const _, std::ptr::null(), &mut color_view); }

    let mut rp_info = [0u8; 64];
    rp_info[0..4].copy_from_slice(&38u32.to_le_bytes());
    let mut render_pass: u64 = 0;
    unsafe { vkCreateRenderPass(device, rp_info.as_ptr() as *const _, std::ptr::null(), &mut render_pass); }

    let mut fb_info = [0u8; 64];
    fb_info[ 0.. 4].copy_from_slice(&37u32.to_le_bytes());
    fb_info[24..32].copy_from_slice(&render_pass.to_le_bytes());
    fb_info[32..36].copy_from_slice(&1u32.to_le_bytes());
    let atts = [color_view];
    fb_info[40..48].copy_from_slice(&(atts.as_ptr() as u64).to_le_bytes());
    fb_info[48..52].copy_from_slice(&8u32.to_le_bytes());
    fb_info[52..56].copy_from_slice(&8u32.to_le_bytes());
    let mut framebuffer: u64 = 0;
    unsafe { vkCreateFramebuffer(device, fb_info.as_ptr() as *const _, std::ptr::null(), &mut framebuffer); }

    // Pipeline with vertex input.
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
    let mut pl_layout: u64 = 0;
    unsafe { vkCreatePipelineLayout(device, std::ptr::null(), std::ptr::null(), &mut pl_layout); }

    // Cmd buf.
    let mut pool: u64 = 0;
    unsafe { vkCreateCommandPool(device, std::ptr::null(), std::ptr::null(), &mut pool); }
    let mut cb_info = [0u8; 40];
    cb_info[0..4].copy_from_slice(&40u32.to_le_bytes());
    cb_info[16..24].copy_from_slice(&pool.to_le_bytes());
    cb_info[28..32].copy_from_slice(&1u32.to_le_bytes());
    let mut cbs: [VkCommandBuffer; 1] = [std::ptr::null_mut(); 1];
    unsafe { vkAllocateCommandBuffers(device, cb_info.as_ptr() as *const _, cbs.as_mut_ptr()); }
    let cb = cbs[0];

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
    // Push the colour BEFORE binding the vertex buffer + drawing.
    // VK_SHADER_STAGE_FRAGMENT_BIT = 0x10.
    let pushed: [f32; 4] = [0.0, 0.5, 0.75, 1.0];
    unsafe {
        vkCmdPushConstants(cb, pl_layout, 0x10, 0, 16,
                           pushed.as_ptr() as *const std::ffi::c_void);
    }
    let vbufs = [vbuf];
    let voffs = [0u64];
    unsafe { vkCmdBindVertexBuffers(cb, 0, 1, vbufs.as_ptr(), voffs.as_ptr()); }
    unsafe { vkCmdDraw(cb, 3, 1, 0, 0); }
    unsafe { vkCmdEndRenderPass(cb); }
    unsafe { vkEndCommandBuffer(cb); }

    let mut submit = [0u8; 72];
    submit[0..4].copy_from_slice(&4u32.to_le_bytes());
    submit[40..44].copy_from_slice(&1u32.to_le_bytes());
    let cb_arr = [cb];
    submit[48..56].copy_from_slice(&(cb_arr.as_ptr() as u64).to_le_bytes());
    unsafe {
        vkQueueSubmit(queue, 1, submit.as_ptr() as *const _, std::ptr::null_mut());
    }
    thread::sleep(Duration::from_millis(300));

    // Find the 8x8 image and verify interior pixels match the
    // pushed (0.0, 0.5, 0.75, 1.0) -> (0, 128, 191, 255).
    let mut pixels: Option<Vec<u8>> = None;
    for raw in 1u32..64 {
        let id = aqueduct_gpu::ids::ResourceId::new(
            aqueduct_gpu::ids::IdNamespace::IcdRuntime, raw);
        if let Some(px) = tier2_backend.read_image_pixels(id) {
            if px.len() == 8 * 8 * 4 { pixels = Some(px); break; }
        }
    }
    let pixels = pixels.expect("rendered image should be on Tier2Backend");
    let px = |x: usize, y: usize| -> [u8; 4] {
        let i = (y * 8 + x) * 4;
        [pixels[i], pixels[i+1], pixels[i+2], pixels[i+3]]
    };
    // (0.0, 0.5, 0.75, 1.0) under the rasterizer's
    // `f32 * 255.0 + 0.5 as u8` quantisation -> (0, 128, 191, 255).
    let expected = [0u8, 128, 191, 255];
    assert_eq!(px(3, 3), expected, "(3,3) = {:?}", px(3, 3));
    assert_eq!(px(4, 4), expected, "(4,4) = {:?}", px(4, 4));
    assert_eq!(px(0, 0), [0, 0, 0, 0]);

    unsafe { atrium_vk_icd::vkDestroyInstance(instance, std::ptr::null()); }
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}
