//! The rest of what the module imports from a Linux desktop: libudev and
//! libcurl. Neither exists in the module's jail.
//!
//! - **udev**: a context and an enumerator exist, and they find no devices.
//!   The token is reached through PC/SC only. Each call is logged with the
//!   names it asks about (subsystems, attributes; never data), so a module
//!   that needs a device found this way shows what it looks for.
//! - **curl**: no handle can be made (`curl_easy_init` is null) and every
//!   call fails with `CURLE_FAILED_INIT`. The module's network features
//!   (certificate status lookups, firmware checks) fail; signing doesn't need
//!   them. The jail has no network either.
#![allow(clippy::missing_safety_doc)]
use core::ffi::{c_char, c_int, c_void};
use core::sync::atomic::{AtomicUsize, Ordering};

static LOG: AtomicUsize = AtomicUsize::new(0);

/// Where the stubs report calls: the host's log.
pub fn set_log(f: fn(core::fmt::Arguments)) {
    LOG.store(f as usize, Ordering::Release);
}

pub(crate) fn log(a: core::fmt::Arguments) {
    let f = LOG.load(Ordering::Acquire);
    if f != 0 {
        // Safety: only `set_log` stores here, with a `fn(Arguments)`.
        let f: fn(core::fmt::Arguments) = unsafe { core::mem::transmute(f) };
        f(a);
    }
}

unsafe fn name<'a>(p: *const c_char) -> &'a str {
    if p.is_null() {
        return "(null)";
    }
    let mut n = 0;
    while *p.add(n) != 0 && n < 128 {
        n += 1;
    }
    core::str::from_utf8(core::slice::from_raw_parts(p as *const u8, n)).unwrap_or("?")
}

/// The one udev context and enumerator: non-null, opaque, never dereferenced.
static UDEV: u8 = 0;
static ENUMERATE: u8 = 0;
fn udev() -> *mut c_void {
    &UDEV as *const u8 as *mut c_void
}
fn enumerate() -> *mut c_void {
    &ENUMERATE as *const u8 as *mut c_void
}
const NULL: *mut c_void = core::ptr::null_mut();

pub extern "C" fn udev_new() -> *mut c_void {
    log(format_args!("udev_new"));
    udev()
}
pub extern "C" fn udev_unref(_u: *mut c_void) -> *mut c_void {
    NULL
}
pub extern "C" fn udev_enumerate_new(_u: *mut c_void) -> *mut c_void {
    enumerate()
}
pub extern "C" fn udev_enumerate_unref(_e: *mut c_void) -> *mut c_void {
    NULL
}
pub unsafe extern "C" fn udev_enumerate_add_match_subsystem(_e: *mut c_void, s: *const c_char) -> c_int {
    log(format_args!("udev: match subsystem {}", name(s)));
    0
}
pub extern "C" fn udev_enumerate_scan_devices(_e: *mut c_void) -> c_int {
    0
}
/// No devices.
pub extern "C" fn udev_enumerate_get_list_entry(_e: *mut c_void) -> *mut c_void {
    log(format_args!("udev: enumeration found no devices"));
    NULL
}
pub extern "C" fn udev_list_entry_get_name(_l: *mut c_void) -> *const c_char {
    core::ptr::null()
}
pub extern "C" fn udev_list_entry_get_next(_l: *mut c_void) -> *mut c_void {
    NULL
}
pub unsafe extern "C" fn udev_device_new_from_syspath(_u: *mut c_void, p: *const c_char) -> *mut c_void {
    log(format_args!("udev: device at {}: none", name(p)));
    NULL
}
pub extern "C" fn udev_device_new_from_devnum(_u: *mut c_void, kind: c_char, dev: u64) -> *mut c_void {
    log(format_args!("udev: device {} {:#x}: none", kind as u8 as char, dev));
    NULL
}
pub unsafe extern "C" fn udev_device_get_parent_with_subsystem_devtype(
    _d: *mut c_void,
    s: *const c_char,
    t: *const c_char,
) -> *mut c_void {
    log(format_args!("udev: parent {}/{}: none", name(s), name(t)));
    NULL
}
pub unsafe extern "C" fn udev_device_get_sysattr_value(_d: *mut c_void, a: *const c_char) -> *const c_char {
    log(format_args!("udev: attribute {}: none", name(a)));
    core::ptr::null()
}
pub extern "C" fn udev_device_get_devnode(_d: *mut c_void) -> *const c_char {
    core::ptr::null()
}
pub extern "C" fn udev_device_get_syspath(_d: *mut c_void) -> *const c_char {
    core::ptr::null()
}
pub extern "C" fn udev_device_unref(_d: *mut c_void) -> *mut c_void {
    NULL
}

/// `CURLE_FAILED_INIT`.
const FAILED_INIT: c_int = 2;

