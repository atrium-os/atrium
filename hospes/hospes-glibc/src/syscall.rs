//! Linux system calls made by the foreign library's own code (docs/spec/hospes.md §13 R1,
//! step K1). The kernel traps them ([`hospes_rt::trap`]); [`handle`] answers each from the
//! saved registers, with Linux numbers and Linux errno values, and never passes one to the
//! host kernel.
//!
//! - `openat`, `read`, `close`: the virtual filesystem ([`crate::vfs`]). Only virtual
//!   descriptors exist for raw calls; any other is `EBADF`.
//! - `rt_sigaction`: queries are answered truthfully from the host's real dispositions
//!   (our `SIGSYS` handler is visible, as a seccomp sandbox's is on Linux). Setting a handler
//!   is refused with `EPERM`: the library must not replace the trap's handler.
//! - anything else: `ENOSYS`, logged once per number.
use crate::{abi, vfs};
use core::sync::atomic::{AtomicU64, Ordering};
use hospes_rt::trap::Call;

#[cfg(target_arch = "x86_64")]
mod nr {
    pub const READ: usize = 0;
    pub const CLOSE: usize = 3;
    pub const RT_SIGACTION: usize = 13;
    pub const OPENAT: usize = 257;
}
#[cfg(target_arch = "aarch64")]
mod nr {
    pub const OPENAT: usize = 56;
    pub const CLOSE: usize = 57;
    pub const READ: usize = 63;
    pub const RT_SIGACTION: usize = 134;
}

const AT_FDCWD: i32 = -100;
const O_ACCMODE: usize = 3;
const O_CREAT: usize = 0o100;
const PATH_MAX: usize = 4096;

/// Numbers already logged as unsupported (the first 64 of each kind are enough to see).
static LOGGED: AtomicU64 = AtomicU64::new(0);

/// Answer one trapped call: the value Linux would return.
pub fn handle(c: &Call) -> isize {
    let a = c.args;
    let r: Result<usize, i32> = match c.nr {
        nr::OPENAT => openat(a[0] as i32, a[1] as *const u8, a[2]),
        nr::READ => read(a[0] as i32, a[1] as *mut u8, a[2]),
        nr::CLOSE => vfs::close(a[0] as i32).map(|_| 0),
        nr::RT_SIGACTION => rt_sigaction(a[0] as i32, a[1], a[2] as *mut KSigAction, a[3]),
        n => {
            let bit = 1u64 << (n % 64);
            if LOGGED.fetch_or(bit, Ordering::Relaxed) & bit == 0 {
                hospes_rt::hlog!("hospes: raw Linux system call {} from {:#x}: not supported (ENOSYS)", n, c.pc);
            }
            Err(abi::ENOSYS)
        }
    };
    match r {
        Ok(v) => v as isize,
        Err(e) => -(e as isize),
    }
}

/// The NUL-terminated path at `p`, at most `PATH_MAX` bytes.
unsafe fn path<'a>(p: *const u8) -> Result<&'a [u8], i32> {
    if p.is_null() {
        return Err(abi::EFAULT);
    }
    let mut n = 0;
    while *p.add(n) != 0 {
        n += 1;
        if n >= PATH_MAX {
            return Err(abi::ENAMETOOLONG);
        }
    }
    Ok(core::slice::from_raw_parts(p, n))
}

fn openat(dirfd: i32, p: *const u8, flags: usize) -> Result<usize, i32> {
    let path = unsafe { path(p)? };
    // Only absolute paths name anything; a relative one would need a directory we never
    // gave out.
    if path.first() != Some(&b'/') {
        return Err(if dirfd == AT_FDCWD || vfs::is_virtual(dirfd) { abi::ENOENT } else { abi::EBADF });
    }
    let write = flags & O_ACCMODE != 0 || flags & O_CREAT != 0;
    vfs::open(path, write).map(|fd| fd as usize)
}

fn read(fd: i32, buf: *mut u8, len: usize) -> Result<usize, i32> {
    if !vfs::is_virtual(fd) {
        return Err(abi::EBADF);
    }
    if buf.is_null() && len != 0 {
        return Err(abi::EFAULT);
    }
    let out = unsafe { core::slice::from_raw_parts_mut(buf, len.min(isize::MAX as usize)) };
    vfs::read(fd, out)
}

/// Linux `struct kernel_sigaction` (both machines define `SA_RESTORER`).
#[repr(C)]
pub struct KSigAction {
    handler: usize,
    flags: usize,
    restorer: usize,
    mask: u64,
}

const LINUX_SIGKILL: i32 = 9;
const LINUX_SIGSTOP: i32 = 19;

fn rt_sigaction(sig: i32, act: usize, old: *mut KSigAction, setsize: usize) -> Result<usize, i32> {
    if setsize != 8 || !(1..=64).contains(&sig) {
        return Err(abi::EINVAL);
    }
    if act != 0 {
        if sig == LINUX_SIGKILL || sig == LINUX_SIGSTOP {
            return Err(abi::EINVAL);
        }
        hospes_rt::hlog!("hospes: raw rt_sigaction({}) tried to set a handler: refused (EPERM)", sig);
        return Err(abi::EPERM);
    }
    if !old.is_null() {
        let v = query(sig);
        unsafe { old.write(v) };
    }
    Ok(0)
}

#[cfg(target_os = "linux")]
fn query(sig: i32) -> KSigAction {
    // The development backend's host is Linux: its answer is already in Linux terms.
    match hospes_rt::sys::sigaction_query(sig) {
        Ok((h, f, m)) => KSigAction { handler: h, flags: f as usize, restorer: 0, mask: m[0] as u64 | (m[1] as u64) << 32 },
        Err(_) => KSigAction { handler: 0, flags: 0, restorer: 0, mask: 0 },
    }
}

