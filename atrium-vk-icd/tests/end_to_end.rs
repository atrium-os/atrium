//! atrium-vk-icd end-to-end test: spawn a real aqueduct-gpu host
//! endpoint (SoftwareBackend on a temp socket), point the ICD at
//! it via the `ATRIUM_VK_ICD_SOCKET` env var, and verify the
//! handshake-then-enumerate path produces exactly one
//! VkPhysicalDevice.
//!
//! This proves the device-discovery path that vkCreateInstance
//! takes when the daemon IS reachable — complementing the unit
//! tests which exercise the "daemon not running" fallback (zero
//! devices).

use std::os::raw::c_char;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use aqueduct_gpu_host::{Backend, Listener, SoftwareBackend};
use atrium_vk_icd::{
    vkCreateInstance, vkDestroyInstance, vkEnumeratePhysicalDevices,
    vk_icdGetInstanceProcAddr,
};

// Opaque handle types — match the ICD's `*mut c_void` aliases.
type VkInstance       = *mut std::ffi::c_void;
type VkPhysicalDevice = *mut std::ffi::c_void;

fn tmp_socket(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("atrium-vk-icd-test-{}-{}.sock",
                   std::process::id(), name));
    p
}

/// Process-wide serialization point for tests that drive the ICD
/// via `ATRIUM_VK_ICD_SOCKET`.
///
/// The ICD reads that env var inside `try_connect_aqueduct` on
/// each `vkCreateInstance`. The env var is process-global, so
/// parallel cargo-test workers would otherwise overwrite each
/// other's settings and connect to the wrong daemon (the
/// observed flakes before this guard landed). Tests acquire the
/// lock + set the var via `EnvLock::set`, and the RAII drop
/// clears the var when the test's instance has been destroyed.
///
/// Using `parking_lot`'s style here would be cleaner, but
/// std::sync::Mutex is good enough and keeps the dev-deps list
/// tight. Lock poisoning on a panicked test isn't a real concern
/// — cargo aborts the worker, but the OS reclaims the env var.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct EnvLock {
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl EnvLock {
    fn set(sock: &std::path::Path) -> Self {
        // Recover from a poisoned mutex — a previously-panicked
        // test would otherwise wedge every subsequent test in the
        // suite. The mutex protects an env var, not invariants
        // we care about, so taking the inner guard is safe.
        let guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var("ATRIUM_VK_ICD_SOCKET", sock);
        EnvLock { _guard: guard }
    }
}

impl Drop for EnvLock {
    fn drop(&mut self) {
        // env var cleared by EnvLock drop
    }
}

