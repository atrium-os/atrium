//! Raw system calls. One module per OS (numbers, flags, quirks) over one module per
//! architecture (the trap instruction). FreeBSD is the product; Linux is a development
//! backend so the loader can be tested natively and under qemu-user, and never gets
//! capability mode.

#[cfg(target_arch = "x86_64")]
#[path = "x86_64.rs"]
mod arch;
#[cfg(target_arch = "aarch64")]
#[path = "aarch64.rs"]
mod arch;

#[cfg(target_os = "freebsd")]
#[path = "freebsd.rs"]
mod os;
#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod os;

pub use os::*;

/// The raw trap instruction: `(value, error flag)`; the flag is FreeBSD's carry (on Linux
/// the error is a negative value instead).
pub use arch::sc as arch_sc;

/// A kernel error number, in the host OS's numbering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Errno(pub i32);

pub type R<T> = Result<T, Errno>;

pub const PROT_NONE: usize = 0;
pub const PROT_READ: usize = 1;
pub const PROT_WRITE: usize = 2;
pub const PROT_EXEC: usize = 4;

pub const PAGE: usize = 4096;

#[inline]
pub fn page_down(x: usize) -> usize {
    x & !(PAGE - 1)
}
#[inline]
pub fn page_up(x: usize) -> usize {
    (x + PAGE - 1) & !(PAGE - 1)
}

pub fn write_all(fd: i32, mut b: &[u8]) -> R<()> {
    while !b.is_empty() {
        let n = write(fd, b)?;
        if n == 0 {
            return Err(Errno(5));
        }
        b = &b[n..];
    }
    Ok(())
}

/// Map anonymous memory with `prot`.
pub fn mmap_anon(len: usize, prot: usize) -> R<usize> {
    mmap(0, len, prot, MAP_PRIVATE | MAP_ANON, -1, 0)
}

/// Map `len` bytes of anonymous memory aligned to `align` (a power of two >= PAGE).
pub fn mmap_aligned(len: usize, align: usize, prot: usize) -> R<usize> {
    let len = page_up(len);
    if align <= PAGE {
        return mmap_anon(len, prot);
    }
    let raw = mmap_anon(len + align, prot)?;
    let start = (raw + align - 1) & !(align - 1);
    if start > raw {
        munmap(raw, start - raw)?;
    }
    let tail = raw + len + align - (start + len);
    if tail > 0 {
        munmap(start + len, tail)?;
    }
    Ok(start)
}

/// `struct pollfd` (the same on FreeBSD and Linux).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct PollFd {
    pub fd: i32,
    pub events: i16,
    pub revents: i16,
}
pub const POLLIN: i16 = 0x1;
pub const POLLERR: i16 = 0x8;
pub const POLLHUP: i16 = 0x10;

/// Read exactly `b.len()` bytes; `Ok(false)` if the peer closed before the first byte.
pub fn read_exact(fd: i32, b: &mut [u8]) -> R<bool> {
    let mut o = 0;
    while o < b.len() {
        match read(fd, &mut b[o..]) {
            Ok(0) => return if o == 0 { Ok(false) } else { Err(Errno(EIO)) },
            Ok(n) => o += n,
            Err(Errno(e)) if e == EINTR => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}
const EIO: i32 = 5;
