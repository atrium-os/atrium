//! `examples/loader_smoke` — drive atrium-vk-icd through the actual
//! Khronos Vulkan loader (`libvulkan.so.1` / `libvulkan.dylib`) using
//! the `ash` Rust bindings, the way a real Vulkan app does.
//!
//! Companion to `examples/headless_triangle`, which calls into the
//! cdylib directly and bypasses the loader entirely.  This example
//! tests the bit the loader-direct example can't: the `vk_icdNegotiate
//! LoaderICDInterfaceVersion` / `vk_icdGetInstanceProcAddr` ABI
//! surface that the Khronos loader's dlopen-and-call-through path
//! actually exercises.
//!
//! # What it does
//!
//! Walks the minimum call sequence that exercises the loader-to-ICD
//! dispatch path end-to-end:
//!
//!   1. `ash::Entry::load`     -- dlopens `libvulkan` (the loader).
//!   2. `vkCreateInstance`     -- loader -> ICD instance dispatch.
//!   3. `vkEnumeratePhysicalDevices` -- handshake with the daemon.
//!   4. `vkGetPhysicalDeviceProperties` -- pre-device queries.
//!   5. `vkCreateDevice`       -- single graphics queue, no features.
//!   6. `vkCreateShaderModule` -- uploads a trivial compute SPIR-V
//!      blob, which the daemon routes through its Tier2Registry +
//!      `atrium-spv-compile` if `--tier2` was passed.
//!
//! # Usage
//!
//! ```sh
//! # No daemon -- exercises the loader/ICD ABI surface only.
//! VK_DRIVER_FILES=/path/to/atrium_icd.json \
//!   cargo run --example loader_smoke
//!
//! # With a tier-1-only daemon -- physical device + properties work,
//! # shader-module upload returns SUCCESS but is recorded with
//! # tier2_id=None (no compile happened).
//! aqueduct-gpu-host --socket /tmp/x.sock --backend software &
//! VK_DRIVER_FILES=/path/to/atrium_icd.json \
//! ATRIUM_VK_ICD_SOCKET=/tmp/x.sock \
//!   cargo run --example loader_smoke
//!
//! # With a tier-2 daemon -- shader-module upload triggers an
//! # atrium-spv-compile invocation; the cache directory ends up
//! # with a fresh `.afblob` + `.pcmap` pair.
//! aqueduct-gpu-host --socket /tmp/x.sock --backend software \
//!     --tier2 --cache-root /tmp/cache \
//!     --compile-binary /path/to/atrium-spv-compile &
//! VK_DRIVER_FILES=/path/to/atrium_icd.json \
//! ATRIUM_VK_ICD_SOCKET=/tmp/x.sock \
//!   cargo run --example loader_smoke
//! ```
//!
//! `scripts/loader_smoke_macos.sh` is the canonical macOS-host
//! wrapper that performs all three rungs in one go.

use ash::vk;
use rspirv::binary::Assemble;
use rspirv::spirv::{
    AddressingModel, Capability, ExecutionMode, ExecutionModel,
    FunctionControl, MemoryModel,
};

/// Build the smallest valid compute SPIR-V module: a void `main`
/// that returns immediately.  ~140 bytes; the atrium-spv-frontend
/// phase-1 path handles this exactly.
fn build_trivial_compute_spirv() -> Vec<u8> {
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void   = b.type_void();
    let void_fn = b.type_function(void, vec![]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

fn main() {
    let entry = unsafe {
        match ash::Entry::load() {
            Ok(e)  => { println!("ash::Entry::load           -> OK"); e }
            Err(e) => { println!("ash::Entry::load           -> ERROR: {e}"); return; }
        }
    };

    // ENUMERATE_PORTABILITY_KHR + the matching extension so the
    // macOS loader doesn't filter our ICD as a non-portability
    // driver before the dispatch even reaches it.
    let app_info = vk::ApplicationInfo::default()
        .api_version(vk::API_VERSION_1_3);
    let flags = vk::InstanceCreateFlags::ENUMERATE_PORTABILITY_KHR;
    let exts = [vk::KHR_PORTABILITY_ENUMERATION_NAME.as_ptr()];
    let create_info = vk::InstanceCreateInfo::default()
        .application_info(&app_info)
        .flags(flags)
        .enabled_extension_names(&exts);

    let instance = unsafe {
        match entry.create_instance(&create_info, None) {
            Ok(i)  => { println!("vkCreateInstance           -> OK ({:?})", i.handle()); i }
            Err(e) => { println!("vkCreateInstance           -> ERROR: {e:?}"); return; }
        }
    };

    let pds = unsafe {
        match instance.enumerate_physical_devices() {
            Ok(v)  => {
                println!("vkEnumeratePhysicalDevices -> OK, {} device(s)", v.len());
                v
            }
            Err(e) => {
                println!("vkEnumeratePhysicalDevices -> ERROR: {e:?}");
                Vec::new()
            }
        }
    };
    if pds.is_empty() {
        // No daemon reachable.  This is the documented "ABI rung
        // only" path; we've already exercised CreateInstance.
        unsafe { instance.destroy_instance(None); }
        return;
    }

    let pd = pds[0];
    let props = unsafe { instance.get_physical_device_properties(pd) };
    let name = unsafe {
        std::ffi::CStr::from_ptr(props.device_name.as_ptr())
            .to_string_lossy().to_string()
    };
    println!("  device[0] = {name:?}, api = 0x{:08x}", props.api_version);

    let qp = vk::DeviceQueueCreateInfo::default()
        .queue_family_index(0)
        .queue_priorities(&[1.0]);
    let queue_infos = [qp];
    let device_create = vk::DeviceCreateInfo::default()
        .queue_create_infos(&queue_infos);
    let device = unsafe {
        match instance.create_device(pd, &device_create, None) {
            Ok(d)  => { println!("vkCreateDevice             -> OK"); d }
            Err(e) => {
                println!("vkCreateDevice             -> ERROR: {e:?}");
                instance.destroy_instance(None);
                return;
            }
        }
    };

    // Build + upload the trivial compute SPIR-V.  Under --tier2,
    // this fires an atrium-spv-compile invocation on the daemon
    // side and an `.afblob` + `.pcmap` pair lands in the cache.
    let spv_bytes = build_trivial_compute_spirv();
    let spv_words: Vec<u32> = spv_bytes.chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    println!("trivial compute SPIR-V:    {} bytes ({} words)",
        spv_bytes.len(), spv_words.len());
    let sm_info = vk::ShaderModuleCreateInfo::default()
        .code(&spv_words);
    let shader = unsafe {
        match device.create_shader_module(&sm_info, None) {
            Ok(s)  => { println!("vkCreateShaderModule       -> OK ({s:?})"); Some(s) }
            Err(e) => { println!("vkCreateShaderModule       -> ERROR: {e:?}"); None }
        }
    };

    if let Some(s) = shader {
        unsafe { device.destroy_shader_module(s, None); }
    }
    unsafe { device.destroy_device(None); }
    unsafe { instance.destroy_instance(None); }
    println!("done.");
}
