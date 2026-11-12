//! Linux system calls: development backend only (native and qemu-user testing). There is
//! no capability mode here; `cap_enter` is a no-op that says so.
use super::arch::sc;
pub use super::arch::tp;
use super::{Errno, R};

pub const OS: &str = "linux";

#[cfg(target_arch = "x86_64")]
mod nr {
    pub const READ: usize = 0;
    pub const WRITE: usize = 1;
    pub const CLOSE: usize = 3;
    pub const LSEEK: usize = 8;
    pub const MMAP: usize = 9;
    pub const MPROTECT: usize = 10;
    pub const MUNMAP: usize = 11;
    pub const SCHED_YIELD: usize = 24;
    pub const MADVISE: usize = 28;
    pub const GETPID: usize = 39;
    pub const EXIT: usize = 60;
    pub const ARCH_PRCTL: usize = 158;
    pub const GETTID: usize = 186;
    pub const FUTEX: usize = 202;
    pub const CLOCK_GETTIME: usize = 228;
    pub const EXIT_GROUP: usize = 231;
    pub const OPENAT: usize = 257;
    pub const GETRANDOM: usize = 318;
    pub const NANOSLEEP: usize = 35;
}
#[cfg(target_arch = "aarch64")]
mod nr {
    pub const OPENAT: usize = 56;
    pub const CLOSE: usize = 57;
    pub const LSEEK: usize = 62;
    pub const READ: usize = 63;
    pub const WRITE: usize = 64;
    pub const EXIT: usize = 93;
    pub const EXIT_GROUP: usize = 94;
    pub const FUTEX: usize = 98;
    pub const CLOCK_GETTIME: usize = 113;
    pub const SCHED_YIELD: usize = 124;
    pub const GETPID: usize = 172;
    pub const GETTID: usize = 178;
    pub const MUNMAP: usize = 215;
    pub const MMAP: usize = 222;
    pub const MPROTECT: usize = 226;
    pub const MADVISE: usize = 233;
    pub const GETRANDOM: usize = 278;
    pub const NANOSLEEP: usize = 101;
}
pub const SYS_EXIT: usize = nr::EXIT;
const SYS_CLONE: usize = if cfg!(target_arch = "x86_64") { 56 } else { 220 };

pub const MAP_SHARED: usize = 0x1;
pub const MAP_PRIVATE: usize = 0x2;
pub const MAP_FIXED: usize = 0x10;
pub const MAP_ANON: usize = 0x20;
pub const O_RDONLY: usize = 0;
pub const O_CLOEXEC: usize = 0x80000;
pub const AT_FDCWD: isize = -100;
pub const ECAPMODE: i32 = -1; // no such thing
pub const ENOSYS: i32 = 38;
pub const EINTR: i32 = 4;
pub const EAGAIN: i32 = 11;

pub const AT_NULL: usize = 0;
pub const AT_PAGESZ: usize = 6;
pub const AT_HWCAP: usize = 16;
pub const AT_HWCAP2: usize = 26;

#[inline]
fn call(n: usize, a: [usize; 6]) -> R<usize> {
    let (v, _) = unsafe { sc(n, a) };
    if (v as isize) < 0 && (v as isize) > -4096 {
        Err(Errno(-(v as isize) as i32))
    } else {
        Ok(v)
    }
}

