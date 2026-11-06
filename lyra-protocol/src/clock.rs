//! The clock page (lyra-v1.md §6): when will frame *n* of my stream be heard?
//!
//! lyrad writes one page per stream every period, under a sequence lock. The
//! app maps it **read-only**: the fd it receives was opened `O_RDONLY`, so the
//! kernel refuses a writable mapping. The app reads it whenever it likes, with
//! no message and no wake.
//!
//! Frame *n* (stream rate) is heard at
//! `heard_ns + (n − frame) / (rate × (1 + rate_ppm / 10⁶))` on `CLOCK_MONOTONIC`.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{fence, AtomicI32, AtomicI64, AtomicU32, AtomicU64, Ordering};

/// The page, little-endian, at offset 0 of a 4 KiB shared segment.
#[repr(C)]
pub struct ClockPage {
    /// Odd while lyrad writes.
    pub seq: AtomicU32,
    /// [`STATE_PAUSED`] or [`STATE_PLAYING`].
    pub state: AtomicU32,
    /// Stream position (stream rate) of the anchor.
    pub frame: AtomicU64,
    /// `CLOCK_MONOTONIC` ns when `frame` reaches the speaker.
    pub heard_ns: AtomicI64,
    /// Measured device speed against nominal, parts per million.
    pub rate_ppm: AtomicI32,
    /// Bumps on each `Flush`.
    pub flush_epoch: AtomicU32,
    /// The stream's nominal rate.
    pub rate: AtomicU32,
}

pub const PAGE_LEN: usize = 4096;
pub const STATE_PAUSED: u32 = 0;
pub const STATE_PLAYING: u32 = 1;

/// One consistent reading of the page.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Clock {
    pub state: u32,
    pub frame: u64,
    pub heard_ns: i64,
    pub rate_ppm: i32,
    pub flush_epoch: u32,
    pub rate: u32,
}

impl Clock {
    /// When stream frame `n` will be (or was) heard, in `CLOCK_MONOTONIC` ns.
    pub fn heard_at(&self, n: u64) -> i64 {
        let rate = self.rate as f64 * (1.0 + self.rate_ppm as f64 / 1e6);
        if rate <= 0.0 {
            return self.heard_ns;
        }
        let d = n as f64 - self.frame as f64;
        self.heard_ns + (d / rate * 1e9).round() as i64
    }
}

fn map(fd: &OwnedFd, prot: libc::c_int) -> io::Result<*mut ClockPage> {
    let p = unsafe { libc::mmap(std::ptr::null_mut(), PAGE_LEN, prot, libc::MAP_SHARED, fd.as_raw_fd(), 0) };
    if p == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok(p as *mut ClockPage)
}

/// lyrad's end: writes the page.
pub struct ClockWriter {
    page: *mut ClockPage,
}
unsafe impl Send for ClockWriter {}

impl ClockWriter {
    /// Publish one reading.
    pub fn publish(&self, c: &Clock) {
        let p = unsafe { &*self.page };
        let s = p.seq.load(Ordering::Relaxed);
        p.seq.store(s.wrapping_add(1), Ordering::Relaxed); // odd: writing
        fence(Ordering::Release);
        p.state.store(c.state, Ordering::Relaxed);
        p.frame.store(c.frame, Ordering::Relaxed);
        p.heard_ns.store(c.heard_ns, Ordering::Relaxed);
        p.rate_ppm.store(c.rate_ppm, Ordering::Relaxed);
        p.flush_epoch.store(c.flush_epoch, Ordering::Relaxed);
        p.rate.store(c.rate, Ordering::Relaxed);
        p.seq.store(s.wrapping_add(2), Ordering::Release); // even: done
    }
}

impl Drop for ClockWriter {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.page as *mut _, PAGE_LEN) };
    }
}

/// The app's end: reads the page.
pub struct ClockReader {
    page: *const ClockPage,
}
unsafe impl Send for ClockReader {}

