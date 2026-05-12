//! `atrium-vk-icd` — Vulkan ICD speaking aqueduct-gpu.
//!
//! The Vulkan loader (`libvulkan.so.1`) discovers ICDs via a JSON
//! manifest (`/usr/share/vulkan/icd.d/atrium_icd.json`) that points
//! at this `cdylib`. The loader negotiates an ABI version with us
//! via `vk_icdNegotiateLoaderICDInterfaceVersion`, then resolves
//! every Vulkan function the application calls through
//! `vk_icdGetInstanceProcAddr`. We translate the Vulkan calls into
//! aqueduct-gpu protocol messages sent to the host endpoint
//! (`frescod-aqueduct` today, the kmod direct at D5+).
//!
//! This is NOT a general Vulkan driver. It targets aqueduct-gpu
//! specifically — the format-rules table, the bundle-pipeline model,
//! the install-time AOT-compiled shader cache. See
//! `docs/spec/aqueduct-gpu.md` §7.1 for the translation table.
//!
//! # Status
//!
//! Skeleton only. The two loader-ICD entry points are stubbed so the
//! loader's negotiation succeeds and individual `vkGetInstanceProcAddr`
//! lookups return null. Application code calling actual Vulkan
//! functions on the resulting `VkInstance` would fail with
//! `VK_ERROR_INCOMPATIBLE_DRIVER`.
//!
//! ## Build status
//!
//! - ✅ `cargo build` — produces `libatrium_vk_icd.dylib` (macOS) /
//!   `.so` (FreeBSD).
//! - ⚠️ Phase 1.3b — `VkCommandBuffer` recording / submission
//!   plumbing. **pending**: the meat of the ICD; translates
//!   `vkCmd*` calls into `FrameOp` records in an aqueduct-gpu
//!   frame buffer, flushed on `vkQueueSubmit`. Today this skeleton
//!   has none of that.
//! - ⚠️ All other Vulkan entry points — also pending. Phasing per
//!   `docs/spec/aqueduct-gpu.md` §7.1's table.
//!
//! ## ICD ABI version
//!
//! We support up to loader-ICD interface version 7 (the current max
//! at time of writing; matches MoltenVK and Mesa's RADV). The loader
//! sends us its supported version; we clamp to our max and reply.

#![allow(non_snake_case)]
#![allow(non_camel_case_types)]

use std::ffi::CStr;
use std::os::raw::{c_char, c_void};

/// Maximum loader-ICD interface version we support. v7 covers
/// everything the current Khronos loader needs.
const ATRIUM_LOADER_ICD_INTERFACE_VERSION_MAX: u32 = 7;

/// Vulkan loader-ICD ABI: `VkResult` is `i32`.
type VkResult = i32;

const VK_SUCCESS:                  VkResult =  0;

/// The Vulkan API version we currently claim. Bumps as more entry
/// points reach correctness. Encoded per VK_MAKE_API_VERSION(0, 1, 3, 0).
const ATRIUM_ICD_API_VERSION: u32 = (1 << 22) | (3 << 12);

/// Type-erased function pointer the loader uses for resolved
/// Vulkan entry points.
type PFN_vkVoidFunction = Option<unsafe extern "C" fn()>;

/// Vulkan's max string length for layer/extension/device names.
/// Defined by Vk spec as VK_MAX_EXTENSION_NAME_SIZE / VK_MAX_DESCRIPTION_SIZE.
const VK_MAX_EXTENSION_NAME_SIZE: usize = 256;
const VK_MAX_DESCRIPTION_SIZE:    usize = 256;

