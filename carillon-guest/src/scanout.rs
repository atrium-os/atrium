//! D0 display path bindings — present a CPU framebuffer on the VM's screen.
//!
//! Mirrors the `/dev/atrium-gpu0` + `/dev/atrium-display0` ioctl ABI from
//! `atrium-kmod/atrium_gpu.h` (the same chain `test_scanout.c` exercises):
//! bind the GPU to the display, enumerate a connector + mode, allocate a
//! CPU-visible SCANOUT BO, mmap it, set the mode, and page-flip. This lets
//! the Carillon guest take pixels rendered on the *host* (through the energy
//! router) and put them on the QEMU Cocoa window — the first time the stack's
//! output is actually visible rather than read back headless.
//!
//! ioctl request numbers are computed from the Rust `repr(C)` struct sizes
//! (FreeBSD encodes the payload length into the request), so they stay
//! correct as long as the structs mirror the C ones byte-for-byte.

use std::os::fd::RawFd;

// ── FreeBSD _IOC encoding (sys/ioccom.h). ───────────────────────────────
const IOC_VOID: libc::c_ulong = 0x2000_0000;
const IOC_OUT: libc::c_ulong = 0x4000_0000;
const IOC_IN: libc::c_ulong = 0x8000_0000;
const IOC_INOUT: libc::c_ulong = IOC_IN | IOC_OUT;
const IOCPARM_MASK: libc::c_ulong = 0x1fff; // 13-bit length field

const fn ioc(inout: libc::c_ulong, group: u8, num: u8, len: usize) -> libc::c_ulong {
    inout
        | (((len as libc::c_ulong) & IOCPARM_MASK) << 16)
        | ((group as libc::c_ulong) << 8)
        | (num as libc::c_ulong)
}
fn iow<T>(group: u8, num: u8) -> libc::c_ulong {
    ioc(IOC_IN, group, num, std::mem::size_of::<T>())
}
fn iowr<T>(group: u8, num: u8) -> libc::c_ulong {
    ioc(IOC_INOUT, group, num, std::mem::size_of::<T>())
}
#[allow(dead_code)]
fn ior<T>(group: u8, num: u8) -> libc::c_ulong {
    ioc(IOC_OUT, group, num, std::mem::size_of::<T>())
}
const G: u8 = b'G';
const D: u8 = b'D';

// ── Struct mirrors (must match atrium_gpu.h exactly). ───────────────────
const ATRIUM_GPU_BO_GPU_VISIBLE: u32 = 0x01;
const ATRIUM_GPU_BO_CPU_VISIBLE: u32 = 0x02;
const ATRIUM_GPU_BO_COHERENT: u32 = 0x04;
const ATRIUM_GPU_BO_SCANOUT: u32 = 0x08;

#[repr(C)]
#[derive(Default)]
struct GpuAlloc {
    size: u64,
    flags: u32,
    alignment: u32,
    handle: u32,
    _pad0: u32,
    mmap_offset: u64,
}

