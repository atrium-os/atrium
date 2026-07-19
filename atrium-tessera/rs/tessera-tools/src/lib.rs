//! Shared internals for the Tessera CLI tools.
//!
//! The block-I/O glue lives here so both `mkfs-tessera` and
//! `tessera-debug` link against the same implementation. It uses
//! `pread`/`pwrite` against a raw fd — no caching layer; the kernel
//! page cache is sufficient for the tooling workloads.

use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

use tessera_sys::tessera_block_io_t;

pub const SECTOR_SIZE: u64 = 4096;

pub struct DiskCtx {
    pub fd: i32,
    /// Next free sector for the metadata-reserve bump allocator (repair).
    /// `bump_max == 0` disables allocation — read-only tools keep the
    /// original stub behaviour (alloc returns -1).
    pub bump: std::cell::Cell<u64>,
    /// One past the last allocatable reserve sector (meta_reserve_start +
    /// meta_reserve_length). 0 = allocation disabled.
    pub bump_max: u64,
}

impl DiskCtx {
    /// Read-only context: allocation disabled.
    pub fn ro(fd: i32) -> Self {
        DiskCtx { fd, bump: std::cell::Cell::new(0), bump_max: 0 }
    }
    /// Read-write context with a metadata-reserve bump allocator over
    /// [bump_start, bump_max). Used by tessera-fsck --repair to allocate
    /// COW btree / free-tree nodes exactly like the kmod's runtime bump.
    pub fn rw(fd: i32, bump_start: u64, bump_max: u64) -> Self {
        DiskCtx { fd, bump: std::cell::Cell::new(bump_start), bump_max }
    }
}

pub extern "C" fn disk_read(ctx: *mut c_void, sector: u64, out: *mut u8) -> i32 {
    let d = unsafe { &*(ctx as *const DiskCtx) };
    let n = unsafe {
        libc::pread(
            d.fd,
            out as *mut c_void,
            SECTOR_SIZE as usize,
            (sector * SECTOR_SIZE) as i64,
        )
    };
    if n == SECTOR_SIZE as isize { 0 } else { -1 }
}

pub extern "C" fn disk_write(ctx: *mut c_void, sector: u64, buf: *const u8) -> i32 {
    let d = unsafe { &*(ctx as *const DiskCtx) };
    let n = unsafe {
        libc::pwrite(
            d.fd,
            buf as *const c_void,
            SECTOR_SIZE as usize,
            (sector * SECTOR_SIZE) as i64,
        )
    };
    if n == SECTOR_SIZE as isize { 0 } else { -1 }
}

/// Metadata-reserve bump allocator. Mirrors core's `fmt_alloc`: hand out
/// `n` contiguous sectors from the reserve, advancing the bump pointer.
/// Read-only contexts (bump_max == 0) return -1, preserving the old stub
/// behaviour for tools that never mutate. On exhaustion returns -1
/// (ENOSPC) — the repair aborts before committing a partial change.
pub extern "C" fn disk_alloc(ctx: *mut c_void, n: u64, out_start: *mut u64) -> i32 {
    let d = unsafe { &*(ctx as *const DiskCtx) };
    if d.bump_max == 0 {
        return -1;
    }
    let start = d.bump.get();
    if start + n > d.bump_max {
        return -1;
    }
    d.bump.set(start + n);
    unsafe { *out_start = start; }
    0
}
/// Free is a no-op bump-back is not tracked: retired COW nodes leak into
/// the reserve until the next `tessera repack`, exactly as the runtime
/// bump allocator behaves. Offline repair touches only a handful of
/// nodes, so the leak is negligible.
pub extern "C" fn disk_free(_: *mut c_void, _: u64, _: u64) -> i32 { 0 }

pub fn make_io(ctx: &mut DiskCtx) -> tessera_block_io_t {
    tessera_block_io_t {
        read_block:  Some(disk_read),
        write_block: Some(disk_write),
        alloc:       Some(disk_alloc),
        free:        Some(disk_free),
        ctx:         ctx as *mut DiskCtx as *mut c_void,
    }
}

/// Read 16 bytes from /dev/urandom for a v4-shape UUID. Sets the
/// version + variant bits per RFC 4122 §4.4 so external consumers can
/// recognise it as a "random" UUID, but Tessera doesn't otherwise
/// interpret the bytes.
pub fn random_uuid_v4() -> io::Result<[u8; 16]> {
    use std::io::Read;
    let mut buf = [0u8; 16];
    let mut f = File::open("/dev/urandom")?;
    f.read_exact(&mut buf)?;
    buf[6] = (buf[6] & 0x0f) | 0x40;     /* version 4 */
    buf[8] = (buf[8] & 0x3f) | 0x80;     /* variant 1 */
    Ok(buf)
}

pub fn format_uuid(u: &[u8; 16]) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-\
         {:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        u[0], u[1], u[2], u[3], u[4], u[5], u[6], u[7],
        u[8], u[9], u[10], u[11], u[12], u[13], u[14], u[15]
    )
}

pub fn open_file_rw(path: &str) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_SYNC)
        .open(path)
}

pub fn open_file_ro(path: &str) -> io::Result<File> {
    std::fs::OpenOptions::new().read(true).open(path)
}

pub fn fd_of(f: &File) -> i32 {
    f.as_raw_fd()
}

pub fn file_size(f: &File) -> io::Result<u64> {
    let m = f.metadata()?;
    let len = m.len();
    if len > 0 {
        return Ok(len);
    }
    // Block / character device: stat reports st_size==0. Use the
    // FreeBSD DIOCGMEDIASIZE ioctl to get the actual extent.
    #[cfg(target_os = "freebsd")]
    {
        use std::os::unix::io::AsRawFd;
        // _IOR('d', 129, off_t) — see <sys/disk.h>.
        const DIOCGMEDIASIZE: libc::c_ulong = 0x40086481;
        let mut sz: i64 = 0;
        let r = unsafe {
            libc::ioctl(f.as_raw_fd(), DIOCGMEDIASIZE, &mut sz as *mut i64)
        };
        if r == 0 && sz > 0 {
            return Ok(sz as u64);
        }
    }
    Ok(len)
}
