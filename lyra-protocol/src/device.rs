//! The device page (lyra-external-device.md §4): an external output device
//! (Bluetooth via vinculum-audio, a test device) tells lyrad how far behind
//! the ring the listener is.
//!
//! The device writes the page under a sequence lock; lyrad reads it each
//! period. lyrad trusts it only for that one device's clock: every value is
//! clamped, and a page that never settles reads as stopped.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{fence, AtomicU32, Ordering};

#[repr(C)]
struct DevicePage {
    seq: AtomicU32,
    state: AtomicU32,
    delay_frames: AtomicU32,
    underruns: AtomicU32,
    rate: AtomicU32,
}

pub const PAGE_LEN: usize = 4096;
pub const STATE_STOPPED: u32 = 0;
pub const STATE_STREAMING: u32 = 1;

/// One reading of the page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct DeviceState {
    pub state: u32,
    /// Frames taken from the ring but not yet heard.
    pub delay_frames: u32,
    /// Times the device found the ring empty.
    pub underruns: u32,
    pub rate: u32,
}

fn map(fd: &OwnedFd, prot: libc::c_int) -> io::Result<*mut DevicePage> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if (st.st_size as usize) < PAGE_LEN {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "short device page"));
    }
    let p = unsafe { libc::mmap(std::ptr::null_mut(), PAGE_LEN, prot, libc::MAP_SHARED, fd.as_raw_fd(), 0) };
    if p == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok(p as *mut DevicePage)
}

/// lyrad's end.
pub struct DeviceReader {
    page: *const DevicePage,
}
unsafe impl Send for DeviceReader {}

impl DeviceReader {
    /// A consistent reading, or `None` if the device is mid-write every time
    /// it looks (a stuck or hostile writer: lyrad treats the device as stopped).
    pub fn read(&self) -> Option<DeviceState> {
        let p = unsafe { &*self.page };
        for _ in 0..16 {
            let s1 = p.seq.load(Ordering::Acquire);
            if s1 & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let d = DeviceState {
                state: p.state.load(Ordering::Relaxed),
                delay_frames: p.delay_frames.load(Ordering::Relaxed),
                underruns: p.underruns.load(Ordering::Relaxed),
                rate: p.rate.load(Ordering::Relaxed),
            };
            fence(Ordering::Acquire);
            if p.seq.load(Ordering::Relaxed) == s1 {
                return Some(d);
            }
        }
        None
    }
}

impl Drop for DeviceReader {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.page as *mut _, PAGE_LEN) };
    }
}

/// The device's end.
pub struct DeviceWriter {
    page: *mut DevicePage,
}
unsafe impl Send for DeviceWriter {}

impl DeviceWriter {
    /// Map a received device page fd.
    pub fn from_fd(fd: OwnedFd) -> io::Result<DeviceWriter> {
        Ok(DeviceWriter { page: map(&fd, libc::PROT_READ | libc::PROT_WRITE)? })
    }

    pub fn publish(&self, d: &DeviceState) {
        let p = unsafe { &*self.page };
        let s = p.seq.load(Ordering::Relaxed);
        p.seq.store(s.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        p.state.store(d.state, Ordering::Relaxed);
        p.delay_frames.store(d.delay_frames, Ordering::Relaxed);
        p.underruns.store(d.underruns, Ordering::Relaxed);
        p.rate.store(d.rate, Ordering::Relaxed);
        p.seq.store(s.wrapping_add(2), Ordering::Release);
    }
}

impl Drop for DeviceWriter {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.page as *mut _, PAGE_LEN) };
    }
}

/// Make a device page: lyrad's reader, and the read-write fd for the device.
/// The segment has no name once this returns.
pub fn create() -> io::Result<(DeviceReader, OwnedFd)> {
    use std::io::Read;
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    let name = std::ffi::CString::new(format!("/lyra-device-{hex}")).unwrap();
    let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDWR | libc::O_CREAT | libc::O_EXCL, 0o600) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe { libc::shm_unlink(name.as_ptr()) };
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    if unsafe { libc::ftruncate(fd.as_raw_fd(), PAGE_LEN as libc::off_t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let page = map(&fd, libc::PROT_READ)?;
    Ok((DeviceReader { page }, fd))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_device_writes_and_lyrad_reads() {
        let (r, fd) = create().unwrap();
        assert_eq!(r.read(), Some(DeviceState::default()));
        let w = DeviceWriter::from_fd(fd).unwrap();
        let d = DeviceState { state: STATE_STREAMING, delay_frames: 7200, underruns: 2, rate: 48_000 };
        w.publish(&d);
        assert_eq!(r.read(), Some(d));
    }

    #[test]
    fn a_writer_stuck_mid_update_reads_as_nothing() {
        let (r, fd) = create().unwrap();
        let w = DeviceWriter::from_fd(fd).unwrap();
        unsafe { (*w.page).seq.store(1, Ordering::Release) };
        assert_eq!(r.read(), None);
    }
}
