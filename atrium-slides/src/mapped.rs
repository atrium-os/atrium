//! A deck file mapped read-only (officina.md §9.3): opening a 2 GB deck reads
//! its index, not its bytes; slides and pictures are paged in as they are
//! drawn. The file must not shrink under the mapping, so Slides never writes
//! over the file a deck is mapped from: a save goes to a new file first
//! ([`crate::doc::Doc::spool`]) and the deck is mapped from that.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

pub struct Mapped {
    ptr: *mut libc::c_void,
    len: usize,
}

// SAFETY: the mapping is read-only and owned by this value until it drops.
unsafe impl Send for Mapped {}
// SAFETY: as above; shared references only read.
unsafe impl Sync for Mapped {}

impl Mapped {
    /// Maps all of `f` (which stays valid after `f` is closed).
    pub fn new(f: &File) -> io::Result<Mapped> {
        let len = f.metadata()?.len();
        let len = usize::try_from(len).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "file too large to map"))?;
        if len == 0 {
            return Ok(Mapped { ptr: std::ptr::null_mut(), len: 0 });
        }
        // SAFETY: a fresh read-only private mapping of a descriptor we hold.
        let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_PRIVATE, f.as_raw_fd(), 0) };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Mapped { ptr, len })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl AsRef<[u8]> for Mapped {
    fn as_ref(&self) -> &[u8] {
        if self.len == 0 {
            return &[];
        }
        // SAFETY: `ptr` maps `len` readable bytes for this value's life.
        unsafe { std::slice::from_raw_parts(self.ptr as *const u8, self.len) }
    }
}

impl Drop for Mapped {
    fn drop(&mut self) {
        if self.len > 0 {
            // SAFETY: unmapping exactly what `new` mapped.
            unsafe { libc::munmap(self.ptr, self.len) };
        }
    }
}

/// Copies all of `src` into `dst` from its start (`dst` truncated first),
/// then syncs `dst`.
pub fn copy_file(src: &File, dst: &mut File) -> io::Result<u64> {
    use std::io::{Read, Seek, SeekFrom, Write};
    let mut src = src.try_clone()?;
    src.seek(SeekFrom::Start(0))?;
    dst.seek(SeekFrom::Start(0))?;
    dst.set_len(0)?;
    let mut buf = vec![0u8; 4 << 20];
    let mut n = 0u64;
    loop {
        let k = src.read(&mut buf)?;
        if k == 0 {
            break;
        }
        dst.write_all(&buf[..k])?;
        n += k as u64;
    }
    dst.sync_all()?;
    Ok(n)
}
