//! What a hosted library may ask for and never gets (docs/spec/hospes.md §6.3, sigillum.md
//! §16.5): the network, other processes, other code, shared memory, signals, and changes
//! to the filesystem. Each call fails the way glibc fails when the kernel refuses (-1 and
//! errno, a null pointer, an `EAI_*` code), and the first call of each name is logged, so
//! a library that depends on one shows it. The jail refuses all of these too; failing here
//! means the library's own error path runs instead of a trapped system call.
use crate::{abi, set_errno};
use core::ffi::c_void;
use core::sync::atomic::{AtomicU64, Ordering};

static LOGGED: AtomicU64 = AtomicU64::new(0);

/// Log the first call of `name` (by a 64-bit hash bitmap) and set errno.
pub(crate) fn refuse(name: &str, e: i32) {
    let h = name.bytes().fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32));
    let bit = 1u64 << (h % 64);
    if LOGGED.fetch_or(bit, Ordering::Relaxed) & bit == 0 {
        hospes_rt::hlog!("hospes: {}() refused", name);
    }
    if e != 0 {
        set_errno(e);
    }
}

macro_rules! refused {
    ($( $name:ident ( $($a:ident : $t:ty),* ) -> $r:ty = $v:expr, $e:expr; )*) => {
        $(
            pub unsafe extern "C" fn $name($($a: $t),*) -> $r {
                $( let _ = $a; )*
                refuse(stringify!($name), $e);
                $v
            }
        )*
    };
}

const ENOTTY: i32 = 25;
const ECHILD: i32 = 10;
const EROFS: i32 = 30;
/// `getaddrinfo`/`getnameinfo`: a permanent failure.
const EAI_FAIL: i32 = -4;

refused! {
    // network: no sockets exist, so descriptor calls see a bad descriptor
    socket(d: i32, t: i32, p: i32) -> i32 = -1, abi::EACCES;
    bind(fd: i32, a: *const c_void, l: u32) -> i32 = -1, abi::EBADF;
    connect(fd: i32, a: *const c_void, l: u32) -> i32 = -1, abi::EBADF;
    listen(fd: i32, n: i32) -> i32 = -1, abi::EBADF;
    accept(fd: i32, a: *mut c_void, l: *mut u32) -> i32 = -1, abi::EBADF;
    recv(fd: i32, b: *mut c_void, n: usize, f: i32) -> isize = -1, abi::EBADF;
    recvfrom(fd: i32, b: *mut c_void, n: usize, f: i32, a: *mut c_void, l: *mut u32) -> isize = -1, abi::EBADF;
    send(fd: i32, b: *const c_void, n: usize, f: i32) -> isize = -1, abi::EBADF;
    sendto(fd: i32, b: *const c_void, n: usize, f: i32, a: *const c_void, l: u32) -> isize = -1, abi::EBADF;
    sendmsg(fd: i32, m: *const c_void, f: i32) -> isize = -1, abi::EBADF;
    getsockopt(fd: i32, lv: i32, o: i32, v: *mut c_void, l: *mut u32) -> i32 = -1, abi::EBADF;
    setsockopt(fd: i32, lv: i32, o: i32, v: *const c_void, l: u32) -> i32 = -1, abi::EBADF;
    getpeername(fd: i32, a: *mut c_void, l: *mut u32) -> i32 = -1, abi::EBADF;
    getsockname(fd: i32, a: *mut c_void, l: *mut u32) -> i32 = -1, abi::EBADF;
    shutdown(fd: i32, how: i32) -> i32 = -1, abi::EBADF;
    getaddrinfo(n: *const u8, s: *const u8, h: *const c_void, r: *mut *mut c_void) -> i32 = EAI_FAIL, 0;
    getnameinfo(a: *const c_void, l: u32, h: *mut u8, hl: u32, s: *mut u8, sl: u32, f: i32) -> i32 = EAI_FAIL, 0;
    gethostbyname(n: *const u8) -> *mut c_void = core::ptr::null_mut(), 0;
    // other processes and other code
    fork() -> i32 = -1, abi::ENOSYS;
    popen(c: *const u8, m: *const u8) -> *mut c_void = core::ptr::null_mut(), abi::EACCES;
    pclose(f: *mut c_void) -> i32 = -1, ECHILD;
    getcontext(u: *mut c_void) -> i32 = -1, abi::ENOSYS;
    setcontext(u: *const c_void) -> i32 = -1, abi::ENOSYS;
    dladdr(a: *const c_void, i: *mut c_void) -> i32 = 0, 0;
    // System V shared memory
    shmget(k: i32, s: usize, f: i32) -> i32 = -1, abi::ENOSYS;
    shmat(id: i32, a: *const c_void, f: i32) -> *mut c_void = usize::MAX as *mut c_void, abi::ENOSYS;
    shmdt(a: *const c_void) -> i32 = -1, abi::ENOSYS;
    // the filesystem is read-only and has no directories to list
    chdir(p: *const u8) -> i32 = -1, abi::EACCES;
    chmod(p: *const u8, m: u32) -> i32 = -1, EROFS;
    fchmod(fd: i32, m: u32) -> i32 = -1, EROFS;
    fchmodat(d: i32, p: *const u8, m: u32, f: i32) -> i32 = -1, EROFS;
    mkdir(p: *const u8, m: u32) -> i32 = -1, EROFS;
    remove(p: *const u8) -> i32 = -1, EROFS;
    rename(a: *const u8, b: *const u8) -> i32 = -1, EROFS;
    link(a: *const u8, b: *const u8) -> i32 = -1, EROFS;
    symlink(a: *const u8, b: *const u8) -> i32 = -1, EROFS;
    truncate(p: *const u8, n: i64) -> i32 = -1, EROFS;
    utimensat(d: i32, p: *const u8, t: *const c_void, f: i32) -> i32 = -1, EROFS;
    opendir(p: *const u8) -> *mut c_void = core::ptr::null_mut(), abi::ENOENT;
    readdir(d: *mut c_void) -> *mut c_void = core::ptr::null_mut(), abi::EBADF;
    closedir(d: *mut c_void) -> i32 = -1, abi::EBADF;
    statvfs(p: *const u8, b: *mut c_void) -> i32 = -1, abi::ENOSYS;
    sendfile(o: i32, i: i32, off: *mut i64, n: usize) -> isize = -1, abi::EINVAL;
    dup(fd: i32) -> i32 = -1, abi::EMFILE;
    dup2(a: i32, b: i32) -> i32 = -1, abi::EBADF;
    // terminals and devices: none
    tcgetattr(fd: i32, t: *mut c_void) -> i32 = -1, ENOTTY;
    tcsetattr(fd: i32, a: i32, t: *const c_void) -> i32 = -1, ENOTTY;
}

