//! Process and system facts, memory mapping, clocks and the generic `syscall()` (H4:
//! Widevine's imports). The library sees a Linux machine: `uname` says Linux, the
//! auxiliary vector is the Linux-shaped one we computed, and every number it passes is a
//! Linux number we translate.
use crate::{abi, set_errno};
use core::ffi::c_void;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use hospes_rt::sys;

static HWCAP: AtomicUsize = AtomicUsize::new(0);
static HWCAP2: AtomicUsize = AtomicUsize::new(0);
/// 16 random bytes for `AT_RANDOM`, drawn at init.
static mut AT_RANDOM_BYTES: [u8; 16] = [0; 16];

/// glibc's `__libc_stack_end`: where the main thread's stack starts (its highest used
/// address at entry).
pub static mut __libc_stack_end: usize = 0;

pub(crate) fn init(hwcap: (usize, usize), stack_end: usize) {
    HWCAP.store(hwcap.0, Ordering::Relaxed);
    HWCAP2.store(hwcap.1, Ordering::Relaxed);
    unsafe {
        let _ = sys::getrandom(&mut *core::ptr::addr_of_mut!(AT_RANDOM_BYTES));
        __libc_stack_end = stack_end;
    }
}

pub extern "C" fn _exit(code: i32) -> ! {
    sys::exit(code)
}

/// The library's environment is empty: it is not given ours (spec §6.3).
pub extern "C" fn getenv(_name: *const u8) -> *const u8 {
    core::ptr::null()
}

pub extern "C" fn geteuid() -> u32 {
    sys::geteuid()
}

pub extern "C" fn getpagesize() -> i32 {
    sys::PAGE as i32
}

pub extern "C" fn sysconf(name: i32) -> i64 {
    match name {
        2 => 100,                    // _SC_CLK_TCK
        4 => 1024,                   // _SC_OPEN_MAX
        30 => sys::PAGE as i64,      // _SC_PAGESIZE
        83 | 84 => 1,                // _SC_NPROCESSORS_CONF / _ONLN: one CPU (misc.rs)
        _ => {
            set_errno(abi::EINVAL);
            -1
        }
    }
}

/// `struct utsname`: six 65-byte fields.
#[repr(C)]
pub struct Utsname {
    f: [[u8; 65]; 6],
}

fn put(dst: &mut [u8; 65], s: &[u8]) {
    dst.fill(0);
    dst[..s.len()].copy_from_slice(s);
}

/// A Linux machine, as the library expects (it never runs anywhere else). The release is
/// a current LTS kernel; nothing in it identifies the real host.
pub unsafe extern "C" fn uname(u: *mut Utsname) -> i32 {
    let u = &mut *u;
    put(&mut u.f[0], b"Linux");
    put(&mut u.f[1], b"localhost");
    put(&mut u.f[2], b"6.8.0");
    put(&mut u.f[3], b"#1 SMP PREEMPT_DYNAMIC");
    put(&mut u.f[4], if cfg!(target_arch = "x86_64") { b"x86_64" } else { b"aarch64" });
    put(&mut u.f[5], b"(none)");
    0
}

#[repr(C)]
pub struct Rlimit {
    cur: u64,
    max: u64,
}
const RLIM_INFINITY: u64 = u64::MAX;

pub unsafe extern "C" fn getrlimit64(res: i32, r: *mut Rlimit) -> i32 {
    let (cur, max) = match res {
        3 => (crate::pthread::STACK as u64, RLIM_INFINITY), // RLIMIT_STACK
        7 => (1024, 4096),                                  // RLIMIT_NOFILE
        0..=15 => (RLIM_INFINITY, RLIM_INFINITY),
        _ => {
            set_errno(abi::EINVAL);
            return -1;
        }
    };
    *r = Rlimit { cur, max };
    0
}

const PR_SET_NAME: i32 = 15;
const PR_GET_NAME: i32 = 16;
const PR_SET_VMA: i32 = 0x5356_4d41;