pub extern "C" fn curl_easy_init() -> *mut c_void {
    log(format_args!("curl: no network in this jail"));
    NULL
}
pub unsafe extern "C" fn curl_easy_setopt(_h: *mut c_void, _opt: c_int, _args: ...) -> c_int {
    FAILED_INIT
}
pub extern "C" fn curl_easy_perform(_h: *mut c_void) -> c_int {
    FAILED_INIT
}
pub unsafe extern "C" fn curl_easy_getinfo(_h: *mut c_void, _info: c_int, _args: ...) -> c_int {
    FAILED_INIT
}
pub extern "C" fn curl_easy_cleanup(_h: *mut c_void) {}
pub extern "C" fn curl_slist_append(_l: *mut c_void, _s: *const c_char) -> *mut c_void {
    NULL
}
pub extern "C" fn curl_slist_free_all(_l: *mut c_void) {}
pub extern "C" fn curl_mime_init(_h: *mut c_void) -> *mut c_void {
    NULL
}
pub extern "C" fn curl_mime_addpart(_m: *mut c_void) -> *mut c_void {
    NULL
}
pub extern "C" fn curl_mime_name(_p: *mut c_void, _n: *const c_char) -> c_int {
    FAILED_INIT
}
pub extern "C" fn curl_mime_filedata(_p: *mut c_void, _f: *const c_char) -> c_int {
    FAILED_INIT
}

/// Every udev and curl import of the module, by name.
pub fn resolve(n: &str) -> Option<usize> {
    macro_rules! a {
        ($f:expr) => {
            $f as *const () as usize
        };
    }
    Some(match n {
        "udev_new" => a!(udev_new),
        "udev_unref" => a!(udev_unref),
        "udev_enumerate_new" => a!(udev_enumerate_new),
        "udev_enumerate_unref" => a!(udev_enumerate_unref),
        "udev_enumerate_add_match_subsystem" => a!(udev_enumerate_add_match_subsystem),
        "udev_enumerate_scan_devices" => a!(udev_enumerate_scan_devices),
        "udev_enumerate_get_list_entry" => a!(udev_enumerate_get_list_entry),
        "udev_list_entry_get_name" => a!(udev_list_entry_get_name),
        "udev_list_entry_get_next" => a!(udev_list_entry_get_next),
        "udev_device_new_from_syspath" => a!(udev_device_new_from_syspath),
        "udev_device_new_from_devnum" => a!(udev_device_new_from_devnum),
        "udev_device_get_parent_with_subsystem_devtype" => a!(udev_device_get_parent_with_subsystem_devtype),
        "udev_device_get_sysattr_value" => a!(udev_device_get_sysattr_value),
        "udev_device_get_devnode" => a!(udev_device_get_devnode),
        "udev_device_get_syspath" => a!(udev_device_get_syspath),
        "udev_device_unref" => a!(udev_device_unref),
        "curl_easy_init" => a!(curl_easy_init),
        "curl_easy_setopt" => a!(curl_easy_setopt),
        "curl_easy_perform" => a!(curl_easy_perform),
        "curl_easy_getinfo" => a!(curl_easy_getinfo),
        "curl_easy_cleanup" => a!(curl_easy_cleanup),
        "curl_slist_append" => a!(curl_slist_append),
        "curl_slist_free_all" => a!(curl_slist_free_all),
        "curl_mime_init" => a!(curl_mime_init),
        "curl_mime_addpart" => a!(curl_mime_addpart),
        "curl_mime_name" => a!(curl_mime_name),
        "curl_mime_filedata" => a!(curl_mime_filedata),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_udev_and_curl_import_of_the_module_resolves() {
        let list = include_str!("../../../hospes/probe/h0-data/innait-dsc-4.0.7.1-x64.so.imports");
        let mut n = 0;
        for l in list.lines() {
            let sym = l.split_whitespace().next().unwrap_or("");
            // With its version, as the host's resolver sees it: libudev's are `@LIBUDEV_183`.
            let (name, ver) = match sym.split_once('@') {
                Some((n, v)) => (n, Some(v)),
                None => (sym, None),
            };
            if name.starts_with("udev_") || name.starts_with("curl_") {
                assert!(crate::resolve_import(name, ver).is_some(), "{sym}");
                n += 1;
            }
            if name.starts_with("SCard") {
                assert!(crate::resolve_import(name, ver).is_some(), "{sym}");
            }
        }
        assert_eq!(n, 27);
        // A version the module does not use binds nothing of ours.
        assert!(crate::resolve_import("udev_new", Some("LIBUDEV_999")).is_none());
        assert!(crate::resolve_import("SCardConnect", Some("LIBUDEV_183")).is_none());
        assert!(super::curl_easy_init().is_null());
        assert!(!super::udev_new().is_null());
        assert!(super::udev_enumerate_get_list_entry(super::enumerate()).is_null());
    }
}
