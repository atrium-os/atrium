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

/// Stub allocators: format() never invokes io->alloc/free; tessera-debug
/// only opens (never mutates). Phase-3 mutation paths will need a real
/// allocator backed by tessera_extent_alloc.
pub extern "C" fn disk_alloc(_: *mut c_void, _: u64, _: *mut u64) -> i32 { -1 }
pub extern "C" fn disk_free (_: *mut c_void, _: u64, _: u64)  -> i32 { 0 }

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
