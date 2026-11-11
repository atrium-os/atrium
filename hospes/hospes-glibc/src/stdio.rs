//! stdio, for logging only: `stderr` reaches our log, and no file can be opened (§6.3:
//! "No real filesystem").
use crate::printf::{self, Buf};
use crate::{abi, set_errno};
use core::ffi::{c_void, VaList};
use hospes_rt::sys;

/// A `FILE` we recognise by address; glibc code only passes the pointer back to us.
#[repr(C)]
pub struct File {
    magic: u64,
}
static STDERR_FILE: File = File { magic: 0x7374_6465_7272 };
/// glibc's `stderr` is a data symbol holding a `FILE *`.
pub static mut stderr: *const File = &STDERR_FILE;

fn is_stderr(f: *const File) -> bool {
    core::ptr::eq(f, &STDERR_FILE)
}

fn log_bytes(b: &[u8]) {
    let _ = sys::write_all(2, b);
}

pub unsafe extern "C" fn __fprintf_chk(f: *const File, _flag: i32, fmt: *const u8, mut args: ...) -> i32 {
    let mut tmp = [0u8; 1024];
    let mut b = Buf::new(tmp.as_mut_ptr(), tmp.len());
    printf::format(&mut b, fmt, &mut args);
    let n = b.len();
    if is_stderr(f) {
        log_bytes(&tmp[..n.min(tmp.len() - 1)]);
        n as i32
    } else {
        set_errno(abi::EBADF);
        -1
    }
}

pub unsafe extern "C" fn __vsnprintf_chk(s: *mut u8, maxlen: usize, _flag: i32, slen: usize, fmt: *const u8, mut ap: VaList) -> i32 {
    if slen < maxlen {
        crate::die("__vsnprintf_chk: buffer overflow");
    }
    let mut b = Buf::new(s, maxlen);
    printf::format(&mut b, fmt, &mut ap);
    b.terminate();
    b.len() as i32
}

pub unsafe extern "C" fn fopen(_path: *const u8, _mode: *const u8) -> *mut File {
    set_errno(abi::EACCES);
    core::ptr::null_mut()
}
pub unsafe extern "C" fn fclose(f: *mut File) -> i32 {
    if is_stderr(f) { 0 } else { set_errno(abi::EBADF); -1 }
}
pub unsafe extern "C" fn fflush(_f: *mut File) -> i32 {
    0
}
pub unsafe extern "C" fn fseek(_f: *mut File, _off: i64, _whence: i32) -> i32 {
    set_errno(abi::EBADF);
    -1
}
pub unsafe extern "C" fn fwrite(p: *const c_void, size: usize, n: usize, f: *mut File) -> usize {
    if !is_stderr(f) {
        set_errno(abi::EBADF);
        return 0;
    }
    let Some(len) = size.checked_mul(n) else { return 0 };
    log_bytes(core::slice::from_raw_parts(p as *const u8, len));
    n
}
