//! Compute pipeline + dispatch end-to-end through tier-2.
//!
//! Drives an empty compute shader (read-only inputs only;
//! the existing CsMain ABI has no output pointer) through:
//!   vkCreateShaderModule -> vkCreateComputePipelines
//!   -> vkCmdBindPipeline -> vkCmdDispatch -> vkQueueSubmit.
//!
//! Asserts Tier2Backend's cs_invocation_count matches
//! groupCount[xyz] * local_size[xyz] -- proving the wire is
//! correct end-to-end: SPIR-V `LocalSize` extraction, Compute
//! state-blob round-trip, Tier2ComputeStateBlob unpacking,
//! and the (workgroup, local-invocation) iteration loop.
//!
//! Real compute workloads need an output-buffer pointer added
//! to the CsMain ABI -- separate cross-backend arc.

use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use aqueduct_gpu_host::{Backend, Listener, Tier2Backend, Tier2Registry};
use atrium_spv_loader::LoaderConfig;
use atrium_vk_icd::{
    vkAllocateCommandBuffers, vkBeginCommandBuffer,
    vkCmdBindPipeline, vkCmdDispatch, vkCreateCommandPool,
    vkCreateComputePipelines, vkCreateDevice, vkCreateInstance,
    vkCreateShaderModule, vkDestroyInstance, vkEndCommandBuffer,
    vkEnumeratePhysicalDevices, vkGetDeviceQueue, vkQueueSubmit,
};
use tempfile::TempDir;

type VkInstance       = *mut std::ffi::c_void;
type VkDevice         = *mut std::ffi::c_void;
type VkQueue          = *mut std::ffi::c_void;
type VkCommandBuffer  = *mut std::ffi::c_void;
type VkPhysicalDevice = *mut std::ffi::c_void;

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
    assert!(p.exists(), "atrium-spv-compile not at {}", p.display());
    p
}

/// Minimal compute SPIR-V: GLCompute entry point with
/// `LocalSize = (4, 1, 1)`, empty body.
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

fn make_shader_module(device: VkDevice, spv: &[u8]) -> u64 {
    let mut info = [0u8; 40];
    info[ 0.. 4].copy_from_slice(&16u32.to_le_bytes());
    info[24..32].copy_from_slice(&(spv.len() as u64).to_le_bytes());
    info[32..40].copy_from_slice(&(spv.as_ptr() as u64).to_le_bytes());
    let mut sm: u64 = 0;
    unsafe {
        vkCreateShaderModule(device, info.as_ptr() as *const _,
                             std::ptr::null(), &mut sm);
    }
    assert!(sm != 0);
    sm
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
