//! Safe Rust binding for `/dev/atrium-bootfb0`.
//!
//! Exposes the EFI GOP framebuffer the bootloader handed to the
//! kernel as a userspace-mappable cdev. Used by `atrium-splash` to
//! draw a boot-time splash before the native GPU driver claims the
//! display.
//!
//! Usage:
//!   let fb = BootFb::open()?;
//!   println!("{}x{} stride={} format={:?}", fb.width(), fb.height(),
//!            fb.stride(), fb.format());
//!   let pixels = fb.pixels_mut();   // &mut [u8] of length size
//!   // write ARGB/BGRA per fb.format() ...
//!   // visible immediately on the GOP scanout, no flush needed.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::ptr;

mod abi;
pub use abi::{
    AtriumBootfbInfo, ATRIUM_BOOTFB_FORMAT_BGRA8, ATRIUM_BOOTFB_FORMAT_RGBA8,
    ATRIUM_BOOTFB_FORMAT_UNKNOWN, ATRIUM_BOOTFB_IOC_GET_INFO,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Bgra8,
    Rgba8,
    Unknown,
}

impl From<u32> for PixelFormat {
    fn from(v: u32) -> Self {
        match v {
            ATRIUM_BOOTFB_FORMAT_BGRA8 => PixelFormat::Bgra8,
            ATRIUM_BOOTFB_FORMAT_RGBA8 => PixelFormat::Rgba8,
            _ => PixelFormat::Unknown,
        }
    }
}

pub struct BootFb {
    file: File,
    info: AtriumBootfbInfo,
    map_ptr: *mut u8,
    map_len: usize,
}

unsafe impl Send for BootFb {}

impl BootFb {
    /// Open `/dev/atrium-bootfb0`, query the framebuffer info, and
    /// `mmap` it into userspace. Returns `Err` if the kmod isn't
    /// loaded or the system has no EFI GOP framebuffer.
    pub fn open() -> io::Result<Self> {
        Self::open_path("/dev/atrium-bootfb0")
    }

    pub fn open_path(path: &str) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let mut info = AtriumBootfbInfo::default();
        let r = unsafe {
            libc::ioctl(file.as_raw_fd(), ATRIUM_BOOTFB_IOC_GET_INFO, &mut info)
        };
        if r < 0 { return Err(io::Error::last_os_error()); }
        if info.size == 0 || info.width == 0 || info.height == 0 {
            return Err(io::Error::new(io::ErrorKind::Other, "bootfb info zeroed"));
        }
        let map_len = info.size as usize;
        let map_ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                map_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if map_ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            file,
            info,
            map_ptr: map_ptr as *mut u8,
            map_len,
        })
    }

    pub fn width(&self) -> u32 { self.info.width }
    pub fn height(&self) -> u32 { self.info.height }
    /// Bytes per row. May exceed `width * 4` if the GOP padded the
    /// scanline.
    pub fn stride(&self) -> u32 { self.info.stride }
    pub fn format(&self) -> PixelFormat { self.info.format.into() }
    pub fn size(&self) -> usize { self.map_len }

    /// Mutable framebuffer slice. Length is `size()`. Writes are
    /// visible to the scanout immediately (no flush).
    pub fn pixels_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.map_ptr, self.map_len) }
    }

    pub fn pixels(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.map_ptr, self.map_len) }
    }
}

impl AsRawFd for BootFb {
    fn as_raw_fd(&self) -> RawFd { self.file.as_raw_fd() }
}

impl Drop for BootFb {
    fn drop(&mut self) {
        if !self.map_ptr.is_null() {
            unsafe { libc::munmap(self.map_ptr as *mut libc::c_void, self.map_len) };
        }
    }
}
