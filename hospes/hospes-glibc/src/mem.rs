//! malloc family and the memory/string routines.
use crate::{abi, set_errno};
use core::ffi::c_void;
use hospes_rt::heap;

pub unsafe extern "C" fn malloc(n: usize) -> *mut c_void {
    let p = heap::alloc(n, 16);
    if p.is_null() {
        set_errno(abi::ENOMEM);
    }
    p as *mut c_void
}

pub unsafe extern "C" fn calloc(n: usize, size: usize) -> *mut c_void {
    let Some(total) = n.checked_mul(size) else {
        set_errno(abi::ENOMEM);
        return core::ptr::null_mut();
    };
    let p = malloc(total);
    if !p.is_null() {
        core::ptr::write_bytes(p as *mut u8, 0, total);
    }
    p
}

pub unsafe extern "C" fn free(p: *mut c_void) {
    heap::free(p as *mut u8)
}

pub unsafe extern "C" fn realloc(p: *mut c_void, n: usize) -> *mut c_void {
    if !p.is_null() && n == 0 {
        heap::free(p as *mut u8);
        return core::ptr::null_mut();
    }
    let q = heap::realloc(p as *mut u8, n, 16);
    if q.is_null() {
        set_errno(abi::ENOMEM);
    }
    q as *mut c_void
}

// These lower to compiler-builtins' optimised routines (our own Rust symbols, not these).
pub unsafe extern "C" fn memcpy(d: *mut c_void, s: *const c_void, n: usize) -> *mut c_void {
    core::ptr::copy_nonoverlapping(s as *const u8, d as *mut u8, n);
    d
}
pub unsafe extern "C" fn memmove(d: *mut c_void, s: *const c_void, n: usize) -> *mut c_void {
    core::ptr::copy(s as *const u8, d as *mut u8, n);
    d
}
pub unsafe extern "C" fn memset(d: *mut c_void, c: i32, n: usize) -> *mut c_void {
    core::ptr::write_bytes(d as *mut u8, c as u8, n);
    d
}
pub unsafe extern "C" fn memcmp(a: *const c_void, b: *const c_void, n: usize) -> i32 {
    let (a, b) = (core::slice::from_raw_parts(a as *const u8, n), core::slice::from_raw_parts(b as *const u8, n));
    for i in 0..n {
        if a[i] != b[i] {
            return a[i] as i32 - b[i] as i32;
        }
    }
    0
}
pub unsafe extern "C" fn strlen(s: *const u8) -> usize {
    let mut n = 0;
    while *s.add(n) != 0 {
        n += 1;
    }
    n
}
pub unsafe extern "C" fn strncpy(d: *mut u8, s: *const u8, n: usize) -> *mut u8 {
    let mut i = 0;
    while i < n && *s.add(i) != 0 {
        *d.add(i) = *s.add(i);
        i += 1;
    }
    while i < n {
        *d.add(i) = 0;
        i += 1;
    }
    d
}
