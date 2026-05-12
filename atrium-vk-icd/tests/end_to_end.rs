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

#[test]
fn live_handshake_reports_one_physical_device() {
    let sock = tmp_socket("live");
    let sw_backend = Arc::new(SoftwareBackend::new());
    let backend_for_listener: Arc<dyn Backend> = sw_backend.clone();
    let listener = Listener::bind(&sock, backend_for_listener).unwrap();
    let server_thread = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));

    // Point the ICD's connect path at our temp socket.
    std::env::set_var("ATRIUM_VK_ICD_SOCKET", &sock);

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

    std::env::remove_var("ATRIUM_VK_ICD_SOCKET");
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

    std::env::set_var("ATRIUM_VK_ICD_SOCKET", &sock);

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
    std::env::remove_var("ATRIUM_VK_ICD_SOCKET");
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

    std::env::set_var("ATRIUM_VK_ICD_SOCKET", &sock);

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
    std::env::remove_var("ATRIUM_VK_ICD_SOCKET");
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

    std::env::set_var("ATRIUM_VK_ICD_SOCKET", &sock);

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
    std::env::remove_var("ATRIUM_VK_ICD_SOCKET");
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

    std::env::set_var("ATRIUM_VK_ICD_SOCKET", &sock);

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
    std::env::remove_var("ATRIUM_VK_ICD_SOCKET");
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

    std::env::set_var("ATRIUM_VK_ICD_SOCKET", &sock);

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

    // Format: all zero today.
    let mut fp = ash::vk::FormatProperties::default();
    unsafe {
        vkGetPhysicalDeviceFormatProperties(
            devices[0], ash::vk::Format::R8G8B8A8_UNORM, &mut fp,
        );
    }
    assert!(fp.linear_tiling_features.is_empty());
    assert!(fp.optimal_tiling_features.is_empty());
    assert!(fp.buffer_features.is_empty());

    unsafe { vkDestroyInstance(instance, std::ptr::null()); }
    std::env::remove_var("ATRIUM_VK_ICD_SOCKET");
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

    std::env::set_var("ATRIUM_VK_ICD_SOCKET", &sock);

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
    std::env::remove_var("ATRIUM_VK_ICD_SOCKET");
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

    std::env::set_var("ATRIUM_VK_ICD_SOCKET", &sock);

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
    std::env::remove_var("ATRIUM_VK_ICD_SOCKET");
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

    std::env::set_var("ATRIUM_VK_ICD_SOCKET", &sock);

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
    std::env::remove_var("ATRIUM_VK_ICD_SOCKET");
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
    std::env::set_var("ATRIUM_VK_ICD_SOCKET", &sock);

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
    std::env::remove_var("ATRIUM_VK_ICD_SOCKET");
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
    std::env::set_var("ATRIUM_VK_ICD_SOCKET", &sock);

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
    std::env::remove_var("ATRIUM_VK_ICD_SOCKET");
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
    std::env::set_var("ATRIUM_VK_ICD_SOCKET", &sock);

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
    std::env::remove_var("ATRIUM_VK_ICD_SOCKET");
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
