//! Compute pipeline + dispatch end-to-end through tier-2.
//!
//! Four tests, each layering one more capability:
//!
//!   * `vk_app_compute_dispatch_drives_cs_main_per_invocation`
//!     -- empty CS, asserts cs_invocation_count matches
//!     groupCount[xyz] * local_size[xyz]. Wire + dispatcher
//!     iteration.
//!   * `vk_app_compute_ssbo_write_lands_in_output_buffer` --
//!     CS writes a vec4 to a StorageBuffer SSBO. CsMain
//!     `out_buffer` ABI parameter.
//!   * `vk_app_compute_ssbo_scalar_write` -- CS writes a u32
//!     to ssbo[0]. Cranelift Op::Store scalar path.
//!   * `vk_app_compute_local_invocation_id_visible_to_shader`
//!     -- CS reads gl_LocalInvocationID.x, multiplies by 100,
//!     writes to ssbo[0]. Op::LoadBuiltin lowering + the
//!     Compute LocalInvocationId → params[6..9] mapping.
//!   * `vk_app_compute_global_invocation_id_folds_local_size`
//!     -- CS reads gl_GlobalInvocationID.x, asserts it
//!     equals `WorkgroupID.x * LocalSize.x + LocalInvocationID.x`.
//!     Proves the frontend extracts LocalSize from the SPIR-V
//!     OpExecutionMode and the Cranelift backend folds it
//!     into the GlobalInvocationId codegen.
//!   * `vk_app_compute_global_invocation_id_z_via_stack` --
//!     CS reads gl_GlobalInvocationID.z (lane 2). On bespoke
//!     this exercises the lid.z stack-load path: lid.z is
//!     the 9th AAPCS64 arg and lives at [SP + frame_bytes]
//!     *after* the prologue's stp_x_pre instructions push
//!     SP down for callee-saved register saves. A wrong
//!     offset would produce garbage; the test catches it
//!     by asserting the expected value 3.

mod common;

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use aqueduct_gpu_host::{Backend, Listener, Tier2Backend, Tier2Registry};
use atrium_spv_loader::LoaderConfig;
use atrium_vk_icd::{
    vkAllocateCommandBuffers, vkBeginCommandBuffer,
    vkCmdBindPipeline, vkCmdDispatch, vkCreateCommandPool,
    vkCreateComputePipelines, vkCreateDevice, vkCreateInstance,
    vkDestroyInstance, vkEndCommandBuffer,
    vkEnumeratePhysicalDevices, vkGetDeviceQueue, vkQueueSubmit,
};
use tempfile::TempDir;

use common::{
    EnvLock, VkCommandBuffer, VkDevice, VkInstance, VkPhysicalDevice, VkQueue,
    locate_compile_binary, make_shader_module, tmp_socket,
};

