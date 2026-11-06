//! Memory that holds clear secrets (arca.md §6.3).
//!
//! Every clear key and item value lives in a [`LockedBuf`]: its own
//! anonymous mapping, `mlock`ed so it never reaches swap or the zram
//! compressor, excluded from core dumps, and zeroized before it is
//! unmapped. One mapping per buffer keeps `munlock` from unlocking a page
//! another secret still uses. Long-lived buffers are few (the master key
//! and three class keys); data keys and values are unwrapped per use and
//! dropped at once.

use std::fmt;
use std::io;
use std::ptr::NonNull;

use zeroize::Zeroize;

fn page_size() -> usize {
    // SAFETY: sysconf has no preconditions.
    let p = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if p <= 0 { 4096 } else { p as usize }
}

/// A fixed-length buffer of secret bytes in locked, non-dumpable memory.
pub struct LockedBuf {
    ptr: NonNull<u8>,
    len: usize,
    map_len: usize,
}

// SAFETY: the mapping is owned exclusively by this value.
unsafe impl Send for LockedBuf {}
unsafe impl Sync for LockedBuf {}

impl LockedBuf {
    /// A zero-filled buffer of `len` bytes. Fails if the memory can't be
    /// locked: Arca refuses to hold a secret in pageable memory.
    pub fn zeroed(len: usize) -> io::Result<LockedBuf> {
        let ps = page_size();
        let map_len = len.max(1).div_ceil(ps) * ps;
        // SAFETY: anonymous private mapping, checked below.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                map_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: p..p+map_len is the mapping we just made.
        if unsafe { libc::mlock(p, map_len) } != 0 {
            let e = io::Error::last_os_error();
            unsafe { libc::munmap(p, map_len) };
            return Err(e);
        }
        #[cfg(target_os = "linux")]
        unsafe {
            libc::madvise(p, map_len, libc::MADV_DONTDUMP);
        }
        #[cfg(target_os = "freebsd")]
        unsafe {
            libc::madvise(p, map_len, libc::MADV_NOCORE);
        }
        Ok(LockedBuf { ptr: NonNull::new(p as *mut u8).expect("mmap non-null"), len, map_len })
    }

    /// Copy `src` into a new locked buffer. The caller zeroizes `src`.
    pub fn from_slice(src: &[u8]) -> io::Result<LockedBuf> {
        let mut b = LockedBuf::zeroed(src.len())?;
        b.as_mut().copy_from_slice(src);
        Ok(b)
    }

    /// A buffer of `len` random bytes, drawn straight into locked memory.
    pub fn random(len: usize) -> io::Result<LockedBuf> {
        let mut b = LockedBuf::zeroed(len)?;
        getrandom::getrandom(b.as_mut()).map_err(io::Error::other)?;
        Ok(b)
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Constant-time equality.
    pub fn ct_eq(&self, other: &[u8]) -> bool {
        ct_eq(self.as_ref(), other)
    }
}

impl AsRef<[u8]> for LockedBuf {
    fn as_ref(&self) -> &[u8] {
        // SAFETY: ptr..ptr+len is inside our live mapping.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl AsMut<[u8]> for LockedBuf {
    fn as_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, and we hold &mut self.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for LockedBuf {
    fn drop(&mut self) {
        // SAFETY: the whole mapping is ours; zeroize it, then release it.
        unsafe {
            std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.map_len).zeroize();
            libc::munlock(self.ptr.as_ptr() as *mut libc::c_void, self.map_len);
            libc::munmap(self.ptr.as_ptr() as *mut libc::c_void, self.map_len);
        }
    }
}

impl fmt::Debug for LockedBuf {
    // Never print a secret, not even in a debug log.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LockedBuf({} bytes, redacted)", self.len)
    }
}

/// Constant-time byte comparison (length is not secret).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut d = 0u8;
    for (x, y) in a.iter().zip(b) {
        d |= x ^ y;
    }
    // Keep the optimiser from short-circuiting the loop.
    std::hint::black_box(d) == 0
}

/// Harden the calling process for holding secrets: no core dumps, not
/// traceable or debuggable by its own uid (arca.md §6.3). Call before any
/// secret is loaded. Errors are fatal to the caller by design.
pub fn harden_process() -> io::Result<()> {
    let zero = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: plain syscalls on our own process.
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &zero) } != 0 {
        return Err(io::Error::last_os_error());
    }
    #[cfg(target_os = "linux")]
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    #[cfg(target_os = "freebsd")]
    {
        let mut v: libc::c_int = libc::PROC_TRACE_CTL_DISABLE;
        if unsafe {
            libc::procctl(libc::P_PID, 0, libc::PROC_TRACE_CTL, &mut v as *mut _ as *mut libc::c_void)
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locked_roundtrip_and_redacted_debug() {
        let b = LockedBuf::from_slice(b"hunter2").unwrap();
        assert_eq!(b.as_ref(), b"hunter2");
        assert!(b.ct_eq(b"hunter2"));
        assert!(!b.ct_eq(b"hunter3"));
        assert!(!format!("{b:?}").contains("hunter"));
    }

    #[test]
    fn random_is_not_zero() {
        let b = LockedBuf::random(32).unwrap();
        assert!(b.as_ref().iter().any(|&x| x != 0));
    }
}
