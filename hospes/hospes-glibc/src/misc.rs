//! errno, process facts, the stack protector, math.
use crate::{abi, die};
use hospes_rt::{sys, tls};

pub unsafe extern "C" fn __errno_location() -> *mut i32 {
    crate::errno_ptr()
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

/// `abort(3)`: the library gave up. End the process; the peer sees the channel close.
pub extern "C" fn abort() -> ! {
    crate::die("abort() called by the library")
}
pub extern "C" fn ceilf(x: f32) -> f32 {
    libm::ceilf(x)
}
pub extern "C" fn floor(x: f64) -> f64 {
    libm::floor(x)
}
pub extern "C" fn exp(x: f64) -> f64 {
    libm::exp(x)
}
pub extern "C" fn log2(x: f64) -> f64 {
    libm::log2(x)
}
/// Round to nearest, ties to even: the default rounding mode, which the library never changes.
pub extern "C" fn rint(x: f64) -> f64 {
    libm::rint(x)
}
pub extern "C" fn round(x: f64) -> f64 {
    libm::round(x)
}
pub unsafe extern "C" fn frexp(x: f64, e: *mut i32) -> f64 {
    let (m, ex) = libm::frexp(x);
    *e = ex;
    m
}
pub extern "C" fn ldexp(x: f64, n: i32) -> f64 {
    libm::ldexp(x, n)
}

/// libgcc's 128-bit unsigned division (`__udivti3`), from compiler_builtins.
#[allow(improper_ctypes_definitions)]
pub extern "C" fn udivti3(a: u128, b: u128) -> u128 {
    if b == 0 {
        die("__udivti3: division by zero");
    }
    a / b
}

#[cfg(test)]
mod tests {
    #[test]
    fn math() {
        let mut e = 0;
        assert_eq!(unsafe { super::frexp(8.0, &mut e) }, 0.5);
        assert_eq!(e, 4);
        assert_eq!(super::rint(2.5), 2.0);
        assert_eq!(super::round(2.5), 3.0);
        assert_eq!(super::udivti3(u128::MAX, 3), u128::MAX / 3);
    }
}
