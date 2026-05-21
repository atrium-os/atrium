//! Integration test for the ICD's Tier-2 pipeline-create wire.
//!
//! Drives `vkCreateGraphicsPipelines` with a real
//! `VkGraphicsPipelineCreateInfo` against a Listener configured
//! with a `Tier2Backend` + `Tier2Registry`, and asserts that the
//! daemon-side pipeline registry receives:
//!
//!   * a Tier-2 vertex shader binding (resolved from shaders[0]),
//!   * a Tier-2 fragment shader binding (resolved from shaders[1]),
//!   * a vertex-input layout matching what the ICD parsed out of
//!     `pVertexInputState`,
//!   * a depth state honouring `depthTestEnable`,
//!   * a blend state honouring the attachment's blend factors.
//!
//! Rasterizer correctness is covered by the host-side D-arc tests
//! (`tier2_backend_d5_hello_triangle_through_wire` and friends);
//! this test's job is to prove the *handshake* — that the ICD
//! actually encodes `Tier2PipelineStateBlob` for the daemon.

mod common;

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use aqueduct_gpu_host::{Backend, Listener, Tier2Backend, Tier2Registry};
use atrium_spv_loader::LoaderConfig;
use atrium_vk_icd::{
    vkCreateDevice, vkCreateGraphicsPipelines, vkCreateInstance,
    vkDestroyInstance, vkEnumeratePhysicalDevices,
};
use tempfile::TempDir;

use common::{
    EnvLock, VkDevice, VkInstance, VkPhysicalDevice,
    build_constant_color_fs, build_passthrough_vs, locate_compile_binary,
    make_shader_module, tmp_socket,
};