/// `prctl`: naming a thread or a mapping is accepted and has no effect (H0: the CDM names
/// its anonymous mappings with `PR_SET_VMA`); anything else is `EINVAL`, as on a kernel
/// without the option.
pub unsafe extern "C" fn prctl(op: i32, a2: usize, _a3: usize, _a4: usize, _a5: usize) -> i32 {
    match op {
        PR_SET_NAME | PR_SET_VMA => 0,
        PR_GET_NAME => {
            core::ptr::copy_nonoverlapping(b"hospes\0".as_ptr(), a2 as *mut u8, 7);
            0
        }
        _ => {
            set_errno(abi::EINVAL);
            -1
        }
    }
}

/// Scheduling requests are accepted and ignored (the host decides priorities).
pub extern "C" fn setpriority(_which: i32, _who: u32, _prio: i32) -> i32 {
    0
}
pub extern "C" fn sched_setscheduler(_pid: i32, _policy: i32, _param: *const c_void) -> i32 {
    set_errno(abi::EPERM);
    -1
}
pub extern "C" fn sched_yield() -> i32 {
    sys::sched_yield();
    0
}

const AT_PAGESZ: usize = 6;
const AT_HWCAP: usize = 16;
const AT_CLKTCK: usize = 17;
const AT_SECURE: usize = 23;
const AT_RANDOM: usize = 25;
const AT_HWCAP2: usize = 26;

pub extern "C" fn getauxval(t: usize) -> usize {
    match t {
        AT_PAGESZ => sys::PAGE,
        AT_HWCAP => HWCAP.load(Ordering::Relaxed),
        AT_HWCAP2 => HWCAP2.load(Ordering::Relaxed),
        AT_CLKTCK => 100,
        AT_SECURE => 0,
        AT_RANDOM => core::ptr::addr_of!(AT_RANDOM_BYTES) as usize,
        _ => {
            set_errno(abi::ENOENT);
            0
        }
    }
}

/// `pthread_atfork`'s glibc entry. Hospes never forks, so handlers never run.
pub extern "C" fn __register_atfork(_pre: usize, _parent: usize, _child: usize, _dso: usize) -> i32 {
    0
}

/// Exit-time destructors: the library lives until the process ends, and the host exits
/// without running library code (the peer sees the channel close), so none are kept.
pub extern "C" fn __cxa_atexit(_f: usize, _arg: usize, _dso: usize) -> i32 {
    0
}

// ---- dynamic linking: there is one library and no dlopen -------------------------------

static DLSYM_LOGGED: AtomicU64 = AtomicU64::new(0);

/// `dlsym`: nothing is found. Logged (the first 64 distinct calls by name hash), so a
/// library that depends on a lookup shows it.
pub unsafe extern "C" fn dlsym(_h: *mut c_void, name: *const u8) -> *const c_void {
    if !name.is_null() {
        let n = core::slice::from_raw_parts(name, crate::mem::strlen(name).min(128));
        let h = n.iter().fold(0u32, |a, &b| a.wrapping_mul(31).wrapping_add(b as u32));
        let bit = 1u64 << (h % 64);
        if DLSYM_LOGGED.fetch_or(bit, Ordering::Relaxed) & bit == 0 {
            hospes_rt::hlog!("hospes: dlsym({}) -> not found", core::str::from_utf8(n).unwrap_or("?"));
        }
    }
    core::ptr::null()
}
pub extern "C" fn dlclose(_h: *mut c_void) -> i32 {
    0
}

// ---- memory mapping ------------------------------------------------------------------

// Linux flag values (same on both machines).
const L_MAP_SHARED: i32 = 0x01;
const L_MAP_PRIVATE: i32 = 0x02;
const L_MAP_FIXED: i32 = 0x10;
const L_MAP_ANONYMOUS: i32 = 0x20;
const L_MAP_KNOWN: i32 = 0x03 | 0x10 | 0x20 | 0x4000 /* NORESERVE */ | 0x8000 /* POPULATE */ | 0x100000 /* FIXED_NOREPLACE */ | 0x0100 /* GROWSDOWN */;
const L_MAP_FIXED_NOREPLACE: i32 = 0x100000;
const PROT_EXEC: i32 = 4;

