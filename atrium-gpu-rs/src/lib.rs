//! Safe Rust bindings for the Atrium GPU ABI ([`docs/spec/gpu-abi.md`]).
//!
//! Two cdevs are wrapped:
//!   * [`Gpu`]     — `/dev/atrium-gpu0`     — buffer objects, command submission, fences
//!   * [`Display`] — `/dev/atrium-display0` — modesetting, page flip
//!
//! [`Bo`] is a buffer object handle paired with its CPU-visible mmap; it
//! frees the underlying kernel memory on drop. [`Display::bind`] ties a
//! display fd to a gpu fd so subsequent display ioctls can resolve BO
//! handles.
//!
//! Errors are surfaced as [`std::io::Error`] from the syscall layer; no
//! string-only error type. Callers that want richer context wrap with
//! `?` plus their own `Context` types.
//!
//! [`docs/spec/gpu-abi.md`]: ../../docs/spec/gpu-abi.md

pub mod abi;
pub mod virtio_gpu;

use std::ffi::CString;
use std::io;
use std::os::unix::io::{AsRawFd, RawFd};
use std::ptr;

use libc::{c_int, c_void, ioctl, mmap, munmap, MAP_FAILED, MAP_SHARED, PROT_READ, PROT_WRITE};

fn open_rdwr(path: &str) -> io::Result<RawFd> {
    let cpath = CString::new(path).unwrap();
    let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDWR) };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(fd)
    }
}

fn rc<T>(rc: c_int, ok: T) -> io::Result<T> {
    if rc == 0 {
        Ok(ok)
    } else {
        Err(io::Error::last_os_error())
    }
}

// -------------------------------------------------------------------------
// Gpu
// -------------------------------------------------------------------------

pub struct Gpu {
    fd: RawFd,
}

impl Gpu {
    pub fn open() -> io::Result<Self> {
        Ok(Self { fd: open_rdwr("/dev/atrium-gpu0")? })
    }

    pub fn caps(&self) -> io::Result<abi::atrium_gpu_caps> {
        let mut c = abi::atrium_gpu_caps::default();
        let r = unsafe { ioctl(self.fd, abi::ATRIUM_GPU_IOC_CAPS, &mut c) };
        rc(r, c)
    }

    pub fn family(&self) -> io::Result<String> {
        let c = self.caps()?;
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(c.family.as_ptr() as *const u8, c.family.len())
        };
        let nul = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        Ok(String::from_utf8_lossy(&bytes[..nul]).into_owned())
    }

    /// Allocate a buffer object. The returned [`Bo`] owns the BO and
    /// frees it on drop.
    pub fn alloc(&self, size: u64, flags: u32) -> io::Result<Bo<'_>> {
        let mut a = abi::atrium_gpu_alloc {
            size,
            flags,
            ..Default::default()
        };
        let r = unsafe { ioctl(self.fd, abi::ATRIUM_GPU_IOC_ALLOC, &mut a) };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }

        // Round up to page granularity to match kernel's allocation, so
        // mmap covers the full region the kernel sees.
        let page = unsafe { libc::sysconf(libc::_SC_PAGE_SIZE) } as usize;
        let map_len = ((size as usize) + page - 1) & !(page - 1);
        let p = unsafe {
            mmap(
                ptr::null_mut(),
                map_len,
                PROT_READ | PROT_WRITE,
                MAP_SHARED,
                self.fd,
                a.mmap_offset as i64,
            )
        };
        if p == MAP_FAILED {
            let e = io::Error::last_os_error();
            // Best-effort free of the BO; caller can't recover it.
            unsafe { ioctl(self.fd, abi::ATRIUM_GPU_IOC_FREE, &a.handle) };
            return Err(e);
        }

        Ok(Bo {
            gpu: self,
            handle: a.handle,
            map: p as *mut u8,
            map_len,
        })
    }

    /// Submit pre-encoded engine bytes from `cmd` (a BO range). v0.1 is
    /// synchronous: the returned fence has already retired.
    pub fn submit(&self, cmd: &Bo<'_>, offset: u64, size: u64) -> io::Result<u64> {
        let mut s = abi::atrium_gpu_submit {
            cmd_handle: cmd.handle,
            cmd_offset: offset,
            cmd_size: size,
            engine: abi::FRESCO_ENGINE_GRAPHICS,
            ..Default::default()
        };
        let r = unsafe { ioctl(self.fd, abi::ATRIUM_GPU_IOC_SUBMIT, &mut s) };
        rc(r, s.fence_out)
    }

    pub fn fence_query(&self, engine: u32) -> io::Result<u64> {
        let mut q = abi::atrium_gpu_fence_query { engine, ..Default::default() };
        let r = unsafe { ioctl(self.fd, abi::ATRIUM_GPU_IOC_FENCE_QUERY, &mut q) };
        rc(r, q.latest_retired)
    }
}

impl AsRawFd for Gpu {
    fn as_raw_fd(&self) -> RawFd { self.fd }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

// -------------------------------------------------------------------------
// Bo
// -------------------------------------------------------------------------

pub struct Bo<'a> {
    gpu: &'a Gpu,
    handle: u32,
    map: *mut u8,
    map_len: usize,
}

