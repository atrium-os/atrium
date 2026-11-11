//! FreeBSD system calls (amd64 and arm64 share the numbers). Atrium ships its own kernel, so
//! this list is a contract we control (docs/spec/hospes.md §4).
use super::arch::sc;
pub use super::arch::tp;
use super::{Errno, R};

pub const OS: &str = "freebsd";

const SYS_EXIT: usize = 1;
const SYS_READ: usize = 3;
const SYS_WRITE: usize = 4;
const SYS_CLOSE: usize = 6;
const SYS_GETPID: usize = 20;
const SYS_MUNMAP: usize = 73;
const SYS_MPROTECT: usize = 74;
const SYS_MADVISE: usize = 75;
#[cfg(target_arch = "x86_64")]
const SYS_SYSARCH: usize = 165;
const SYS_CLOCK_GETTIME: usize = 232;
const SYS_SCHED_YIELD: usize = 331;
const SYS_THR_EXIT: usize = 431;
const SYS_THR_SELF: usize = 432;
const SYS_UMTX_OP: usize = 454;
const SYS_THR_NEW: usize = 455;
const SYS_MMAP: usize = 477;
const SYS_LSEEK: usize = 478;
const SYS_OPENAT: usize = 499;
const SYS_CAP_ENTER: usize = 516;
const SYS_CAP_GETMODE: usize = 517;
const SYS_GETRANDOM: usize = 563;

pub const MAP_SHARED: usize = 0x1;
pub const MAP_PRIVATE: usize = 0x2;
pub const MAP_FIXED: usize = 0x10;
pub const MAP_ANON: usize = 0x1000;
pub const O_RDONLY: usize = 0;
pub const O_CLOEXEC: usize = 0x0010_0000;
pub const AT_FDCWD: isize = -100;
pub const ECAPMODE: i32 = 94;
pub const ENOSYS: i32 = 78;
pub const EINTR: i32 = 4;
pub const EAGAIN: i32 = 35;

/// Auxiliary vector keys (sys/elf_common.h).
pub const AT_NULL: usize = 0;
pub const AT_PAGESZ: usize = 6;
pub const AT_HWCAP: usize = 25;
pub const AT_HWCAP2: usize = 26;

#[inline]
fn call(n: usize, a: [usize; 6]) -> R<usize> {
    let (v, err) = unsafe { sc(n, a) };
    if err {
        Err(Errno(v as i32))
    } else {
        Ok(v)
    }
}

pub fn exit(code: i32) -> ! {
    let _ = call(SYS_EXIT, [code as usize, 0, 0, 0, 0, 0]);
    loop {}
}
pub fn read(fd: i32, b: &mut [u8]) -> R<usize> {
    call(SYS_READ, [fd as usize, b.as_mut_ptr() as usize, b.len(), 0, 0, 0])
}
pub fn write(fd: i32, b: &[u8]) -> R<usize> {
    call(SYS_WRITE, [fd as usize, b.as_ptr() as usize, b.len(), 0, 0, 0])
}
pub fn close(fd: i32) -> R<()> {
    call(SYS_CLOSE, [fd as usize, 0, 0, 0, 0, 0]).map(|_| ())
}
pub fn getpid() -> i32 {
    call(SYS_GETPID, [0; 6]).unwrap_or(0) as i32
}
/// `path` must be NUL-terminated.
pub fn openat(dirfd: isize, path: &[u8], flags: usize) -> R<i32> {
    debug_assert!(path.last() == Some(&0));
    call(SYS_OPENAT, [dirfd as usize, path.as_ptr() as usize, flags, 0, 0, 0]).map(|v| v as i32)
}
pub fn lseek(fd: i32, off: i64, whence: i32) -> R<u64> {
    call(SYS_LSEEK, [fd as usize, off as usize, whence as usize, 0, 0, 0]).map(|v| v as u64)
}
pub fn mmap(addr: usize, len: usize, prot: usize, flags: usize, fd: i32, off: u64) -> R<usize> {
    call(SYS_MMAP, [addr, len, prot, flags, fd as isize as usize, off as usize])
}
pub fn munmap(addr: usize, len: usize) -> R<()> {
    call(SYS_MUNMAP, [addr, len, 0, 0, 0, 0]).map(|_| ())
}
pub fn mprotect(addr: usize, len: usize, prot: usize) -> R<()> {
    call(SYS_MPROTECT, [addr, len, prot, 0, 0, 0]).map(|_| ())
}
pub fn madvise(addr: usize, len: usize, advice: usize) -> R<()> {
    call(SYS_MADVISE, [addr, len, advice, 0, 0, 0]).map(|_| ())
}
pub fn sched_yield() {
    let _ = call(SYS_SCHED_YIELD, [0; 6]);
}
pub fn getrandom(b: &mut [u8]) -> R<usize> {
    call(SYS_GETRANDOM, [b.as_mut_ptr() as usize, b.len(), 0, 0, 0, 0])
}
/// CLOCK_MONOTONIC = 4 on FreeBSD.
pub fn monotonic_ns() -> u64 {
    let mut ts = [0i64; 2];
    let _ = call(SYS_CLOCK_GETTIME, [4, ts.as_mut_ptr() as usize, 0, 0, 0, 0]);
    ts[0] as u64 * 1_000_000_000 + ts[1] as u64
}
pub fn gettid() -> i64 {
    let mut id: i64 = 0;
    let _ = call(SYS_THR_SELF, [&mut id as *mut i64 as usize, 0, 0, 0, 0, 0]);
    id
}

