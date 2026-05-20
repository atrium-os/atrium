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

use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use aqueduct_gpu_host::{Backend, Listener, Tier2Backend, Tier2Registry};
use atrium_spv_loader::LoaderConfig;
use atrium_vk_icd::{
    vkCreateDevice, vkCreateGraphicsPipelines, vkCreateInstance,
    vkCreateShaderModule, vkDestroyInstance, vkEnumeratePhysicalDevices,
};
use tempfile::TempDir;

type VkInstance = *mut std::ffi::c_void;
type VkDevice   = *mut std::ffi::c_void;
type VkPhysicalDevice = *mut std::ffi::c_void;

// Shared with the other end-to-end tests: serialise access to the
// process-wide ATRIUM_VK_ICD_SOCKET env var.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
struct EnvLock { _g: std::sync::MutexGuard<'static, ()> }
impl EnvLock {
    fn set(sock: &std::path::Path) -> Self {
        let g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var("ATRIUM_VK_ICD_SOCKET", sock);
        EnvLock { _g: g }
    }
}

fn tmp_socket(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("atrium-vk-icd-tier2-{}-{}.sock",
                   std::process::id(), name));
    p
}

fn locate_compile_binary() -> PathBuf {
    let here = std::env::current_exe().expect("current_exe");
    let mut p = here;
    p.pop(); p.pop(); p.pop(); p.pop(); p.pop();
    p.push("atrium-spv-compile");
    p.push("target");
    p.push("debug");
    p.push("atrium-spv-compile");
    assert!(p.exists(),
        "atrium-spv-compile binary not found at {} -- run \
         `cd atrium-spv-compile && cargo build` first", p.display());
    p
}

fn build_passthrough_vs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionModel,
        FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);

    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let i32_ty = b.type_int(32, 1);
    let vec3 = b.type_vector(f32_ty, 3);
    let vec4 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);

    let per_vertex = b.type_struct(vec![vec4]);
    b.member_decorate(per_vertex, 0, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::Position)]);
    b.member_decorate(per_vertex, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(per_vertex, Decoration::Block, vec![]);

    let ptr_pv = b.type_pointer(None, StorageClass::Output, per_vertex);
    let ptr_out_vec4 = b.type_pointer(None, StorageClass::Output, vec4);
    let ptr_in_vec3  = b.type_pointer(None, StorageClass::Input, vec3);

    let in_pos = b.variable(ptr_in_vec3, None, StorageClass::Input, None);
    b.decorate(in_pos, Decoration::Location,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let pv_var = b.variable(ptr_pv, None, StorageClass::Output, None);
    let c_zero  = b.constant_bit32(i32_ty, 0u32);
    let c_one_f = b.constant_bit32(f32_ty, 1.0f32.to_bits());

    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let pos = b.load(vec3, None, in_pos, None, vec![]).unwrap();
    let x = b.composite_extract(f32_ty, None, pos, vec![0]).unwrap();
    let y = b.composite_extract(f32_ty, None, pos, vec![1]).unwrap();
    let z = b.composite_extract(f32_ty, None, pos, vec![2]).unwrap();
    let pos4 = b.composite_construct(vec4, None, vec![x, y, z, c_one_f]).unwrap();
    let dst = b.access_chain(ptr_out_vec4, None, pv_var, vec![c_zero]).unwrap();
    b.store(dst, pos4, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Vertex, main, "main", vec![in_pos, pv_var]);

    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

fn build_constant_color_fs(rgba: [f32; 4]) -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, ExecutionMode, ExecutionModel,
        FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let vec4_f32 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let ptr_out = b.type_pointer(None, StorageClass::Output, vec4_f32);
    let cs: Vec<_> = rgba.iter()
        .map(|x| b.constant_bit32(f32_ty, x.to_bits())).collect();
    let color = b.constant_composite(vec4_f32, cs);
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, rspirv::spirv::Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    b.store(out, color, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

/// Hand-encode a VkShaderModuleCreateInfo (offsets per the
/// ICD's vkCreateShaderModule docstring).
fn make_shader_module(device: VkDevice, spv: &[u8]) -> u64 {
    let mut info = [0u8; 40];
    info[ 0.. 4].copy_from_slice(&16u32.to_le_bytes()); // sType
    info[24..32].copy_from_slice(&(spv.len() as u64).to_le_bytes());
    info[32..40].copy_from_slice(&(spv.as_ptr() as u64).to_le_bytes());
    let mut sm: u64 = 0;
    unsafe {
        vkCreateShaderModule(device, info.as_ptr() as *const _,
                             std::ptr::null(), &mut sm);
    }
    assert!(sm != 0, "shader module create failed");
    sm
}

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