/// Minimal compute SPIR-V: GLCompute entry point with
/// `LocalSize = (4, 1, 1)`, empty body.  Local to this file
/// since other ICD tests don't need a compute shader.
fn build_empty_cs_local_4() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, ExecutionMode, ExecutionModel,
        FunctionControl, MemoryModel,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);

    let void = b.type_void();
    let void_fn = b.type_function(void, vec![]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![]);
    b.execution_mode(main, ExecutionMode::LocalSize, [4u32, 1, 1]);

    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn vk_app_compute_dispatch_drives_cs_main_per_invocation() {
    let sock = tmp_socket("compute");
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

    let cs_spv = build_empty_cs_local_4();
    let cs_mod = make_shader_module(device, &cs_spv);

    // VkComputePipelineCreateInfo with the embedded stage
    // pointing at our CS module. Layout from ash bindings.
    use ash::vk;
    use ash::vk::Handle;
    let stage = vk::PipelineShaderStageCreateInfo {
        s_type: vk::StructureType::PIPELINE_SHADER_STAGE_CREATE_INFO,
        p_next: std::ptr::null(),
        flags: vk::PipelineShaderStageCreateFlags::empty(),
        stage: vk::ShaderStageFlags::COMPUTE,
        module: vk::ShaderModule::from_raw(cs_mod),
        p_name: b"main\0".as_ptr() as *const i8,
        p_specialization_info: std::ptr::null(),
        _marker: std::marker::PhantomData,
    };
    let info = vk::ComputePipelineCreateInfo {
        s_type: vk::StructureType::COMPUTE_PIPELINE_CREATE_INFO,
        p_next: std::ptr::null(),
        flags: vk::PipelineCreateFlags::empty(),
        stage,
        layout: vk::PipelineLayout::null(),
        base_pipeline_handle: vk::Pipeline::null(),
        base_pipeline_index: 0,
        _marker: std::marker::PhantomData,
    };
    let infos = [info];
    let mut pipeline: u64 = 0;
    let r = unsafe {
        vkCreateComputePipelines(
            device, 0, 1,
            infos.as_ptr() as *const std::ffi::c_void,
            std::ptr::null(), &mut pipeline,
        )
    };
    assert_eq!(r, 0);
    assert!(pipeline != 0);

    // Brief wait so the daemon side has bound the compute
    // pipeline before we submit.
    thread::sleep(Duration::from_millis(100));

    // Verify backend received the compute pipeline binding
    // with local_size = (4, 1, 1) extracted from the SPIR-V.
    let pid = aqueduct_gpu::ids::ResourceId(pipeline as u32);
    let (_sid, cs_state) = tier2_backend.pipeline_compute(pid)
        .expect("compute pipeline should be bound on the backend");
    assert_eq!(cs_state.local_size_x, 4);
    assert_eq!(cs_state.local_size_y, 1);
    assert_eq!(cs_state.local_size_z, 1);

    // Command buffer + record bind+dispatch.
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
    // bind_point = 1 (VK_PIPELINE_BIND_POINT_COMPUTE).
    unsafe { vkCmdBindPipeline(cb, 1, pipeline); }
    // Dispatch 3 x 2 x 1 workgroups, each 4-wide local =>
    // 3 * 2 * 1 * 4 * 1 * 1 = 24 invocations.
    unsafe { vkCmdDispatch(cb, 3, 2, 1); }
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

    assert_eq!(tier2_backend.cs_invocation_count(), 24,
        "expected groupCount[3,2,1] * localSize[4,1,1] = 24 invocations");

    unsafe { vkDestroyInstance(instance, std::ptr::null()); }
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

/// Compute SPIR-V that writes a `vec4(values[0..4])` to an
/// SSBO at offset 0.  The SSBO is bound at DescriptorSet 0
/// Binding 0; tier-2's Cranelift backend maps any
/// StorageBuffer pointer for a compute shader to the
/// dispatcher's `out_buffer` parameter, so the 16 bytes land
/// in `Tier2Backend::compute_output_bytes()[0..16]`.
///
/// Vec4-typed because the Cranelift backend's Op::Store phase
/// only supports vector stores today (scalar stores are
/// queued; see the "Op::Store value is not a vector" reject
/// in atrium-spv-backend-cranelift).
fn build_ssbo_write_cs(values: [f32; 4]) -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode, ExecutionModel,
        FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);

    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let f32_ty = b.type_float(32, None);
    let vec4   = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);

    // struct SSBO { vec4 data; };
    let ssbo_struct = b.type_struct(vec![vec4]);
    b.decorate(ssbo_struct, Decoration::Block, vec![]);
    b.member_decorate(ssbo_struct, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let ptr_ssbo_struct = b.type_pointer(None, StorageClass::StorageBuffer, ssbo_struct);
    let ptr_ssbo_vec4   = b.type_pointer(None, StorageClass::StorageBuffer, vec4);
    let ssbo_var = b.variable(ptr_ssbo_struct, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo_var, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo_var, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let c_zero = b.constant_bit32(u32_ty, 0);
    let cs: Vec<_> = values.iter()
        .map(|v| b.constant_bit32(f32_ty, v.to_bits())).collect();
    let c_vec = b.constant_composite(vec4, cs);

    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let dst = b.access_chain(ptr_ssbo_vec4, None, ssbo_var, vec![c_zero]).unwrap();
    b.store(dst, c_vec, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![ssbo_var]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);

    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn vk_app_compute_ssbo_write_lands_in_output_buffer() {
    let _ = env_logger::builder().is_test(true).try_init();
    let sock = tmp_socket("compute_ssbo");
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

    let expected = [1.5_f32, 2.5, 3.5, 4.5];
    let cs_spv = build_ssbo_write_cs(expected);
    let cs_mod = make_shader_module(device, &cs_spv);

    use ash::vk;
    use ash::vk::Handle;
    let stage = vk::PipelineShaderStageCreateInfo {
        s_type: vk::StructureType::PIPELINE_SHADER_STAGE_CREATE_INFO,
        p_next: std::ptr::null(),
        flags: vk::PipelineShaderStageCreateFlags::empty(),
        stage: vk::ShaderStageFlags::COMPUTE,
        module: vk::ShaderModule::from_raw(cs_mod),
        p_name: b"main\0".as_ptr() as *const i8,
        p_specialization_info: std::ptr::null(),
        _marker: std::marker::PhantomData,
    };
    let info = vk::ComputePipelineCreateInfo {
        s_type: vk::StructureType::COMPUTE_PIPELINE_CREATE_INFO,
        p_next: std::ptr::null(),
        flags: vk::PipelineCreateFlags::empty(),
        stage,
        layout: vk::PipelineLayout::null(),
        base_pipeline_handle: vk::Pipeline::null(),
        base_pipeline_index: 0,
        _marker: std::marker::PhantomData,
    };
    let infos = [info];
    let mut pipeline: u64 = 0;
    unsafe {
        vkCreateComputePipelines(
            device, 0, 1,
            infos.as_ptr() as *const std::ffi::c_void,
            std::ptr::null(), &mut pipeline,
        );
    }
    assert!(pipeline != 0);
    thread::sleep(Duration::from_millis(200));

    // Diagnostic: confirm the SSBO-writing CS made it onto the
    // backend.  If this fails, the SPIR-V compile step did --
    // run with `RUST_LOG=debug` to see the daemon's reject.
    let pid = aqueduct_gpu::ids::ResourceId(pipeline as u32);
    let bind = tier2_backend.pipeline_compute(pid);
    assert!(bind.is_some(),
        "SSBO-writing CS should have compiled + bound; if this fails, \
         the cranelift backend rejected the StorageBuffer codegen \
         path (re-run with RUST_LOG=debug for the daemon's reject)");

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
    unsafe { vkCmdBindPipeline(cb, 1, pipeline); } // COMPUTE bind point
    unsafe { vkCmdDispatch(cb, 1, 1, 1); }
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

    let out = tier2_backend.compute_output_bytes()
        .expect("dispatch should have produced an output buffer");
    assert!(out.len() >= 16, "output buffer too small: {} bytes", out.len());
    for (i, exp) in expected.iter().enumerate() {
        let off = i * 4;
        let got = f32::from_le_bytes(out[off..off+4].try_into().unwrap());
        assert_eq!(got.to_bits(), exp.to_bits(),
            "ssbo[{i}]: expected {exp}, got {got}");
    }
    // Bytes beyond the vec4 stay zero (the dispatcher cleared
    // the buffer before invocations).
    assert!(out[16..].iter().all(|&b| b == 0),
        "buffer past offset 16 should be zero, got {:?}", &out[16..32.min(out.len())]);

    unsafe { atrium_vk_icd::vkDestroyInstance(instance, std::ptr::null()); }
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

/// Compute SPIR-V that writes a u32 constant to the first
/// slot of an SSBO -- exercising the scalar-store path the
/// Cranelift backend just gained.  Pre-fix this would have
/// failed with "Op::Store value is not a vector".
fn build_ssbo_scalar_write_cs(value: u32) -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode, ExecutionModel,
        FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);

    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let void_fn = b.type_function(void, vec![]);

    let ssbo_struct = b.type_struct(vec![u32_ty]);
    b.decorate(ssbo_struct, Decoration::Block, vec![]);
    b.member_decorate(ssbo_struct, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let ptr_ssbo_struct = b.type_pointer(None, StorageClass::StorageBuffer, ssbo_struct);
    let ptr_ssbo_u32    = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let ssbo_var = b.variable(ptr_ssbo_struct, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo_var, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo_var, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let c_zero  = b.constant_bit32(u32_ty, 0);
    let c_value = b.constant_bit32(u32_ty, value);

    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let dst = b.access_chain(ptr_ssbo_u32, None, ssbo_var, vec![c_zero]).unwrap();
    b.store(dst, c_value, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![ssbo_var]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);

    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn vk_app_compute_ssbo_scalar_write() {
    let _ = env_logger::builder().is_test(true).try_init();
    let sock = tmp_socket("compute_ssbo_scalar");
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

    let cs_spv = build_ssbo_scalar_write_cs(0xCAFE_BABE);
    let cs_mod = make_shader_module(device, &cs_spv);

    use ash::vk;
    use ash::vk::Handle;
    let stage = vk::PipelineShaderStageCreateInfo {
        s_type: vk::StructureType::PIPELINE_SHADER_STAGE_CREATE_INFO,
        p_next: std::ptr::null(),
        flags: vk::PipelineShaderStageCreateFlags::empty(),
        stage: vk::ShaderStageFlags::COMPUTE,
        module: vk::ShaderModule::from_raw(cs_mod),
        p_name: b"main\0".as_ptr() as *const i8,
        p_specialization_info: std::ptr::null(),
        _marker: std::marker::PhantomData,
    };
    let info = vk::ComputePipelineCreateInfo {
        s_type: vk::StructureType::COMPUTE_PIPELINE_CREATE_INFO,
        p_next: std::ptr::null(),
        flags: vk::PipelineCreateFlags::empty(),
        stage,
        layout: vk::PipelineLayout::null(),
        base_pipeline_handle: vk::Pipeline::null(),
        base_pipeline_index: 0,
        _marker: std::marker::PhantomData,
    };
    let infos = [info];
    let mut pipeline: u64 = 0;
    unsafe {
        vkCreateComputePipelines(
            device, 0, 1,
            infos.as_ptr() as *const std::ffi::c_void,
            std::ptr::null(), &mut pipeline,
        );
    }
    assert!(pipeline != 0);
    thread::sleep(Duration::from_millis(200));

    let pid = aqueduct_gpu::ids::ResourceId(pipeline as u32);
    assert!(tier2_backend.pipeline_compute(pid).is_some(),
        "scalar-write CS should compile + bind after the Op::Store \
         scalar-path landed");

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
    unsafe { vkCmdBindPipeline(cb, 1, pipeline); }
    unsafe { vkCmdDispatch(cb, 1, 1, 1); }
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

    let out = tier2_backend.compute_output_bytes().expect("output");
    let got = u32::from_le_bytes(out[0..4].try_into().unwrap());
    assert_eq!(got, 0xCAFE_BABE,
        "expected 0xCAFEBABE in ssbo[0], got {got:#x}");

    unsafe { atrium_vk_icd::vkDestroyInstance(instance, std::ptr::null()); }
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

/// Compute SPIR-V that reads gl_LocalInvocationID.x, multiplies
/// by 100, and writes the product to ssbo[0]. With LocalSize=4
/// + a single workgroup, the four invocations race to write
/// 0, 100, 200, 300 to the same slot; the dispatcher's nested
/// (gz, gy, gx, lz, ly, lx) loop iterates lx innermost so the
/// last write is from lx=3 (value 300).
fn build_lid_write_cs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);

    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let uvec3  = b.type_vector(u32_ty, 3);
    let void_fn = b.type_function(void, vec![]);

    // gl_LocalInvocationID variable.
    let ptr_in_uvec3 = b.type_pointer(None, StorageClass::Input, uvec3);
    let lid_var = b.variable(ptr_in_uvec3, None, StorageClass::Input, None);
    b.decorate(lid_var, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::LocalInvocationId)]);

    // SSBO struct { uint data; }.
    let ssbo_struct = b.type_struct(vec![u32_ty]);
    b.decorate(ssbo_struct, Decoration::Block, vec![]);
    b.member_decorate(ssbo_struct, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_ssbo_struct = b.type_pointer(None, StorageClass::StorageBuffer, ssbo_struct);
    let ptr_ssbo_u32    = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let ssbo_var = b.variable(ptr_ssbo_struct, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo_var, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo_var, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let c_zero    = b.constant_bit32(u32_ty, 0);
    let c_hundred = b.constant_bit32(u32_ty, 100);

    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let lid = b.load(uvec3, None, lid_var, None, vec![]).unwrap();
    let lid_x = b.composite_extract(u32_ty, None, lid, vec![0]).unwrap();
    let product = b.i_mul(u32_ty, None, lid_x, c_hundred).unwrap();
    let dst = b.access_chain(ptr_ssbo_u32, None, ssbo_var, vec![c_zero]).unwrap();
    b.store(dst, product, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main",
        vec![lid_var, ssbo_var]);
    b.execution_mode(main, ExecutionMode::LocalSize, [4u32, 1, 1]);

    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn vk_app_compute_local_invocation_id_visible_to_shader() {
    let _ = env_logger::builder().is_test(true).try_init();
    let sock = tmp_socket("compute_lid");
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

    let cs_spv = build_lid_write_cs();
    let cs_mod = make_shader_module(device, &cs_spv);

    use ash::vk;
    use ash::vk::Handle;
    let stage = vk::PipelineShaderStageCreateInfo {
        s_type: vk::StructureType::PIPELINE_SHADER_STAGE_CREATE_INFO,
        p_next: std::ptr::null(),
        flags: vk::PipelineShaderStageCreateFlags::empty(),
        stage: vk::ShaderStageFlags::COMPUTE,
        module: vk::ShaderModule::from_raw(cs_mod),
        p_name: b"main\0".as_ptr() as *const i8,
        p_specialization_info: std::ptr::null(),
        _marker: std::marker::PhantomData,
    };
    let info = vk::ComputePipelineCreateInfo {
        s_type: vk::StructureType::COMPUTE_PIPELINE_CREATE_INFO,
        p_next: std::ptr::null(),
        flags: vk::PipelineCreateFlags::empty(),
        stage,
        layout: vk::PipelineLayout::null(),
        base_pipeline_handle: vk::Pipeline::null(),
        base_pipeline_index: 0,
        _marker: std::marker::PhantomData,
    };
    let infos = [info];
    let mut pipeline: u64 = 0;
    unsafe {
        vkCreateComputePipelines(
            device, 0, 1,
            infos.as_ptr() as *const std::ffi::c_void,
            std::ptr::null(), &mut pipeline,
        );
    }
    assert!(pipeline != 0);
    thread::sleep(Duration::from_millis(200));

    let pid = aqueduct_gpu::ids::ResourceId(pipeline as u32);
    assert!(tier2_backend.pipeline_compute(pid).is_some(),
        "LID-reading CS should compile + bind; if this fails, \
         the frontend's BuiltIn lowering or the backend's \
         Op::LoadBuiltin codegen broke (re-run with RUST_LOG=warn)");

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
    unsafe { vkCmdBindPipeline(cb, 1, pipeline); }
    unsafe { vkCmdDispatch(cb, 1, 1, 1); }
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

    let out = tier2_backend.compute_output_bytes().expect("output");
    let got = u32::from_le_bytes(out[0..4].try_into().unwrap());
    // The dispatcher loops lx innermost; with LocalSize=(4,1,1)
    // the last invocation has lid.x=3, writing 300.
    assert_eq!(got, 300,
        "expected last-writer (lid.x=3, value=300) at ssbo[0], got {got}");

    unsafe { atrium_vk_icd::vkDestroyInstance(instance, std::ptr::null()); }
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

/// Compute SPIR-V that reads gl_GlobalInvocationID.x and
/// writes it to ssbo[0]. With LocalSize=4 and Dispatch(2,1,1),
/// the dispatcher fires 8 invocations. The (gx, lx) iteration
/// order is gx outermost, lx innermost; last invocation
/// (gx=1, lx=3) has gid.x = 1*4 + 3 = 7.
fn build_gid_write_cs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);

    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let uvec3  = b.type_vector(u32_ty, 3);
    let void_fn = b.type_function(void, vec![]);

    let ptr_in_uvec3 = b.type_pointer(None, StorageClass::Input, uvec3);
    let gid_var = b.variable(ptr_in_uvec3, None, StorageClass::Input, None);
    b.decorate(gid_var, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::GlobalInvocationId)]);

    let ssbo_struct = b.type_struct(vec![u32_ty]);
    b.decorate(ssbo_struct, Decoration::Block, vec![]);
    b.member_decorate(ssbo_struct, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_ssbo_struct = b.type_pointer(None, StorageClass::StorageBuffer, ssbo_struct);
    let ptr_ssbo_u32    = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let ssbo_var = b.variable(ptr_ssbo_struct, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo_var, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo_var, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let c_zero = b.constant_bit32(u32_ty, 0);

    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let gid = b.load(uvec3, None, gid_var, None, vec![]).unwrap();
    let gid_x = b.composite_extract(u32_ty, None, gid, vec![0]).unwrap();
    let dst = b.access_chain(ptr_ssbo_u32, None, ssbo_var, vec![c_zero]).unwrap();
    b.store(dst, gid_x, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main",
        vec![gid_var, ssbo_var]);
    b.execution_mode(main, ExecutionMode::LocalSize, [4u32, 1, 1]);

    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn vk_app_compute_global_invocation_id_folds_local_size() {
    let _ = env_logger::builder().is_test(true).try_init();
    let sock = tmp_socket("compute_gid");
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

    let cs_spv = build_gid_write_cs();
    let cs_mod = make_shader_module(device, &cs_spv);

    use ash::vk;
    use ash::vk::Handle;
    let stage = vk::PipelineShaderStageCreateInfo {
        s_type: vk::StructureType::PIPELINE_SHADER_STAGE_CREATE_INFO,
        p_next: std::ptr::null(),
        flags: vk::PipelineShaderStageCreateFlags::empty(),
        stage: vk::ShaderStageFlags::COMPUTE,
        module: vk::ShaderModule::from_raw(cs_mod),
        p_name: b"main\0".as_ptr() as *const i8,
        p_specialization_info: std::ptr::null(),
        _marker: std::marker::PhantomData,
    };
    let info = vk::ComputePipelineCreateInfo {
        s_type: vk::StructureType::COMPUTE_PIPELINE_CREATE_INFO,
        p_next: std::ptr::null(),
        flags: vk::PipelineCreateFlags::empty(),
        stage,
        layout: vk::PipelineLayout::null(),
        base_pipeline_handle: vk::Pipeline::null(),
        base_pipeline_index: 0,
        _marker: std::marker::PhantomData,
    };
    let infos = [info];
    let mut pipeline: u64 = 0;
    unsafe {
        vkCreateComputePipelines(
            device, 0, 1,
            infos.as_ptr() as *const std::ffi::c_void,
            std::ptr::null(), &mut pipeline,
        );
    }
    assert!(pipeline != 0);
    thread::sleep(Duration::from_millis(200));

    let pid = aqueduct_gpu::ids::ResourceId(pipeline as u32);
    assert!(tier2_backend.pipeline_compute(pid).is_some(),
        "GID-reading CS should compile + bind");

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
    unsafe { vkCmdBindPipeline(cb, 1, pipeline); }
    // Dispatch (2,1,1) workgroups; each is 4-wide local.
    // Total 8 invocations; loop iterates lx innermost, gx
    // outermost, so last is gx=1, lx=3 -> gid.x = 1*4+3 = 7.
    unsafe { vkCmdDispatch(cb, 2, 1, 1); }
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

    let out = tier2_backend.compute_output_bytes().expect("output");
    let got = u32::from_le_bytes(out[0..4].try_into().unwrap());
    assert_eq!(got, 7,
        "GlobalInvocationID.x should = workgroup_id.x * 4 + local_id.x; \
         last invocation (gx=1, lx=3) -> 7, got {got}");

    unsafe { atrium_vk_icd::vkDestroyInstance(instance, std::ptr::null()); }
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

/// Compute SPIR-V that reads gl_GlobalInvocationID.z and
/// writes it to ssbo[0]. With LocalSize=(1,1,4) and
/// Dispatch=(1,1,1), the dispatcher fires 4 invocations
/// (lz iterates 0..3 with lx=ly=0); the last invocation
/// has gid.z = wg.z*1 + lid.z = 0 + 3 = 3.
///
/// Exercises the LID.z stack-load path AND the post-prologue
/// patch logic.  If the bespoke backend's prologue accidentally
/// shifted SP without updating the lid.z offset, this test
/// would observe a stale / garbage value.
fn build_gid_z_cs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let uvec3  = b.type_vector(u32_ty, 3);
    let void_fn = b.type_function(void, vec![]);
    let ptr_in_uvec3 = b.type_pointer(None, StorageClass::Input, uvec3);
    let gid_var = b.variable(ptr_in_uvec3, None, StorageClass::Input, None);
    b.decorate(gid_var, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::GlobalInvocationId)]);
    let ssbo_struct = b.type_struct(vec![u32_ty]);
    b.decorate(ssbo_struct, Decoration::Block, vec![]);
    b.member_decorate(ssbo_struct, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_ssbo_struct = b.type_pointer(None, StorageClass::StorageBuffer, ssbo_struct);
    let ptr_ssbo_u32    = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let ssbo_var = b.variable(ptr_ssbo_struct, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo_var, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo_var, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let c_zero = b.constant_bit32(u32_ty, 0);

    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let gid = b.load(uvec3, None, gid_var, None, vec![]).unwrap();
    // Extract lane 2 (z) -- exercises lid.z stack-load path.
    let gid_z = b.composite_extract(u32_ty, None, gid, vec![2]).unwrap();
    let dst = b.access_chain(ptr_ssbo_u32, None, ssbo_var, vec![c_zero]).unwrap();
    b.store(dst, gid_z, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main",
        vec![gid_var, ssbo_var]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 4]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn vk_app_compute_global_invocation_id_z_via_stack() {
    let _ = env_logger::builder().is_test(true).try_init();
    let sock = tmp_socket("compute_gid_z");
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

    let cs_spv = build_gid_z_cs();
    let cs_mod = make_shader_module(device, &cs_spv);

    use ash::vk;
    use ash::vk::Handle;
    let stage = vk::PipelineShaderStageCreateInfo {
        s_type: vk::StructureType::PIPELINE_SHADER_STAGE_CREATE_INFO,
        p_next: std::ptr::null(),
        flags: vk::PipelineShaderStageCreateFlags::empty(),
        stage: vk::ShaderStageFlags::COMPUTE,
        module: vk::ShaderModule::from_raw(cs_mod),
        p_name: b"main\0".as_ptr() as *const i8,
        p_specialization_info: std::ptr::null(),
        _marker: std::marker::PhantomData,
    };
    let info = vk::ComputePipelineCreateInfo {
        s_type: vk::StructureType::COMPUTE_PIPELINE_CREATE_INFO,
        p_next: std::ptr::null(),
        flags: vk::PipelineCreateFlags::empty(),
        stage,
        layout: vk::PipelineLayout::null(),
        base_pipeline_handle: vk::Pipeline::null(),
        base_pipeline_index: 0,
        _marker: std::marker::PhantomData,
    };
    let infos = [info];
    let mut pipeline: u64 = 0;
    unsafe {
        vkCreateComputePipelines(
            device, 0, 1,
            infos.as_ptr() as *const std::ffi::c_void,
            std::ptr::null(), &mut pipeline,
        );
    }
    thread::sleep(Duration::from_millis(200));

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
    unsafe { vkCmdBindPipeline(cb, 1, pipeline); }
    unsafe { vkCmdDispatch(cb, 1, 1, 1); }
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

    let out = tier2_backend.compute_output_bytes().expect("output");
    let got = u32::from_le_bytes(out[0..4].try_into().unwrap());
    // Last invocation has gid.z = 3.  If the lid.z stack
    // load read the wrong offset (e.g. into garbage memory
    // after the prologue shifted SP), this would be some
    // wrong value -- the test catches that.
    assert_eq!(got, 3,
        "gid.z = wg.z*1 + lid.z; last invocation lz=3, expected 3, got {got}");

    unsafe { atrium_vk_icd::vkDestroyInstance(instance, std::ptr::null()); }
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}