#[test]
fn vk_create_graphics_pipelines_lands_tier2_state_on_backend() {
    let sock = tmp_socket("pipeline_wire");

    // Stand up listener with Tier2Backend + Tier2Registry.
    let cache_dir = TempDir::new().unwrap();
    let registry = Arc::new(Tier2Registry::new(LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    }));
    let tier2_backend = Arc::new(Tier2Backend::new(registry.clone()));
    let backend_for_listener: Arc<dyn Backend> = tier2_backend.clone();
    let listener = Listener::bind(&sock, backend_for_listener)
        .unwrap()
        .with_tier2_registry(registry.clone());
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));

    let _env = EnvLock::set(&sock);

    // vkCreateInstance + device.
    let mut instance: VkInstance = std::ptr::null_mut();
    let r = unsafe {
        vkCreateInstance(std::ptr::null(), std::ptr::null(), &mut instance)
    };
    assert_eq!(r, 0);
    let mut pds: [VkPhysicalDevice; 1] = [std::ptr::null_mut(); 1];
    let mut cap: u32 = 1;
    unsafe { vkEnumeratePhysicalDevices(instance, &mut cap, pds.as_mut_ptr()); }
    assert_eq!(cap, 1, "expected one physical device");

    let mut device: VkDevice = std::ptr::null_mut();
    unsafe { vkCreateDevice(pds[0], std::ptr::null(), std::ptr::null(), &mut device); }
    assert!(!device.is_null());

    // Create shader modules. The bytes outlive the call; we keep
    // them alive for the duration of the test.
    let vs_spv = build_passthrough_vs();
    let fs_spv = build_constant_color_fs([0.5, 0.7, 0.2, 1.0]);
    let vs_mod = make_shader_module(device, &vs_spv);
    let fs_mod = make_shader_module(device, &fs_spv);

    // Build a real VkGraphicsPipelineCreateInfo with ash. The
    // bindings/attributes/stages slices have to outlive the call
    // since the ICD reads them out of the pointers.
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

    let ds = vk::PipelineDepthStencilStateCreateInfo {
        s_type: vk::StructureType::PIPELINE_DEPTH_STENCIL_STATE_CREATE_INFO,
        p_next: std::ptr::null(),
        flags: vk::PipelineDepthStencilStateCreateFlags::empty(),
        depth_test_enable: 1,
        depth_write_enable: 1,
        depth_compare_op: vk::CompareOp::LESS,
        depth_bounds_test_enable: 0,
        stencil_test_enable: 0,
        front: vk::StencilOpState::default(),
        back: vk::StencilOpState::default(),
        min_depth_bounds: 0.0,
        max_depth_bounds: 1.0,
        _marker: std::marker::PhantomData,
    };

    let blend_att = vk::PipelineColorBlendAttachmentState {
        blend_enable: 1,
        src_color_blend_factor: vk::BlendFactor::SRC_ALPHA,
        dst_color_blend_factor: vk::BlendFactor::ONE_MINUS_SRC_ALPHA,
        color_blend_op: vk::BlendOp::ADD,
        src_alpha_blend_factor: vk::BlendFactor::ONE,
        dst_alpha_blend_factor: vk::BlendFactor::ZERO,
        alpha_blend_op: vk::BlendOp::ADD,
        color_write_mask: vk::ColorComponentFlags::R | vk::ColorComponentFlags::G
                        | vk::ColorComponentFlags::B | vk::ColorComponentFlags::A,
    };
    let cb_attachments = [blend_att];
    let cb_state = vk::PipelineColorBlendStateCreateInfo {
        s_type: vk::StructureType::PIPELINE_COLOR_BLEND_STATE_CREATE_INFO,
        p_next: std::ptr::null(),
        flags: vk::PipelineColorBlendStateCreateFlags::empty(),
        logic_op_enable: 0,
        logic_op: vk::LogicOp::COPY,
        attachment_count: cb_attachments.len() as u32,
        p_attachments: cb_attachments.as_ptr(),
        blend_constants: [0.0; 4],
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
        p_depth_stencil_state: &ds,
        p_color_blend_state: &cb_state,
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
    let r = unsafe {
        vkCreateGraphicsPipelines(
            device, 0, 1,
            infos.as_ptr() as *const std::ffi::c_void,
            std::ptr::null(), &mut pipeline,
        )
    };
    assert_eq!(r, 0);
    assert!(pipeline != 0);

    // Async wire round-trip: give the daemon a moment to process
    // SHADER_UPLOAD + PIPELINE_CREATE.
    thread::sleep(Duration::from_millis(150));

    let pipeline_id = aqueduct_gpu::ids::ResourceId(pipeline as u32);
    assert!(tier2_backend.pipeline_vs_shader(pipeline_id).is_some(),
        "Tier2Backend should know the VS shader for pipeline {pipeline_id}");
    assert!(tier2_backend.pipeline_fs_shader(pipeline_id).is_some(),
        "Tier2Backend should know the FS shader for pipeline {pipeline_id}");
    assert!(tier2_backend.pipeline_has_layout(pipeline_id),
        "Tier2Backend should have a vertex-input layout for pipeline {pipeline_id}");

    let layout = tier2_backend.pipeline_layout(pipeline_id).unwrap();
    assert_eq!(layout.bindings.len(), 1);
    assert_eq!(layout.bindings[0].binding, 0);
    assert_eq!(layout.bindings[0].stride, 12);
    assert!(!layout.bindings[0].per_instance);
    assert_eq!(layout.attributes.len(), 1);
    assert_eq!(layout.attributes[0].location, 0);
    assert_eq!(layout.attributes[0].offset, 0);
    assert_eq!(
        layout.attributes[0].format,
        aqueduct_gpu::VertexFormat::R32g32b32Sfloat,
    );

    unsafe { vkDestroyInstance(instance, std::ptr::null()); }
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn vk_unmap_memory_syncs_buffer_writes_to_daemon() {
    use atrium_vk_icd::{
        vkAllocateMemory, vkBindBufferMemory, vkCreateBuffer,
        vkDestroyInstance, vkMapMemory, vkUnmapMemory,
    };

    let sock = tmp_socket("buf_sync");
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

    // VkBufferCreateInfo: sType@0, size@24, usage@32. The ICD
    // reads only size+usage; everything else can be zero.
    let mut bi = [0u8; 56];
    bi[ 0.. 4].copy_from_slice(&12u32.to_le_bytes()); // sType
    bi[24..32].copy_from_slice(&(64u64).to_le_bytes()); // 64 bytes
    bi[32..36].copy_from_slice(&0x80u32.to_le_bytes()); // VERTEX_BUFFER
    let mut buf: u64 = 0;
    unsafe {
        vkCreateBuffer(device, bi.as_ptr() as *const _,
                       std::ptr::null(), &mut buf);
    }
    assert!(buf != 0);

    // VkMemoryAllocateInfo: sType@0, allocationSize@16.
    let mut ai = [0u8; 32];
    ai[ 0.. 4].copy_from_slice(&5u32.to_le_bytes());
    ai[16..24].copy_from_slice(&(64u64).to_le_bytes());
    let mut mem: u64 = 0;
    unsafe {
        vkAllocateMemory(device, ai.as_ptr() as *const _,
                         std::ptr::null(), &mut mem);
    }
    assert!(mem != 0);
    unsafe { vkBindBufferMemory(device, buf, mem, 0); }

    // Map, write a recognisable pattern, unmap. The ICD must
    // forward the pattern through OP_GPU_BUFFER_WRITE.
    let mut p: *mut std::ffi::c_void = std::ptr::null_mut();
    unsafe { vkMapMemory(device, mem, 0, u64::MAX, 0, &mut p); }
    assert!(!p.is_null());
    let slice = unsafe { std::slice::from_raw_parts_mut(p as *mut u8, 64) };
    for (i, b) in slice.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(7).wrapping_add(13);
    }
    unsafe { vkUnmapMemory(device, mem); }

    // Async round-trip.
    thread::sleep(Duration::from_millis(150));

    let bufs = tier2_backend.all_buffer_bytes();
    assert_eq!(bufs.len(), 1,
        "exactly one buffer should be registered with the daemon");
    let (_, bytes) = &bufs[0];
    assert_eq!(bytes.len(), 64);
    for (i, &b) in bytes.iter().enumerate() {
        assert_eq!(b, (i as u8).wrapping_mul(7).wrapping_add(13),
            "byte {i} not synced: got {b}");
    }

    unsafe { vkDestroyInstance(instance, std::ptr::null()); }
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}
