//! Zero-copy SPSC audio ring — the graph **edge** between adjacent nodes
//! (`docs/spec/atrium-lyra-architecture.md` §5.2).
//!
//! One producer, one consumer, lock-free: the producer owns `head`, the consumer
//! owns `tail`, each only reads the other's index (a single-producer/
//! single-consumer ring needs no locks, only release/acquire ordering). Backed by
//! POSIX shared memory (`shm_open`/`mmap`) so a **jailed effect node** and lyrad
//! share the buffer across a Portcullis boundary with no copy — the data plane
//! the deadline graph runs on. The SPSC algorithm is host-testable over any
//! backing pointer; only `create`/`open` are FreeBSD/unix-specific.

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};

/// Header at the base of the shared segment, followed by the float data.
#[repr(C)]
struct Header {
    capacity_frames: u64, // ring depth in frames (power of two)
    frame_floats: u64,    // floats per frame (channels)
    head: AtomicU64,      // producer write position (monotonic frames)
    tail: AtomicU64,      // consumer read position (monotonic frames)
}

/// A handle to one end of the ring. `producer` selects which index this end
/// advances. The backing memory is shared; dropping unmaps it.
pub struct Ring {
    hdr: *mut Header,
    data: *mut f32,
    capacity: u64,
    frame_floats: u64,
    map: *mut libc::c_void,
    map_len: usize,
    producer: bool,
    owns_map: bool,                      // unmap on drop (false for shared test maps)
    owns_shm: Option<std::ffi::CString>, // Some(name) on the creator → unlink on drop
}

unsafe impl Send for Ring {}

impl Ring {
    fn from_map(map: *mut libc::c_void, map_len: usize, producer: bool, owns_map: bool) -> Self {
        let hdr = map as *mut Header;
        let data = unsafe { (map as *mut u8).add(std::mem::size_of::<Header>()) as *mut f32 };
        let (capacity, frame_floats) =
            unsafe { ((*hdr).capacity_frames, (*hdr).frame_floats) };
        Ring {
            hdr,
            data,
            capacity,
            frame_floats,
            map,
            map_len,
            producer,
            owns_map,
            owns_shm: None,
        }
    }

    fn bytes_for(capacity_frames: u64, frame_floats: u64) -> usize {
        std::mem::size_of::<Header>() + (capacity_frames * frame_floats) as usize * 4
    }