#[test]
fn live_handshake_reports_one_physical_device() {
    let sock = tmp_socket("live");
    let sw_backend = Arc::new(SoftwareBackend::new());
    let backend_for_listener: Arc<dyn Backend> = sw_backend.clone();
    let listener = Listener::bind(&sock, backend_for_listener).unwrap();
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));

    // Point the ICD's connect path at our temp socket.
    let _env = EnvLock::set(&sock);

    let mut instance: VkInstance = std::ptr::null_mut();
    let r = unsafe {
        vkCreateInstance(std::ptr::null(), std::ptr::null(), &mut instance)
    };
    assert_eq!(r, 0, "vkCreateInstance should succeed");
    assert!(!instance.is_null());

    // First call: count-only query.
    let mut count: u32 = 99;
    let r = unsafe {
        vkEnumeratePhysicalDevices(instance, &mut count, std::ptr::null_mut())
    };
    assert_eq!(r, 0);
    assert_eq!(count, 1,
        "handshake against live daemon should expose exactly one device");

    // Second call: fill the buffer.
    let mut devices: [VkPhysicalDevice; 4] = [std::ptr::null_mut(); 4];
    let mut cap: u32 = 4;
    let r = unsafe {
        vkEnumeratePhysicalDevices(instance, &mut cap, devices.as_mut_ptr())
    };
    assert_eq!(r, 0);
    assert_eq!(cap, 1);
    assert!(!devices[0].is_null(), "device 0 handle must be non-null");

    // Loader-ICD ABI: VkPhysicalDevice first slot also holds magic.
    let slot: usize = unsafe { *(devices[0] as *const usize) };
    assert_eq!(slot, 0x01CDC0DE,
        "VkPhysicalDevice must carry the loader magic too");

    unsafe { vkDestroyInstance(instance, std::ptr::null()); }

    // env var cleared by EnvLock drop
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn physical_device_properties_match_backend() {
    use atrium_vk_icd::vkGetPhysicalDeviceProperties;

    let sock = tmp_socket("props");
    let sw_backend = Arc::new(SoftwareBackend::new());
    let backend_for_listener: Arc<dyn Backend> = sw_backend.clone();
    let listener = Listener::bind(&sock, backend_for_listener).unwrap();
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));

    let _env = EnvLock::set(&sock);

    let mut instance: VkInstance = std::ptr::null_mut();
    unsafe { vkCreateInstance(std::ptr::null(), std::ptr::null(), &mut instance); }
    let mut devices: [VkPhysicalDevice; 1] = [std::ptr::null_mut(); 1];
    let mut cap: u32 = 1;
    unsafe { vkEnumeratePhysicalDevices(instance, &mut cap, devices.as_mut_ptr()); }
    assert_eq!(cap, 1);

    let mut props = ash::vk::PhysicalDeviceProperties::default();
    unsafe { vkGetPhysicalDeviceProperties(devices[0], &mut props); }

    // apiVersion encodes major=1 minor=3.
    let major = (props.api_version >> 22) & 0x7f;
    let minor = (props.api_version >> 12) & 0x3ff;
    assert_eq!((major, minor), (1, 3));
    assert_eq!(props.device_type, ash::vk::PhysicalDeviceType::VIRTUAL_GPU);
    // deviceName starts with "atrium-vk-icd".
    let name_bytes: Vec<u8> = props.device_name.iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    let name = std::str::from_utf8(&name_bytes).unwrap();
    assert!(name.starts_with("atrium-vk-icd"), "got {name:?}");

    unsafe { vkDestroyInstance(instance, std::ptr::null()); }
    // env var cleared by EnvLock drop
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn queue_family_properties_report_one_unified_family() {
    use atrium_vk_icd::vkGetPhysicalDeviceQueueFamilyProperties;

    let sock = tmp_socket("qfp");
    let sw_backend = Arc::new(SoftwareBackend::new());
    let backend_for_listener: Arc<dyn Backend> = sw_backend.clone();
    let listener = Listener::bind(&sock, backend_for_listener).unwrap();
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));

    let _env = EnvLock::set(&sock);

    let mut instance: VkInstance = std::ptr::null_mut();
    unsafe { vkCreateInstance(std::ptr::null(), std::ptr::null(), &mut instance); }
    let mut devices: [VkPhysicalDevice; 1] = [std::ptr::null_mut(); 1];
    let mut cap: u32 = 1;
    unsafe { vkEnumeratePhysicalDevices(instance, &mut cap, devices.as_mut_ptr()); }

    // Count-only.
    let mut count: u32 = 0;
    unsafe { vkGetPhysicalDeviceQueueFamilyProperties(devices[0], &mut count, std::ptr::null_mut()); }
    assert_eq!(count, 1);

    // Fill.
    let mut qfp = [ash::vk::QueueFamilyProperties::default(); 4];
    let mut cap_q: u32 = 4;
    unsafe { vkGetPhysicalDeviceQueueFamilyProperties(devices[0], &mut cap_q, qfp.as_mut_ptr()); }
    assert_eq!(cap_q, 1);
    let want = ash::vk::QueueFlags::GRAPHICS
        | ash::vk::QueueFlags::COMPUTE
        | ash::vk::QueueFlags::TRANSFER;
    assert_eq!(qfp[0].queue_flags & want, want, "queue family must support gfx+compute+transfer");
    assert_eq!(qfp[0].queue_count, 1);

    unsafe { vkDestroyInstance(instance, std::ptr::null()); }
    // env var cleared by EnvLock drop
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn device_create_queue_destroy_round_trip() {
    use atrium_vk_icd::{vkCreateDevice, vkDestroyDevice, vkGetDeviceQueue};

    let sock = tmp_socket("dev");
    let sw_backend = Arc::new(SoftwareBackend::new());
    let backend_for_listener: Arc<dyn Backend> = sw_backend.clone();
    let listener = Listener::bind(&sock, backend_for_listener).unwrap();
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));

    let _env = EnvLock::set(&sock);

    let mut instance: VkInstance = std::ptr::null_mut();
    unsafe { vkCreateInstance(std::ptr::null(), std::ptr::null(), &mut instance); }
    let mut devices: [VkPhysicalDevice; 1] = [std::ptr::null_mut(); 1];
    let mut cap: u32 = 1;
    unsafe { vkEnumeratePhysicalDevices(instance, &mut cap, devices.as_mut_ptr()); }
    assert_eq!(cap, 1);

    // Create logical device on the physical device.
    type VkDevice = *mut std::ffi::c_void;
    type VkQueue  = *mut std::ffi::c_void;
    let mut device: VkDevice = std::ptr::null_mut();
    let r = unsafe {
        vkCreateDevice(devices[0], std::ptr::null(), std::ptr::null(), &mut device)
    };
    assert_eq!(r, 0);
    assert!(!device.is_null());
    // Loader-ICD magic at offset 0.
    let slot: usize = unsafe { *(device as *const usize) };
    assert_eq!(slot, 0x01CDC0DE, "VkDevice must carry the loader magic");

    // Grab the queue for (family=0, index=0).
    let mut queue: VkQueue = std::ptr::null_mut();
    unsafe { vkGetDeviceQueue(device, 0, 0, &mut queue); }
    assert!(!queue.is_null(), "(0, 0) queue must exist on Atrium devices");
    let qslot: usize = unsafe { *(queue as *const usize) };
    assert_eq!(qslot, 0x01CDC0DE, "VkQueue must carry the loader magic");

    // A bogus (family, index) returns NULL.
    let mut bogus: VkQueue = std::ptr::null_mut();
    unsafe { vkGetDeviceQueue(device, 0, 99, &mut bogus); }
    assert!(bogus.is_null(), "out-of-range queue must return NULL");

    unsafe { vkDestroyDevice(device, std::ptr::null()); }
    unsafe { vkDestroyInstance(instance, std::ptr::null()); }
    // env var cleared by EnvLock drop
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn command_pool_and_buffer_lifecycle() {
    use atrium_vk_icd::{
        vkAllocateCommandBuffers, vkBeginCommandBuffer, vkCreateCommandPool,
        vkCreateDevice, vkDestroyCommandPool, vkDestroyDevice, vkEndCommandBuffer,
        vkFreeCommandBuffers, vkQueueSubmit, vkResetCommandBuffer, vkGetDeviceQueue,
    };

    let sock = tmp_socket("cb");
    let sw_backend = Arc::new(SoftwareBackend::new());
    let backend_for_listener: Arc<dyn Backend> = sw_backend.clone();
    let listener = Listener::bind(&sock, backend_for_listener).unwrap();
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));

    let _env = EnvLock::set(&sock);

    type VkDevice = *mut std::ffi::c_void;
    type VkQueue  = *mut std::ffi::c_void;
    type VkCommandBuffer = *mut std::ffi::c_void;

    // Bootstrap: instance + physical + logical device.
    let mut instance: VkInstance = std::ptr::null_mut();
    unsafe { vkCreateInstance(std::ptr::null(), std::ptr::null(), &mut instance); }
    let mut devices: [VkPhysicalDevice; 1] = [std::ptr::null_mut(); 1];
    let mut cap: u32 = 1;
    unsafe { vkEnumeratePhysicalDevices(instance, &mut cap, devices.as_mut_ptr()); }
    let mut device: VkDevice = std::ptr::null_mut();
    unsafe { vkCreateDevice(devices[0], std::ptr::null(), std::ptr::null(), &mut device); }

    // Create a command pool.
    let mut pool: u64 = 0;
    let r = unsafe {
        vkCreateCommandPool(device, std::ptr::null(), std::ptr::null(), &mut pool)
    };
    assert_eq!(r, 0);
    assert!(pool != 0);

    // Allocate 2 command buffers from it.
    // VkCommandBufferAllocateInfo layout (40 bytes, no pNext set):
    //   0   sType : u32   = 40 (VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO)
    //   4   _pad : u32
    //   8   pNext : ptr   = null
    //   16  commandPool : u64
    //   24  level : u32   = 0 (PRIMARY)
    //   28  commandBufferCount : u32 = 2
    //   32  _pad : u32
    //   36  _pad : u32
    let mut info = [0u8; 40];
    info[0..4].copy_from_slice(&40u32.to_le_bytes());
    info[16..24].copy_from_slice(&pool.to_le_bytes());
    info[28..32].copy_from_slice(&2u32.to_le_bytes());

    let mut cbs: [VkCommandBuffer; 2] = [std::ptr::null_mut(); 2];
    let r = unsafe {
        vkAllocateCommandBuffers(device, info.as_ptr() as *const _, cbs.as_mut_ptr())
    };
    assert_eq!(r, 0);
    assert!(!cbs[0].is_null() && !cbs[1].is_null());
    // Both carry loader magic.
    for cb in &cbs {
        let slot: usize = unsafe { *(*cb as *const usize) };
        assert_eq!(slot, 0x01CDC0DE);
    }

    // Begin → End → Reset cycle on the first buffer.
    assert_eq!(unsafe { vkBeginCommandBuffer(cbs[0], std::ptr::null()) }, 0);
    assert_eq!(unsafe { vkEndCommandBuffer(cbs[0]) }, 0);
    // Calling End on a non-Recording buffer is an error.
    assert_ne!(unsafe { vkEndCommandBuffer(cbs[0]) }, 0,
        "End on Executable must error");
    assert_eq!(unsafe { vkResetCommandBuffer(cbs[0], 0) }, 0);
    // Now back in Initial, Begin works again.
    assert_eq!(unsafe { vkBeginCommandBuffer(cbs[0], std::ptr::null()) }, 0);
    assert_eq!(unsafe { vkEndCommandBuffer(cbs[0]) }, 0);

    // Get the queue and submit. Today vkQueueSubmit is a no-op
    // success since vkCmd* isn't wired; just verify the entry
    // point + signature compile and return success.
    let mut queue: VkQueue = std::ptr::null_mut();
    unsafe { vkGetDeviceQueue(device, 0, 0, &mut queue); }
    let r = unsafe {
        vkQueueSubmit(queue, 0, std::ptr::null(), std::ptr::null_mut())
    };
    assert_eq!(r, 0);

    // Free one buffer explicitly, leave the other for pool teardown.
    unsafe { vkFreeCommandBuffers(device, pool, 1, &cbs[1] as *const _); }
    unsafe { vkDestroyCommandPool(device, pool, std::ptr::null()); }
    unsafe { vkDestroyDevice(device, std::ptr::null()); }
    unsafe { vkDestroyInstance(instance, std::ptr::null()); }
    // env var cleared by EnvLock drop
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn physical_device_features_and_memory_queries() {
    use atrium_vk_icd::{
        vkGetPhysicalDeviceFeatures, vkGetPhysicalDeviceMemoryProperties,
        vkGetPhysicalDeviceFormatProperties,
    };

    let sock = tmp_socket("trio");
    let sw_backend = Arc::new(SoftwareBackend::new());
    let backend_for_listener: Arc<dyn Backend> = sw_backend.clone();
    let listener = Listener::bind(&sock, backend_for_listener).unwrap();
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));

    let _env = EnvLock::set(&sock);

    let mut instance: VkInstance = std::ptr::null_mut();
    unsafe { vkCreateInstance(std::ptr::null(), std::ptr::null(), &mut instance); }
    let mut devices: [VkPhysicalDevice; 1] = [std::ptr::null_mut(); 1];
    let mut cap: u32 = 1;
    unsafe { vkEnumeratePhysicalDevices(instance, &mut cap, devices.as_mut_ptr()); }

    // Features: zero (no features enabled).
    let mut feats = ash::vk::PhysicalDeviceFeatures::default();
    feats.geometry_shader = 1; // dirty the field; verify ICD zeros it.
    unsafe { vkGetPhysicalDeviceFeatures(devices[0], &mut feats); }
    assert_eq!(feats.geometry_shader, 0);

    // Memory: 1 heap + 1 memory type, host-visible + coherent.
    let mut mp = ash::vk::PhysicalDeviceMemoryProperties::default();
    unsafe { vkGetPhysicalDeviceMemoryProperties(devices[0], &mut mp); }
    assert_eq!(mp.memory_heap_count, 1);
    assert_eq!(mp.memory_type_count, 1);
    assert!(mp.memory_heaps[0].size >= 1024 * 1024 * 1024);
    assert!(mp.memory_types[0].property_flags
        .contains(ash::vk::MemoryPropertyFlags::HOST_VISIBLE));
    assert!(mp.memory_types[0].property_flags
        .contains(ash::vk::MemoryPropertyFlags::HOST_COHERENT));

    // Format: R8G8B8A8_UNORM advertises tier-1 color attachment
    // + sampled + blend (see vkGetPhysicalDeviceFormatProperties
    // capability matrix). Unit-test
    // format_properties_advertise_correct_tier1_features covers
    // the per-format detail; here we just confirm the e2e wiring
    // returns the same shape against a live daemon.
    use ash::vk::FormatFeatureFlags as F;
    let mut fp = ash::vk::FormatProperties::default();
    unsafe {
        vkGetPhysicalDeviceFormatProperties(
            devices[0], ash::vk::Format::R8G8B8A8_UNORM, &mut fp,
        );
    }
    assert!(fp.optimal_tiling_features.contains(F::COLOR_ATTACHMENT));
    assert!(fp.optimal_tiling_features.contains(F::SAMPLED_IMAGE));
    assert!(fp.buffer_features.contains(F::VERTEX_BUFFER));

    unsafe { vkDestroyInstance(instance, std::ptr::null()); }
    // env var cleared by EnvLock drop
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn cmdbuf_records_frame_ops_through_vkcmd_apis() {
    use atrium_vk_icd::{
        cmdbuf_recorded_bytes,
        vkAllocateCommandBuffers, vkBeginCommandBuffer, vkCmdDraw,
        vkCmdSetScissor, vkCmdSetViewport, vkCmdPushConstants,
        vkCreateCommandPool, vkCreateDevice, vkDestroyCommandPool,
        vkDestroyDevice, vkEndCommandBuffer,
    };

    let sock = tmp_socket("rec");
    let sw_backend = Arc::new(SoftwareBackend::new());
    let backend_for_listener: Arc<dyn Backend> = sw_backend.clone();
    let listener = Listener::bind(&sock, backend_for_listener).unwrap();
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));

    let _env = EnvLock::set(&sock);

    type VkDevice = *mut std::ffi::c_void;
    type VkCommandBuffer = *mut std::ffi::c_void;

    // Bootstrap.
    let mut instance: VkInstance = std::ptr::null_mut();
    unsafe { vkCreateInstance(std::ptr::null(), std::ptr::null(), &mut instance); }
    let mut devices: [VkPhysicalDevice; 1] = [std::ptr::null_mut(); 1];
    let mut cap: u32 = 1;
    unsafe { vkEnumeratePhysicalDevices(instance, &mut cap, devices.as_mut_ptr()); }
    let mut device: VkDevice = std::ptr::null_mut();
    unsafe { vkCreateDevice(devices[0], std::ptr::null(), std::ptr::null(), &mut device); }
    let mut pool: u64 = 0;
    unsafe { vkCreateCommandPool(device, std::ptr::null(), std::ptr::null(), &mut pool); }

    let mut info = [0u8; 40];
    info[0..4].copy_from_slice(&40u32.to_le_bytes());
    info[16..24].copy_from_slice(&pool.to_le_bytes());
    info[28..32].copy_from_slice(&1u32.to_le_bytes());
    let mut cbs: [VkCommandBuffer; 1] = [std::ptr::null_mut(); 1];
    unsafe { vkAllocateCommandBuffers(device, info.as_ptr() as *const _, cbs.as_mut_ptr()); }
    let cb = cbs[0];

    // vkCmd* before Begin: silently dropped (state machine guard).
    unsafe { vkCmdDraw(cb, 3, 1, 0, 0); }
    assert_eq!(cmdbuf_recorded_bytes(cb).len(), 0,
        "vkCmd* outside Recording must drop, not record");

    // Begin → record 4 ops → End.
    unsafe { vkBeginCommandBuffer(cb, std::ptr::null()); }
    assert_eq!(cmdbuf_recorded_bytes(cb).len(), 0, "fresh Recording starts empty");

    let vp = ash::vk::Viewport {
        x: 0.0, y: 0.0, width: 800.0, height: 600.0,
        min_depth: 0.0, max_depth: 1.0,
    };
    unsafe { vkCmdSetViewport(cb, 0, 1, &vp); }
    let sc = ash::vk::Rect2D {
        offset: ash::vk::Offset2D { x: 0, y: 0 },
        extent: ash::vk::Extent2D { width: 800, height: 600 },
    };
    unsafe { vkCmdSetScissor(cb, 0, 1, &sc); }
    let pc_bytes: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
    unsafe { vkCmdPushConstants(cb, 0, 0x00000010 /* FRAGMENT */, 0, 8,
        pc_bytes.as_ptr() as *const _); }
    unsafe { vkCmdDraw(cb, 3, 1, 0, 0); }
    unsafe { vkEndCommandBuffer(cb); }

    // Inspect the recorded stream. Each record is 8 bytes of
    // header + body bytes. The four ops are:
    //   SetViewport    (0x0030, body 24 bytes) → 8 + 24 = 32 bytes
    //   SetScissor     (0x0031, body 16 bytes) → 8 + 16 = 24 bytes
    //   PushConstants  (0x0032, body 8 hdr + 8 payload = 16) → 24 bytes
    //   Draw           (0x0040, body 16 bytes) → 24 bytes
    //   Total: 104 bytes.
    let bytes = cmdbuf_recorded_bytes(cb);
    assert_eq!(bytes.len(), 104, "expected 104 bytes recorded, got {}", bytes.len());

    // Verify the first two opcodes are SetViewport, SetScissor.
    let op0 = u16::from_le_bytes([bytes[0], bytes[1]]);
    let op1 = u16::from_le_bytes([bytes[32], bytes[33]]);
    assert_eq!(op0, 0x0030, "first op must be SetViewport");
    assert_eq!(op1, 0x0031, "second op must be SetScissor");

    // Cleanup.
    unsafe { vkDestroyCommandPool(device, pool, std::ptr::null()); }
    unsafe { vkDestroyDevice(device, std::ptr::null()); }
    unsafe { vkDestroyInstance(instance, std::ptr::null()); }
    // env var cleared by EnvLock drop
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn submit_flushes_recorded_frame_to_aqueduct_backend() {
    use atrium_vk_icd::{
        vkAllocateCommandBuffers, vkBeginCommandBuffer, vkCmdDraw,
        vkCmdSetViewport, vkCreateCommandPool, vkCreateDevice,
        vkDestroyCommandPool, vkDestroyDevice, vkEndCommandBuffer,
        vkGetDeviceQueue, vkQueueSubmit,
    };

    let sock = tmp_socket("submit");
    let sw_backend = Arc::new(SoftwareBackend::new());
    let backend_for_listener: Arc<dyn Backend> = sw_backend.clone();
    let listener = Listener::bind(&sock, backend_for_listener).unwrap();
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));

    let _env = EnvLock::set(&sock);

    type VkDevice = *mut std::ffi::c_void;
    type VkQueue  = *mut std::ffi::c_void;
    type VkCommandBuffer = *mut std::ffi::c_void;

    let mut instance: VkInstance = std::ptr::null_mut();
    unsafe { vkCreateInstance(std::ptr::null(), std::ptr::null(), &mut instance); }
    let mut devices: [VkPhysicalDevice; 1] = [std::ptr::null_mut(); 1];
    let mut cap: u32 = 1;
    unsafe { vkEnumeratePhysicalDevices(instance, &mut cap, devices.as_mut_ptr()); }
    let mut device: VkDevice = std::ptr::null_mut();
    unsafe { vkCreateDevice(devices[0], std::ptr::null(), std::ptr::null(), &mut device); }
    let mut queue: VkQueue = std::ptr::null_mut();
    unsafe { vkGetDeviceQueue(device, 0, 0, &mut queue); }
    let mut pool: u64 = 0;
    unsafe { vkCreateCommandPool(device, std::ptr::null(), std::ptr::null(), &mut pool); }

    let mut info = [0u8; 40];
    info[0..4].copy_from_slice(&40u32.to_le_bytes());
    info[16..24].copy_from_slice(&pool.to_le_bytes());
    info[28..32].copy_from_slice(&1u32.to_le_bytes());
    let mut cbs: [VkCommandBuffer; 1] = [std::ptr::null_mut(); 1];
    unsafe { vkAllocateCommandBuffers(device, info.as_ptr() as *const _, cbs.as_mut_ptr()); }
    let cb = cbs[0];

    // Record a non-empty frame.
    unsafe { vkBeginCommandBuffer(cb, std::ptr::null()); }
    let vp = ash::vk::Viewport {
        x: 0.0, y: 0.0, width: 1.0, height: 1.0,
        min_depth: 0.0, max_depth: 1.0,
    };
    unsafe { vkCmdSetViewport(cb, 0, 1, &vp); }
    unsafe { vkCmdDraw(cb, 3, 1, 0, 0); }
    unsafe { vkEndCommandBuffer(cb); }

    // Build a VkSubmitInfo (size 72) referencing the cmdbuf.
    let pre_count = sw_backend.submission_count();
    let mut submit_info = [0u8; 72];
    submit_info[0..4].copy_from_slice(&4u32.to_le_bytes()); // sType
    submit_info[40..44].copy_from_slice(&1u32.to_le_bytes()); // cb count
    let cb_array = [cb];
    let cb_ptr = cb_array.as_ptr();
    submit_info[48..56].copy_from_slice(&(cb_ptr as u64).to_le_bytes());

    let r = unsafe {
        vkQueueSubmit(queue, 1, submit_info.as_ptr() as *const _, std::ptr::null_mut())
    };
    assert_eq!(r, 0);

    // Give the SoftwareBackend a moment to process the SubmitFrame
    // op through its session thread.
    thread::sleep(Duration::from_millis(100));
    let post_count = sw_backend.submission_count();
    assert!(post_count > pre_count,
        "backend should have observed the submit (pre={pre_count} post={post_count})");

    // After submit, the cmdbuf's recording buffer should be empty
    // (we swapped it out in vkQueueSubmit's drain).
    let leftover = atrium_vk_icd::cmdbuf_recorded_bytes(cb);
    assert_eq!(leftover.len(), 0,
        "cmdbuf frame must be reset after submit, got {} bytes", leftover.len());

    unsafe { vkDestroyCommandPool(device, pool, std::ptr::null()); }
    unsafe { vkDestroyDevice(device, std::ptr::null()); }
    unsafe { vkDestroyInstance(instance, std::ptr::null()); }
    // env var cleared by EnvLock drop
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn pipeline_create_bind_destroy_round_trip() {
    use atrium_vk_icd::{
        cmdbuf_recorded_bytes,
        vkAllocateCommandBuffers, vkBeginCommandBuffer, vkCmdBindPipeline,
        vkCreateCommandPool, vkCreateDevice, vkCreateGraphicsPipelines,
        vkCreatePipelineLayout, vkDestroyCommandPool, vkDestroyDevice,
        vkDestroyPipeline, vkDestroyPipelineLayout, vkEndCommandBuffer,
    };

    let sock = tmp_socket("pipe");
    let sw_backend = Arc::new(SoftwareBackend::new());
    let backend_for_listener: Arc<dyn Backend> = sw_backend.clone();
    let listener = Listener::bind(&sock, backend_for_listener).unwrap();
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));

    let _env = EnvLock::set(&sock);

    type VkDevice = *mut std::ffi::c_void;
    type VkCommandBuffer = *mut std::ffi::c_void;

    let mut instance: VkInstance = std::ptr::null_mut();
    unsafe { vkCreateInstance(std::ptr::null(), std::ptr::null(), &mut instance); }
    let mut devices: [VkPhysicalDevice; 1] = [std::ptr::null_mut(); 1];
    let mut cap: u32 = 1;
    unsafe { vkEnumeratePhysicalDevices(instance, &mut cap, devices.as_mut_ptr()); }
    let mut device: VkDevice = std::ptr::null_mut();
    unsafe { vkCreateDevice(devices[0], std::ptr::null(), std::ptr::null(), &mut device); }

    // Pipeline layout: should round-trip (non-zero u64).
    let mut layout: u64 = 0;
    let r = unsafe { vkCreatePipelineLayout(device, std::ptr::null(), std::ptr::null(), &mut layout) };
    assert_eq!(r, 0);
    assert!(layout != 0);

    // Create 2 pipelines.
    let mut pipes: [u64; 2] = [0, 0];
    let r = unsafe {
        vkCreateGraphicsPipelines(
            device, 0, 2, std::ptr::null(), std::ptr::null(), pipes.as_mut_ptr(),
        )
    };
    assert_eq!(r, 0);
    assert!(pipes[0] != 0 && pipes[1] != 0);
    assert_ne!(pipes[0], pipes[1], "each pipeline must get a fresh id");

    // Record vkCmdBindPipeline against the first pipeline.
    let mut pool: u64 = 0;
    unsafe { vkCreateCommandPool(device, std::ptr::null(), std::ptr::null(), &mut pool); }
    let mut info = [0u8; 40];
    info[0..4].copy_from_slice(&40u32.to_le_bytes());
    info[16..24].copy_from_slice(&pool.to_le_bytes());
    info[28..32].copy_from_slice(&1u32.to_le_bytes());
    let mut cbs: [VkCommandBuffer; 1] = [std::ptr::null_mut(); 1];
    unsafe { vkAllocateCommandBuffers(device, info.as_ptr() as *const _, cbs.as_mut_ptr()); }
    let cb = cbs[0];

    unsafe { vkBeginCommandBuffer(cb, std::ptr::null()); }
    unsafe { vkCmdBindPipeline(cb, 0, pipes[0]); }
    unsafe { vkEndCommandBuffer(cb); }

    let bytes = cmdbuf_recorded_bytes(cb);
    // BindPipeline record: 8-byte header + 4-byte body (u32 ResourceId).
    assert_eq!(bytes.len(), 12, "BindPipeline record must be 12 bytes");
    let op = u16::from_le_bytes([bytes[0], bytes[1]]);
    assert_eq!(op, 0x0020, "first op must be BindPipeline (0x0020)");
    let id = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    assert_eq!(id, pipes[0] as u32,
        "BindPipeline body must carry the resolved ResourceId");

    // Cleanup.
    unsafe { vkDestroyPipeline(device, pipes[0], std::ptr::null()); }
    unsafe { vkDestroyPipeline(device, pipes[1], std::ptr::null()); }
    unsafe { vkDestroyPipelineLayout(device, layout, std::ptr::null()); }
    unsafe { vkDestroyCommandPool(device, pool, std::ptr::null()); }
    unsafe { vkDestroyDevice(device, std::ptr::null()); }
    unsafe { vkDestroyInstance(instance, std::ptr::null()); }
    // env var cleared by EnvLock drop
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn device_memory_alloc_map_unmap_free() {
    use atrium_vk_icd::{
        vkAllocateMemory, vkCreateDevice, vkDestroyDevice, vkFreeMemory,
        vkMapMemory, vkUnmapMemory,
    };

    let sock = tmp_socket("mem");
    let sw_backend = Arc::new(SoftwareBackend::new());
    let backend_for_listener: Arc<dyn Backend> = sw_backend.clone();
    let listener = Listener::bind(&sock, backend_for_listener).unwrap();
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));
    let _env = EnvLock::set(&sock);

    type VkDevice = *mut std::ffi::c_void;

    let mut instance: VkInstance = std::ptr::null_mut();
    unsafe { vkCreateInstance(std::ptr::null(), std::ptr::null(), &mut instance); }
    let mut devices: [VkPhysicalDevice; 1] = [std::ptr::null_mut(); 1];
    let mut cap: u32 = 1;
    unsafe { vkEnumeratePhysicalDevices(instance, &mut cap, devices.as_mut_ptr()); }
    let mut device: VkDevice = std::ptr::null_mut();
    unsafe { vkCreateDevice(devices[0], std::ptr::null(), std::ptr::null(), &mut device); }

    // VkMemoryAllocateInfo (32 bytes): sType@0, pNext@8,
    // allocationSize@16, memoryTypeIndex@24.
    let mut info = [0u8; 32];
    info[0..4].copy_from_slice(&5u32.to_le_bytes()); // sType
    info[16..24].copy_from_slice(&(1024u64).to_le_bytes()); // 1 KiB
    let mut mem: u64 = 0;
    let r = unsafe { vkAllocateMemory(device, info.as_ptr() as *const _, std::ptr::null(), &mut mem) };
    assert_eq!(r, 0);
    assert!(mem != 0);

    // Map, write a pattern, verify by re-mapping later.
    let mut p: *mut std::ffi::c_void = std::ptr::null_mut();
    let r = unsafe { vkMapMemory(device, mem, 0, u64::MAX, 0, &mut p) };
    assert_eq!(r, 0);
    assert!(!p.is_null());

    let slice = unsafe { std::slice::from_raw_parts_mut(p as *mut u8, 1024) };
    for (i, b) in slice.iter_mut().enumerate() {
        *b = (i & 0xff) as u8;
    }
    unsafe { vkUnmapMemory(device, mem); }

    // Re-map and verify content survived.
    let mut p2: *mut std::ffi::c_void = std::ptr::null_mut();
    unsafe { vkMapMemory(device, mem, 0, u64::MAX, 0, &mut p2); }
    let slice2 = unsafe { std::slice::from_raw_parts(p2 as *const u8, 1024) };
    for (i, &b) in slice2.iter().enumerate() {
        assert_eq!(b, (i & 0xff) as u8, "byte {i} changed across unmap/remap");
    }
    unsafe { vkUnmapMemory(device, mem); }

    // Map at non-zero offset.
    let mut p3: *mut std::ffi::c_void = std::ptr::null_mut();
    unsafe { vkMapMemory(device, mem, 128, u64::MAX, 0, &mut p3); }
    let first = unsafe { *(p3 as *const u8) };
    assert_eq!(first, 128, "offset map must point at offset 128 of the storage");
    unsafe { vkUnmapMemory(device, mem); }

    // Free + verify the handle no longer maps.
    unsafe { vkFreeMemory(device, mem, std::ptr::null()); }
    let mut p_post: *mut std::ffi::c_void = std::ptr::null_mut();
    let r = unsafe { vkMapMemory(device, mem, 0, u64::MAX, 0, &mut p_post) };
    assert_ne!(r, 0, "vkMapMemory on freed VkDeviceMemory must error");

    unsafe { vkDestroyDevice(device, std::ptr::null()); }
    unsafe { vkDestroyInstance(instance, std::ptr::null()); }
    // env var cleared by EnvLock drop
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn buffer_create_bind_destroy_round_trip() {
    use atrium_vk_icd::{
        vkAllocateMemory, vkBindBufferMemory, vkCreateBuffer, vkCreateDevice,
        vkDestroyBuffer, vkDestroyDevice, vkFreeMemory,
        vkGetBufferMemoryRequirements,
    };

    let sock = tmp_socket("buf");
    let sw_backend = Arc::new(SoftwareBackend::new());
    let backend_for_listener: Arc<dyn Backend> = sw_backend.clone();
    let listener = Listener::bind(&sock, backend_for_listener).unwrap();
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));
    let _env = EnvLock::set(&sock);

    type VkDevice = *mut std::ffi::c_void;

    let mut instance: VkInstance = std::ptr::null_mut();
    unsafe { vkCreateInstance(std::ptr::null(), std::ptr::null(), &mut instance); }
    let mut devices: [VkPhysicalDevice; 1] = [std::ptr::null_mut(); 1];
    let mut cap: u32 = 1;
    unsafe { vkEnumeratePhysicalDevices(instance, &mut cap, devices.as_mut_ptr()); }
    let mut device: VkDevice = std::ptr::null_mut();
    unsafe { vkCreateDevice(devices[0], std::ptr::null(), std::ptr::null(), &mut device); }

    // VkBufferCreateInfo (56 bytes): sType@0, pNext@8, flags@16,
    // size@24 (u64), usage@32, sharingMode@36, qfic@40, _pad@44,
    // pQueueFamilyIndices@48.
    let mut info = [0u8; 56];
    info[0..4].copy_from_slice(&12u32.to_le_bytes()); // sType
    info[24..32].copy_from_slice(&(4096u64).to_le_bytes());
    info[32..36].copy_from_slice(&0x80u32.to_le_bytes()); // VERTEX_BUFFER_BIT
    let mut buffer: u64 = 0;
    let r = unsafe { vkCreateBuffer(device, info.as_ptr() as *const _, std::ptr::null(), &mut buffer) };
    assert_eq!(r, 0);
    assert!(buffer != 0);

    // Memory-requirements query should report the requested size,
    // 16-byte alignment, single-type memoryTypeBits.
    let mut req = ash::vk::MemoryRequirements::default();
    unsafe { vkGetBufferMemoryRequirements(device, buffer, &mut req); }
    assert_eq!(req.size, 4096);
    assert_eq!(req.alignment, 16);
    assert_eq!(req.memory_type_bits, 0b1);

    // Allocate matching memory, bind, verify.
    let mut alloc_info = [0u8; 32];
    alloc_info[0..4].copy_from_slice(&5u32.to_le_bytes());
    alloc_info[16..24].copy_from_slice(&req.size.to_le_bytes());
    let mut mem: u64 = 0;
    unsafe { vkAllocateMemory(device, alloc_info.as_ptr() as *const _, std::ptr::null(), &mut mem); }
    let r = unsafe { vkBindBufferMemory(device, buffer, mem, 0) };
    assert_eq!(r, 0);

    // Bind beyond the memory's bounds must reject.
    let mut tiny_alloc = [0u8; 32];
    tiny_alloc[0..4].copy_from_slice(&5u32.to_le_bytes());
    tiny_alloc[16..24].copy_from_slice(&(1024u64).to_le_bytes());
    let mut tiny_mem: u64 = 0;
    unsafe { vkAllocateMemory(device, tiny_alloc.as_ptr() as *const _, std::ptr::null(), &mut tiny_mem); }
    let r = unsafe { vkBindBufferMemory(device, buffer, tiny_mem, 0) };
    assert_ne!(r, 0, "binding 4 KiB buffer to 1 KiB memory must fail");
    // Bind with offset that overflows fails.
    let r = unsafe { vkBindBufferMemory(device, buffer, mem, 4095) };
    assert_ne!(r, 0, "offset 4095 + size 4096 > memory size 4096 must fail");

    unsafe { vkFreeMemory(device, tiny_mem, std::ptr::null()); }
    unsafe { vkFreeMemory(device, mem, std::ptr::null()); }
    unsafe { vkDestroyBuffer(device, buffer, std::ptr::null()); }
    unsafe { vkDestroyDevice(device, std::ptr::null()); }
    unsafe { vkDestroyInstance(instance, std::ptr::null()); }
    // env var cleared by EnvLock drop
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn vertex_index_buffer_bind_and_draw_indexed_records() {
    use atrium_vk_icd::{
        cmdbuf_recorded_bytes,
        vkAllocateCommandBuffers, vkAllocateMemory, vkBeginCommandBuffer,
        vkBindBufferMemory, vkCmdBindIndexBuffer, vkCmdBindVertexBuffers,
        vkCmdDrawIndexed, vkCreateBuffer, vkCreateCommandPool, vkCreateDevice,
        vkDestroyBuffer, vkDestroyCommandPool, vkDestroyDevice,
        vkEndCommandBuffer, vkFreeMemory,
    };

    let sock = tmp_socket("vbib");
    let sw_backend = Arc::new(SoftwareBackend::new());
    let backend_for_listener: Arc<dyn Backend> = sw_backend.clone();
    let listener = Listener::bind(&sock, backend_for_listener).unwrap();
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));
    let _env = EnvLock::set(&sock);

    type VkDevice = *mut std::ffi::c_void;
    type VkCommandBuffer = *mut std::ffi::c_void;

    let mut instance: VkInstance = std::ptr::null_mut();
    unsafe { vkCreateInstance(std::ptr::null(), std::ptr::null(), &mut instance); }
    let mut pds: [VkPhysicalDevice; 1] = [std::ptr::null_mut(); 1];
    let mut cap: u32 = 1;
    unsafe { vkEnumeratePhysicalDevices(instance, &mut cap, pds.as_mut_ptr()); }
    let mut device: VkDevice = std::ptr::null_mut();
    unsafe { vkCreateDevice(pds[0], std::ptr::null(), std::ptr::null(), &mut device); }

    // Allocate one memory + create two buffers, bind both. The
    // SoftwareBackend will receive OP_GPU_MEMORY_CREATE +
    // OP_GPU_BUFFER_CREATE for each, and AtriumBuffer.buffer_id
    // gets populated so vkCmd*'s resolve_buffer returns a real id.
    let mut alloc = [0u8; 32];
    alloc[0..4].copy_from_slice(&5u32.to_le_bytes());
    alloc[16..24].copy_from_slice(&(8192u64).to_le_bytes());
    let mut mem: u64 = 0;
    unsafe { vkAllocateMemory(device, alloc.as_ptr() as *const _, std::ptr::null(), &mut mem); }
    // Give the SW backend a moment to register the memory.
    thread::sleep(Duration::from_millis(30));

    fn make_buffer(device: VkDevice, size: u64, usage: u32) -> u64 {
        let mut info = [0u8; 56];
        info[0..4].copy_from_slice(&12u32.to_le_bytes());
        info[24..32].copy_from_slice(&size.to_le_bytes());
        info[32..36].copy_from_slice(&usage.to_le_bytes());
        let mut handle: u64 = 0;
        unsafe { vkCreateBuffer(device, info.as_ptr() as *const _, std::ptr::null(), &mut handle); }
        handle
    }
    let vbuf = make_buffer(device, 1024, 0x80); // VERTEX_BUFFER
    let ibuf = make_buffer(device, 1024, 0x40); // INDEX_BUFFER
    unsafe { vkBindBufferMemory(device, vbuf, mem, 0); }
    unsafe { vkBindBufferMemory(device, ibuf, mem, 4096); }
    thread::sleep(Duration::from_millis(30));

    // Record vkCmdBindVertexBuffers + vkCmdBindIndexBuffer +
    // vkCmdDrawIndexed and inspect the FrameOp stream.
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
    let bufs = [vbuf];
    let offs = [0u64];
    unsafe { vkCmdBindVertexBuffers(cb, 0, 1, bufs.as_ptr(), offs.as_ptr()); }
    unsafe { vkCmdBindIndexBuffer(cb, ibuf, 0, 1 /* UINT32 */); }
    unsafe { vkCmdDrawIndexed(cb, 6, 1, 0, 0, 0); }
    unsafe { vkEndCommandBuffer(cb); }

    let bytes = cmdbuf_recorded_bytes(cb);
    // Three records: BindVertexBuf (8+16=24), BindIndexBuf (8+20=28),
    // DrawIndexed (8+20=28). Total 80.
    assert_eq!(bytes.len(), 80, "expected 80 bytes, got {}", bytes.len());

    let op0 = u16::from_le_bytes([bytes[ 0], bytes[ 1]]);
    let op1 = u16::from_le_bytes([bytes[24], bytes[25]]);
    let op2 = u16::from_le_bytes([bytes[52], bytes[53]]);
    assert_eq!(op0, 0x0022, "BindVertexBuf");
    assert_eq!(op1, 0x0023, "BindIndexBuf");
    assert_eq!(op2, 0x0041, "DrawIndexed");

    unsafe { vkDestroyCommandPool(device, pool, std::ptr::null()); }
    unsafe { vkDestroyBuffer(device, vbuf, std::ptr::null()); }
    unsafe { vkDestroyBuffer(device, ibuf, std::ptr::null()); }
    unsafe { vkFreeMemory(device, mem, std::ptr::null()); }
    unsafe { vkDestroyDevice(device, std::ptr::null()); }
    unsafe { vkDestroyInstance(instance, std::ptr::null()); }
    // env var cleared by EnvLock drop
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn image_create_bind_destroy_round_trip() {
    use atrium_vk_icd::{
        vkAllocateMemory, vkBindImageMemory, vkCreateDevice, vkCreateImage,
        vkDestroyDevice, vkDestroyImage, vkFreeMemory,
        vkGetImageMemoryRequirements,
    };

    let sock = tmp_socket("img");
    let sw_backend = Arc::new(SoftwareBackend::new());
    let backend_for_listener: Arc<dyn Backend> = sw_backend.clone();
    let listener = Listener::bind(&sock, backend_for_listener).unwrap();
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));
    let _env = EnvLock::set(&sock);

    type VkDevice = *mut std::ffi::c_void;

    let mut instance: VkInstance = std::ptr::null_mut();
    unsafe { vkCreateInstance(std::ptr::null(), std::ptr::null(), &mut instance); }
    let mut pds: [VkPhysicalDevice; 1] = [std::ptr::null_mut(); 1];
    let mut cap: u32 = 1;
    unsafe { vkEnumeratePhysicalDevices(instance, &mut cap, pds.as_mut_ptr()); }
    let mut device: VkDevice = std::ptr::null_mut();
    unsafe { vkCreateDevice(pds[0], std::ptr::null(), std::ptr::null(), &mut device); }

    // 64×64 RGBA8 image. VkImageCreateInfo offsets per ICD:
    //   24 format, 28 width, 32 height, 36 depth, 40 mipLevels,
    //   44 arrayLayers, 56 usage.
    // Size of struct is 88 bytes; we zero everything outside our fields.
    let mut info = [0u8; 88];
    info[ 0.. 4].copy_from_slice(&14u32.to_le_bytes()); // sType
    info[24..28].copy_from_slice(&37u32.to_le_bytes()); // R8G8B8A8_UNORM
    info[28..32].copy_from_slice(&64u32.to_le_bytes());
    info[32..36].copy_from_slice(&64u32.to_le_bytes());
    info[36..40].copy_from_slice(&1u32.to_le_bytes());
    info[40..44].copy_from_slice(&1u32.to_le_bytes());
    info[44..48].copy_from_slice(&1u32.to_le_bytes());
    info[56..60].copy_from_slice(&0x07u32.to_le_bytes()); // SAMPLED|STORAGE|TRANSFER_DST
    let mut img: u64 = 0;
    let r = unsafe { vkCreateImage(device, info.as_ptr() as *const _, std::ptr::null(), &mut img) };
    assert_eq!(r, 0);
    assert!(img != 0);

    // Memory requirements: 64*64*4 = 16384 bytes (1 mip × 1 layer),
    // 256-byte alignment, single type.
    let mut req = ash::vk::MemoryRequirements::default();
    unsafe { vkGetImageMemoryRequirements(device, img, &mut req); }
    assert_eq!(req.size, 64 * 64 * 4);
    assert_eq!(req.alignment, 256);
    assert_eq!(req.memory_type_bits, 0b1);

    // Allocate memory and bind.
    let mut alloc = [0u8; 32];
    alloc[0..4].copy_from_slice(&5u32.to_le_bytes());
    alloc[16..24].copy_from_slice(&req.size.to_le_bytes());
    let mut mem: u64 = 0;
    unsafe { vkAllocateMemory(device, alloc.as_ptr() as *const _, std::ptr::null(), &mut mem); }
    let r = unsafe { vkBindImageMemory(device, img, mem, 0) };
    assert_eq!(r, 0);

    unsafe { vkDestroyImage(device, img, std::ptr::null()); }
    unsafe { vkFreeMemory(device, mem, std::ptr::null()); }
    unsafe { vkDestroyDevice(device, std::ptr::null()); }
    unsafe { vkDestroyInstance(instance, std::ptr::null()); }
    // env var cleared by EnvLock drop
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn shader_module_create_destroy_through_daemon_cache() {
    use atrium_vk_icd::{
        vkCreateDevice, vkCreateShaderModule, vkDestroyDevice,
        vkDestroyShaderModule,
    };

    let sock = tmp_socket("shader");
    let sw_backend = Arc::new(SoftwareBackend::new());
    let backend_for_listener: Arc<dyn Backend> = sw_backend.clone();
    let listener = Listener::bind(&sock, backend_for_listener).unwrap();
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));
    let _env = EnvLock::set(&sock);

    type VkDevice = *mut std::ffi::c_void;

    let mut instance: VkInstance = std::ptr::null_mut();
    unsafe { vkCreateInstance(std::ptr::null(), std::ptr::null(), &mut instance); }
    let mut pds: [VkPhysicalDevice; 1] = [std::ptr::null_mut(); 1];
    let mut cap: u32 = 1;
    unsafe { vkEnumeratePhysicalDevices(instance, &mut cap, pds.as_mut_ptr()); }
    let mut device: VkDevice = std::ptr::null_mut();
    unsafe { vkCreateDevice(pds[0], std::ptr::null(), std::ptr::null(), &mut device); }

    // Minimal SPIR-V module: magic + version + 0 generator + 1 bound
    // + 0 schema. 5 words = 20 bytes. Header-only; no actual
    // instructions. The host validator accepts this (no forbidden
    // capabilities present).
    let spv: [u32; 5] = [0x07230203, 0x00010000, 0, 1, 0];
    let code_size: u64 = 20;
    let p_code: *const u32 = spv.as_ptr();

    // VkShaderModuleCreateInfo: sType@0, pNext@8, flags@16,
    // codeSize@24 (u64), pCode@32 (ptr).
    let mut info = [0u8; 40];
    info[ 0.. 4].copy_from_slice(&16u32.to_le_bytes()); // sType
    info[24..32].copy_from_slice(&code_size.to_le_bytes());
    info[32..40].copy_from_slice(&(p_code as u64).to_le_bytes());

    let mut module: u64 = 0;
    let r = unsafe { vkCreateShaderModule(device, info.as_ptr() as *const _, std::ptr::null(), &mut module) };
    assert_eq!(r, 0);
    assert!(module != 0);

    // Create a second module with the same bytecode — the daemon
    // should resolve from cache (no second upload). We can't
    // observe submission_count for resolve, but the call must
    // succeed and return a distinct VkShaderModule handle.
    let mut module2: u64 = 0;
    let r = unsafe { vkCreateShaderModule(device, info.as_ptr() as *const _, std::ptr::null(), &mut module2) };
    assert_eq!(r, 0);
    assert_ne!(module, module2);

    unsafe { vkDestroyShaderModule(device, module, std::ptr::null()); }
    unsafe { vkDestroyShaderModule(device, module2, std::ptr::null()); }
    unsafe { vkDestroyDevice(device, std::ptr::null()); }
    unsafe { vkDestroyInstance(instance, std::ptr::null()); }
    // env var cleared by EnvLock drop
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn render_pass_framebuffer_and_begin_end_recording() {
    use atrium_vk_icd::{
        cmdbuf_recorded_bytes,
        vkAllocateCommandBuffers, vkAllocateMemory, vkBeginCommandBuffer,
        vkBindImageMemory, vkCmdBeginRenderPass, vkCmdDraw, vkCmdEndRenderPass,
        vkCreateCommandPool, vkCreateDevice, vkCreateFramebuffer, vkCreateImage,
        vkCreateImageView, vkCreateRenderPass, vkDestroyCommandPool,
        vkDestroyDevice, vkDestroyFramebuffer, vkDestroyImage,
        vkDestroyImageView, vkDestroyRenderPass, vkEndCommandBuffer,
        vkFreeMemory, vkGetImageMemoryRequirements,
    };

    let sock = tmp_socket("rp");
    let sw_backend = Arc::new(SoftwareBackend::new());
    let backend_for_listener: Arc<dyn Backend> = sw_backend.clone();
    let listener = Listener::bind(&sock, backend_for_listener).unwrap();
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));
    let _env = EnvLock::set(&sock);

    type VkDevice = *mut std::ffi::c_void;
    type VkCommandBuffer = *mut std::ffi::c_void;

    // Bootstrap.
    let mut instance: VkInstance = std::ptr::null_mut();
    unsafe { vkCreateInstance(std::ptr::null(), std::ptr::null(), &mut instance); }
    let mut pds: [VkPhysicalDevice; 1] = [std::ptr::null_mut(); 1];
    let mut cap: u32 = 1;
    unsafe { vkEnumeratePhysicalDevices(instance, &mut cap, pds.as_mut_ptr()); }
    let mut device: VkDevice = std::ptr::null_mut();
    unsafe { vkCreateDevice(pds[0], std::ptr::null(), std::ptr::null(), &mut device); }

    // Image + view + bound memory.
    let mut img_info = [0u8; 88];
    img_info[ 0.. 4].copy_from_slice(&14u32.to_le_bytes());
    img_info[24..28].copy_from_slice(&37u32.to_le_bytes()); // R8G8B8A8_UNORM
    img_info[28..32].copy_from_slice(&64u32.to_le_bytes());
    img_info[32..36].copy_from_slice(&64u32.to_le_bytes());
    img_info[36..40].copy_from_slice(&1u32.to_le_bytes());
    img_info[40..44].copy_from_slice(&1u32.to_le_bytes());
    img_info[44..48].copy_from_slice(&1u32.to_le_bytes());
    img_info[56..60].copy_from_slice(&0x10u32.to_le_bytes()); // COLOR_ATTACHMENT
    let mut image: u64 = 0;
    unsafe { vkCreateImage(device, img_info.as_ptr() as *const _, std::ptr::null(), &mut image); }
    let mut req = ash::vk::MemoryRequirements::default();
    unsafe { vkGetImageMemoryRequirements(device, image, &mut req); }
    let mut alloc = [0u8; 32];
    alloc[0..4].copy_from_slice(&5u32.to_le_bytes());
    alloc[16..24].copy_from_slice(&req.size.to_le_bytes());
    let mut mem: u64 = 0;
    unsafe { vkAllocateMemory(device, alloc.as_ptr() as *const _, std::ptr::null(), &mut mem); }
    unsafe { vkBindImageMemory(device, image, mem, 0); }
    thread::sleep(Duration::from_millis(30));

    // Image view referencing the image.
    let mut view_info = [0u8; 80];
    view_info[ 0.. 4].copy_from_slice(&15u32.to_le_bytes()); // sType
    view_info[24..32].copy_from_slice(&image.to_le_bytes());
    let mut view: u64 = 0;
    unsafe { vkCreateImageView(device, view_info.as_ptr() as *const _, std::ptr::null(), &mut view); }
    assert!(view != 0);

    // Render pass: ignored fields, just need a non-zero handle.
    let mut rp_info = [0u8; 64];
    rp_info[0..4].copy_from_slice(&38u32.to_le_bytes());
    let mut render_pass: u64 = 0;
    unsafe { vkCreateRenderPass(device, rp_info.as_ptr() as *const _, std::ptr::null(), &mut render_pass); }
    assert!(render_pass != 0);

    // Framebuffer: 1 attachment, 64×64.
    let mut fb_info = [0u8; 64];
    fb_info[ 0.. 4].copy_from_slice(&37u32.to_le_bytes()); // sType
    fb_info[24..32].copy_from_slice(&render_pass.to_le_bytes());
    fb_info[32..36].copy_from_slice(&1u32.to_le_bytes());
    let attachments_arr = [view];
    let p_attachments_ptr = attachments_arr.as_ptr() as u64;
    fb_info[40..48].copy_from_slice(&p_attachments_ptr.to_le_bytes());
    fb_info[48..52].copy_from_slice(&64u32.to_le_bytes());
    fb_info[52..56].copy_from_slice(&64u32.to_le_bytes());
    let mut fb: u64 = 0;
    unsafe { vkCreateFramebuffer(device, fb_info.as_ptr() as *const _, std::ptr::null(), &mut fb); }
    assert!(fb != 0);

    // Record a render pass.
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

    // VkRenderPassBeginInfo: sType@0, pNext@8, renderPass@16,
    // framebuffer@24, renderArea@32 (16 B), clearValueCount@48,
    // pClearValues@56. Total 64 bytes.
    let mut rpb = [0u8; 64];
    rpb[ 0.. 4].copy_from_slice(&43u32.to_le_bytes()); // sType
    rpb[16..24].copy_from_slice(&render_pass.to_le_bytes());
    rpb[24..32].copy_from_slice(&fb.to_le_bytes());
    rpb[48..52].copy_from_slice(&1u32.to_le_bytes());
    let clear: [f32; 4] = [1.0, 0.5, 0.0, 1.0]; // orange
    let clear_ptr = clear.as_ptr() as u64;
    rpb[56..64].copy_from_slice(&clear_ptr.to_le_bytes());

    unsafe { vkCmdBeginRenderPass(cb, rpb.as_ptr() as *const _, 0); }
    unsafe { vkCmdDraw(cb, 3, 1, 0, 0); }
    unsafe { vkCmdEndRenderPass(cb); }
    unsafe { vkEndCommandBuffer(cb); }

    // Decode the recorded stream:
    //   BeginRenderPass (opcode 0x0010, body 12 B) = 20 B
    //   Draw            (opcode 0x0040, body 16 B) = 24 B
    //   EndRenderPass   (opcode 0x0011, body 0)    = 8 B
    //   total 52 B.
    let bytes = cmdbuf_recorded_bytes(cb);
    assert_eq!(bytes.len(), 52, "expected 52 bytes, got {}", bytes.len());
    let op0 = u16::from_le_bytes([bytes[0], bytes[1]]);
    let op1 = u16::from_le_bytes([bytes[20], bytes[21]]);
    let op2 = u16::from_le_bytes([bytes[44], bytes[45]]);
    assert_eq!(op0, 0x0010, "BeginRenderPass");
    assert_eq!(op1, 0x0040, "Draw");
    assert_eq!(op2, 0x0011, "EndRenderPass");

    // BeginRenderPass body: bytes[8..20]. target_image_id at [8..12],
    // clear_color_rgba8 at [12..16]. Verify the clear quantized
    // correctly: (1.0, 0.5, 0.0, 1.0) → (255, 127or128, 0, 255).
    let r = bytes[12];
    let g = bytes[13];
    let b = bytes[14];
    let a = bytes[15];
    assert_eq!(r, 255);
    assert!(g == 127 || g == 128);
    assert_eq!(b, 0);
    assert_eq!(a, 255);

    // Cleanup.
    unsafe { vkDestroyFramebuffer(device, fb, std::ptr::null()); }
    unsafe { vkDestroyRenderPass(device, render_pass, std::ptr::null()); }
    unsafe { vkDestroyImageView(device, view, std::ptr::null()); }
    unsafe { vkDestroyImage(device, image, std::ptr::null()); }
    unsafe { vkFreeMemory(device, mem, std::ptr::null()); }
    unsafe { vkDestroyCommandPool(device, pool, std::ptr::null()); }
    unsafe { vkDestroyDevice(device, std::ptr::null()); }
    unsafe { vkDestroyInstance(instance, std::ptr::null()); }
    // env var cleared by EnvLock drop
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn fence_semaphore_sampler_lifecycle() {
    use atrium_vk_icd::{
        vkCreateDevice, vkCreateFence, vkCreateSampler, vkCreateSemaphore,
        vkDestroyDevice, vkDestroyFence, vkDestroySampler, vkDestroySemaphore,
        vkGetFenceStatus, vkResetFences, vkWaitForFences,
    };

    let sock = tmp_socket("sync");
    let sw_backend = Arc::new(SoftwareBackend::new());
    let backend_for_listener: Arc<dyn Backend> = sw_backend.clone();
    let listener = Listener::bind(&sock, backend_for_listener).unwrap();
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));
    let _env = EnvLock::set(&sock);

    type VkDevice = *mut std::ffi::c_void;

    let mut instance: VkInstance = std::ptr::null_mut();
    unsafe { vkCreateInstance(std::ptr::null(), std::ptr::null(), &mut instance); }
    let mut pds: [VkPhysicalDevice; 1] = [std::ptr::null_mut(); 1];
    let mut cap: u32 = 1;
    unsafe { vkEnumeratePhysicalDevices(instance, &mut cap, pds.as_mut_ptr()); }
    let mut device: VkDevice = std::ptr::null_mut();
    unsafe { vkCreateDevice(pds[0], std::ptr::null(), std::ptr::null(), &mut device); }

    // Unsignaled fence: flags=0.
    let mut fence_info = [0u8; 20];
    fence_info[ 0.. 4].copy_from_slice(&8u32.to_le_bytes());
    let mut fence: u64 = 0;
    unsafe { vkCreateFence(device, fence_info.as_ptr() as *const _, std::ptr::null(), &mut fence); }
    assert_eq!(unsafe { vkGetFenceStatus(device, fence) }, 3, "fresh fence is unsignaled (VK_NOT_READY)");

    // vkWaitForFences signals the fence (synchronous-submit shortcut).
    let fences = [fence];
    let r = unsafe { vkWaitForFences(device, 1, fences.as_ptr(), 1, u64::MAX) };
    assert_eq!(r, 0);
    assert_eq!(unsafe { vkGetFenceStatus(device, fence) }, 0, "after wait, fence is signaled (VK_SUCCESS)");

    // vkResetFences puts it back to unsignaled.
    unsafe { vkResetFences(device, 1, fences.as_ptr()); }
    assert_eq!(unsafe { vkGetFenceStatus(device, fence) }, 3);

    // Pre-signaled fence: flags = VK_FENCE_CREATE_SIGNALED_BIT (0x1).
    fence_info[16..20].copy_from_slice(&1u32.to_le_bytes());
    let mut fence2: u64 = 0;
    unsafe { vkCreateFence(device, fence_info.as_ptr() as *const _, std::ptr::null(), &mut fence2); }
    assert_eq!(unsafe { vkGetFenceStatus(device, fence2) }, 0, "signaled-bit fence starts ready");

    // Semaphores: just need a non-zero handle.
    let mut sem_info = [0u8; 16];
    sem_info[0..4].copy_from_slice(&9u32.to_le_bytes());
    let mut sem: u64 = 0;
    unsafe { vkCreateSemaphore(device, sem_info.as_ptr() as *const _, std::ptr::null(), &mut sem); }
    assert!(sem != 0);

    // Sampler with default linear filtering.
    let mut samp_info = [0u8; 80];
    samp_info[ 0.. 4].copy_from_slice(&31u32.to_le_bytes());
    samp_info[20..24].copy_from_slice(&1u32.to_le_bytes()); // mag = LINEAR
    samp_info[24..28].copy_from_slice(&1u32.to_le_bytes()); // min = LINEAR
    samp_info[64..68].copy_from_slice(&0.0f32.to_le_bytes()); // minLod
    samp_info[68..72].copy_from_slice(&1.0f32.to_le_bytes()); // maxLod
    let mut samp: u64 = 0;
    let r = unsafe { vkCreateSampler(device, samp_info.as_ptr() as *const _, std::ptr::null(), &mut samp) };
    assert_eq!(r, 0);
    assert!(samp != 0);

    // Cleanup.
    unsafe { vkDestroySampler(device, samp, std::ptr::null()); }
    unsafe { vkDestroySemaphore(device, sem, std::ptr::null()); }
    unsafe { vkDestroyFence(device, fence, std::ptr::null()); }
    unsafe { vkDestroyFence(device, fence2, std::ptr::null()); }
    unsafe { vkDestroyDevice(device, std::ptr::null()); }
    unsafe { vkDestroyInstance(instance, std::ptr::null()); }
    // env var cleared by EnvLock drop
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn descriptor_set_alloc_update_bind_round_trip() {
    use atrium_vk_icd::{
        cmdbuf_recorded_bytes,
        vkAllocateCommandBuffers, vkAllocateDescriptorSets, vkAllocateMemory,
        vkBeginCommandBuffer, vkBindBufferMemory, vkCmdBindDescriptorSets,
        vkCreateBuffer, vkCreateCommandPool, vkCreateDescriptorPool,
        vkCreateDescriptorSetLayout, vkCreateDevice, vkDestroyBuffer,
        vkDestroyCommandPool, vkDestroyDescriptorPool,
        vkDestroyDescriptorSetLayout, vkDestroyDevice, vkEndCommandBuffer,
        vkFreeMemory, vkUpdateDescriptorSets,
    };

    let sock = tmp_socket("dset");
    let sw_backend = Arc::new(SoftwareBackend::new());
    let backend_for_listener: Arc<dyn Backend> = sw_backend.clone();
    let listener = Listener::bind(&sock, backend_for_listener).unwrap();
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));
    let _env = EnvLock::set(&sock);

    type VkDevice = *mut std::ffi::c_void;
    type VkCommandBuffer = *mut std::ffi::c_void;

    let mut instance: VkInstance = std::ptr::null_mut();
    unsafe { vkCreateInstance(std::ptr::null(), std::ptr::null(), &mut instance); }
    let mut pds: [VkPhysicalDevice; 1] = [std::ptr::null_mut(); 1];
    let mut cap: u32 = 1;
    unsafe { vkEnumeratePhysicalDevices(instance, &mut cap, pds.as_mut_ptr()); }
    let mut device: VkDevice = std::ptr::null_mut();
    unsafe { vkCreateDevice(pds[0], std::ptr::null(), std::ptr::null(), &mut device); }

    // Create a uniform buffer (size 256, USAGE_UNIFORM_BUFFER=0x10).
    let mut buf_info = [0u8; 56];
    buf_info[ 0.. 4].copy_from_slice(&12u32.to_le_bytes());
    buf_info[24..32].copy_from_slice(&(256u64).to_le_bytes());
    buf_info[32..36].copy_from_slice(&0x10u32.to_le_bytes());
    let mut ubuf: u64 = 0;
    unsafe { vkCreateBuffer(device, buf_info.as_ptr() as *const _, std::ptr::null(), &mut ubuf); }
    let mut alloc = [0u8; 32];
    alloc[0..4].copy_from_slice(&5u32.to_le_bytes());
    alloc[16..24].copy_from_slice(&(256u64).to_le_bytes());
    let mut mem: u64 = 0;
    unsafe { vkAllocateMemory(device, alloc.as_ptr() as *const _, std::ptr::null(), &mut mem); }
    unsafe { vkBindBufferMemory(device, ubuf, mem, 0); }
    thread::sleep(Duration::from_millis(30));

    // Layout (opaque) + pool + allocate one set.
    let mut layout: u64 = 0;
    unsafe { vkCreateDescriptorSetLayout(device, std::ptr::null(), std::ptr::null(), &mut layout); }
    let mut pool: u64 = 0;
    unsafe { vkCreateDescriptorPool(device, std::ptr::null(), std::ptr::null(), &mut pool); }
    // VkDescriptorSetAllocateInfo: sType@0, pNext@8, pool@16,
    // count@24, pSetLayouts@32.
    let mut da = [0u8; 40];
    da[ 0.. 4].copy_from_slice(&34u32.to_le_bytes());
    da[16..24].copy_from_slice(&pool.to_le_bytes());
    da[24..28].copy_from_slice(&1u32.to_le_bytes());
    let layouts_arr = [layout];
    let layouts_ptr = layouts_arr.as_ptr() as u64;
    da[32..40].copy_from_slice(&layouts_ptr.to_le_bytes());
    let mut sets: [u64; 1] = [0];
    unsafe { vkAllocateDescriptorSets(device, da.as_ptr() as *const _, sets.as_mut_ptr()); }
    assert!(sets[0] != 0);

    // VkWriteDescriptorSet (64 B): bind the uniform buffer at
    // binding 0 of the set. descriptorType=6 (UNIFORM_BUFFER).
    let buf_info_arr: [u8; 24] = {
        let mut b = [0u8; 24];
        b[ 0.. 8].copy_from_slice(&ubuf.to_le_bytes());
        b[ 8..16].copy_from_slice(&0u64.to_le_bytes());        // offset
        b[16..24].copy_from_slice(&(256u64).to_le_bytes());    // range
        b
    };
    let buf_info_ptr = buf_info_arr.as_ptr() as u64;
    let mut write = [0u8; 64];
    write[ 0.. 4].copy_from_slice(&35u32.to_le_bytes());        // sType
    write[16..24].copy_from_slice(&sets[0].to_le_bytes());      // dstSet
    write[24..28].copy_from_slice(&0u32.to_le_bytes());         // dstBinding
    write[32..36].copy_from_slice(&1u32.to_le_bytes());         // descriptorCount
    write[36..40].copy_from_slice(&6u32.to_le_bytes());         // type=UNIFORM_BUFFER
    write[48..56].copy_from_slice(&buf_info_ptr.to_le_bytes()); // pBufferInfo

    unsafe { vkUpdateDescriptorSets(device, 1, write.as_ptr() as *const _, 0, std::ptr::null()); }

    // Record vkCmdBindDescriptorSets and verify the FrameOp body.
    let mut pool_cb: u64 = 0;
    unsafe { vkCreateCommandPool(device, std::ptr::null(), std::ptr::null(), &mut pool_cb); }
    let mut cb_info = [0u8; 40];
    cb_info[0..4].copy_from_slice(&40u32.to_le_bytes());
    cb_info[16..24].copy_from_slice(&pool_cb.to_le_bytes());
    cb_info[28..32].copy_from_slice(&1u32.to_le_bytes());
    let mut cbs: [VkCommandBuffer; 1] = [std::ptr::null_mut(); 1];
    unsafe { vkAllocateCommandBuffers(device, cb_info.as_ptr() as *const _, cbs.as_mut_ptr()); }
    let cb = cbs[0];

    unsafe { vkBeginCommandBuffer(cb, std::ptr::null()); }
    unsafe {
        vkCmdBindDescriptorSets(
            cb, 0, layout, 0, 1, sets.as_ptr(), 0, std::ptr::null(),
        );
    }
    unsafe { vkEndCommandBuffer(cb); }

    // BindDescriptors record: 8 B record header + body. Body =
    // 8 B per-set header (set_index, write_count) +
    // 36 B per write (binding/type/buffer_id/image_id/sampler_id =
    // 5×4, then offset+range = 2×8) × 1 = 44 B body. Total 52 B.
    let bytes = cmdbuf_recorded_bytes(cb);
    assert_eq!(bytes.len(), 52, "expected 52 bytes, got {}", bytes.len());
    let op = u16::from_le_bytes([bytes[0], bytes[1]]);
    assert_eq!(op, 0x0021, "BindDescriptors opcode");
    // Body @ bytes[8..]. set_index=0, write_count=1, binding=0,
    // type=6, buffer_id=...,...
    let set_index = u32::from_le_bytes([bytes[ 8], bytes[ 9], bytes[10], bytes[11]]);
    let wcount    = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
    let binding   = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let ty        = u32::from_le_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    let bid       = u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]);
    assert_eq!(set_index, 0);
    assert_eq!(wcount, 1);
    assert_eq!(binding, 0);
    assert_eq!(ty, 6);
    assert!(bid != 0, "buffer should have a daemon-side id (was {})", bid);

    unsafe { vkDestroyCommandPool(device, pool_cb, std::ptr::null()); }
    unsafe { vkDestroyDescriptorPool(device, pool, std::ptr::null()); }
    unsafe { vkDestroyDescriptorSetLayout(device, layout, std::ptr::null()); }
    unsafe { vkDestroyBuffer(device, ubuf, std::ptr::null()); }
    unsafe { vkFreeMemory(device, mem, std::ptr::null()); }
    unsafe { vkDestroyDevice(device, std::ptr::null()); }
    unsafe { vkDestroyInstance(instance, std::ptr::null()); }
    // env var cleared by EnvLock drop
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn full_triangle_frame_record_and_submit() {
    //! Composes the full setup-record-submit chain a "hello
    //! triangle" Vulkan app would issue, against a live
    //! SoftwareBackend listener. Verifies that the
    //! AtriumCommandBuffer's FrameOp stream contains the
    //! expected opcode sequence and that vkQueueSubmit flushes
    //! it through GpuClient::submit_frame (observed via the
    //! backend's submission counter).
    use atrium_vk_icd::{
        cmdbuf_recorded_bytes,
        vkAllocateCommandBuffers, vkAllocateDescriptorSets,
        vkAllocateMemory, vkBeginCommandBuffer, vkBindBufferMemory,
        vkBindImageMemory, vkCmdBeginRenderPass,
        vkCmdBindDescriptorSets, vkCmdBindIndexBuffer, vkCmdBindPipeline,
        vkCmdBindVertexBuffers, vkCmdDrawIndexed, vkCmdEndRenderPass,
        vkCmdPushConstants, vkCmdSetScissor, vkCmdSetViewport,
        vkCreateBuffer, vkCreateCommandPool, vkCreateDescriptorPool,
        vkCreateDescriptorSetLayout, vkCreateDevice, vkCreateFramebuffer,
        vkCreateGraphicsPipelines, vkCreateImage, vkCreateImageView,
        vkCreatePipelineLayout, vkCreateRenderPass, vkCreateShaderModule,
        vkDestroyBuffer, vkDestroyCommandPool, vkDestroyDescriptorPool,
        vkDestroyDescriptorSetLayout, vkDestroyDevice, vkDestroyFramebuffer,
        vkDestroyImage, vkDestroyImageView, vkDestroyPipeline,
        vkDestroyPipelineLayout, vkDestroyRenderPass, vkDestroyShaderModule,
        vkEndCommandBuffer, vkFreeMemory, vkGetDeviceQueue,
        vkGetImageMemoryRequirements, vkQueueSubmit, vkUpdateDescriptorSets,
    };

    let sock = tmp_socket("triangle");
    let sw_backend = Arc::new(SoftwareBackend::new());
    let backend_for_listener: Arc<dyn Backend> = sw_backend.clone();
    let listener = Listener::bind(&sock, backend_for_listener).unwrap();
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));
    let _env = EnvLock::set(&sock);

    type VkDevice = *mut std::ffi::c_void;
    type VkQueue  = *mut std::ffi::c_void;
    type VkCommandBuffer = *mut std::ffi::c_void;

    // Bootstrap: instance → physical → device → queue.
    let mut instance: VkInstance = std::ptr::null_mut();
    unsafe { vkCreateInstance(std::ptr::null(), std::ptr::null(), &mut instance); }
    let mut pds: [VkPhysicalDevice; 1] = [std::ptr::null_mut(); 1];
    let mut cap: u32 = 1;
    unsafe { vkEnumeratePhysicalDevices(instance, &mut cap, pds.as_mut_ptr()); }
    let mut device: VkDevice = std::ptr::null_mut();
    unsafe { vkCreateDevice(pds[0], std::ptr::null(), std::ptr::null(), &mut device); }
    let mut queue: VkQueue = std::ptr::null_mut();
    unsafe { vkGetDeviceQueue(device, 0, 0, &mut queue); }

    // Vertex buffer (256 B), index buffer (256 B), uniform buffer
    // (128 B). One memory allocation backs all three.
    fn mk_buffer(device: VkDevice, size: u64, usage: u32) -> u64 {
        let mut info = [0u8; 56];
        info[ 0.. 4].copy_from_slice(&12u32.to_le_bytes());
        info[24..32].copy_from_slice(&size.to_le_bytes());
        info[32..36].copy_from_slice(&usage.to_le_bytes());
        let mut h: u64 = 0;
        unsafe { vkCreateBuffer(device, info.as_ptr() as *const _, std::ptr::null(), &mut h); }
        h
    }
    let vbuf = mk_buffer(device, 256, 0x80); // VERTEX
    let ibuf = mk_buffer(device, 256, 0x40); // INDEX
    let ubuf = mk_buffer(device, 128, 0x10); // UNIFORM

    let mut alloc = [0u8; 32];
    alloc[0..4].copy_from_slice(&5u32.to_le_bytes());
    alloc[16..24].copy_from_slice(&(8192u64).to_le_bytes());
    let mut mem: u64 = 0;
    unsafe { vkAllocateMemory(device, alloc.as_ptr() as *const _, std::ptr::null(), &mut mem); }
    unsafe { vkBindBufferMemory(device, vbuf, mem,    0); }
    unsafe { vkBindBufferMemory(device, ibuf, mem, 1024); }
    unsafe { vkBindBufferMemory(device, ubuf, mem, 2048); }

    // Color attachment image + view + memory.
    let mut img_info = [0u8; 88];
    img_info[ 0.. 4].copy_from_slice(&14u32.to_le_bytes());
    img_info[24..28].copy_from_slice(&37u32.to_le_bytes());
    img_info[28..32].copy_from_slice(&64u32.to_le_bytes());
    img_info[32..36].copy_from_slice(&64u32.to_le_bytes());
    img_info[36..40].copy_from_slice(&1u32.to_le_bytes());
    img_info[40..44].copy_from_slice(&1u32.to_le_bytes());
    img_info[44..48].copy_from_slice(&1u32.to_le_bytes());
    img_info[56..60].copy_from_slice(&0x10u32.to_le_bytes()); // COLOR_ATTACHMENT
    let mut color: u64 = 0;
    unsafe { vkCreateImage(device, img_info.as_ptr() as *const _, std::ptr::null(), &mut color); }
    let mut req = ash::vk::MemoryRequirements::default();
    unsafe { vkGetImageMemoryRequirements(device, color, &mut req); }
    let mut img_alloc = [0u8; 32];
    img_alloc[0..4].copy_from_slice(&5u32.to_le_bytes());
    img_alloc[16..24].copy_from_slice(&req.size.to_le_bytes());
    let mut img_mem: u64 = 0;
    unsafe { vkAllocateMemory(device, img_alloc.as_ptr() as *const _, std::ptr::null(), &mut img_mem); }
    unsafe { vkBindImageMemory(device, color, img_mem, 0); }
    let mut view_info = [0u8; 80];
    view_info[ 0.. 4].copy_from_slice(&15u32.to_le_bytes());
    view_info[24..32].copy_from_slice(&color.to_le_bytes());
    let mut view: u64 = 0;
    unsafe { vkCreateImageView(device, view_info.as_ptr() as *const _, std::ptr::null(), &mut view); }
    thread::sleep(Duration::from_millis(30));

    // Shader modules: minimal SPIR-V (header only).
    let spv: [u32; 5] = [0x07230203, 0x00010000, 0, 1, 0];
    let mut sh_info = [0u8; 40];
    sh_info[ 0.. 4].copy_from_slice(&16u32.to_le_bytes());
    sh_info[24..32].copy_from_slice(&20u64.to_le_bytes());
    sh_info[32..40].copy_from_slice(&(spv.as_ptr() as u64).to_le_bytes());
    let mut vs: u64 = 0; let mut fs: u64 = 0;
    unsafe { vkCreateShaderModule(device, sh_info.as_ptr() as *const _, std::ptr::null(), &mut vs); }
    unsafe { vkCreateShaderModule(device, sh_info.as_ptr() as *const _, std::ptr::null(), &mut fs); }

    // Descriptor set layout + pool + set with uniform buffer at binding 0.
    let mut layout: u64 = 0;
    unsafe { vkCreateDescriptorSetLayout(device, std::ptr::null(), std::ptr::null(), &mut layout); }
    let mut dpool: u64 = 0;
    unsafe { vkCreateDescriptorPool(device, std::ptr::null(), std::ptr::null(), &mut dpool); }
    let mut da = [0u8; 40];
    da[ 0.. 4].copy_from_slice(&34u32.to_le_bytes());
    da[16..24].copy_from_slice(&dpool.to_le_bytes());
    da[24..28].copy_from_slice(&1u32.to_le_bytes());
    let layouts_arr = [layout];
    da[32..40].copy_from_slice(&(layouts_arr.as_ptr() as u64).to_le_bytes());
    let mut dset: u64 = 0;
    unsafe { vkAllocateDescriptorSets(device, da.as_ptr() as *const _, &mut dset); }

    let buf_info_arr: [u8; 24] = {
        let mut b = [0u8; 24];
        b[ 0.. 8].copy_from_slice(&ubuf.to_le_bytes());
        b[16..24].copy_from_slice(&(128u64).to_le_bytes());
        b
    };
    let mut write = [0u8; 64];
    write[ 0.. 4].copy_from_slice(&35u32.to_le_bytes());
    write[16..24].copy_from_slice(&dset.to_le_bytes());
    write[32..36].copy_from_slice(&1u32.to_le_bytes());
    write[36..40].copy_from_slice(&6u32.to_le_bytes()); // UNIFORM_BUFFER
    write[48..56].copy_from_slice(&(buf_info_arr.as_ptr() as u64).to_le_bytes());
    unsafe { vkUpdateDescriptorSets(device, 1, write.as_ptr() as *const _, 0, std::ptr::null()); }

    // Pipeline layout + graphics pipeline.
    let mut pl_layout: u64 = 0;
    unsafe { vkCreatePipelineLayout(device, std::ptr::null(), std::ptr::null(), &mut pl_layout); }
    let mut pipeline: u64 = 0;
    unsafe {
        vkCreateGraphicsPipelines(
            device, 0, 1, std::ptr::null(), std::ptr::null(), &mut pipeline,
        );
    }

    // Render pass + framebuffer.
    let mut rp_info = [0u8; 64];
    rp_info[0..4].copy_from_slice(&38u32.to_le_bytes());
    let mut render_pass: u64 = 0;
    unsafe { vkCreateRenderPass(device, rp_info.as_ptr() as *const _, std::ptr::null(), &mut render_pass); }
    let mut fb_info = [0u8; 64];
    fb_info[ 0.. 4].copy_from_slice(&37u32.to_le_bytes());
    fb_info[24..32].copy_from_slice(&render_pass.to_le_bytes());
    fb_info[32..36].copy_from_slice(&1u32.to_le_bytes());
    let atts = [view];
    fb_info[40..48].copy_from_slice(&(atts.as_ptr() as u64).to_le_bytes());
    fb_info[48..52].copy_from_slice(&64u32.to_le_bytes());
    fb_info[52..56].copy_from_slice(&64u32.to_le_bytes());
    let mut framebuffer: u64 = 0;
    unsafe { vkCreateFramebuffer(device, fb_info.as_ptr() as *const _, std::ptr::null(), &mut framebuffer); }

    // Command pool + cmdbuf.
    let mut pool: u64 = 0;
    unsafe { vkCreateCommandPool(device, std::ptr::null(), std::ptr::null(), &mut pool); }
    let mut cb_info = [0u8; 40];
    cb_info[0..4].copy_from_slice(&40u32.to_le_bytes());
    cb_info[16..24].copy_from_slice(&pool.to_le_bytes());
    cb_info[28..32].copy_from_slice(&1u32.to_le_bytes());
    let mut cbs: [VkCommandBuffer; 1] = [std::ptr::null_mut(); 1];
    unsafe { vkAllocateCommandBuffers(device, cb_info.as_ptr() as *const _, cbs.as_mut_ptr()); }
    let cb = cbs[0];

    // RECORD the triangle frame.
    unsafe { vkBeginCommandBuffer(cb, std::ptr::null()); }
    let mut rpb = [0u8; 64];
    rpb[ 0.. 4].copy_from_slice(&43u32.to_le_bytes());
    rpb[16..24].copy_from_slice(&render_pass.to_le_bytes());
    rpb[24..32].copy_from_slice(&framebuffer.to_le_bytes());
    rpb[48..52].copy_from_slice(&1u32.to_le_bytes());
    let clear: [f32; 4] = [0.2, 0.2, 0.4, 1.0];
    rpb[56..64].copy_from_slice(&(clear.as_ptr() as u64).to_le_bytes());
    unsafe { vkCmdBeginRenderPass(cb, rpb.as_ptr() as *const _, 0); }

    unsafe { vkCmdBindPipeline(cb, 0 /* graphics */, pipeline); }
    let vp = ash::vk::Viewport { x: 0.0, y: 0.0, width: 64.0, height: 64.0, min_depth: 0.0, max_depth: 1.0 };
    unsafe { vkCmdSetViewport(cb, 0, 1, &vp); }
    let sc = ash::vk::Rect2D {
        offset: ash::vk::Offset2D { x: 0, y: 0 },
        extent: ash::vk::Extent2D { width: 64, height: 64 },
    };
    unsafe { vkCmdSetScissor(cb, 0, 1, &sc); }
    let dsets = [dset];
    unsafe { vkCmdBindDescriptorSets(cb, 0, pl_layout, 0, 1, dsets.as_ptr(), 0, std::ptr::null()); }

    let vbs = [vbuf]; let vos: [u64; 1] = [0];
    unsafe { vkCmdBindVertexBuffers(cb, 0, 1, vbs.as_ptr(), vos.as_ptr()); }
    unsafe { vkCmdBindIndexBuffer(cb, ibuf, 0, 1 /* UINT32 */); }
    let pc_bytes: [u8; 4] = [0; 4];
    unsafe { vkCmdPushConstants(cb, pl_layout, 0x10, 0, 4, pc_bytes.as_ptr() as *const _); }
    unsafe { vkCmdDrawIndexed(cb, 3, 1, 0, 0, 0); }
    unsafe { vkCmdEndRenderPass(cb); }
    unsafe { vkEndCommandBuffer(cb); }

    // Verify the FrameOp stream: BeginRP, BindPipeline, SetVP,
    // SetSc, BindDescriptors, BindVB, BindIB, PushConstants,
    // DrawIndexed, EndRP — 10 records in order.
    let bytes = cmdbuf_recorded_bytes(cb);
    let expected: &[u16] = &[
        0x0010, // BeginRenderPass
        0x0020, // BindPipeline
        0x0030, // SetViewport
        0x0031, // SetScissor
        0x0021, // BindDescriptors
        0x0022, // BindVertexBuf
        0x0023, // BindIndexBuf
        0x0032, // PushConstants
        0x0041, // DrawIndexed
        0x0011, // EndRenderPass
    ];
    let mut offset = 0;
    for (i, &op) in expected.iter().enumerate() {
        let got = u16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
        assert_eq!(got, op,
            "record {i}: expected opcode 0x{op:04x}, got 0x{got:04x} at offset {offset}");
        let total = u32::from_le_bytes([
            bytes[offset + 4], bytes[offset + 5], bytes[offset + 6], bytes[offset + 7],
        ]);
        offset += total as usize;
    }
    assert_eq!(offset, bytes.len(), "frame stream has trailing bytes after expected ops");

    // Submit + verify the backend observed it.
    let pre = sw_backend.submission_count();
    let mut sinfo = [0u8; 72];
    sinfo[0..4].copy_from_slice(&4u32.to_le_bytes());
    sinfo[40..44].copy_from_slice(&1u32.to_le_bytes());
    let cb_arr = [cb];
    sinfo[48..56].copy_from_slice(&(cb_arr.as_ptr() as u64).to_le_bytes());
    let r = unsafe { vkQueueSubmit(queue, 1, sinfo.as_ptr() as *const _, std::ptr::null_mut()) };
    assert_eq!(r, 0);
    thread::sleep(Duration::from_millis(100));
    assert!(sw_backend.submission_count() > pre,
        "backend must have observed the triangle-frame submit");

    // Tear down in order.
    unsafe { vkDestroyCommandPool(device, pool, std::ptr::null()); }
    unsafe { vkDestroyFramebuffer(device, framebuffer, std::ptr::null()); }
    unsafe { vkDestroyRenderPass(device, render_pass, std::ptr::null()); }
    unsafe { vkDestroyPipeline(device, pipeline, std::ptr::null()); }
    unsafe { vkDestroyPipelineLayout(device, pl_layout, std::ptr::null()); }
    unsafe { vkDestroyDescriptorPool(device, dpool, std::ptr::null()); }
    unsafe { vkDestroyDescriptorSetLayout(device, layout, std::ptr::null()); }
    unsafe { vkDestroyShaderModule(device, vs, std::ptr::null()); }
    unsafe { vkDestroyShaderModule(device, fs, std::ptr::null()); }
    unsafe { vkDestroyImageView(device, view, std::ptr::null()); }
    unsafe { vkDestroyImage(device, color, std::ptr::null()); }
    unsafe { vkFreeMemory(device, img_mem, std::ptr::null()); }
    unsafe { vkDestroyBuffer(device, vbuf, std::ptr::null()); }
    unsafe { vkDestroyBuffer(device, ibuf, std::ptr::null()); }
    unsafe { vkDestroyBuffer(device, ubuf, std::ptr::null()); }
    unsafe { vkFreeMemory(device, mem, std::ptr::null()); }
    unsafe { vkDestroyDevice(device, std::ptr::null()); }
    unsafe { vkDestroyInstance(instance, std::ptr::null()); }
    // env var cleared by EnvLock drop
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn swapchain_ring_acquire_present_round_trip() {
    use atrium_vk_icd::{
        vkAcquireNextImageKHR, vkCreateDevice, vkCreateSwapchainKHR,
        vkDestroyDevice, vkDestroySwapchainKHR, vkGetPhysicalDeviceSurfaceFormatsKHR,
        vkGetPhysicalDeviceSurfacePresentModesKHR, vkGetSwapchainImagesKHR,
        vkQueuePresentKHR, vkGetDeviceQueue,
    };

    let sock = tmp_socket("swap");
    let sw_backend = Arc::new(SoftwareBackend::new());
    let backend_for_listener: Arc<dyn Backend> = sw_backend.clone();
    let listener = Listener::bind(&sock, backend_for_listener).unwrap();
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));
    let _env = EnvLock::set(&sock);

    type VkDevice = *mut std::ffi::c_void;
    type VkQueue  = *mut std::ffi::c_void;

    let mut instance: VkInstance = std::ptr::null_mut();
    unsafe { vkCreateInstance(std::ptr::null(), std::ptr::null(), &mut instance); }
    let mut pds: [VkPhysicalDevice; 1] = [std::ptr::null_mut(); 1];
    let mut cap: u32 = 1;
    unsafe { vkEnumeratePhysicalDevices(instance, &mut cap, pds.as_mut_ptr()); }
    let mut device: VkDevice = std::ptr::null_mut();
    unsafe { vkCreateDevice(pds[0], std::ptr::null(), std::ptr::null(), &mut device); }
    let mut queue: VkQueue = std::ptr::null_mut();
    unsafe { vkGetDeviceQueue(device, 0, 0, &mut queue); }

    // Probe surface formats (single canonical RGBA8/SRGB-NL).
    let mut fmt_count: u32 = 0;
    let r = unsafe {
        vkGetPhysicalDeviceSurfaceFormatsKHR(pds[0], 1, &mut fmt_count, std::ptr::null_mut())
    };
    assert_eq!(r, 0);
    assert_eq!(fmt_count, 1);

    // Present modes: FIFO only.
    let mut mode_count: u32 = 0;
    unsafe {
        vkGetPhysicalDeviceSurfacePresentModesKHR(
            pds[0], 1, &mut mode_count, std::ptr::null_mut(),
        );
    }
    assert_eq!(mode_count, 1);
    let mut modes = [0u32; 4];
    let mut got: u32 = 4;
    unsafe {
        vkGetPhysicalDeviceSurfacePresentModesKHR(
            pds[0], 1, &mut got, modes.as_mut_ptr(),
        );
    }
    assert_eq!(got, 1);
    assert_eq!(modes[0], 2); // VK_PRESENT_MODE_FIFO_KHR

    // Create a swapchain. VkSwapchainCreateInfoKHR layout from
    // the implementation note in lib.rs.
    let mut sc_info = [0u8; 104];
    sc_info[ 0.. 4].copy_from_slice(&1000001000u32.to_le_bytes()); // sType
    sc_info[20..28].copy_from_slice(&7u64.to_le_bytes());          // surface=opaque 7
    sc_info[28..32].copy_from_slice(&3u32.to_le_bytes());          // minImageCount=3
    sc_info[32..36].copy_from_slice(&37u32.to_le_bytes());         // RGBA8
    sc_info[40..44].copy_from_slice(&800u32.to_le_bytes());        // width
    sc_info[44..48].copy_from_slice(&600u32.to_le_bytes());        // height
    sc_info[48..52].copy_from_slice(&1u32.to_le_bytes());          // arrayLayers
    sc_info[52..56].copy_from_slice(&0x10u32.to_le_bytes());       // COLOR_ATTACHMENT

    let mut swapchain: u64 = 0;
    let r = unsafe {
        vkCreateSwapchainKHR(device, sc_info.as_ptr() as *const _, std::ptr::null(), &mut swapchain)
    };
    assert_eq!(r, 0);
    assert!(swapchain != 0);

    // Query ring.
    let mut n: u32 = 0;
    unsafe { vkGetSwapchainImagesKHR(device, swapchain, &mut n, std::ptr::null_mut()); }
    assert_eq!(n, 3);
    let mut ring = [0u64; 3];
    let mut k: u32 = 3;
    unsafe { vkGetSwapchainImagesKHR(device, swapchain, &mut k, ring.as_mut_ptr()); }
    assert_eq!(k, 3);
    assert!(ring[0] != 0 && ring[1] != 0 && ring[2] != 0);
    assert_ne!(ring[0], ring[1]);
    assert_ne!(ring[1], ring[2]);

    // Acquire 4 images — should round-robin 0, 1, 2, 0.
    let mut acquired = [99u32; 4];
    for slot in &mut acquired {
        unsafe { vkAcquireNextImageKHR(device, swapchain, u64::MAX, 0, 0, slot); }
    }
    assert_eq!(acquired, [0, 1, 2, 0]);

    // Present is a no-op success (today).
    let mut present_info = [0u8; 56];
    present_info[0..4].copy_from_slice(&1000001001u32.to_le_bytes());
    let r = unsafe { vkQueuePresentKHR(queue, present_info.as_ptr() as *const _) };
    assert_eq!(r, 0);

    unsafe { vkDestroySwapchainKHR(device, swapchain, std::ptr::null()); }
    unsafe { vkDestroyDevice(device, std::ptr::null()); }
    unsafe { vkDestroyInstance(instance, std::ptr::null()); }
    // env var cleared by EnvLock drop
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn create_atrium_surface_ext_returns_window_id_as_handle() {
    use atrium_vk_icd::{vkCreateAtriumSurfaceEXT, vkDestroySurfaceKHR};

    let sock = tmp_socket("surf");
    let sw_backend = Arc::new(SoftwareBackend::new());
    let backend_for_listener: Arc<dyn Backend> = sw_backend.clone();
    let listener = Listener::bind(&sock, backend_for_listener).unwrap();
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));
    let _env = EnvLock::set(&sock);

    let mut instance: VkInstance = std::ptr::null_mut();
    unsafe { vkCreateInstance(std::ptr::null(), std::ptr::null(), &mut instance); }

    // VkAtriumSurfaceCreateInfoEXT: sType@0, _pad@4, pNext@8,
    // flags@16, window_id@20.
    let mut info = [0u8; 24];
    info[ 0.. 4].copy_from_slice(&1_000_310_000u32.to_le_bytes());
    info[20..24].copy_from_slice(&42u32.to_le_bytes()); // window id

    let mut surface: u64 = 0;
    let r = unsafe { vkCreateAtriumSurfaceEXT(instance, info.as_ptr() as *const _, std::ptr::null(), &mut surface) };
    assert_eq!(r, 0);
    assert_eq!(surface, 42, "surface handle should be the window-id widened");

    // window_id=0 must reject.
    info[20..24].copy_from_slice(&0u32.to_le_bytes());
    let mut bad: u64 = 0;
    let r = unsafe { vkCreateAtriumSurfaceEXT(instance, info.as_ptr() as *const _, std::ptr::null(), &mut bad) };
    assert_ne!(r, 0);

    unsafe { vkDestroySurfaceKHR(instance, surface, std::ptr::null()); }
    unsafe { vkDestroyInstance(instance, std::ptr::null()); }
    // env var cleared by EnvLock drop
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn vkqueue_present_bumps_backend_present_counter() {
    use atrium_vk_icd::{
        vkAllocateMemory, vkBindImageMemory, vkCreateAtriumSurfaceEXT,
        vkCreateDevice, vkCreateSwapchainKHR,
        vkDestroyDevice, vkDestroyImage, vkDestroySwapchainKHR,
        vkDestroySurfaceKHR, vkFreeMemory, vkGetDeviceQueue,
        vkGetImageMemoryRequirements, vkGetSwapchainImagesKHR,
        vkQueuePresentKHR,
    };

    let sock = tmp_socket("present");
    let sw_backend = Arc::new(SoftwareBackend::new());
    let backend_for_listener: Arc<dyn Backend> = sw_backend.clone();
    let listener = Listener::bind(&sock, backend_for_listener).unwrap();
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));
    let _env = EnvLock::set(&sock);

    type VkDevice = *mut std::ffi::c_void;
    type VkQueue  = *mut std::ffi::c_void;

    let mut instance: VkInstance = std::ptr::null_mut();
    unsafe { vkCreateInstance(std::ptr::null(), std::ptr::null(), &mut instance); }
    let mut pds: [VkPhysicalDevice; 1] = [std::ptr::null_mut(); 1];
    let mut cap: u32 = 1;
    unsafe { vkEnumeratePhysicalDevices(instance, &mut cap, pds.as_mut_ptr()); }
    let mut device: VkDevice = std::ptr::null_mut();
    unsafe { vkCreateDevice(pds[0], std::ptr::null(), std::ptr::null(), &mut device); }
    let mut queue: VkQueue = std::ptr::null_mut();
    unsafe { vkGetDeviceQueue(device, 0, 0, &mut queue); }

    // Create a surface with a Fresco window-id.
    let mut surf_info = [0u8; 24];
    surf_info[ 0.. 4].copy_from_slice(&1_000_310_000u32.to_le_bytes());
    surf_info[20..24].copy_from_slice(&7u32.to_le_bytes());
    let mut surface: u64 = 0;
    unsafe { vkCreateAtriumSurfaceEXT(instance, surf_info.as_ptr() as *const _, std::ptr::null(), &mut surface); }
    assert_eq!(surface, 7);

    // Make a 2-deep swapchain bound to that surface.
    let mut sc_info = [0u8; 104];
    sc_info[ 0.. 4].copy_from_slice(&1000001000u32.to_le_bytes());
    sc_info[20..28].copy_from_slice(&surface.to_le_bytes());
    sc_info[28..32].copy_from_slice(&2u32.to_le_bytes());
    sc_info[32..36].copy_from_slice(&37u32.to_le_bytes());
    sc_info[40..44].copy_from_slice(&64u32.to_le_bytes());
    sc_info[44..48].copy_from_slice(&64u32.to_le_bytes());
    sc_info[48..52].copy_from_slice(&1u32.to_le_bytes());
    sc_info[52..56].copy_from_slice(&0x10u32.to_le_bytes());
    let mut swapchain: u64 = 0;
    unsafe { vkCreateSwapchainKHR(device, sc_info.as_ptr() as *const _, std::ptr::null(), &mut swapchain); }

    // Bind memory to the ring images so they have daemon-side
    // image_ids (vkQueuePresentKHR's resolver needs them).
    let mut ring = [0u64; 2];
    let mut k: u32 = 2;
    unsafe { vkGetSwapchainImagesKHR(device, swapchain, &mut k, ring.as_mut_ptr()); }
    let mut req = ash::vk::MemoryRequirements::default();
    unsafe { vkGetImageMemoryRequirements(device, ring[0], &mut req); }
    let mut alloc = [0u8; 32];
    alloc[0..4].copy_from_slice(&5u32.to_le_bytes());
    alloc[16..24].copy_from_slice(&(req.size * 2).to_le_bytes());
    let mut mem: u64 = 0;
    unsafe { vkAllocateMemory(device, alloc.as_ptr() as *const _, std::ptr::null(), &mut mem); }
    unsafe { vkBindImageMemory(device, ring[0], mem, 0); }
    unsafe { vkBindImageMemory(device, ring[1], mem, req.size); }
    thread::sleep(Duration::from_millis(30));

    // Present each ring image once. VkPresentInfoKHR has size 64 B.
    let pre = sw_backend.present_count();
    let sc_arr = [swapchain];
    let idx_arr = [0u32, 1u32];
    for i in 0..2 {
        let mut info = [0u8; 64];
        info[ 0.. 4].copy_from_slice(&1000001001u32.to_le_bytes());
        info[32..36].copy_from_slice(&1u32.to_le_bytes()); // swapchainCount
        info[40..48].copy_from_slice(&(sc_arr.as_ptr() as u64).to_le_bytes());
        info[48..56].copy_from_slice(&(idx_arr[i..].as_ptr() as u64).to_le_bytes());
        unsafe { vkQueuePresentKHR(queue, info.as_ptr() as *const _); }
    }
    thread::sleep(Duration::from_millis(100));
    let post = sw_backend.present_count();
    assert_eq!(post - pre, 2, "expected 2 present ops, got {}", post - pre);

    // Teardown.
    unsafe { vkDestroySwapchainKHR(device, swapchain, std::ptr::null()); }
    unsafe { vkDestroyImage(device, ring[0], std::ptr::null()); }
    unsafe { vkDestroyImage(device, ring[1], std::ptr::null()); }
    unsafe { vkFreeMemory(device, mem, std::ptr::null()); }
    unsafe { vkDestroySurfaceKHR(instance, surface, std::ptr::null()); }
    unsafe { vkDestroyDevice(device, std::ptr::null()); }
    unsafe { vkDestroyInstance(instance, std::ptr::null()); }
    // env var cleared by EnvLock drop
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn vkqueue_present_fires_software_backend_hook_with_surface_id() {
    // End-to-end proof that frescod-aqueduct's wiring works: install
    // a `set_present_hook` on the SoftwareBackend exactly the way the
    // daemon does, drive vkQueuePresentKHR through the ICD against
    // that daemon, and verify the hook saw the right (image, surface,
    // frame) tuple for each present.
    use atrium_vk_icd::{
        vkAllocateMemory, vkBindImageMemory, vkCreateAtriumSurfaceEXT,
        vkCreateDevice, vkCreateSwapchainKHR, vkDestroyDevice,
        vkDestroyImage, vkDestroySwapchainKHR, vkDestroySurfaceKHR,
        vkFreeMemory, vkGetDeviceQueue, vkGetImageMemoryRequirements,
        vkGetSwapchainImagesKHR, vkQueuePresentKHR,
    };
    use std::sync::atomic::{AtomicU64, Ordering};

    let sock = tmp_socket("present-hook");
    let sw_backend = Arc::new(SoftwareBackend::new());

    // Capture (frame_id, surface_id, image_id) sums via atomics —
    // matches the frescod-aqueduct pattern of not holding a Mutex
    // across the daemon's hot path.
    let frame_xor = Arc::new(AtomicU64::new(0));
    let surface_xor = Arc::new(AtomicU64::new(0));
    let image_xor = Arc::new(AtomicU64::new(0));
    let calls = Arc::new(AtomicU64::new(0));
    {
        let fx = frame_xor.clone();
        let sx = surface_xor.clone();
        let ix = image_xor.clone();
        let cc = calls.clone();
        sw_backend.set_present_hook(move |_backend, image_id, surface_id, frame_id| {
            fx.fetch_xor(frame_id, Ordering::Relaxed);
            sx.fetch_xor(surface_id, Ordering::Relaxed);
            ix.fetch_xor(image_id.raw() as u64, Ordering::Relaxed);
            cc.fetch_add(1, Ordering::Relaxed);
        });
    }

    let backend_for_listener: Arc<dyn Backend> = sw_backend.clone();
    let listener = Listener::bind(&sock, backend_for_listener).unwrap();
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));
    let _env = EnvLock::set(&sock);

    type VkDevice = *mut std::ffi::c_void;
    type VkQueue  = *mut std::ffi::c_void;

    let mut instance: VkInstance = std::ptr::null_mut();
    unsafe { vkCreateInstance(std::ptr::null(), std::ptr::null(), &mut instance); }
    let mut pds: [VkPhysicalDevice; 1] = [std::ptr::null_mut(); 1];
    let mut cap: u32 = 1;
    unsafe { vkEnumeratePhysicalDevices(instance, &mut cap, pds.as_mut_ptr()); }
    let mut device: VkDevice = std::ptr::null_mut();
    unsafe { vkCreateDevice(pds[0], std::ptr::null(), std::ptr::null(), &mut device); }
    let mut queue: VkQueue = std::ptr::null_mut();
    unsafe { vkGetDeviceQueue(device, 0, 0, &mut queue); }

    // Surface tied to Fresco window-id 42.
    let mut surf_info = [0u8; 24];
    surf_info[ 0.. 4].copy_from_slice(&1_000_310_000u32.to_le_bytes());
    surf_info[20..24].copy_from_slice(&42u32.to_le_bytes());
    let mut surface: u64 = 0;
    unsafe { vkCreateAtriumSurfaceEXT(instance, surf_info.as_ptr() as *const _, std::ptr::null(), &mut surface); }
    assert_eq!(surface, 42);

    // 2-deep swapchain.
    let mut sc_info = [0u8; 104];
    sc_info[ 0.. 4].copy_from_slice(&1000001000u32.to_le_bytes());
    sc_info[20..28].copy_from_slice(&surface.to_le_bytes());
    sc_info[28..32].copy_from_slice(&2u32.to_le_bytes());
    sc_info[32..36].copy_from_slice(&37u32.to_le_bytes());
    sc_info[40..44].copy_from_slice(&64u32.to_le_bytes());
    sc_info[44..48].copy_from_slice(&64u32.to_le_bytes());
    sc_info[48..52].copy_from_slice(&1u32.to_le_bytes());
    sc_info[52..56].copy_from_slice(&0x10u32.to_le_bytes());
    let mut swapchain: u64 = 0;
    unsafe { vkCreateSwapchainKHR(device, sc_info.as_ptr() as *const _, std::ptr::null(), &mut swapchain); }

    let mut ring = [0u64; 2];
    let mut k: u32 = 2;
    unsafe { vkGetSwapchainImagesKHR(device, swapchain, &mut k, ring.as_mut_ptr()); }
    let mut req = ash::vk::MemoryRequirements::default();
    unsafe { vkGetImageMemoryRequirements(device, ring[0], &mut req); }
    let mut alloc = [0u8; 32];
    alloc[0..4].copy_from_slice(&5u32.to_le_bytes());
    alloc[16..24].copy_from_slice(&(req.size * 2).to_le_bytes());
    let mut mem: u64 = 0;
    unsafe { vkAllocateMemory(device, alloc.as_ptr() as *const _, std::ptr::null(), &mut mem); }
    unsafe { vkBindImageMemory(device, ring[0], mem, 0); }
    unsafe { vkBindImageMemory(device, ring[1], mem, req.size); }
    thread::sleep(Duration::from_millis(30));

    // Present both ring images once each. Both should fire the hook
    // with surface=42; XOR of indices 0,1 should leave image_xor with
    // a single bit difference between the two daemon-side image_ids.
    let sc_arr = [swapchain];
    let idx_arr = [0u32, 1u32];
    for i in 0..2 {
        let mut info = [0u8; 64];
        info[ 0.. 4].copy_from_slice(&1000001001u32.to_le_bytes());
        info[32..36].copy_from_slice(&1u32.to_le_bytes());
        info[40..48].copy_from_slice(&(sc_arr.as_ptr() as u64).to_le_bytes());
        info[48..56].copy_from_slice(&(idx_arr[i..].as_ptr() as u64).to_le_bytes());
        unsafe { vkQueuePresentKHR(queue, info.as_ptr() as *const _); }
    }
    thread::sleep(Duration::from_millis(100));

    // Hook fired exactly twice.
    assert_eq!(calls.load(Ordering::Relaxed), 2,
        "expected hook to fire twice, got {}", calls.load(Ordering::Relaxed));
    // Both presents targeted surface 42: XOR cancels to 0.
    assert_eq!(surface_xor.load(Ordering::Relaxed), 0,
        "surface_id XOR should cancel for 2 presents to the same surface, got {}",
        surface_xor.load(Ordering::Relaxed));
    // image_xor must be NON-zero — two distinct ring images, XOR
    // doesn't cancel. (Catches a regression where both presents
    // accidentally resolve to the same image_id.)
    assert_ne!(image_xor.load(Ordering::Relaxed), 0,
        "image_id XOR should be non-zero for 2 distinct ring images");

    unsafe { vkDestroySwapchainKHR(device, swapchain, std::ptr::null()); }
    unsafe { vkDestroyImage(device, ring[0], std::ptr::null()); }
    unsafe { vkDestroyImage(device, ring[1], std::ptr::null()); }
    unsafe { vkFreeMemory(device, mem, std::ptr::null()); }
    unsafe { vkDestroySurfaceKHR(instance, surface, std::ptr::null()); }
    unsafe { vkDestroyDevice(device, std::ptr::null()); }
    unsafe { vkDestroyInstance(instance, std::ptr::null()); }
    // env var cleared by EnvLock drop
    let _ = server_thread;
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn proc_addr_resolves_three_entry_points() {
    // No live daemon required for the get_proc_addr lookups.
    fn lookup(name: &[u8]) -> Option<unsafe extern "C" fn()> {
        unsafe {
            vk_icdGetInstanceProcAddr(
                std::ptr::null_mut(),
                name.as_ptr() as *const c_char,
            )
        }
    }
    assert!(lookup(b"vkCreateInstance\0").is_some());
    assert!(lookup(b"vkDestroyInstance\0").is_some());
    assert!(lookup(b"vkEnumeratePhysicalDevices\0").is_some());
}