#[repr(C)]
struct DisplayBindGpu {
    gpu_fd: i32,
    _pad0: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DisplayConnector {
    id: u32,
    type_: u16,
    flags: u16,
    edid_size: u32,
    _pad0: u32,
    edid_ptr: u64,
}

#[repr(C)]
struct DisplayEnum {
    count_in: u32,
    count_out: u32,
    connectors_ptr: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DisplayMode {
    width: u32,
    height: u32,
    pixel_clock_khz: u32,
    refresh_mhz: u32,
    h_sync_start: u16,
    h_sync_end: u16,
    h_total: u16,
    h_skew: u16,
    v_sync_start: u16,
    v_sync_end: u16,
    v_total: u16,
    v_scan: u16,
    flags: u16,
    _pad0: u16,
    _reserved: [u64; 2],
}

#[repr(C)]
struct DisplayModesQuery {
    connector_id: u32,
    count_in: u32,
    count_out: u32,
    _pad0: u32,
    modes_ptr: u64,
}

#[repr(C)]
struct DisplaySetMode {
    connector_id: u32,
    scanout_handle: u32,
    mode: DisplayMode,
}

#[repr(C)]
#[derive(Default)]
struct DisplayPageFlip {
    connector_id: u32,
    scanout_handle: u32,
    wait_fence: u64,
    flip_id: u64,
    flags: u32,
    _pad0: u32,
}

unsafe fn ioctl_ptr<T>(fd: RawFd, req: libc::c_ulong, arg: *mut T) -> Result<(), String> {
    if libc::ioctl(fd, req, arg) != 0 {
        let e = std::io::Error::last_os_error();
        return Err(format!("ioctl({req:#x}): {e}"));
    }
    Ok(())
}

/// An open, mode-set scanout surface: a CPU-visible BGRA framebuffer the
/// guest writes into, then page-flips onto the connector.
pub struct Scanout {
    gpu_fd: RawFd,
    dpy_fd: RawFd,
    pub width: u32,
    pub height: u32,
    connector_id: u32,
    scanout_handle: u32,
    fb: *mut u32,
    fb_len: usize,
}

impl Scanout {
    /// Open the GPU + display devices, pick connector 0's first mode,
    /// allocate + map a scanout BO, and set the mode (showing a cleared
    /// frame). Returns the surface ready for [`Scanout::present_rgba`].
    pub fn open() -> Result<Scanout, String> {
        let gpu_fd = unsafe { libc::open(c"/dev/atrium-gpu0".as_ptr(), libc::O_RDWR) };
        if gpu_fd < 0 {
            return Err(format!("open /dev/atrium-gpu0: {} (is the atrium-virtio-gpu \
                kmod loaded? boot with run-vm.sh --virtio-gpu --display)",
                std::io::Error::last_os_error()));
        }
        let dpy_fd = unsafe { libc::open(c"/dev/atrium-display0".as_ptr(), libc::O_RDWR) };
        if dpy_fd < 0 {
            return Err(format!("open /dev/atrium-display0: {}", std::io::Error::last_os_error()));
        }

        let mut bind = DisplayBindGpu { gpu_fd, _pad0: 0 };
        unsafe { ioctl_ptr(dpy_fd, iow::<DisplayBindGpu>(D, 0), &mut bind)?; }

        // Two-call connector enumeration: count, then list.
        let mut en = DisplayEnum { count_in: 0, count_out: 0, connectors_ptr: 0 };
        unsafe { ioctl_ptr(dpy_fd, iowr::<DisplayEnum>(D, 1), &mut en)?; }
        if en.count_out == 0 {
            return Err("no display connectors".into());
        }
        let mut conns = vec![DisplayConnector::default(); en.count_out as usize];
        en.count_in = en.count_out;
        en.connectors_ptr = conns.as_mut_ptr() as u64;
        unsafe { ioctl_ptr(dpy_fd, iowr::<DisplayEnum>(D, 1), &mut en)?; }
        let connector_id = conns[0].id;

        // First mode for connector 0.
        let mut mode = DisplayMode::default();
        let mut mq = DisplayModesQuery {
            connector_id, count_in: 1, count_out: 0, _pad0: 0,
            modes_ptr: &mut mode as *mut _ as u64,
        };
        unsafe { ioctl_ptr(dpy_fd, iowr::<DisplayModesQuery>(D, 2), &mut mq)?; }
        let (width, height) = (mode.width, mode.height);
        if width == 0 || height == 0 {
            return Err(format!("degenerate mode {width}x{height}"));
        }

        // Allocate + map the scanout BO.
        let fb_len = (width as usize) * (height as usize) * 4;
        let mut al = GpuAlloc {
            size: fb_len as u64,
            flags: ATRIUM_GPU_BO_GPU_VISIBLE | ATRIUM_GPU_BO_CPU_VISIBLE
                | ATRIUM_GPU_BO_COHERENT | ATRIUM_GPU_BO_SCANOUT,
            ..Default::default()
        };
        unsafe { ioctl_ptr(gpu_fd, iowr::<GpuAlloc>(G, 1), &mut al)?; }
        let fb = unsafe {
            libc::mmap(std::ptr::null_mut(), al.size as usize,
                libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED,
                gpu_fd, al.mmap_offset as libc::off_t)
        };
        if fb == libc::MAP_FAILED {
            return Err(format!("mmap scanout BO: {}", std::io::Error::last_os_error()));
        }
        let fb = fb as *mut u32;
        // Clear to opaque black so the first SET_MODE shows a defined frame.
        unsafe { std::ptr::write_bytes(fb as *mut u8, 0, fb_len); }
        for i in 0..(width as usize * height as usize) {
            unsafe { *fb.add(i) = 0xff00_0000; }
        }

        let mut sm = DisplaySetMode { connector_id, scanout_handle: al.handle, mode };
        unsafe { ioctl_ptr(dpy_fd, iow::<DisplaySetMode>(D, 3), &mut sm)?; }

        Ok(Scanout {
            gpu_fd, dpy_fd, width, height, connector_id,
            scanout_handle: al.handle, fb, fb_len,
        })
    }

    /// Copy an `RGBA8` frame (width*height*4 bytes, the aqueduct readback
    /// order) into the scanout BO as `BGRA8` and page-flip it onto screen.
    pub fn present_rgba(&self, rgba: &[u8]) -> Result<(), String> {
        let pixels = self.width as usize * self.height as usize;
        if rgba.len() < pixels * 4 {
            return Err(format!("frame {} bytes < {} expected", rgba.len(), pixels * 4));
        }
        for i in 0..pixels {
            let (r, g, b, a) = (rgba[i * 4], rgba[i * 4 + 1], rgba[i * 4 + 2], rgba[i * 4 + 3]);
            // Little-endian u32 0xAARRGGBB stores bytes B,G,R,A → BGRA scanout.
            let bgra = (a as u32) << 24 | (r as u32) << 16 | (g as u32) << 8 | b as u32;
            unsafe { *self.fb.add(i) = bgra; }
        }
        let mut pf = DisplayPageFlip {
            connector_id: self.connector_id,
            scanout_handle: self.scanout_handle,
            ..Default::default()
        };
        unsafe { ioctl_ptr(self.dpy_fd, iow::<DisplayPageFlip>(D, 4), &mut pf) }
    }
}

impl Drop for Scanout {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.fb as *mut libc::c_void, self.fb_len);
            libc::close(self.gpu_fd);
            libc::close(self.dpy_fd);
        }
    }
}
