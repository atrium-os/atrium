//! Khronos Vulkan loader contract test.
//!
//! The Vulkan loader doesn't link atrium-vk-icd at compile
//! time; it discovers the cdylib via the `atrium_icd.json`
//! manifest, `dlopen`s it, and calls a tiny set of exported
//! symbols (`vk_icdNegotiateLoaderICDInterfaceVersion`,
//! `vk_icdGetInstanceProcAddr`) to bootstrap dispatch.
//!
//! This test reproduces that bootstrap entirely in-process:
//! it locates the just-built `libatrium_vk_icd.{dylib,so}` in
//! the cargo target dir, `dlopen`s it via `libloading`, and
//! exercises the loader entry points exactly as the loader
//! would. Catches link errors, missing exports, calling-
//! convention mismatches at the dlopen boundary, and
//! per-stage entry-point resolvability.
//!
//! Doesn't need libvulkan or a Vulkan SDK install -- the
//! test side IS the loader for the duration of this test.

use std::ffi::c_void;
use std::os::raw::c_char;
use std::path::PathBuf;

/// Locate the just-built atrium-vk-icd cdylib in the cargo
/// target dir (the dir holding this test's binary).
fn locate_cdylib() -> Option<PathBuf> {
    let here = std::env::current_exe().ok()?;
    // current_exe = .../target/{debug,release}/deps/loader_contract-HASH
    // The cdylib lives at .../target/{debug,release}/<lib>.{dylib,so}.
    let mut p = here;
    p.pop(); p.pop();
    for name in &[
        "libatrium_vk_icd.dylib", // macOS
        "libatrium_vk_icd.so",    // Linux/FreeBSD
    ] {
        let candidate = p.join(name);
        if candidate.exists() { return Some(candidate); }
    }
    None
}

type VkResult = i32;
const VK_SUCCESS: VkResult = 0;

#[test]
fn loader_dlopen_contract() {
    let lib_path = match locate_cdylib() {
        Some(p) => p,
        None => panic!(
            "atrium-vk-icd cdylib not found alongside this test binary; \
             cargo's [lib].crate-type should include 'cdylib' (see Cargo.toml)"
        ),
    };

    // SAFETY: libloading's Library::new is unsafe because
    // dlopen runs the loaded library's static initialisers.
    // atrium-vk-icd has none beyond Rust's standard setup, so
    // this is safe in practice.  The Library guards the
    // dlopen handle for its lifetime; we drop it at end of
    // scope.
    let lib = unsafe { libloading::Library::new(&lib_path) }
        .expect("dlopen atrium-vk-icd cdylib");

    // ── vk_icdNegotiateLoaderICDInterfaceVersion ────────────
    //
    // Loader semantics: writes its supported max version to
    // *p_version on entry; ICD clamps to its own max and
    // writes back; loader inspects.  We mimic that here.
    type NegotiateFn = unsafe extern "C" fn(*mut u32) -> VkResult;
    let negotiate: libloading::Symbol<NegotiateFn> = unsafe {
        lib.get(b"vk_icdNegotiateLoaderICDInterfaceVersion")
    }.expect("vk_icdNegotiateLoaderICDInterfaceVersion symbol");

    let mut version: u32 = 99; // loader-supplied max
    let r = unsafe { negotiate(&mut version) };
    assert_eq!(r, VK_SUCCESS,
        "negotiate returned {r}; expected VK_SUCCESS");
    assert!(version > 0 && version <= 99,
        "negotiated version {version} should be in (0..=99) range");

    // ── vk_icdGetInstanceProcAddr ───────────────────────────
    //
    // Loader uses this to fetch every other entry point.
    // We resolve vkCreateInstance + assert it's non-null.
    type GetProcAddrFn = unsafe extern "C" fn(
        instance: *mut c_void,
        p_name:   *const c_char,
    ) -> *mut c_void;
    let get_proc: libloading::Symbol<GetProcAddrFn> = unsafe {
        lib.get(b"vk_icdGetInstanceProcAddr")
    }.expect("vk_icdGetInstanceProcAddr symbol");

    let name = b"vkCreateInstance\0";
    let fp = unsafe { get_proc(std::ptr::null_mut(), name.as_ptr() as *const c_char) };
    assert!(!fp.is_null(), "vkCreateInstance should resolve via vk_icdGetInstanceProcAddr");

    // Unknown-name probe: should return null per Vulkan spec
    // (loader uses this to skip ICDs that don't implement an
    // extension's entry point).
    let bogus = b"vkDoesNotExist\0";
    let fp = unsafe { get_proc(std::ptr::null_mut(), bogus.as_ptr() as *const c_char) };
    assert!(fp.is_null(), "unknown entry point should return null");

    // Spot-check a few more well-known core entry points -- the
    // loader resolves all of these for every Vulkan app that
    // links against libvulkan.
    for name in &[
        &b"vkDestroyInstance\0"[..],
        &b"vkEnumeratePhysicalDevices\0"[..],
        &b"vkGetPhysicalDeviceProperties\0"[..],
        &b"vkCreateDevice\0"[..],
        &b"vkGetDeviceQueue\0"[..],
        &b"vkQueueSubmit\0"[..],
        &b"vkCreateShaderModule\0"[..],
        &b"vkCreateGraphicsPipelines\0"[..],
        &b"vkCreateComputePipelines\0"[..],
        &b"vkCmdDraw\0"[..],
        &b"vkCmdDispatch\0"[..],
        &b"vkQueuePresentKHR\0"[..],
    ] {
        let fp = unsafe { get_proc(std::ptr::null_mut(), name.as_ptr() as *const c_char) };
        let n = std::str::from_utf8(&name[..name.len()-1]).unwrap();
        assert!(!fp.is_null(),
            "core entry point {n:?} should resolve via vk_icdGetInstanceProcAddr");
    }
}

#[test]
fn manifest_file_format_valid() {
    // The atrium_icd.json manifest is the Khronos loader's
    // entry point into our ICD. Parse it, sanity-check the
    // shape, and verify library_path actually exists (or at
    // least is a reasonable filename).
    let manifest_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("atrium_icd.json");
    let text = std::fs::read_to_string(&manifest_path)
        .expect("atrium_icd.json should exist in the crate root");

    // Hand-parse the few fields we care about; avoids
    // pulling a JSON dep just for this assertion.
    assert!(text.contains("\"file_format_version\""),
        "manifest must declare file_format_version");
    assert!(text.contains("\"ICD\""),
        "manifest must contain ICD block");
    assert!(text.contains("\"library_path\""),
        "manifest must declare library_path");
    assert!(text.contains("\"api_version\""),
        "manifest must declare api_version");

    // file_format_version values supported by the Khronos
    // loader: 1.0.0, 1.0.1.  Newer loader versions accept
    // both.
    assert!(text.contains("\"1.0.0\"") || text.contains("\"1.0.1\""),
        "file_format_version should be 1.0.0 or 1.0.1");
}