impl ClockReader {
    /// Map a received clock fd read-only.
    pub fn from_fd(fd: OwnedFd) -> io::Result<ClockReader> {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if (st.st_size as usize) < PAGE_LEN {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "short clock page"));
        }
        Ok(ClockReader { page: map(&fd, libc::PROT_READ)? })
    }

    /// A consistent reading (retries while lyrad is mid-write).
    pub fn read(&self) -> Clock {
        let p = unsafe { &*self.page };
        loop {
            let s1 = p.seq.load(Ordering::Acquire);
            if s1 & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let c = Clock {
                state: p.state.load(Ordering::Relaxed),
                frame: p.frame.load(Ordering::Relaxed),
                heard_ns: p.heard_ns.load(Ordering::Relaxed),
                rate_ppm: p.rate_ppm.load(Ordering::Relaxed),
                flush_epoch: p.flush_epoch.load(Ordering::Relaxed),
                rate: p.rate.load(Ordering::Relaxed),
            };
            fence(Ordering::Acquire);
            if p.seq.load(Ordering::Relaxed) == s1 {
                return c;
            }
        }
    }
}

impl Drop for ClockReader {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.page as *mut _, PAGE_LEN) };
    }
}

fn random_name() -> io::Result<std::ffi::CString> {
    use std::io::Read;
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(std::ffi::CString::new(format!("/lyra-clock-{hex}")).unwrap())
}

/// Make a clock page: lyrad's writer, and a **read-only** fd for the app.
///
/// A segment is created under a random name (mode 0600, so only lyrad's uid
/// could open it, and only in its jail), a second, `O_RDONLY` descriptor is
/// opened on it, and the name is removed at once. The app's descriptor can't
/// be mapped writable.
pub fn create() -> io::Result<(ClockWriter, OwnedFd)> {
    let name = random_name()?;
    let rw = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDWR | libc::O_CREAT | libc::O_EXCL, 0o600) };
    if rw < 0 {
        return Err(io::Error::last_os_error());
    }
    let rw = unsafe { OwnedFd::from_raw_fd(rw) };
    let ro = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDONLY, 0) };
    let ro_err = io::Error::last_os_error();
    unsafe { libc::shm_unlink(name.as_ptr()) };
    if ro < 0 {
        return Err(ro_err);
    }
    let ro = unsafe { OwnedFd::from_raw_fd(ro) };
    if unsafe { libc::ftruncate(rw.as_raw_fd(), PAGE_LEN as libc::off_t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let page = map(&rw, libc::PROT_READ | libc::PROT_WRITE)?;
    Ok((ClockWriter { page }, ro))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_then_read() {
        let (w, fd) = create().unwrap();
        let r = ClockReader::from_fd(fd).unwrap();
        let c = Clock { state: STATE_PLAYING, frame: 4800, heard_ns: 1_000_000_000, rate_ppm: 0, flush_epoch: 2, rate: 48_000 };
        w.publish(&c);
        assert_eq!(r.read(), c);
        // 48000 frames later is one second later.
        assert_eq!(r.read().heard_at(4800 + 48_000), 2_000_000_000);
        // A device running 100 ppm fast plays them sooner.
        let fast = Clock { rate_ppm: 100, ..c };
        assert!(fast.heard_at(4800 + 48_000) < 2_000_000_000);
        // Frames before the anchor were heard earlier.
        assert_eq!(c.heard_at(0), 1_000_000_000 - 100_000_000);
    }

    /// ★ The app's fd can't be mapped writable, so it can't scribble on the page.
    #[test]
    fn the_app_fd_is_read_only() {
        let (_w, fd) = create().unwrap();
        let p = unsafe {
            libc::mmap(std::ptr::null_mut(), PAGE_LEN, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd.as_raw_fd(), 0)
        };
        assert_eq!(p, libc::MAP_FAILED, "a writable mapping of the app's fd must fail");
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EACCES));
        // And the name is gone: nobody else can open the segment.
    }

    #[test]
    fn readers_never_see_a_torn_write() {
        let (w, fd) = create().unwrap();
        let r = ClockReader::from_fd(fd).unwrap();
        let t = std::thread::spawn(move || {
            for i in 0..200_000u64 {
                w.publish(&Clock { state: 1, frame: i, heard_ns: i as i64 * 3, rate_ppm: 0, flush_epoch: 0, rate: 48_000 });
            }
        });
        for _ in 0..200_000 {
            let c = r.read();
            assert_eq!(c.heard_ns, c.frame as i64 * 3, "torn: {c:?}");
        }
        t.join().unwrap();
    }
}