pub unsafe extern "C" fn execle(p: *const u8, a: *const u8, _args: ...) -> i32 {
    let _ = (p, a);
    refuse("execle", abi::EACCES);
    -1
}
pub unsafe extern "C" fn ioctl(fd: i32, req: usize, _args: ...) -> i32 {
    let _ = (fd, req);
    refuse("ioctl", ENOTTY);
    -1
}
/// No context to switch to: `getcontext` already failed.
pub unsafe extern "C" fn makecontext(u: *mut c_void, f: usize, argc: i32, _args: ...) {
    let _ = (u, f, argc);
    refuse("makecontext", 0);
}
pub extern "C" fn freeaddrinfo(_r: *mut c_void) {}
pub extern "C" fn gai_strerror(_e: i32) -> *const u8 {
    b"Name resolution is not available\0".as_ptr()
}

// ---- signals: handlers are never installed --------------------------------------------

/// glibc's `struct sigaction` is 152 bytes on both machines.
const SIZEOF_SIGACTION: usize = 152;

/// Accepted and ignored: no signal reaches the library (the host owns them).
pub unsafe extern "C" fn sigaction(sig: i32, act: *const c_void, old: *mut c_void) -> i32 {
    let _ = (sig, act);
    if !old.is_null() {
        core::ptr::write_bytes(old as *mut u8, 0, SIZEOF_SIGACTION);
    }
    refuse("sigaction", 0);
    0
}
/// `SIG_DFL` was the previous handler; the new one is not installed.
pub unsafe extern "C" fn signal(sig: i32, h: usize) -> usize {
    let _ = (sig, h);
    refuse("signal", 0);
    0
}
pub unsafe extern "C" fn sigemptyset(set: *mut u8) -> i32 {
    core::ptr::write_bytes(set, 0, 128);
    0
}

// ---- dynamic loading: there is no other code ------------------------------------------

static mut DLERROR: bool = false;

pub unsafe extern "C" fn dlopen(name: *const u8, _flags: i32) -> *mut c_void {
    let _ = name;
    refuse("dlopen", 0);
    DLERROR = true;
    core::ptr::null_mut()
}
/// The message once after a failed `dlopen`, then null (glibc clears it on read).
pub unsafe extern "C" fn dlerror() -> *const u8 {
    if core::mem::replace(&mut *core::ptr::addr_of_mut!(DLERROR), false) {
        b"dlopen: no other libraries in this process\0".as_ptr()
    } else {
        core::ptr::null()
    }
}
/// libstdc++ walks the loaded objects only for its unwinder's frame lookup, which is ours.
pub extern "C" fn dl_iterate_phdr(_cb: usize, _data: *mut c_void) -> i32 {
    0
}

// ---- environment ----------------------------------------------------------------------

/// The environment is empty and stays so.
pub unsafe extern "C" fn putenv(s: *mut u8) -> i32 {
    let _ = s;
    refuse("putenv", 0);
    0
}
pub extern "C" fn mlock(_a: *const c_void, _n: usize) -> i32 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_set_errno() {
        unsafe {
            assert_eq!(socket(2, 1, 0), -1);
            assert_eq!(*crate::errno_ptr(), abi::EACCES);
            assert_eq!(connect(3, core::ptr::null(), 0), -1);
            assert_eq!(*crate::errno_ptr(), abi::EBADF);
            assert_eq!(getaddrinfo(b"x\0".as_ptr(), core::ptr::null(), core::ptr::null(), core::ptr::null_mut()), EAI_FAIL);
            assert!(popen(b"ls\0".as_ptr(), b"r\0".as_ptr()).is_null());
            assert!(dlopen(b"libc.so.6\0".as_ptr(), 2).is_null());
            assert!(!dlerror().is_null());
            assert!(dlerror().is_null());
            let mut old = [0xffu8; SIZEOF_SIGACTION];
            assert_eq!(sigaction(13, core::ptr::null(), old.as_mut_ptr() as *mut c_void), 0);
            assert!(old.iter().all(|&b| b == 0));
            assert_eq!(mkdir(b"/tmp/x\0".as_ptr(), 0o755), -1);
            assert_eq!(*crate::errno_ptr(), EROFS);
        }
    }
}