pub fn exit(code: i32) -> ! {
    let _ = call(nr::EXIT_GROUP, [code as usize, 0, 0, 0, 0, 0]);
    loop {}
}
pub fn read(fd: i32, b: &mut [u8]) -> R<usize> {
    call(nr::READ, [fd as usize, b.as_mut_ptr() as usize, b.len(), 0, 0, 0])
}
pub fn write(fd: i32, b: &[u8]) -> R<usize> {
    call(nr::WRITE, [fd as usize, b.as_ptr() as usize, b.len(), 0, 0, 0])
}
pub fn close(fd: i32) -> R<()> {
    call(nr::CLOSE, [fd as usize, 0, 0, 0, 0, 0]).map(|_| ())
}
pub fn getpid() -> i32 {
    call(nr::GETPID, [0; 6]).unwrap_or(0) as i32
}
pub fn openat(dirfd: isize, path: &[u8], flags: usize) -> R<i32> {
    debug_assert!(path.last() == Some(&0));
    call(nr::OPENAT, [dirfd as usize, path.as_ptr() as usize, flags, 0, 0, 0]).map(|v| v as i32)
}
pub fn lseek(fd: i32, off: i64, whence: i32) -> R<u64> {
    call(nr::LSEEK, [fd as usize, off as usize, whence as usize, 0, 0, 0]).map(|v| v as u64)
}
pub fn mmap(addr: usize, len: usize, prot: usize, flags: usize, fd: i32, off: u64) -> R<usize> {
    call(nr::MMAP, [addr, len, prot, flags, fd as isize as usize, off as usize])
}
pub fn munmap(addr: usize, len: usize) -> R<()> {
    call(nr::MUNMAP, [addr, len, 0, 0, 0, 0]).map(|_| ())
}
pub fn mprotect(addr: usize, len: usize, prot: usize) -> R<()> {
    call(nr::MPROTECT, [addr, len, prot, 0, 0, 0]).map(|_| ())
}
pub fn madvise(addr: usize, len: usize, advice: usize) -> R<()> {
    call(nr::MADVISE, [addr, len, advice, 0, 0, 0]).map(|_| ())
}
pub fn sched_yield() {
    let _ = call(nr::SCHED_YIELD, [0; 6]);
}
pub fn getrandom(b: &mut [u8]) -> R<usize> {
    call(nr::GETRANDOM, [b.as_mut_ptr() as usize, b.len(), 0, 0, 0, 0])
}
pub fn monotonic_ns() -> u64 {
    let mut ts = [0i64; 2];
    let _ = call(nr::CLOCK_GETTIME, [1, ts.as_mut_ptr() as usize, 0, 0, 0, 0]);
    ts[0] as u64 * 1_000_000_000 + ts[1] as u64
}
pub fn gettid() -> i64 {
    call(nr::GETTID, [0; 6]).unwrap_or(0) as i64
}
pub fn cap_enter() -> R<()> {
    Ok(())
}
pub fn cap_getmode() -> R<bool> {
    Ok(false)
}

pub unsafe fn set_tp(v: usize) -> R<()> {
    #[cfg(target_arch = "x86_64")]
    {
        call(nr::ARCH_PRCTL, [0x1002, v, 0, 0, 0, 0]).map(|_| ())
    }
    #[cfg(target_arch = "aarch64")]
    {
        super::arch::set_tp_reg(v);
        Ok(())
    }
}

// ---- threads -------------------------------------------------------------------------

// CLONE_VM|FS|FILES|SIGHAND|THREAD|SYSVSEM|SETTLS|PARENT_SETTID|CHILD_CLEARTID
const CLONE_FLAGS: usize = 0x100 | 0x200 | 0x400 | 0x800 | 0x10000 | 0x40000 | 0x80000 | 0x100000 | 0x200000;

/// The kernel stores the tid on start (PARENT_SETTID) and clears it, with a futex wake,
/// once the thread is gone (CHILD_CLEARTID).
pub const DONE_INIT: u32 = u32::MAX;
pub fn thread_done(word: u32) -> bool {
    word == 0
}

extern "C" {
    fn hospes_linux_clone(flags: usize, stack: usize, ptid: *mut u32, ctid: *mut u32, tls: usize, f: extern "C" fn(usize), arg: usize) -> isize;
}

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".globl hospes_linux_clone",
    ".type hospes_linux_clone,@function",
    "hospes_linux_clone:",
    // rdi flags, rsi stack top, rdx ptid, rcx ctid, r8 tls, r9 fn, [rsp+8] arg
    "and rsi, -16",
    "sub rsi, 16",
    "mov [rsi], r9",
    "mov rax, [rsp+8]",
    "mov [rsi+8], rax",
    "mov r10, rcx",
    "mov eax, 56",
    "syscall",
    "test rax, rax",
    "jnz 1f",
    "xor ebp, ebp",
    "pop rax",
    "pop rdi",
    "call rax",
    "ud2",
    "1: ret",
);