    /// Create the shared ring (producer side). `capacity_frames` must be a power
    /// of two. The segment is `shm_unlink`ed when this handle drops.
    pub fn create(name: &str, capacity_frames: u64, frame_floats: u64) -> io::Result<Self> {
        assert!(capacity_frames.is_power_of_two(), "capacity must be 2^n");
        let cname = std::ffi::CString::new(name).unwrap();
        let len = Self::bytes_for(capacity_frames, frame_floats);
        let fd = unsafe {
            libc::shm_open(cname.as_ptr(), libc::O_CREAT | libc::O_RDWR | libc::O_EXCL, 0o600)
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let r = unsafe { libc::ftruncate(fd, len as libc::off_t) };
        if r != 0 {
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd); libc::shm_unlink(cname.as_ptr()); }
            return Err(e);
        }
        let map = unsafe {
            libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0)
        };
        unsafe { libc::close(fd) };
        if map == libc::MAP_FAILED {
            let e = io::Error::last_os_error();
            unsafe { libc::shm_unlink(cname.as_ptr()); }
            return Err(e);
        }
        // initialise the header.
        let hdr = map as *mut Header;
        unsafe {
            (*hdr).capacity_frames = capacity_frames;
            (*hdr).frame_floats = frame_floats;
            (*hdr).head = AtomicU64::new(0);
            (*hdr).tail = AtomicU64::new(0);
        }
        let mut ring = Self::from_map(map, len, true, true);
        ring.owns_shm = Some(cname);
        Ok(ring)
    }

    /// Open an existing ring (consumer side, e.g. the jailed effect process).
    pub fn open(name: &str, producer: bool) -> io::Result<Self> {
        let cname = std::ffi::CString::new(name).unwrap();
        let fd = unsafe { libc::shm_open(cname.as_ptr(), libc::O_RDWR, 0o600) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // read the header to size the mapping.
        let mut h = Header {
            capacity_frames: 0,
            frame_floats: 0,
            head: AtomicU64::new(0),
            tail: AtomicU64::new(0),
        };
        let n = unsafe {
            libc::read(fd, &mut h as *mut _ as *mut libc::c_void, std::mem::size_of::<Header>())
        };
        if n != std::mem::size_of::<Header>() as isize {
            unsafe { libc::close(fd) };
            return Err(io::Error::new(io::ErrorKind::InvalidData, "short header"));
        }
        let len = Self::bytes_for(h.capacity_frames, h.frame_floats);
        let map = unsafe {
            libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0)
        };
        unsafe { libc::close(fd) };
        if map == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self::from_map(map, len, producer, true))
    }

    fn head(&self) -> u64 {
        unsafe { (*self.hdr).head.load(Ordering::Acquire) }
    }
    fn tail(&self) -> u64 {
        unsafe { (*self.hdr).tail.load(Ordering::Acquire) }
    }

    /// Frames available to read.
    pub fn readable(&self) -> u64 {
        self.head().wrapping_sub(self.tail())
    }
    /// Frames of free space to write.
    pub fn writable(&self) -> u64 {
        self.capacity - self.readable()
    }

    /// Producer: write up to `frames.len()/frame_floats` frames; returns frames
    /// written (bounded by free space). Only the producer end may call this.
    pub fn write(&self, frames: &[f32]) -> u64 {
        debug_assert!(self.producer);
        let want = frames.len() as u64 / self.frame_floats;
        let n = want.min(self.writable());
        let head = self.head();
        for i in 0..n {
            let slot = ((head + i) % self.capacity) * self.frame_floats;
            for c in 0..self.frame_floats {
                unsafe {
                    *self.data.add((slot + c) as usize) =
                        frames[(i * self.frame_floats + c) as usize];
                }
            }
        }
        unsafe { (*self.hdr).head.store(head + n, Ordering::Release) };
        n
    }

    /// Consumer: read up to `out.len()/frame_floats` frames; returns frames read.
    /// Only the consumer end may call this.
    pub fn read(&self, out: &mut [f32]) -> u64 {
        debug_assert!(!self.producer);
        let want = out.len() as u64 / self.frame_floats;
        let n = want.min(self.readable());
        let tail = self.tail();
        for i in 0..n {
            let slot = ((tail + i) % self.capacity) * self.frame_floats;
            for c in 0..self.frame_floats {
                unsafe {
                    out[(i * self.frame_floats + c) as usize] =
                        *self.data.add((slot + c) as usize);
                }
            }
        }
        unsafe { (*self.hdr).tail.store(tail + n, Ordering::Release) };
        n
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        if self.owns_map {
            unsafe { libc::munmap(self.map, self.map_len) };
        }
        if let Some(name) = &self.owns_shm {
            unsafe { libc::shm_unlink(name.as_ptr()) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a ring pair over a heap segment (no shm) to test the SPSC algorithm
    /// on host. Producer and consumer share the same backing.
    fn pair(capacity: u64, frame_floats: u64) -> (Ring, Ring, *mut libc::c_void) {
        let len = Ring::bytes_for(capacity, frame_floats);
        let map = unsafe {
            libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON, -1, 0)
        };
        let hdr = map as *mut Header;
        unsafe {
            (*hdr).capacity_frames = capacity;
            (*hdr).frame_floats = frame_floats;
            (*hdr).head = AtomicU64::new(0);
            (*hdr).tail = AtomicU64::new(0);
        }
        let prod = Ring::from_map(map, len, true, false);
        let cons = Ring::from_map(map, len, false, false);
        (prod, cons, map)
    }

    #[test]
    fn write_then_read_round_trips_frames() {
        let (p, c, _m) = pair(8, 2); // 8-frame, stereo
        let inb = [1.0, -1.0, 2.0, -2.0, 3.0, -3.0]; // 3 frames
        assert_eq!(p.write(&inb), 3);
        assert_eq!(c.readable(), 3);
        let mut out = [0.0f32; 6];
        assert_eq!(c.read(&mut out), 3);
        assert_eq!(out, inb);
        assert_eq!(c.readable(), 0);
    }

    #[test]
    fn write_is_bounded_by_free_space() {
        let (p, _c, _m) = pair(4, 1); // 4-frame mono
        let big = [0.0f32; 10];
        assert_eq!(p.write(&big), 4, "fills exactly the capacity");
        assert_eq!(p.write(&big), 0, "full: no more space");
    }

    #[test]
    fn read_is_bounded_by_available() {
        let (p, c, _m) = pair(8, 1);
        assert_eq!(p.write(&[1.0, 2.0]), 2);
        let mut out = [0.0f32; 8];
        assert_eq!(c.read(&mut out), 2, "only what was written");
    }

    #[test]
    fn wraps_around_the_ring() {
        let (p, c, _m) = pair(4, 1);
        // fill, drain, fill again across the wrap boundary.
        assert_eq!(p.write(&[1.0, 2.0, 3.0]), 3);
        let mut out = [0.0f32; 3];
        assert_eq!(c.read(&mut out), 3);
        assert_eq!(out, [1.0, 2.0, 3.0]);
        // head=3, tail=3; write 3 more wraps past index 4.
        assert_eq!(p.write(&[4.0, 5.0, 6.0]), 3);
        let mut out2 = [0.0f32; 3];
        assert_eq!(c.read(&mut out2), 3);
        assert_eq!(out2, [4.0, 5.0, 6.0], "data correct across the wrap");
    }

    #[test]
    fn concurrent_producer_consumer_lose_no_frames() {
        // the SPSC guarantee: a producer thread and consumer thread streaming
        // 100k frames through a small ring transfer every frame in order.
        let len = Ring::bytes_for(64, 1);
        let map = unsafe {
            libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON, -1, 0)
        };
        let hdr = map as *mut Header;
        unsafe {
            (*hdr).capacity_frames = 64;
            (*hdr).frame_floats = 1;
            (*hdr).head = AtomicU64::new(0);
            (*hdr).tail = AtomicU64::new(0);
        }
        let addr = map as usize;
        let n = 100_000u64;
        let prod = std::thread::spawn(move || {
            let p = Ring::from_map(addr as *mut libc::c_void, len, true, false);
            let mut i = 0u64;
            while i < n {
                if p.write(&[i as f32]) == 1 {
                    i += 1;
                } else {
                    std::hint::spin_loop();
                }
            }
        });
        let cons = Ring::from_map(map, len, false, false);
        let mut got = 0u64;
        let mut buf = [0.0f32; 1];
        while got < n {
            if cons.read(&mut buf) == 1 {
                assert_eq!(buf[0], got as f32, "in-order, no loss");
                got += 1;
            } else {
                std::hint::spin_loop();
            }
        }
        prod.join().unwrap();
        unsafe { libc::munmap(map, len) };
    }
}
