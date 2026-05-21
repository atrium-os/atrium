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

/// Compute SPIR-V: read u32 from ssbo[0], add 7, write to
/// ssbo[1]. The dispatcher zeroes the SSBO before each
/// dispatch, so ssbo[0]=0, and ssbo[1] should be 7 after.
/// Exercises the OpLoad-through-StorageBuffer + OpIAdd +
/// OpStore-scalar-u32 path end-to-end.
fn build_rmw_cs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let void_fn = b.type_function(void, vec![]);
    let ssbo_struct = b.type_struct(vec![u32_ty, u32_ty]);
    b.decorate(ssbo_struct, Decoration::Block, vec![]);
    b.member_decorate(ssbo_struct, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.member_decorate(ssbo_struct, 1, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(4)]);
    let ptr_ssbo_struct = b.type_pointer(None, StorageClass::StorageBuffer, ssbo_struct);
    let ptr_ssbo_u32    = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let ssbo_var = b.variable(ptr_ssbo_struct, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo_var, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo_var, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let c_zero  = b.constant_bit32(u32_ty, 0);
    let c_one   = b.constant_bit32(u32_ty, 1);
    let c_seven = b.constant_bit32(u32_ty, 7);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let src = b.access_chain(ptr_ssbo_u32, None, ssbo_var, vec![c_zero]).unwrap();
    let loaded = b.load(u32_ty, None, src, None, vec![]).unwrap();
    let sum = b.i_add(u32_ty, None, loaded, c_seven).unwrap();
    let dst = b.access_chain(ptr_ssbo_u32, None, ssbo_var, vec![c_one]).unwrap();
    b.store(dst, sum, None, vec![]).unwrap();
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
fn vk_app_compute_ssbo_read_modify_write() {
    let _ = env_logger::builder().is_test(true).try_init();
    let sock = tmp_socket("compute_rmw");
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

    let cs_spv = build_rmw_cs();
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
    // ssbo[0] was 0 (zero-init by the dispatcher); add 7;
    // write to ssbo[1] at byte offset 4.
    assert_eq!(u32::from_le_bytes(out[0..4].try_into().unwrap()), 0,
        "ssbo[0] should be unchanged from its zero-init");
    assert_eq!(u32::from_le_bytes(out[4..8].try_into().unwrap()), 7,
        "ssbo[1] should equal ssbo[0] + 7 = 7");

    unsafe { atrium_vk_icd::vkDestroyInstance(instance, std::ptr::null()); }
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

/// Compute SPIR-V with TWO SSBO bindings -- writes the
/// constant 0xBADF00D to binding 0 and 0xDEADBEEF to
/// binding 1.  Exercises the multi-binding descriptor-table
/// path end-to-end: ICD scans the binding count, blobs it
/// onto Tier2ComputeStateBlob, host builds a descriptor
/// table at dispatch + bespoke pre-loads X16=tbl[0] /
/// X17=tbl[1].  If the table mapping is wrong (e.g.
/// bindings swapped, or both alias the same buffer), the
/// per-binding readback will catch it.
fn build_two_binding_constants_cs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let void_fn = b.type_function(void, vec![]);
    let s0 = b.type_struct(vec![u32_ty]);
    b.decorate(s0, Decoration::Block, vec![]);
    b.member_decorate(s0, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let s1 = b.type_struct(vec![u32_ty]);
    b.decorate(s1, Decoration::Block, vec![]);
    b.member_decorate(s1, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let p0 = b.type_pointer(None, StorageClass::StorageBuffer, s0);
    let p1 = b.type_pointer(None, StorageClass::StorageBuffer, s1);
    let pu = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let v0 = b.variable(p0, None, StorageClass::StorageBuffer, None);
    b.decorate(v0, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(v0, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let v1 = b.variable(p1, None, StorageClass::StorageBuffer, None);
    b.decorate(v1, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(v1, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(1)]);
    let c_zero    = b.constant_bit32(u32_ty, 0);
    let c_badfood = b.constant_bit32(u32_ty, 0x0BADF00D);
    let c_dead    = b.constant_bit32(u32_ty, 0xDEADBEEF);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let d0 = b.access_chain(pu, None, v0, vec![c_zero]).unwrap();
    b.store(d0, c_badfood, None, vec![]).unwrap();
    let d1 = b.access_chain(pu, None, v1, vec![c_zero]).unwrap();
    b.store(d1, c_dead, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![v0, v1]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn vk_app_compute_multi_binding_ssbo_routes_to_correct_buffers() {
    let _ = env_logger::builder().is_test(true).try_init();
    let sock = tmp_socket("compute_multi_binding");
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

    let cs_spv = build_two_binding_constants_cs();
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
    let (_sid, cs_state) = tier2_backend.pipeline_compute(pid)
        .expect("compute pipeline bound");
    assert_eq!(cs_state.ssbo_binding_count, 2,
        "ICD should detect 2 SSBO bindings; got {}",
        cs_state.ssbo_binding_count);

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

    let b0 = tier2_backend.compute_output_bytes_for_binding(0)
        .expect("binding 0 buffer");
    let b1 = tier2_backend.compute_output_bytes_for_binding(1)
        .expect("binding 1 buffer");
    let got0 = u32::from_le_bytes(b0[0..4].try_into().unwrap());
    let got1 = u32::from_le_bytes(b1[0..4].try_into().unwrap());
    assert_eq!(got0, 0x0BADF00D,
        "binding 0 should be 0x0BADF00D, got {got0:#x}");
    assert_eq!(got1, 0xDEADBEEF,
        "binding 1 should be 0xDEADBEEF, got {got1:#x}");

    unsafe { atrium_vk_icd::vkDestroyInstance(instance, std::ptr::null()); }
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

/// 4-SSBO CS that writes a distinct magic to each binding.
/// Validates the bespoke extended binding pool (X12..X17)
/// + the host's N-buffer descriptor-table build.  Catches
/// reg-pool misnumbering (e.g. swapped bindings) the same
/// way the 2-SSBO test catches the X16/X17 case.
fn build_four_binding_constants_cs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let void_fn = b.type_function(void, vec![]);
    let pu = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let mut vars = Vec::new();
    for i in 0..4 {
        let s = b.type_struct(vec![u32_ty]);
        b.decorate(s, Decoration::Block, vec![]);
        b.member_decorate(s, 0, Decoration::Offset,
            vec![rspirv::dr::Operand::LiteralBit32(0)]);
        let p = b.type_pointer(None, StorageClass::StorageBuffer, s);
        let v = b.variable(p, None, StorageClass::StorageBuffer, None);
        b.decorate(v, Decoration::DescriptorSet,
            vec![rspirv::dr::Operand::LiteralBit32(0)]);
        b.decorate(v, Decoration::Binding,
            vec![rspirv::dr::Operand::LiteralBit32(i)]);
        vars.push(v);
    }
    let c_zero = b.constant_bit32(u32_ty, 0);
    let consts: Vec<_> = [0x1111_1111u32, 0x2222_2222, 0x3333_3333, 0x4444_4444]
        .iter().map(|x| b.constant_bit32(u32_ty, *x)).collect();
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    for (i, v) in vars.iter().enumerate() {
        let d = b.access_chain(pu, None, *v, vec![c_zero]).unwrap();
        b.store(d, consts[i], None, vec![]).unwrap();
    }
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vars);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn vk_app_compute_four_binding_ssbo_routes_correctly() {
    let _ = env_logger::builder().is_test(true).try_init();
    let sock = tmp_socket("compute_four_binding");
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

    let cs_spv = build_four_binding_constants_cs();
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
    let (_sid, cs_state) = tier2_backend.pipeline_compute(pid)
        .expect("compute pipeline bound");
    assert_eq!(cs_state.ssbo_binding_count, 4,
        "ICD should detect 4 SSBO bindings; got {}",
        cs_state.ssbo_binding_count);

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

    let expected = [0x1111_1111u32, 0x2222_2222, 0x3333_3333, 0x4444_4444];
    for i in 0..4 {
        let buf = tier2_backend.compute_output_bytes_for_binding(i as u32)
            .unwrap_or_else(|| panic!("missing binding {i} buffer"));
        let got = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        assert_eq!(got, expected[i],
            "binding {i} should be {:#x}, got {:#x}", expected[i], got);
    }

    unsafe { atrium_vk_icd::vkDestroyInstance(instance, std::ptr::null()); }
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn vk_app_compute_multi_binding_via_cranelift_force() {
    // Re-run the 2-SSBO routing test but force the
    // production binary to use Cranelift via env var.
    // Proves the cranelift descriptor-table prologue
    // works through the daemon's atrium-spv-compile
    // subprocess, not just the dlopen unit test.
    let _ = env_logger::builder().is_test(true).try_init();
    let sock = tmp_socket("c_mb_clift");
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

    // Force Cranelift for this test's atrium-spv-compile
    // subprocess invocations; cleared on EnvLock drop.
    let _env = EnvLock::set_with_force_backend(&sock, "cranelift");

    let mut instance: VkInstance = std::ptr::null_mut();
    unsafe { vkCreateInstance(std::ptr::null(), std::ptr::null(), &mut instance); }
    let mut pds: [VkPhysicalDevice; 1] = [std::ptr::null_mut(); 1];
    let mut cap: u32 = 1;
    unsafe { vkEnumeratePhysicalDevices(instance, &mut cap, pds.as_mut_ptr()); }
    let mut device: VkDevice = std::ptr::null_mut();
    unsafe { vkCreateDevice(pds[0], std::ptr::null(), std::ptr::null(), &mut device); }
    let mut queue: VkQueue = std::ptr::null_mut();
    unsafe { vkGetDeviceQueue(device, 0, 0, &mut queue); }

    let cs_spv = build_two_binding_constants_cs();
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

    let b0 = tier2_backend.compute_output_bytes_for_binding(0)
        .expect("binding 0 buffer");
    let b1 = tier2_backend.compute_output_bytes_for_binding(1)
        .expect("binding 1 buffer");
    let got0 = u32::from_le_bytes(b0[0..4].try_into().unwrap());
    let got1 = u32::from_le_bytes(b1[0..4].try_into().unwrap());
    assert_eq!(got0, 0x0BADF00D,
        "(cranelift forced) binding 0 should be 0x0BADF00D, got {got0:#x}");
    assert_eq!(got1, 0xDEADBEEF,
        "(cranelift forced) binding 1 should be 0xDEADBEEF, got {got1:#x}");

    unsafe { atrium_vk_icd::vkDestroyInstance(instance, std::ptr::null()); }
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

/// CS: `ssbo.data[gid_x] = gid_x` with LocalSize=(4,1,1).
/// After dispatching 1 workgroup, ssbo.data[0..4] = [0,1,2,3].
/// Exercises the dynamic AccessChain path (Op::PtrOffsetDynamic)
/// end-to-end through the production daemon -> bespoke
/// codegen -> dispatch flow.
fn build_dyn_index_cs() -> Vec<u8> {
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
    let rt_arr = b.type_runtime_array(u32_ty);
    b.decorate(rt_arr, Decoration::ArrayStride,
        vec![rspirv::dr::Operand::LiteralBit32(4)]);
    let s = b.type_struct(vec![rt_arr]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_u = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let ssbo = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_in_uvec3 = b.type_pointer(None, StorageClass::Input, uvec3);
    let gid_var = b.variable(ptr_in_uvec3, None, StorageClass::Input, None);
    b.decorate(gid_var, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::GlobalInvocationId)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let gid = b.load(uvec3, None, gid_var, None, vec![]).unwrap();
    let gid_x = b.composite_extract(u32_ty, None, gid, vec![0]).unwrap();
    let dst = b.access_chain(ptr_u, None, ssbo, vec![c_zero, gid_x]).unwrap();
    b.store(dst, gid_x, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main",
        vec![gid_var, ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [4u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn vk_app_compute_dynamic_ssbo_index_writes_per_lane() {
    let _ = env_logger::builder().is_test(true).try_init();
    let sock = tmp_socket("c_dyn_idx");
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

    let cs_spv = build_dyn_index_cs();
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
    for i in 0..4usize {
        let v = u32::from_le_bytes(out[i*4..i*4+4].try_into().unwrap());
        assert_eq!(v, i as u32,
            "ssbo.data[{i}] should be {i}, got {v}");
    }

    unsafe { atrium_vk_icd::vkDestroyInstance(instance, std::ptr::null()); }
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn vk_app_compute_dynamic_ssbo_read_then_write_with_prefill() {
    // CS: ssbo.data[gid_x + 4] = ssbo.data[gid_x] + 100
    // Pre-fill ssbo.data[0..4] = [10, 20, 30, 40].
    // After dispatch (LocalSize=4): ssbo.data[4..8] = [110, 120, 130, 140].
    // Original [0..4] should be unchanged.
    // Exercises Op::PtrOffsetDynamic on BOTH the load and
    // the store side, plus pre-fill input infrastructure.
    let _ = env_logger::builder().is_test(true).try_init();
    let sock = tmp_socket("c_dyn_rmw");
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

    let cs_spv = build_dyn_rmw_cs();

    let mut instance: VkInstance = std::ptr::null_mut();
    unsafe { vkCreateInstance(std::ptr::null(), std::ptr::null(), &mut instance); }
    let mut pds: [VkPhysicalDevice; 1] = [std::ptr::null_mut(); 1];
    let mut cap: u32 = 1;
    unsafe { vkEnumeratePhysicalDevices(instance, &mut cap, pds.as_mut_ptr()); }
    let mut device: VkDevice = std::ptr::null_mut();
    unsafe { vkCreateDevice(pds[0], std::ptr::null(), std::ptr::null(), &mut device); }
    let mut queue: VkQueue = std::ptr::null_mut();
    unsafe { vkGetDeviceQueue(device, 0, 0, &mut queue); }

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

    // Pre-fill binding 0 with [10, 20, 30, 40] u32.
    let mut prefill = vec![0u8; 16];
    for (i, v) in [10u32, 20, 30, 40].iter().enumerate() {
        prefill[i*4..i*4+4].copy_from_slice(&v.to_le_bytes());
    }
    tier2_backend.set_compute_input_for_binding(0, prefill);

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
    let expected_in  = [10u32, 20, 30, 40];
    let expected_out = [110u32, 120, 130, 140];
    for i in 0..4usize {
        let v = u32::from_le_bytes(out[i*4..i*4+4].try_into().unwrap());
        assert_eq!(v, expected_in[i],
            "ssbo[{i}] (pre-fill) should be unchanged at {}, got {v}",
            expected_in[i]);
    }
    for i in 0..4usize {
        let off = (i + 4) * 4;
        let v = u32::from_le_bytes(out[off..off+4].try_into().unwrap());
        assert_eq!(v, expected_out[i],
            "ssbo[{}] (RMW out) should be {}, got {v}",
            i + 4, expected_out[i]);
    }

    unsafe { atrium_vk_icd::vkDestroyInstance(instance, std::ptr::null()); }
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

fn build_dyn_rmw_cs() -> Vec<u8> {
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
    let rt_arr = b.type_runtime_array(u32_ty);
    b.decorate(rt_arr, Decoration::ArrayStride,
        vec![rspirv::dr::Operand::LiteralBit32(4)]);
    let s = b.type_struct(vec![rt_arr]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_u = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let ssbo = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_in_uvec3 = b.type_pointer(None, StorageClass::Input, uvec3);
    let gid_var = b.variable(ptr_in_uvec3, None, StorageClass::Input, None);
    b.decorate(gid_var, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::GlobalInvocationId)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let c_four = b.constant_bit32(u32_ty, 4);
    let c_100  = b.constant_bit32(u32_ty, 100);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let gid = b.load(uvec3, None, gid_var, None, vec![]).unwrap();
    let gid_x = b.composite_extract(u32_ty, None, gid, vec![0]).unwrap();
    // ssbo.data[gid_x] read
    let src = b.access_chain(ptr_u, None, ssbo, vec![c_zero, gid_x]).unwrap();
    let v = b.load(u32_ty, None, src, None, vec![]).unwrap();
    let v_plus = b.i_add(u32_ty, None, v, c_100).unwrap();
    // ssbo.data[gid_x + 4] write
    let idx_out = b.i_add(u32_ty, None, gid_x, c_four).unwrap();
    let dst = b.access_chain(ptr_u, None, ssbo, vec![c_zero, idx_out]).unwrap();
    b.store(dst, v_plus, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![gid_var, ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [4u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

fn build_atomic_counter_cs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
        MemorySemantics, Scope,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let uvec3  = b.type_vector(u32_ty, 3);
    let void_fn = b.type_function(void, vec![]);
    let s = b.type_struct(vec![u32_ty]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_u = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let ssbo = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_in_uvec3 = b.type_pointer(None, StorageClass::Input, uvec3);
    let gid_var = b.variable(ptr_in_uvec3, None, StorageClass::Input, None);
    b.decorate(gid_var, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::GlobalInvocationId)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let c_scope = b.constant_bit32(u32_ty, Scope::Device as u32);
    let c_sem   = b.constant_bit32(u32_ty,
        MemorySemantics::ATOMIC_COUNTER_MEMORY.bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let gid = b.load(uvec3, None, gid_var, None, vec![]).unwrap();
    let gid_x = b.composite_extract(u32_ty, None, gid, vec![0]).unwrap();
    let dst = b.access_chain(ptr_u, None, ssbo, vec![c_zero]).unwrap();
    let _ = b.atomic_i_add(u32_ty, None, dst, c_scope, c_sem, gid_x).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![gid_var, ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [4u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn vk_app_compute_atomic_iadd_accumulates_per_lane() {
    // atomicAdd(ssbo.counter, gid_x) with LocalSize=4 -> 4 invocations
    // each add 0, 1, 2, 3 -> counter ends at 6.
    let _ = env_logger::builder().is_test(true).try_init();
    let sock = tmp_socket("c_atomic");
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

    let cs_spv = build_atomic_counter_cs();
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
        stage, layout: vk::PipelineLayout::null(),
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
    unsafe { vkQueueSubmit(queue, 1, submit.as_ptr() as *const _, std::ptr::null_mut()); }
    thread::sleep(Duration::from_millis(300));

    let out = tier2_backend.compute_output_bytes().expect("output");
    let counter = u32::from_le_bytes(out[0..4].try_into().unwrap());
    assert_eq!(counter, 0 + 1 + 2 + 3,
        "4 invocations adding 0..4 should sum to 6, got {counter}");

    unsafe { atrium_vk_icd::vkDestroyInstance(instance, std::ptr::null()); }
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

fn build_histogram_cs() -> Vec<u8> {
    // Real-world-shaped CS: each invocation reads a sample
    // from the input array, computes a bucket index (sample %
    // 4 here), and atomicAdds 1 into the bucket counter.
    //
    //   uint sample = in.data[gid_x];
    //   uint bucket = sample & 3;     // 4 buckets
    //   atomicAdd(out.bins[bucket], 1);
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
        MemorySemantics, Scope,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let uvec3  = b.type_vector(u32_ty, 3);
    let void_fn = b.type_function(void, vec![]);
    let rt_arr = b.type_runtime_array(u32_ty);
    b.decorate(rt_arr, Decoration::ArrayStride,
        vec![rspirv::dr::Operand::LiteralBit32(4)]);
    // struct In  { uint data[]; };
    let s_in  = b.type_struct(vec![rt_arr]);
    b.decorate(s_in, Decoration::Block, vec![]);
    b.member_decorate(s_in, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    // struct Out { uint bins[]; };
    let s_out = b.type_struct(vec![rt_arr]);
    b.decorate(s_out, Decoration::Block, vec![]);
    b.member_decorate(s_out, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_s_in  = b.type_pointer(None, StorageClass::StorageBuffer, s_in);
    let ptr_s_out = b.type_pointer(None, StorageClass::StorageBuffer, s_out);
    let ptr_u     = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let v_in  = b.variable(ptr_s_in,  None, StorageClass::StorageBuffer, None);
    b.decorate(v_in, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(v_in, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let v_out = b.variable(ptr_s_out, None, StorageClass::StorageBuffer, None);
    b.decorate(v_out, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(v_out, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(1)]);
    let ptr_in_uvec3 = b.type_pointer(None, StorageClass::Input, uvec3);
    let gid_var = b.variable(ptr_in_uvec3, None, StorageClass::Input, None);
    b.decorate(gid_var, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::GlobalInvocationId)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let c_three = b.constant_bit32(u32_ty, 3);
    let c_one   = b.constant_bit32(u32_ty, 1);
    let c_scope = b.constant_bit32(u32_ty, Scope::Device as u32);
    let c_sem   = b.constant_bit32(u32_ty,
        MemorySemantics::ATOMIC_COUNTER_MEMORY.bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let gid = b.load(uvec3, None, gid_var, None, vec![]).unwrap();
    let gid_x = b.composite_extract(u32_ty, None, gid, vec![0]).unwrap();
    // sample = in.data[gid_x]
    let in_ptr = b.access_chain(ptr_u, None, v_in, vec![c_zero, gid_x]).unwrap();
    let sample = b.load(u32_ty, None, in_ptr, None, vec![]).unwrap();
    // bucket = sample & 3
    let bucket = b.bitwise_and(u32_ty, None, sample, c_three).unwrap();
    // atomicAdd(out.bins[bucket], 1)
    let out_ptr = b.access_chain(ptr_u, None, v_out, vec![c_zero, bucket]).unwrap();
    let _ = b.atomic_i_add(u32_ty, None, out_ptr, c_scope, c_sem, c_one).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![gid_var, v_in, v_out]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn vk_app_compute_histogram_end_to_end() {
    // Real-world compute primitive: histogram into 4 buckets
    // mod-4 over an 8-element input array.  Touches every
    // major feature landed in the session:
    //   - multi-binding SSBO (in@0, out@1) via descriptor table
    //   - dynamic AccessChain (in.data[gid_x], out.bins[bucket])
    //   - atomic IAdd into a dynamically-indexed slot
    //   - IAnd for the mod-4 computation
    let _ = env_logger::builder().is_test(true).try_init();
    let sock = tmp_socket("c_hist");
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

    let cs_spv = build_histogram_cs();
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
        stage, layout: vk::PipelineLayout::null(),
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

    // Input: 8 samples [3, 1, 4, 1, 5, 9, 2, 6] -- a classic.
    // Buckets are sample & 3:
    //   3->3, 1->1, 4->0, 1->1, 5->1, 9->1, 2->2, 6->2
    // Expected histogram: [1, 4, 2, 1]
    let samples = [3u32, 1, 4, 1, 5, 9, 2, 6];
    let mut prefill = vec![0u8; samples.len() * 4];
    for (i, v) in samples.iter().enumerate() {
        prefill[i*4..i*4+4].copy_from_slice(&v.to_le_bytes());
    }
    tier2_backend.set_compute_input_for_binding(0, prefill);

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
    // Dispatch 8 workgroups of 1 invocation each = 8 invocations
    // (one per sample).  Could also be 1 wg of 8 lanes; same
    // total invocation count, but the test's LocalSize=1 makes
    // gid_x equal the workgroup id which is what the host
    // iterates.
    unsafe { vkCmdDispatch(cb, 8, 1, 1); }
    unsafe { vkEndCommandBuffer(cb); }
    let mut submit = [0u8; 72];
    submit[0..4].copy_from_slice(&4u32.to_le_bytes());
    submit[40..44].copy_from_slice(&1u32.to_le_bytes());
    let cb_arr = [cb];
    submit[48..56].copy_from_slice(&(cb_arr.as_ptr() as u64).to_le_bytes());
    unsafe { vkQueueSubmit(queue, 1, submit.as_ptr() as *const _, std::ptr::null_mut()); }
    thread::sleep(Duration::from_millis(300));

    let bins = tier2_backend.compute_output_bytes_for_binding(1)
        .expect("histogram output");
    let counts: Vec<u32> = (0..4)
        .map(|i| u32::from_le_bytes(bins[i*4..i*4+4].try_into().unwrap()))
        .collect();
    // 3, 1, 4, 1, 5, 9, 2, 6  ->  buckets: 3,1,0,1,1,1,2,2
    //   bucket 0: 1 (sample 4)
    //   bucket 1: 4 (samples 1, 1, 5, 9)
    //   bucket 2: 2 (samples 2, 6)
    //   bucket 3: 1 (sample 3)
    assert_eq!(counts, vec![1, 4, 2, 1],
        "histogram bins should be [1,4,2,1]; got {counts:?}");

    unsafe { atrium_vk_icd::vkDestroyInstance(instance, std::ptr::null()); }
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}