#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(
    ".globl hospes_linux_clone",
    ".type hospes_linux_clone,%function",
    "hospes_linux_clone:",
    // x0 flags, x1 stack top, x2 ptid, x3 ctid, x4 tls, x5 fn, x6 arg
    "and x1, x1, #-16",
    "stp x5, x6, [x1, #-16]!",
    "mov x9, x3",
    "mov x3, x4",
    "mov x4, x9",
    "mov x8, #220",
    "svc #0",
    "cbnz x0, 1f",
    "ldp x9, x0, [sp], #16",
    "mov x29, #0",
    "mov x30, #0",
    "blr x9",
    "brk #1",
    "1: ret",
);

pub unsafe fn thread_new(entry: extern "C" fn(usize), arg: usize, stack: usize, size: usize, tp: usize, done: *mut u32) -> R<()> {
    let _ = SYS_CLONE;
    let r = hospes_linux_clone(CLONE_FLAGS, stack + size, done, done, tp, entry, arg);
    if r < 0 {
        Err(Errno(-r as i32))
    } else {
        Ok(())
    }
}

/// Exit the calling thread only (the kernel clears `*done` and wakes the joiner).
pub unsafe fn thread_exit(_done: *mut u32) -> ! {
    let _ = call(nr::EXIT, [0; 6]);
    loop {}
}

pub unsafe fn futex_wait(addr: *const u32, val: u32) {
    let _ = call(nr::FUTEX, [addr as usize, 0, val as usize, 0, 0, 0]);
}
pub unsafe fn futex_wake(addr: *const u32, n: u32) {
    let _ = call(nr::FUTEX, [addr as usize, 1, n as usize, 0, 0, 0]);
}

pub fn linux_hwcap(auxv: &[(usize, usize)]) -> (usize, usize) {
    let get = |k| auxv.iter().find(|e| e.0 == k).map_or(0, |e| e.1);
    (get(AT_HWCAP), get(AT_HWCAP2))
}

/// On Linux the stack pointer points at argc at entry.
pub const ENTRY_ARG_IS_FIRST_REG: bool = false;

pub fn realtime() -> (i64, i64) {
    let mut ts = [0i64; 2];
    let _ = call(nr::CLOCK_GETTIME, [0, ts.as_mut_ptr() as usize, 0, 0, 0, 0]);
    (ts[0], ts[1])
}

pub fn nanosleep(sec: i64, nsec: i64) {
    let ts = [sec, nsec];
    let _ = call(nr::NANOSLEEP, [ts.as_ptr() as usize, 0, 0, 0, 0, 0]);
}

/// FUTEX_WAIT_BITSET | FUTEX_CLOCK_REALTIME with an absolute deadline. True if it timed out.
pub unsafe fn futex_wait_until_realtime(addr: *const u32, val: u32, sec: i64, nsec: i64) -> bool {
    let ts = [sec, nsec];
    let r = call(nr::FUTEX, [addr as usize, 9 | 256, val as usize, ts.as_ptr() as usize, 0, 0xffff_ffff]);
    r == Err(Errno(110))
}

/// Create or truncate `path` (NUL-terminated) for writing, mode 0644.
pub fn create(path: &[u8]) -> R<i32> {
    debug_assert!(path.last() == Some(&0));
    // O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC
    call(nr::OPENAT, [AT_FDCWD as usize, path.as_ptr() as usize, 0x1 | 0x40 | 0x200 | O_CLOEXEC, 0o644, 0, 0]).map(|v| v as i32)
}