/// Anonymous private or shared memory only (there are no files to map), never executable:
/// H0 saw no executable mapping from the CDM, and code written at run time would break
/// the W^X rule the loader keeps (spec §13 R3). Refused requests are logged.
pub unsafe extern "C" fn mmap64(addr: *mut c_void, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut c_void {
    const FAILED: *mut c_void = usize::MAX as *mut c_void;
    if prot & PROT_EXEC != 0 || flags & L_MAP_ANONYMOUS == 0 || flags & !L_MAP_KNOWN != 0 {
        hospes_rt::hlog!("hospes: mmap(prot {:#x}, flags {:#x}, fd {}) refused", prot, flags, fd);
        set_errno(if prot & PROT_EXEC != 0 { abi::EPERM } else { abi::EINVAL });
        return FAILED;
    }
    let _ = off;
    let mut f = if flags & L_MAP_SHARED != 0 { sys::MAP_SHARED } else { sys::MAP_PRIVATE };
    f |= sys::MAP_ANON;
    let fixed = flags & L_MAP_FIXED != 0;
    if fixed {
        f |= sys::MAP_FIXED;
    }
    if flags & (L_MAP_SHARED | L_MAP_PRIVATE) == 0 {
        set_errno(abi::EINVAL);
        return FAILED;
    }
    match sys::mmap(addr as usize, len, (prot & 3) as usize, f, -1, 0) {
        Ok(p) => {
            if flags & L_MAP_FIXED_NOREPLACE != 0 && !addr.is_null() && p != addr as usize {
                let _ = sys::munmap(p, len);
                set_errno(abi::EEXIST);
                return FAILED;
            }
            p as *mut c_void
        }
        Err(_) => {
            set_errno(abi::ENOMEM);
            FAILED
        }
    }
}

pub unsafe extern "C" fn munmap(addr: *mut c_void, len: usize) -> i32 {
    match sys::munmap(addr as usize, len) {
        Ok(()) => 0,
        Err(_) => {
            set_errno(abi::EINVAL);
            -1
        }
    }
}

/// The library's own executable segments, `[start, end)` (set at load).
static TEXT: crate::Spans = crate::Spans::new();

pub(crate) fn set_text(start: usize, end: usize) {
    TEXT.add(start, end);
}

/// Executable only inside the library's own text, and never writable and executable at
/// once (see `mmap64`): re-protecting its own code read-execute is allowed, making new code
/// is not.
pub unsafe extern "C" fn mprotect(addr: *mut c_void, len: usize, prot: i32) -> i32 {
    if prot & PROT_EXEC != 0 && (prot & 2 != 0 || !TEXT.covers(addr as usize, len)) {
        hospes_rt::hlog!("hospes: mprotect({:#x}, {:#x}, {:#x}) refused: executable outside the library's text", addr as usize, len, prot);
        set_errno(abi::EACCES);
        return -1;
    }
    match sys::mprotect(addr as usize, len, (prot & 7) as usize) {
        Ok(()) => 0,
        Err(_) => {
            set_errno(abi::ENOMEM);
            -1
        }
    }
}

/// Linux advice to FreeBSD's: DONTNEED and FREE release pages, DONTDUMP excludes from core
/// dumps; the rest are hints and succeed.
pub unsafe extern "C" fn madvise(addr: *mut c_void, len: usize, advice: i32) -> i32 {
    let f = match (advice, sys::OS) {
        (4, _) => 4,             // MADV_DONTNEED
        (8, "freebsd") => 5,     // MADV_FREE
        (8, _) => 8,
        (16, "freebsd") => 8,    // MADV_DONTDUMP -> MADV_NOCORE
        (16, _) => 16,
        (17, "freebsd") => 9,    // MADV_DODUMP -> MADV_CORE
        (17, _) => 17,
        _ => return 0,
    };
    let _ = sys::madvise(addr as usize, len, f);
    0
}

// ---- time ----------------------------------------------------------------------------

#[repr(C)]
pub struct Timespec {
    pub sec: i64,
    pub nsec: i64,
}

pub unsafe extern "C" fn clock_gettime(id: i32, ts: *mut Timespec) -> i32 {
    match sys::clock_gettime_linux(id) {
        Ok((s, ns)) => {
            *ts = Timespec { sec: s, nsec: ns };
            0
        }
        Err(_) => {
            set_errno(abi::EINVAL);
            -1
        }
    }
}

pub unsafe extern "C" fn nanosleep(req: *const Timespec, rem: *mut Timespec) -> i32 {
    let r = &*req;
    if r.nsec < 0 || r.nsec >= 1_000_000_000 || r.sec < 0 {
        set_errno(abi::EINVAL);
        return -1;
    }
    sys::nanosleep(r.sec, r.nsec);
    if !rem.is_null() {
        *rem = Timespec { sec: 0, nsec: 0 };
    }
    0
}

// ---- signals: the library's view only ------------------------------------------------

/// glibc's `sigset_t` is 128 bytes.
pub unsafe extern "C" fn sigfillset(set: *mut u8) -> i32 {
    core::ptr::write_bytes(set, 0xff, 128);
    0
}

/// The library may block signals for its threads; Hospes delivers none to it but the
/// trap's synchronous `SIGSYS`, which cannot be blocked usefully. Accepted, no effect.
pub unsafe extern "C" fn pthread_sigmask(_how: i32, _set: *const u8, old: *mut u8) -> i32 {
    if !old.is_null() {
        core::ptr::write_bytes(old, 0, 128);
    }
    0
}

// ---- syscall(): the Linux numbers glibc callers use -----------------------------------

#[cfg(target_arch = "x86_64")]
mod nr {
    pub const GETTID: i64 = 186;
    pub const GETRANDOM: i64 = 318;
}
#[cfg(target_arch = "aarch64")]
mod nr {
    pub const GETTID: i64 = 178;
    pub const GETRANDOM: i64 = 278;
}

static SYSCALL_LOGGED: AtomicU64 = AtomicU64::new(0);

/// `syscall(2)` through glibc (H0: `gettid` ×15, `getrandom` ×10). Others are `ENOSYS`,
/// logged once per number.
pub unsafe extern "C" fn syscall(n: i64, mut args: ...) -> i64 {
    match n {
        nr::GETTID => sys::gettid() as i32 as i64,
        nr::GETRANDOM => {
            let p: *mut u8 = args.next_arg::<usize>() as *mut u8;
            let len: usize = args.next_arg::<usize>();
            if p.is_null() && len != 0 {
                set_errno(abi::EFAULT);
                return -1;
            }
            let b = core::slice::from_raw_parts_mut(p, len.min(1 << 25));
            match sys::getrandom(b) {
                Ok(k) => k as i64,
                Err(_) => {
                    set_errno(abi::EIO);
                    -1
                }
            }
        }
        _ => {
            let bit = 1u64 << (n as u64 % 64);
            if SYSCALL_LOGGED.fetch_or(bit, Ordering::Relaxed) & bit == 0 {
                hospes_rt::hlog!("hospes: syscall({}) through glibc: not supported (ENOSYS)", n);
            }
            set_errno(abi::ENOSYS);
            -1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping_rules() {
        unsafe {
            let p = mmap64(core::ptr::null_mut(), 8192, 3, L_MAP_PRIVATE | L_MAP_ANONYMOUS, -1, 0);
            assert_ne!(p as usize, usize::MAX);
            *(p as *mut u8) = 7;
            assert_eq!(madvise(p, 8192, 4), 0);
            assert_eq!(mprotect(p, 8192, 1), 0);
            assert_eq!(mprotect(p, 8192, 5), -1);
            assert_eq!(munmap(p, 8192), 0);
            assert_eq!(mmap64(core::ptr::null_mut(), 4096, 7, L_MAP_PRIVATE | L_MAP_ANONYMOUS, -1, 0) as usize, usize::MAX);
            assert_eq!(mmap64(core::ptr::null_mut(), 4096, 1, L_MAP_PRIVATE, 3, 0) as usize, usize::MAX);
        }
    }

    #[test]
    fn facts() {
        let mut u = Utsname { f: [[0; 65]; 6] };
        unsafe { uname(&mut u) };
        assert_eq!(&u.f[0][..6], b"Linux\0");
        assert_eq!(sysconf(30), 4096);
        assert_eq!(getauxval(AT_PAGESZ), 4096);
        assert!(getenv(b"HOME\0".as_ptr()).is_null());
        let mut b = [0u8; 16];
        unsafe {
            assert_eq!(syscall(nr::GETRANDOM, b.as_mut_ptr(), 16usize, 0u32), 16);
            assert!(syscall(nr::GETTID) > 0);
            assert_eq!(syscall(9999), -1);
            let mut ts = Timespec { sec: 0, nsec: 0 };
            assert_eq!(clock_gettime(1, &mut ts), 0);
            assert!(ts.sec > 0 || ts.nsec > 0);
        }
    }
}
