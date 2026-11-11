//! errno, process facts, the stack protector, math.
use crate::{abi, die};
use hospes_rt::{sys, tls};

pub unsafe extern "C" fn __errno_location() -> *mut i32 {
    core::ptr::addr_of_mut!((*tls::current()).errno)
}

pub extern "C" fn getpid() -> i32 {
    sys::getpid()
}

/// The library sees one CPU: Hospes runs codecs single-threaded (H0 §4: OpenH264's threaded
/// mode does not decode identically), and the CDM gets no more than it needs.
pub unsafe extern "C" fn sched_getaffinity(_pid: i32, size: usize, mask: *mut u8) -> i32 {
    if size == 0 || mask.is_null() {
        crate::set_errno(abi::EINVAL);
        return -1;
    }
    core::ptr::write_bytes(mask, 0, size);
    *mask = 1;
    0
}

pub unsafe extern "C" fn __sched_cpucount(size: usize, mask: *const u8) -> i32 {
    (0..size).map(|i| (*mask.add(i)).count_ones() as i32).sum()
}

pub extern "C" fn __stack_chk_fail() -> ! {
    die("stack smashing detected in the library")
}

/// aarch64 glibc code reads the canary from this data symbol.
pub static mut __stack_chk_guard: usize = 0;

pub(crate) fn publish_canary() {
    unsafe { __stack_chk_guard = tls::canary() };
}

/// Called by the library's own fini code; there is nothing of ours to finalise.
pub extern "C" fn __cxa_finalize(_d: *mut u8) {}

pub extern "C" fn ceil(x: f64) -> f64 {
    libm::ceil(x)
}
pub extern "C" fn log(x: f64) -> f64 {
    libm::log(x)
}
pub extern "C" fn log10(x: f64) -> f64 {
    libm::log10(x)
}
pub extern "C" fn logf(x: f32) -> f32 {
    libm::logf(x)
}
pub extern "C" fn sqrtf(x: f32) -> f32 {
    libm::sqrtf(x)
}