const SYS_PPOLL: usize = if cfg!(target_arch = "x86_64") { 271 } else { 73 };
const SYS_RENAMEAT: usize = if cfg!(target_arch = "x86_64") { 264 } else { 38 };
const SYS_UNLINKAT: usize = if cfg!(target_arch = "x86_64") { 263 } else { 35 };
const O_WRONLY: usize = 0x1;
const O_CREAT: usize = 0x40;
const O_TRUNC: usize = 0x200;
const O_NOFOLLOW: usize = if cfg!(target_arch = "x86_64") { 0x20000 } else { 0x8000 };
pub const ENOENT: i32 = 2;

/// Open `name` (NUL-terminated, one path component) beneath `dirfd`: for reading, or
/// created/truncated for writing (mode 0600). Never follows a symbolic link.
pub fn open_beneath(dirfd: i32, name: &[u8], write: bool) -> R<i32> {
    debug_assert!(name.last() == Some(&0));
    let f = if write { O_WRONLY | O_CREAT | O_TRUNC } else { O_RDONLY };
    call(nr::OPENAT, [dirfd as usize, name.as_ptr() as usize, f | O_NOFOLLOW | O_CLOEXEC, 0o600, 0, 0]).map(|v| v as i32)
}

/// `renameat(dirfd, from, dirfd, to)`; both NUL-terminated.
pub fn rename_beneath(dirfd: i32, from: &[u8], to: &[u8]) -> R<()> {
    call(SYS_RENAMEAT, [dirfd as usize, from.as_ptr() as usize, dirfd as usize, to.as_ptr() as usize, 0, 0]).map(|_| ())
}

/// `unlinkat(dirfd, name, 0)`; NUL-terminated.
pub fn unlink_beneath(dirfd: i32, name: &[u8]) -> R<()> {
    call(SYS_UNLINKAT, [dirfd as usize, name.as_ptr() as usize, 0, 0, 0, 0]).map(|_| ())
}

/// `poll(2)` through `ppoll`; `timeout_ms < 0` waits forever.
pub fn poll(fds: &mut [super::PollFd], timeout_ms: i32) -> R<usize> {
    let ts = [(timeout_ms / 1000) as i64, ((timeout_ms % 1000) as i64) * 1_000_000];
    let tp = if timeout_ms < 0 { 0 } else { ts.as_ptr() as usize };
    call(SYS_PPOLL, [fds.as_mut_ptr() as usize, fds.len(), tp, 0, 8, 0])
}

/// The current disposition of `sig`: `(handler, flags, mask)`, Linux `struct
/// kernel_sigaction` terms (the mask's low 64 bits in the first two words).
pub fn sigaction_query(sig: i32) -> R<(usize, u32, [u32; 4])> {
    #[repr(C)]
    struct KSigAction {
        handler: usize,
        flags: usize,
        restorer: usize,
        mask: u64,
    }
    let n = if cfg!(target_arch = "x86_64") { 13 } else { 134 };
    let mut o = KSigAction { handler: 0, flags: 0, restorer: 0, mask: 0 };
    call(n, [sig as usize, 0, &mut o as *mut KSigAction as usize, 8, 0, 0])?;
    Ok((o.handler, o.flags as u32, [o.mask as u32, (o.mask >> 32) as u32, 0, 0]))
}

/// `clock_gettime` for a Linux clock id (the host's own numbering here).
pub fn clock_gettime_linux(id: i32) -> R<(i64, i64)> {
    let mut ts = [0i64; 2];
    call(nr::CLOCK_GETTIME, [id as usize, ts.as_mut_ptr() as usize, 0, 0, 0, 0])?;
    Ok((ts[0], ts[1]))
}

pub fn geteuid() -> u32 {
    let n = if cfg!(target_arch = "x86_64") { 107 } else { 175 };
    call(n, [0; 6]).unwrap_or(0) as u32
}