impl<'a> Bo<'a> {
    pub fn handle(&self) -> u32 { self.handle }
    pub fn len(&self) -> usize { self.map_len }
    pub fn is_empty(&self) -> bool { self.map_len == 0 }

    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.map, self.map_len) }
    }
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.map, self.map_len) }
    }

    /// Reinterpret the BO as a slice of `T`. Caller asserts alignment +
    /// size are sane for the layout.
    pub fn as_mut_typed<T>(&mut self) -> &mut [T] {
        let n = self.map_len / std::mem::size_of::<T>();
        unsafe { std::slice::from_raw_parts_mut(self.map as *mut T, n) }
    }
}

impl<'a> Drop for Bo<'a> {
    fn drop(&mut self) {
        unsafe {
            munmap(self.map as *mut c_void, self.map_len);
            ioctl(self.gpu.fd, abi::ATRIUM_GPU_IOC_FREE, &self.handle);
        }
    }
}

// -------------------------------------------------------------------------
// Display
// -------------------------------------------------------------------------

pub struct Display {
    fd: RawFd,
}

#[derive(Debug, Clone)]
pub struct Connector {
    pub id: u32,
    pub kind: u16,
    pub flags: u16,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
    pub flags: u16,
}

impl Display {
    pub fn open() -> io::Result<Self> {
        Ok(Self { fd: open_rdwr("/dev/atrium-display0")? })
    }

    /// Bind a Gpu cdev fd so subsequent ioctls can resolve BO handles.
    pub fn bind(&self, gpu: &Gpu) -> io::Result<()> {
        let mut b = abi::atrium_display_bind_gpu { gpu_fd: gpu.as_raw_fd(), _pad0: 0 };
        let r = unsafe { ioctl(self.fd, abi::ATRIUM_DISPLAY_IOC_BIND_GPU, &mut b) };
        rc(r, ())
    }

    pub fn connectors(&self) -> io::Result<Vec<Connector>> {
        let mut probe = abi::atrium_display_enum::default();
        let r = unsafe { ioctl(self.fd, abi::ATRIUM_DISPLAY_IOC_ENUM_CONNECTORS, &mut probe) };
        if r != 0 { return Err(io::Error::last_os_error()); }

        let n = probe.count_out as usize;
        let mut buf: Vec<abi::atrium_display_connector> =
            vec![Default::default(); n.max(1)];
        let mut q = abi::atrium_display_enum {
            count_in: n as u32,
            count_out: 0,
            connectors_ptr: buf.as_mut_ptr() as u64,
        };
        let r = unsafe { ioctl(self.fd, abi::ATRIUM_DISPLAY_IOC_ENUM_CONNECTORS, &mut q) };
        if r != 0 { return Err(io::Error::last_os_error()); }

        Ok(buf.into_iter().take(q.count_out as usize).map(|c| Connector {
            id: c.id, kind: c.r#type, flags: c.flags,
        }).collect())
    }

    pub fn preferred_mode(&self, connector_id: u32) -> io::Result<Mode> {
        let mut m = abi::atrium_display_mode::default();
        let mut q = abi::atrium_display_modes_query {
            connector_id,
            count_in: 1,
            modes_ptr: &mut m as *mut _ as u64,
            ..Default::default()
        };
        let r = unsafe { ioctl(self.fd, abi::ATRIUM_DISPLAY_IOC_MODES, &mut q) };
        if r != 0 { return Err(io::Error::last_os_error()); }
        Ok(Mode {
            width: m.width, height: m.height,
            refresh_mhz: m.refresh_mhz, flags: m.flags,
        })
    }

    pub fn set_mode(&self, connector_id: u32, scanout: &Bo<'_>, m: Mode) -> io::Result<()> {
        let mut s = abi::atrium_display_set_mode {
            connector_id,
            scanout_handle: scanout.handle,
            mode: abi::atrium_display_mode {
                width: m.width, height: m.height,
                refresh_mhz: m.refresh_mhz, flags: m.flags,
                ..Default::default()
            },
        };
        let r = unsafe { ioctl(self.fd, abi::ATRIUM_DISPLAY_IOC_SET_MODE, &mut s) };
        rc(r, ())
    }

    pub fn page_flip(&self, connector_id: u32, scanout: &Bo<'_>) -> io::Result<()> {
        let mut p = abi::atrium_display_page_flip {
            connector_id,
            scanout_handle: scanout.handle,
            ..Default::default()
        };
        let r = unsafe { ioctl(self.fd, abi::ATRIUM_DISPLAY_IOC_PAGE_FLIP, &mut p) };
        rc(r, ())
    }

    /// Block until the next vblank tick for `connector_id`. Returns
    /// the kmod's sequence counter after the wait. Callers can
    /// detect missed vblanks by comparing successive `seq` values
    /// (a gap > 1 means a frame slot was skipped).
    ///
    /// Today's kmod emulates vblank with a `callout` at the
    /// connector's mode refresh interval (set_mode-time). When D5+
    /// native drivers land, the source becomes a real GPU IRQ; the
    /// userspace ABI is unchanged.
    pub fn wait_vblank(&self, connector_id: u32) -> io::Result<u64> {
        let mut w = abi::atrium_display_wait_vblank {
            connector_id,
            _pad0: 0,
            seq: 0,
        };
        let r = unsafe { ioctl(self.fd, abi::ATRIUM_DISPLAY_IOC_WAIT_VBLANK, &mut w) };
        rc(r, w.seq)
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}