#[cfg(target_os = "freebsd")]
fn query(sig: i32) -> KSigAction {
    let Some(fsig) = to_freebsd(sig) else {
        // No FreeBSD counterpart (SIGSTKFLT, SIGPWR, real-time signals): never set, so the
        // default disposition.
        return KSigAction { handler: 0, flags: 0, restorer: 0, mask: 0 };
    };
    let Ok((h, f, m)) = hospes_rt::sys::sigaction_query(fsig) else {
        return KSigAction { handler: 0, flags: 0, restorer: 0, mask: 0 };
    };
    let mut mask = 0u64;
    for ls in 1..=64 {
        if let Some(fs) = to_freebsd(ls) {
            let b = (fs - 1) as usize;
            if m[b / 32] & (1 << (b % 32)) != 0 {
                mask |= 1 << (ls - 1);
            }
        }
    }
    KSigAction { handler: h, flags: flags_to_linux(f), restorer: 0, mask }
}

/// Linux signal number to FreeBSD's.
#[cfg(any(target_os = "freebsd", test))]
fn to_freebsd(s: i32) -> Option<i32> {
    Some(match s {
        1..=6 | 8 | 9 | 11 | 13..=15 | 21 | 22 | 24..=28 => s,
        7 => 10,  // SIGBUS
        10 => 30, // SIGUSR1
        12 => 31, // SIGUSR2
        17 => 20, // SIGCHLD
        18 => 19, // SIGCONT
        19 => 17, // SIGSTOP
        20 => 18, // SIGTSTP
        23 => 16, // SIGURG
        29 => 23, // SIGIO
        31 => 12, // SIGSYS
        _ => return None,
    })
}

#[cfg(any(target_os = "freebsd", test))]
fn flags_to_linux(f: u32) -> usize {
    const MAP: [(u32, usize); 7] = [
        (0x0001, 0x0800_0000), // SA_ONSTACK
        (0x0002, 0x1000_0000), // SA_RESTART
        (0x0004, 0x8000_0000), // SA_RESETHAND
        (0x0008, 0x0000_0001), // SA_NOCLDSTOP
        (0x0010, 0x4000_0000), // SA_NODEFER
        (0x0020, 0x0000_0002), // SA_NOCLDWAIT
        (0x0040, 0x0000_0004), // SA_SIGINFO
    ];
    MAP.iter().filter(|(b, _)| f & b != 0).fold(0, |acc, (_, l)| acc | l)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(n: usize, args: [usize; 6]) -> isize {
        handle(&Call { nr: n, args, pc: 0x1000 })
    }

    #[test]
    fn open_read_close_net_unix() {
        let p = b"/proc/net/unix\0";
        let fd = call(nr::OPENAT, [AT_FDCWD as isize as usize, p.as_ptr() as usize, 0, 0, 0, 0]);
        assert!(fd >= vfs::FD_BASE as isize, "{fd}");
        let mut b = [0u8; 200];
        let n = call(nr::READ, [fd as usize, b.as_mut_ptr() as usize, b.len(), 0, 0, 0]);
        assert!(b[..n as usize].starts_with(b"Num       RefCount"));
        assert_eq!(call(nr::CLOSE, [fd as usize, 0, 0, 0, 0, 0]), 0);
        assert_eq!(call(nr::CLOSE, [fd as usize, 0, 0, 0, 0, 0]), -(abi::EBADF as isize));
    }

    #[test]
    fn errors_are_linux_shaped() {
        let p = b"/nonexistent\0";
        assert_eq!(call(nr::OPENAT, [AT_FDCWD as isize as usize, p.as_ptr() as usize, 0, 0, 0, 0]), -2);
        let rel = b"maps\0";
        assert_eq!(call(nr::OPENAT, [AT_FDCWD as isize as usize, rel.as_ptr() as usize, 0, 0, 0, 0]), -2);
        assert_eq!(call(nr::OPENAT, [5, rel.as_ptr() as usize, 0, 0, 0, 0]), -9);
        let mut b = [0u8; 4];
        assert_eq!(call(nr::READ, [0, b.as_mut_ptr() as usize, 4, 0, 0, 0]), -9);
        assert_eq!(call(nr::CLOSE, [1, 0, 0, 0, 0, 0]), -9);
        assert_eq!(call(9999, [0; 6]), -38);
    }

    #[test]
    fn sigaction_set_refused_query_allowed() {
        let mut old = KSigAction { handler: 7, flags: 7, restorer: 7, mask: 7 };
        let act = KSigAction { handler: 1, flags: 0, restorer: 0, mask: 0 };
        let o = &mut old as *mut KSigAction as usize;
        assert_eq!(call(nr::RT_SIGACTION, [31, &act as *const KSigAction as usize, 0, 8, 0, 0]), -1);
        assert_eq!(call(nr::RT_SIGACTION, [4, 0, o, 8, 0, 0]), 0);
        assert_eq!(call(nr::RT_SIGACTION, [4, 0, o, 16, 0, 0]), -22);
        assert_eq!(call(nr::RT_SIGACTION, [65, 0, o, 8, 0, 0]), -22);
    }

    #[test]
    fn signal_and_flag_maps() {
        assert_eq!(to_freebsd(31), Some(12));
        assert_eq!(to_freebsd(4), Some(4));
        assert_eq!(to_freebsd(16), None);
        assert_eq!(to_freebsd(34), None);
        assert_eq!(flags_to_linux(0x40 | 0x2), 0x4 | 0x1000_0000);
    }
}
