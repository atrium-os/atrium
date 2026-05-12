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