#[repr(C)]
#[derive(Copy, Clone)]
#[allow(dead_code)]
pub struct VkExtensionProperties {
    extensionName: [c_char; VK_MAX_EXTENSION_NAME_SIZE],
    specVersion:   u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
#[allow(dead_code)]
pub struct VkLayerProperties {
    layerName:             [c_char; VK_MAX_EXTENSION_NAME_SIZE],
    specVersion:           u32,
    implementationVersion: u32,
    description:           [c_char; VK_MAX_DESCRIPTION_SIZE],
}

/// `vk_icdGetInstanceProcAddr` — the only symbol the Vulkan loader
/// strictly requires us to export. Called by the loader to resolve
/// every Vulkan function name on the application's behalf.
///
/// Today we return `None` for every lookup (skeleton).
/// `vkCreateInstance` lookup will eventually be wired up; for now
/// applications attempting to use our ICD see
/// `VK_ERROR_INCOMPATIBLE_DRIVER` on instance creation.
///
/// # Safety
///
/// `name` must be a valid NUL-terminated C string. `instance` may be
/// `VK_NULL_HANDLE` for the bootstrap functions (vkCreateInstance,
/// vkEnumerateInstanceExtensionProperties, etc.) or a valid
/// `VkInstance` handle we previously returned for the rest.
#[no_mangle]
pub unsafe extern "C" fn vk_icdGetInstanceProcAddr(
    _instance: *mut c_void, /* VkInstance — opaque */
    name: *const c_char,
) -> PFN_vkVoidFunction {
    if name.is_null() {
        return None;
    }
    let name_str = match CStr::from_ptr(name).to_str() {
        Ok(s) => s,
        Err(_) => return None,
    };

    // Bootstrap dispatch. These are the entry points the Vulkan
    // loader calls BEFORE we've returned any VkInstance handle —
    // probing whether we exist, what API version we claim, and
    // what extensions/layers we support. Implementing them
    // correctly makes us a "real but empty" ICD; the loader will
    // include us in vkEnumeratePhysicalDevices results (zero
    // physical devices today — atrium-vk-icd hasn't wired up the
    // device enumeration yet).
    //
    // Cast via `as PFN_vkVoidFunction`-typed transmute — the
    // returned function pointer is type-erased; the loader knows
    // the real signature from the name it asked for.
    type FnVoidPtr = unsafe extern "C" fn();
    match name_str {
        "vkEnumerateInstanceVersion" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(*mut u32) -> VkResult, FnVoidPtr,
            >(vkEnumerateInstanceVersion)),
        "vkEnumerateInstanceExtensionProperties" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(*const c_char, *mut u32, *mut VkExtensionProperties) -> VkResult, FnVoidPtr,
            >(vkEnumerateInstanceExtensionProperties)),
        "vkEnumerateInstanceLayerProperties" =>
            Some(std::mem::transmute::<
                unsafe extern "C" fn(*mut u32, *mut VkLayerProperties) -> VkResult, FnVoidPtr,
            >(vkEnumerateInstanceLayerProperties)),
        // Phase 1.3b+: vkCreateInstance, vkDestroyInstance,
        // vkEnumeratePhysicalDevices, command-buffer recording, …
        _ => None,
    }
}

/// `vkEnumerateInstanceVersion` — loader's first probe to learn what
/// Vulkan API version this ICD speaks. Returns the value in
/// `ATRIUM_ICD_API_VERSION`; bumps as more entry points reach
/// correctness.
#[no_mangle]
pub unsafe extern "C" fn vkEnumerateInstanceVersion(
    p_api_version: *mut u32,
) -> VkResult {
    if !p_api_version.is_null() {
        *p_api_version = ATRIUM_ICD_API_VERSION;
    }
    VK_SUCCESS
}

/// `vkEnumerateInstanceExtensionProperties` — returns the list of
/// instance-level extensions this ICD supports. Skeleton: zero
/// extensions. Standard two-call query: caller invokes once with
/// `p_props=NULL` to learn the count, then again with a buffer.
#[no_mangle]
pub unsafe extern "C" fn vkEnumerateInstanceExtensionProperties(
    _p_layer_name: *const c_char,
    p_property_count: *mut u32,
    p_properties:     *mut VkExtensionProperties,
) -> VkResult {
    if p_property_count.is_null() {
        return -7 /* VK_ERROR_INITIALIZATION_FAILED */;
    }
    // Zero extensions supported today. p_properties is ignored
    // because count is zero; future implementations would fill it.
    let cap_in = *p_property_count;
    *p_property_count = 0;
    let _ = p_properties;
    let _ = cap_in;
    VK_SUCCESS
}

/// `vkEnumerateInstanceLayerProperties` — returns ICD-side instance
/// layers. ICDs typically expose zero (layers come from external
/// dlopen'd libraries known to the loader, not from drivers).
#[no_mangle]
pub unsafe extern "C" fn vkEnumerateInstanceLayerProperties(
    p_property_count: *mut u32,
    p_properties:     *mut VkLayerProperties,
) -> VkResult {
    if p_property_count.is_null() {
        return -7 /* VK_ERROR_INITIALIZATION_FAILED */;
    }
    *p_property_count = 0;
    let _ = p_properties;
    VK_SUCCESS
}