/// Enter capability mode. After this, no global namespaces: no paths, no sockets by
/// address, no other processes.
pub fn cap_enter() -> R<()> {
    call(SYS_CAP_ENTER, [0; 6]).map(|_| ())
}
pub fn cap_getmode() -> R<bool> {
    let mut m: u32 = 0;
    call(SYS_CAP_GETMODE, [&mut m as *mut u32 as usize, 0, 0, 0, 0, 0]).map(|_| m != 0)
}

/// Set the thread pointer of the calling thread.
pub unsafe fn set_tp(v: usize) -> R<()> {
    #[cfg(target_arch = "x86_64")]
    {
        // AMD64_SET_FSBASE
        let base = v;
        call(SYS_SYSARCH, [129, &base as *const usize as usize, 0, 0, 0, 0]).map(|_| ())
    }
    #[cfg(target_arch = "aarch64")]
    {
        super::arch::set_tp_reg(v);
        Ok(())
    }
}

// ---- threads -------------------------------------------------------------------------

#[repr(C)]
struct ThrParam {
    start_func: extern "C" fn(usize),
    arg: usize,
    stack_base: usize,
    stack_size: usize,
    tls_base: usize,
    tls_size: usize,
    child_tid: *mut i64,
    parent_tid: *mut i64,
    flags: i32,
    rtp: usize,
    spare: [usize; 3],
}

/// The word a joiner waits on: the kernel's `thr_exit(&state)` stores 1 and wakes it.
pub const DONE_INIT: u32 = 0;
pub fn thread_done(word: u32) -> bool {
    word != 0
}

/// Start a thread running `entry(arg)` on `[stack, stack+size)` with thread pointer `tp`.
/// `entry` must end with [`thread_exit`] on the same `done` word.
pub unsafe fn thread_new(entry: extern "C" fn(usize), arg: usize, stack: usize, size: usize, tp: usize, done: *mut u32) -> R<()> {
    let _ = done;
    let mut tid: i64 = 0;
    let p = ThrParam {
        start_func: entry,
        arg,
        stack_base: stack,
        stack_size: size,
        tls_base: tp,
        tls_size: 0,
        child_tid: &mut tid,
        parent_tid: &mut tid,
        flags: 0,
        rtp: 0,
        spare: [0; 3],
    };
    call(SYS_THR_NEW, [&p as *const ThrParam as usize, core::mem::size_of::<ThrParam>(), 0, 0, 0, 0]).map(|_| ())
}

/// Exit the calling thread. The kernel writes 1 to `*done` (as a `long`) and wakes waiters.
pub unsafe fn thread_exit(done: *mut u32) -> ! {
    let _ = call(SYS_THR_EXIT, [done as usize, 0, 0, 0, 0, 0]);
    loop {}
}

const UMTX_OP_WAKE: usize = 3;
const UMTX_OP_WAIT_UINT: usize = 11;

/// Sleep while `*addr == val` (process-shared key, which is what thr_exit wakes).
pub unsafe fn futex_wait(addr: *const u32, val: u32) {
    let _ = call(SYS_UMTX_OP, [addr as usize, UMTX_OP_WAIT_UINT, val as usize, 0, 0, 0]);
}
pub unsafe fn futex_wake(addr: *const u32, n: u32) {
    let _ = call(SYS_UMTX_OP, [addr as usize, UMTX_OP_WAKE, n as usize, 0, 0, 0]);
}

/// Linux-shaped HWCAP for ifunc resolvers. FreeBSD arm64 uses the Linux bit values.
pub fn linux_hwcap(auxv: &[(usize, usize)]) -> (usize, usize) {
    let get = |k| auxv.iter().find(|e| e.0 == k).map_or(0, |e| e.1);
    (get(AT_HWCAP), get(AT_HWCAP2))
}

/// On FreeBSD the kernel passes a pointer to argc in the first argument register; the
/// stack pointer is not at argc on amd64.
pub const ENTRY_ARG_IS_FIRST_REG: bool = true;
