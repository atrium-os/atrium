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