/// `vk_icdNegotiateLoaderICDInterfaceVersion` — the loader proposes
/// its supported version (in `*p_version` on entry); we clamp to our
/// max and write the agreed-on version back.
///
/// Returns `VK_SUCCESS` if a compatible version was negotiated.
/// Returning `VK_ERROR_INCOMPATIBLE_DRIVER` (or a non-success value)
/// makes the loader skip us entirely.
///
/// # Safety
///
/// `p_version` must point to a writable `u32`.
#[no_mangle]
pub unsafe extern "C" fn vk_icdNegotiateLoaderICDInterfaceVersion(
    p_version: *mut u32,
) -> VkResult {
    if p_version.is_null() {
        // Defensive — Khronos loader always provides this, but a
        // malicious or buggy caller might not.
        return -8 /* VK_ERROR_INCOMPATIBLE_DRIVER */;
    }
    let loader_version = *p_version;
    let agreed = loader_version.min(ATRIUM_LOADER_ICD_INTERFACE_VERSION_MAX);
    *p_version = agreed;
    VK_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiate_clamps_to_our_max() {
        let mut v: u32 = 99;
        let r = unsafe { vk_icdNegotiateLoaderICDInterfaceVersion(&mut v) };
        assert_eq!(r, VK_SUCCESS);
        assert_eq!(v, ATRIUM_LOADER_ICD_INTERFACE_VERSION_MAX);
    }

    #[test]
    fn negotiate_accepts_lower_version() {
        let mut v: u32 = 3;
        let r = unsafe { vk_icdNegotiateLoaderICDInterfaceVersion(&mut v) };
        assert_eq!(r, VK_SUCCESS);
        assert_eq!(v, 3);
    }

    #[test]
    fn negotiate_null_version_rejected() {
        let r = unsafe { vk_icdNegotiateLoaderICDInterfaceVersion(std::ptr::null_mut()) };
        assert_ne!(r, VK_SUCCESS);
    }

    #[test]
    fn get_proc_addr_null_name_returns_none() {
        let r = unsafe {
            vk_icdGetInstanceProcAddr(std::ptr::null_mut(), std::ptr::null())
        };
        assert!(r.is_none());
    }

    #[test]
    fn get_proc_addr_unknown_name_returns_none() {
        // vkCreateInstance isn't wired yet (Phase 1.3b+).
        let name = b"vkCreateInstance\0";
        let r = unsafe {
            vk_icdGetInstanceProcAddr(
                std::ptr::null_mut(),
                name.as_ptr() as *const c_char,
            )
        };
        assert!(r.is_none());
    }

    fn lookup(name: &[u8]) -> PFN_vkVoidFunction {
        // Test helper — `name` must include the trailing NUL.
        unsafe {
            vk_icdGetInstanceProcAddr(
                std::ptr::null_mut(),
                name.as_ptr() as *const c_char,
            )
        }
    }

    #[test]
    fn enumerate_instance_version_reports_1_3() {
        let f = lookup(b"vkEnumerateInstanceVersion\0").expect(
            "loader bootstrap entry must resolve",
        );
        let typed: unsafe extern "C" fn(*mut u32) -> VkResult =
            unsafe { std::mem::transmute(f) };
        let mut v: u32 = 0;
        let r = unsafe { typed(&mut v) };
        assert_eq!(r, VK_SUCCESS);
        assert_eq!(v, ATRIUM_ICD_API_VERSION);
        // Confirm we're saying "1.3" and not garbage.
        let major = (v >> 22) & 0x7f;
        let minor = (v >> 12) & 0x3ff;
        assert_eq!((major, minor), (1, 3));
    }

    #[test]
    fn enumerate_instance_extensions_reports_zero() {
        let f = lookup(b"vkEnumerateInstanceExtensionProperties\0").unwrap();
        let typed: unsafe extern "C" fn(*const c_char, *mut u32, *mut VkExtensionProperties) -> VkResult =
            unsafe { std::mem::transmute(f) };
        let mut count: u32 = 99;
        let r = unsafe { typed(std::ptr::null(), &mut count, std::ptr::null_mut()) };
        assert_eq!(r, VK_SUCCESS);
        assert_eq!(count, 0);
    }

    #[test]
    fn enumerate_instance_layers_reports_zero() {
        let f = lookup(b"vkEnumerateInstanceLayerProperties\0").unwrap();
        let typed: unsafe extern "C" fn(*mut u32, *mut VkLayerProperties) -> VkResult =
            unsafe { std::mem::transmute(f) };
        let mut count: u32 = 99;
        let r = unsafe { typed(&mut count, std::ptr::null_mut()) };
        assert_eq!(r, VK_SUCCESS);
        assert_eq!(count, 0);
    }
}
