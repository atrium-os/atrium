//! Carillon — the doorbell-driven guest→host transport for the
//! aqueduct-gpu FrameOp stream. See `docs/spec/carillon.md`.
//!
//! This module is the **host endpoint**: the shared-memory layout
//! (control page + SPSC submission/completion rings), the pipe-backed
//! doorbell (signal/wait), and the host serve-loop that blocks on the
//! guest→host doorbell, drains the submission ring, runs a handler per
//! frame, writes completions, and rings the host→guest doorbell.
//!
//! **Phase T0 (this file).** Host attach + loopback, no QEMU and no
//! guest kmod. The doorbell is a pipe (write 8 bytes to signal); the
//! host serve-loop sleeps on a [`Waiter`] — **kqueue** (`EVFILT_READ`)
//! on BSD/Darwin, `poll(2)` elsewhere — multiplexing the doorbell and an
//! out-of-band shutdown self-pipe, so the thread parks in the kernel (the
//! no-spin invariant) and the loop is the multiplex substrate T-real
//! extends to also watch the ivshmem-server listen socket. The "guest"
//! is a test that opens the same shm file and drives the reference ring
//! protocol via [`GuestRing`]. Real QEMU attach (ivshmem-server +
//! SCM_RIGHTS eventfd passing) and the FreeBSD guest kmod are later
//! phases (T1+); this layer is reused unchanged underneath them.
//!
//! The submission/completion descriptors here carry *references* to a
//! FrameOp stream (offset+len), not the stream bytes — matching the
//! spec's "rings carry descriptors, payload rides the region" rule.
//! Wiring the drained FrameOp stream into `Session`/`Backend` is T3.

#![cfg(unix)]

use std::fs::OpenOptions;
use std::io;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Shared-memory layout. Offsets are BAR2-relative and identical on
/// both sides. Ring base offsets are 16 KiB-aligned so a host mapping
/// never straddles an Apple-silicon 16 KiB page (spec §6.2).
pub mod layout {
    /// `'AGVT'` little-endian — transport id + version tag.
    pub const MAGIC: u32 = 0x5456_4741;
    /// Wire/ABI version of the control page + ring layout.
    pub const ABI_VERSION: u32 = 1;

    /// Control page base (4 KiB).
    pub const CTRL_OFFSET: usize = 0x0_0000;
    /// Submission ring base (guest → host). 16 KiB-aligned.
    pub const SUB_RING_OFFSET: usize = 0x0_1000;
    /// Completion ring base (host → guest). 16 KiB-aligned.
    pub const COMP_RING_OFFSET: usize = 0x1_0000;
    /// Region-table base (BO descriptors). 16 KiB-aligned.
    pub const REGION_TABLE_OFFSET: usize = 0x2_0000;
    /// Frame-staging arena base — where serialized FrameOp streams live;
    /// `SubDesc.frame_off` is relative to this. 16 KiB-aligned. (Large
    /// BO payloads ride the host-visible region transport, not here; this
    /// arena carries the comparatively small command stream.)
    pub const FRAME_ARENA_OFFSET: usize = 0x3_0000;
    /// Default total mapping: control + rings + region table + arena.
    pub const TOTAL_SIZE: usize = 0x10_0000; // 1 MiB
    /// Frame-staging arena byte span.
    pub const FRAME_ARENA_BYTES: usize = TOTAL_SIZE - FRAME_ARENA_OFFSET;

    /// Fixed descriptor size for both rings.
    pub const DESC_SIZE: usize = 64;
    /// Submission ring byte span (60 KiB).
    pub const SUB_RING_BYTES: usize = 0xF000;
    /// Completion ring byte span (60 KiB).
    pub const COMP_RING_BYTES: usize = 0xF000;
    /// Submission ring entry count (960).
    pub const SUB_ENTRIES: u32 = (SUB_RING_BYTES / DESC_SIZE) as u32;
    /// Completion ring entry count (960).
    pub const COMP_ENTRIES: u32 = (COMP_RING_BYTES / DESC_SIZE) as u32;

    /// Control field: magic (u32).
    pub const C_MAGIC: usize = 0x00;
    /// Control field: ABI version (u32).
    pub const C_ABI: usize = 0x04;
    /// Control field: host status — 0=down, 1=ready (u32).
    pub const C_HOST_STATUS: usize = 0x08;
    /// Control field: guest status — 0=down, 1=booted (u32).
    pub const C_GUEST_STATUS: usize = 0x0C;
    /// Control field: host page size, published by the host (u32).
    pub const C_HOST_PAGE_SIZE: usize = 0x10;
    /// Control field: submission write index — guest writes, host reads (u32).
    pub const C_SUB_WRITE: usize = 0x20;
    /// Control field: submission read index — host writes, guest reads (u32).
    pub const C_SUB_READ: usize = 0x24;
    /// Control field: completion write index — host writes, guest reads (u32).
    pub const C_COMP_WRITE: usize = 0x28;
    /// Control field: completion read index — guest writes, host reads (u32).
    pub const C_COMP_READ: usize = 0x2C;
    /// Control field: backend caps mirror (u64).
    pub const C_CAPS: usize = 0x40;
}

use layout::*;

/// Submission descriptor (guest → host). 64 bytes in the ring. References
/// a serialized FrameOp stream by `(frame_off, frame_len)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SubDesc {
    /// 1 = frame, 2 = inline-control, 0xFFFF_FFFF = stop (T0 shutdown).
    pub kind: u32,
    /// 0 = fire-and-forget; else the completion key.
    pub fence_id: u32,
    /// Offset of the FrameOp stream in the frame-staging arena.
    pub frame_off: u32,
    /// Bytes of FrameOp stream.
    pub frame_len: u32,
    /// e.g. NEEDS_READBACK.
    pub flags: u32,
}

impl SubDesc {
    /// A frame submission (references a FrameOp stream).
    pub const KIND_FRAME: u32 = 1;
    /// T0 shutdown sentinel — host breaks its serve-loop.
    pub const KIND_STOP: u32 = 0xFFFF_FFFF;

    fn write_to(&self, dst: &mut [u8; DESC_SIZE]) {
        dst.fill(0);
        dst[0..4].copy_from_slice(&self.kind.to_le_bytes());
        dst[4..8].copy_from_slice(&self.fence_id.to_le_bytes());
        dst[8..12].copy_from_slice(&self.frame_off.to_le_bytes());
        dst[12..16].copy_from_slice(&self.frame_len.to_le_bytes());
        dst[16..20].copy_from_slice(&self.flags.to_le_bytes());
    }
    fn read_from(src: &[u8; DESC_SIZE]) -> Self {
        let u = |o: usize| u32::from_le_bytes(src[o..o + 4].try_into().unwrap());
        SubDesc { kind: u(0), fence_id: u(4), frame_off: u(8), frame_len: u(12), flags: u(16) }
    }
}

/// Completion descriptor (host → guest). 64 bytes in the ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CompDesc {
    /// 1 = frame-done, 2 = error.
    pub kind: u32,
    /// Matches `SubDesc.fence_id`.
    pub fence_id: u32,
    /// 0 = ok; else error code.
    pub result: u32,
    /// If NEEDS_READBACK: where the bytes landed.
    pub readback_off: u32,
    /// If NEEDS_READBACK: byte length of the readback.
    pub readback_len: u32,
}

impl CompDesc {
    /// Frame completed successfully.
    pub const KIND_FRAME_DONE: u32 = 1;
    /// Frame failed; `result` carries the error code.
    pub const KIND_ERROR: u32 = 2;

    fn write_to(&self, dst: &mut [u8; DESC_SIZE]) {
        dst.fill(0);
        dst[0..4].copy_from_slice(&self.kind.to_le_bytes());
        dst[4..8].copy_from_slice(&self.fence_id.to_le_bytes());
        dst[8..12].copy_from_slice(&self.result.to_le_bytes());
        dst[12..16].copy_from_slice(&self.readback_off.to_le_bytes());
        dst[16..20].copy_from_slice(&self.readback_len.to_le_bytes());
    }
    fn read_from(src: &[u8; DESC_SIZE]) -> Self {
        let u = |o: usize| u32::from_le_bytes(src[o..o + 4].try_into().unwrap());
        CompDesc { kind: u(0), fence_id: u(4), result: u(8), readback_off: u(12), readback_len: u(16) }
    }
}

/// A `MAP_SHARED` mapping of the Carillon shared-memory region, backed
/// by a file both sides open. Created by the host; opened by the guest
/// (in T0, the loopback test).
pub struct Region {
    ptr: *mut u8,
    len: usize,
    _file: std::fs::File,
}

// The mapping is plain shared memory; cross-thread/process coordination
// goes through the atomic ring indices. Send is sound (the pointer is a
// stable mapping for the Region's lifetime).
unsafe impl Send for Region {}
unsafe impl Sync for Region {}

impl Region {
    /// Create (truncate) the backing file to `len` and map it. Used by
    /// the host; zeroes + stamps the control page.
    pub fn create(path: &std::path::Path, len: usize) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.set_len(len as u64)?;
        let r = Self::map(file, len)?;
        r.init_control();
        Ok(r)
    }

    /// Open an existing backing file and map it. Used by the guest side.
    pub fn open(path: &std::path::Path, len: usize) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Self::map(file, len)
    }

    /// Map an already-open shared-memory fd (e.g. the one QEMU's
    /// ivshmem-server hands the host via SCM_RIGHTS — see [`recv_fd`]).
    /// Takes ownership of `fd`. Does **not** stamp the control page (the
    /// region already exists; call [`validate_header`](Self::validate_header)).
    ///
    /// # Safety
    /// `fd` must be a valid, owned file/shm descriptor at least `len`
    /// bytes long and not used elsewhere.
    pub unsafe fn from_raw_fd(fd: RawFd, len: usize) -> io::Result<Self> {
        use std::os::unix::io::FromRawFd;
        let file = std::fs::File::from_raw_fd(fd);
        Self::map(file, len)
    }

    fn map(file: std::fs::File, len: usize) -> io::Result<Self> {
        // SAFETY: fd is valid for the file's lifetime (we keep `file`);
        // len matches the file size.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Region { ptr: ptr as *mut u8, len, _file: file })
    }

    /// Stamp magic/abi/host_page_size and zero the ring indices +
    /// status fields. Host-only.
    pub fn init_control(&self) {
        self.ctrl(C_MAGIC).store(MAGIC, Ordering::Relaxed);
        self.ctrl(C_ABI).store(ABI_VERSION, Ordering::Relaxed);
        self.ctrl(C_HOST_PAGE_SIZE).store(host_page_size(), Ordering::Relaxed);
        for off in [C_SUB_WRITE, C_SUB_READ, C_COMP_WRITE, C_COMP_READ,
                    C_HOST_STATUS, C_GUEST_STATUS] {
            self.ctrl(off).store(0, Ordering::Relaxed);
        }
        self.ctrl_u64(C_CAPS).store(0, Ordering::Relaxed);
        std::sync::atomic::fence(Ordering::Release);
    }

    #[inline]
    fn ctrl(&self, off: usize) -> &AtomicU32 {
        debug_assert!(off + 4 <= 0x1000 && off % 4 == 0);
        // SAFETY: control page is within the mapping; off is 4-aligned.
        unsafe { AtomicU32::from_ptr(self.ptr.add(CTRL_OFFSET + off) as *mut u32) }
    }
    #[inline]
    fn ctrl_u64(&self, off: usize) -> &AtomicU64 {
        debug_assert!(off + 8 <= 0x1000 && off % 8 == 0);
        unsafe { AtomicU64::from_ptr(self.ptr.add(CTRL_OFFSET + off) as *mut u64) }
    }

    /// Check the magic stamp matches (call after `open`).
    pub fn validate_header(&self) -> io::Result<()> {
        let magic = self.ctrl(C_MAGIC).load(Ordering::Acquire);
        if magic != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("carillon: bad magic {magic:#010x} (want {MAGIC:#010x})"),
            ));
        }
        Ok(())
    }

    /// Set the host-status field (0=down, 1=ready).
    pub fn set_host_status(&self, v: u32) { self.ctrl(C_HOST_STATUS).store(v, Ordering::Release); }
    /// Set the guest-status field (0=down, 1=booted).
    pub fn set_guest_status(&self, v: u32) { self.ctrl(C_GUEST_STATUS).store(v, Ordering::Release); }
    /// Read the host-page-size field published in the control page.
    pub fn host_page_size_field(&self) -> u32 { self.ctrl(C_HOST_PAGE_SIZE).load(Ordering::Acquire) }

    /// Host read of a staged FrameOp stream: `frame_off` is relative to
    /// the frame arena, `len` bytes. Returns an empty slice if the range
    /// is out of bounds (a malformed descriptor never reads past the
    /// mapping — part of the trust boundary).
    pub fn frame_bytes(&self, frame_off: u32, len: u32) -> &[u8] {
        let off = FRAME_ARENA_OFFSET + frame_off as usize;
        let len = len as usize;
        if frame_off as usize + len > FRAME_ARENA_BYTES {
            return &[];
        }
        // SAFETY: bounds-checked into the frame arena within the mapping;
        // the slice borrows the live mapping for `&self`.
        unsafe { std::slice::from_raw_parts(self.ptr.add(off), len) }
    }

    /// Guest write of a FrameOp stream into the arena at `frame_off`.
    /// Panics on overflow (the guest must size its staging correctly).
    pub fn stage_frame(&self, frame_off: u32, bytes: &[u8]) {
        let off = FRAME_ARENA_OFFSET + frame_off as usize;
        assert!(frame_off as usize + bytes.len() <= FRAME_ARENA_BYTES,
            "carillon: frame stage overflows arena");
        // SAFETY: bounds-checked; dst within the mapping.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.ptr.add(off), bytes.len());
        }
    }

    /// Copy a descriptor's 64 bytes into the ring at `byte_off`.
    #[inline]
    unsafe fn write_desc(&self, byte_off: usize, desc: &[u8; DESC_SIZE]) {
        std::ptr::copy_nonoverlapping(desc.as_ptr(), self.ptr.add(byte_off), DESC_SIZE);
    }
    #[inline]
    unsafe fn read_desc(&self, byte_off: usize) -> [u8; DESC_SIZE] {
        let mut buf = [0u8; DESC_SIZE];
        std::ptr::copy_nonoverlapping(self.ptr.add(byte_off), buf.as_mut_ptr(), DESC_SIZE);
        buf
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: ptr/len came from a successful mmap.
        unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.len); }
    }
}

/// The host's queried host-page size (16384 on Apple silicon, 4096
/// elsewhere). Published to the guest in the control page.
pub fn host_page_size() -> u32 {
    // SAFETY: sysconf is always callable.
    let v = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if v > 0 { v as u32 } else { 4096 }
}

/// Ring a doorbell: one 8-byte write to a pipe/eventfd write end.
fn write_token(write_fd: RawFd) {
    let token: u64 = 1;
    // SAFETY: write_fd is a valid pipe/eventfd write end.
    let _ = unsafe {
        libc::write(write_fd, &token as *const u64 as *const libc::c_void, 8)
    };
}

/// Bytes currently readable on `fd` (FIONREAD), 0 on error/none.
fn fd_pending(fd: RawFd) -> usize {
    let mut n: libc::c_int = 0;
    // SAFETY: fd is a valid descriptor; n is owned.
    if unsafe { libc::ioctl(fd, libc::FIONREAD, &mut n) } == 0 && n > 0 {
        n as usize
    } else {
        0
    }
}

/// Drain all currently-readable tokens on `fd` without blocking (call
/// after a [`Waiter`] reports it ready). Returns tokens drained.
fn drain_fd(read_fd: RawFd) -> u64 {
    let mut buf = [0u8; 4096];
    let mut drained = 0u64;
    while fd_pending(read_fd) > 0 {
        // SAFETY: read_fd valid; only read what's already available.
        let m = unsafe {
            libc::read(read_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
        };
        if m <= 0 {
            break;
        }
        drained += (m as usize / 8).max(1) as u64;
    }
    drained
}

/// A pipe-backed doorbell. `signal` writes an 8-byte token; `wait`
/// blocks in `read` until signalled (a true sleep — the no-spin
/// invariant) and drains all pending tokens so one `wait` consumes a
/// coalesced burst. In T0 the loopback test creates a pair and hands
/// each side the appropriate ends; under real QEMU these fds arrive via
/// the ivshmem-server SCM_RIGHTS handshake (T-real).
pub struct Doorbell {
    read_fd: RawFd,
    write_fd: RawFd,
}

impl Doorbell {
    /// Create a fresh pipe-backed doorbell.
    pub fn new() -> io::Result<Self> {
        let mut fds = [0 as RawFd; 2];
        // SAFETY: fds is a valid 2-element array.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Doorbell { read_fd: fds[0], write_fd: fds[1] })
    }

    /// The read end (wait/`kevent` side).
    pub fn read_fd(&self) -> RawFd { self.read_fd }
    /// The write end (signal side).
    pub fn write_fd(&self) -> RawFd { self.write_fd }

    /// Ring the doorbell. One non-blocking 8-byte write → the waiter's
    /// `read` becomes ready.
    pub fn signal(&self) {
        write_token(self.write_fd);
    }

    /// Drain all currently-readable tokens without blocking. Called by
    /// the host *after* the [`Waiter`] reports the read end ready, so a
    /// single wake clears a coalesced burst and the level-triggered
    /// `kevent`/`poll` doesn't immediately re-fire. Returns tokens drained.
    pub fn drain(&self) -> u64 {
        drain_fd(self.read_fd)
    }
}

/// Which source woke the [`Waiter`]. Both may be set if they fired in
/// the same `kevent`/`poll` return.
#[derive(Debug, Clone, Copy, Default)]
pub struct Wake {
    /// The guest→host doorbell is readable (drain + process the ring).
    pub doorbell: bool,
    /// The shutdown source fired (exit the serve-loop).
    pub shutdown: bool,
}

/// Blocks the host serve-loop until the doorbell or the shutdown source
/// becomes readable. On BSD/Darwin this is **kqueue** (`EVFILT_READ` on
/// both fds) — the platform's native multiplexer and the substrate the
/// real QEMU attach (T-real) extends to also watch the ivshmem-server
/// listen socket. On other unix (Linux CI) it falls back to `poll(2)`.
/// Either way the thread sleeps in the kernel — no spin.
pub struct Waiter {
    doorbell_fd: RawFd,
    shutdown_fd: RawFd,
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd",
              target_os = "dragonfly", target_os = "openbsd", target_os = "netbsd"))]
    kq: RawFd,
}

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd",
          target_os = "dragonfly", target_os = "openbsd", target_os = "netbsd"))]
impl Waiter {
    /// Build a kqueue watching both read fds for `EVFILT_READ`.
    pub fn new(doorbell_fd: RawFd, shutdown_fd: RawFd) -> io::Result<Self> {
        // SAFETY: kqueue takes no args.
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return Err(io::Error::last_os_error());
        }
        let w = Waiter { doorbell_fd, shutdown_fd, kq };
        w.register(doorbell_fd)?;
        w.register(shutdown_fd)?;
        Ok(w)
    }

    fn register(&self, fd: RawFd) -> io::Result<()> {
        // SAFETY: zeroed kevent is valid (null udata); ext[] (on the
        // BSDs) is left zero.
        let mut kev: libc::kevent = unsafe { std::mem::zeroed() };
        kev.ident = fd as usize;
        kev.filter = libc::EVFILT_READ;
        kev.flags = libc::EV_ADD | libc::EV_ENABLE;
        let r = unsafe {
            libc::kevent(self.kq, &kev, 1, std::ptr::null_mut(), 0, std::ptr::null())
        };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Block until at least one watched fd is readable.
    pub fn wait(&self) -> io::Result<Wake> {
        let mut evs: [libc::kevent; 4] = unsafe { std::mem::zeroed() };
        // SAFETY: evs is owned; null timeout = block forever.
        let n = unsafe {
            libc::kevent(self.kq, std::ptr::null(), 0, evs.as_mut_ptr(),
                         evs.len() as libc::c_int, std::ptr::null())
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut wake = Wake::default();
        for ev in evs.iter().take(n as usize) {
            let id = ev.ident as RawFd;
            if id == self.doorbell_fd {
                wake.doorbell = true;
            } else if id == self.shutdown_fd {
                wake.shutdown = true;
            }
        }
        Ok(wake)
    }
}

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd",
          target_os = "dragonfly", target_os = "openbsd", target_os = "netbsd"))]
impl Drop for Waiter {
    fn drop(&mut self) {
        // SAFETY: kq owned by this Waiter.
        unsafe { libc::close(self.kq); }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "freebsd",
              target_os = "dragonfly", target_os = "openbsd", target_os = "netbsd")))]
impl Waiter {
    /// Fallback multiplexer for non-kqueue unix (Linux CI): `poll(2)`.
    pub fn new(doorbell_fd: RawFd, shutdown_fd: RawFd) -> io::Result<Self> {
        Ok(Waiter { doorbell_fd, shutdown_fd })
    }

    /// Block until at least one watched fd is readable.
    pub fn wait(&self) -> io::Result<Wake> {
        let mut fds = [
            libc::pollfd { fd: self.doorbell_fd, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: self.shutdown_fd, events: libc::POLLIN, revents: 0 },
        ];
        // SAFETY: fds is a valid 2-element array; -1 = block forever.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Wake {
            doorbell: fds[0].revents & libc::POLLIN != 0,
            shutdown: fds[1].revents & libc::POLLIN != 0,
        })
    }
}

/// A `Send` handle that wakes a [`Host`]'s serve-loop to exit. Backed by
/// the write end of the host's shutdown self-pipe; the host owns/closes
/// the pipe, so the handle must not outlive the host.
pub struct ShutdownHandle {
    write_fd: RawFd,
}

// SAFETY: the handle only ever writes an 8-byte token to a pipe fd that
// the Host keeps open for the serve-loop's lifetime.
unsafe impl Send for ShutdownHandle {}

impl ShutdownHandle {
    /// Wake the serve-loop and ask it to exit (idempotent).
    pub fn shutdown(&self) {
        let token: u64 = 1;
        // SAFETY: write_fd is a valid pipe write end held by the Host.
        let _ = unsafe {
            libc::write(self.write_fd, &token as *const u64 as *const libc::c_void, 8)
        };
    }
}

impl Drop for Doorbell {
    fn drop(&mut self) {
        // SAFETY: fds owned by this Doorbell.
        unsafe {
            libc::close(self.read_fd);
            libc::close(self.write_fd);
        }
    }
}

/// The host serve-loop: sleeps on a kqueue (or `poll`) multiplexing the
/// guest→host doorbell and an out-of-band shutdown source; on a doorbell
/// wake it drains the submission ring, runs `handler` per frame, writes
/// completions, and rings the host→guest doorbell **once per drained
/// batch** (coalesced). Never spins.
pub struct Host {
    region: Region,
    g2h: Doorbell,      // host waits on this (read end live here)
    h2g: Doorbell,      // host signals this (write end live here)
    shutdown: Doorbell, // out-of-band wake to exit serve()
    waiter: Waiter,
    wakeups: u64,
    frames: u64,
}

impl Host {
    /// Build a host endpoint over a mapped region and the two doorbells
    /// (g2h: the host waits on it; h2g: the host signals it). Creates the
    /// shutdown self-pipe + the kqueue/poll waiter multiplexing both, and
    /// marks the control page host-status ready.
    pub fn new(region: Region, g2h: Doorbell, h2g: Doorbell) -> io::Result<Self> {
        let shutdown = Doorbell::new()?;
        let waiter = Waiter::new(g2h.read_fd(), shutdown.read_fd())?;
        region.set_host_status(1);
        Ok(Host { region, g2h, h2g, shutdown, waiter, wakeups: 0, frames: 0 })
    }

    /// Doorbell fds the guest side needs (g2h write end to ring the host,
    /// h2g read end to wait on completions).
    pub fn guest_doorbell_fds(&self) -> (RawFd, RawFd) {
        (self.g2h.write_fd(), self.h2g.read_fd())
    }

    /// A `Send` handle to stop the serve-loop from another thread.
    pub fn shutdown_handle(&self) -> ShutdownHandle {
        ShutdownHandle { write_fd: self.shutdown.write_fd() }
    }

    /// Number of doorbell wake-ups the serve-loop took (no-spin metric).
    /// The shutdown wake is not counted.
    pub fn wakeups(&self) -> u64 { self.wakeups }
    /// Number of frame submissions processed (excludes STOP).
    pub fn frames_processed(&self) -> u64 { self.frames }

    /// Serve until the [`ShutdownHandle`] fires (or a `SubDesc` with
    /// `KIND_STOP` is drained). `handler` receives each submitted frame's
    /// descriptor and its staged FrameOp byte stream (read from the frame
    /// arena) and returns the completion — e.g. an echo/ack, or a real
    /// `Backend::submit_frame` dispatch.
    pub fn serve<H: FnMut(&SubDesc, &[u8]) -> CompDesc>(&mut self, mut handler: H) -> io::Result<()> {
        loop {
            let wake = self.waiter.wait()?;

            let mut stop = wake.shutdown;
            if wake.doorbell {
                // Clear the coalesced doorbell tokens for this wake, then
                // drain everything currently visible in the submission ring.
                self.g2h.drain();
                self.wakeups += 1;
                let (completed_any, sub_stop) =
                    drain_submission_ring(&self.region, &mut handler, &mut self.frames);
                if sub_stop {
                    stop = true;
                }
                if completed_any {
                    // One coalesced doorbell for the whole drained batch.
                    self.h2g.signal();
                }
            }
            if stop {
                self.shutdown.drain();
                break;
            }
        }
        self.region.set_host_status(0);
        Ok(())
    }
}

/// Push one completion descriptor onto the completion ring (host side).
fn push_completion(region: &Region, comp: &CompDesc) {
    let w = region.ctrl(C_COMP_WRITE).load(Ordering::Relaxed);
    let idx = (w % COMP_ENTRIES) as usize;
    let off = COMP_RING_OFFSET + idx * DESC_SIZE;
    let mut raw = [0u8; DESC_SIZE];
    comp.write_to(&mut raw);
    // SAFETY: idx < COMP_ENTRIES, off within completion ring.
    unsafe { region.write_desc(off, &raw); }
    // Publish: advance write index with Release after the bytes land.
    region.ctrl(C_COMP_WRITE).store(w.wrapping_add(1), Ordering::Release);
}

/// Drain every submission descriptor currently visible, dispatching each
/// frame through `handler` and pushing its completion. Returns
/// `(completed_any, stop)` — `stop` set if a `KIND_STOP` descriptor was
/// seen. Shared by [`Host::serve`] and [`serve_ivshmem`].
fn drain_submission_ring<H: FnMut(&SubDesc, &[u8]) -> CompDesc>(
    region: &Region,
    handler: &mut H,
    frames: &mut u64,
) -> (bool, bool) {
    let mut completed_any = false;
    let mut stop = false;
    loop {
        let w = region.ctrl(C_SUB_WRITE).load(Ordering::Acquire);
        let r = region.ctrl(C_SUB_READ).load(Ordering::Relaxed);
        if r == w {
            break;
        }
        let idx = (r % SUB_ENTRIES) as usize;
        let off = SUB_RING_OFFSET + idx * DESC_SIZE;
        // SAFETY: idx < SUB_ENTRIES, off within submission ring.
        let raw = unsafe { region.read_desc(off) };
        let sub = SubDesc::read_from(&raw);
        // Advance read index (consumer) with Release so the guest sees
        // the slot freed.
        region.ctrl(C_SUB_READ).store(r.wrapping_add(1), Ordering::Release);

        if sub.kind == SubDesc::KIND_STOP {
            stop = true;
            continue;
        }
        *frames += 1;
        let comp = {
            let bytes = region.frame_bytes(sub.frame_off, sub.frame_len);
            handler(&sub, bytes)
        };
        push_completion(region, &comp);
        completed_any = true;
    }
    (completed_any, stop)
}

/// Serve a frame stream arriving over a real QEMU [`IvshmemServer`]
/// attach: block on the guest→host doorbell (kqueue/poll), batch-drain
/// the submission ring, dispatch each frame through `handler`, and ring
/// the guest once per batch via `notify_peer`. Exits when `shutdown_fd`
/// (a self-pipe read end) fires or a `KIND_STOP` descriptor arrives.
/// Returns `(wakeups, frames_processed)`.
///
/// This is the daemon's serve-loop for the VM path — it borrows the
/// server (which owns the region + doorbell fds), so no ownership is
/// transferred and `notify_peer` / `try_accept` stay on the server.
pub fn serve_ivshmem<H: FnMut(&SubDesc, &[u8]) -> CompDesc>(
    server: &IvshmemServer,
    shutdown_fd: RawFd,
    mut handler: H,
) -> io::Result<(u64, u64)> {
    let waiter = Waiter::new(server.doorbell_read_fd(), shutdown_fd)?;
    let mut wakeups = 0u64;
    let mut frames = 0u64;
    loop {
        let wake = waiter.wait()?;
        let mut stop = wake.shutdown;
        if wake.doorbell {
            drain_fd(server.doorbell_read_fd());
            wakeups += 1;
            let (completed_any, sub_stop) =
                drain_submission_ring(server.region(), &mut handler, &mut frames);
            if sub_stop {
                stop = true;
            }
            if completed_any {
                server.notify_peer();
            }
        }
        if stop {
            drain_fd(shutdown_fd);
            break;
        }
    }
    Ok((wakeups, frames))
}

/// Reference guest-side ring driver. In production the FreeBSD kmod (C)
/// mirrors exactly this protocol; here it backs the T0 loopback test and
/// documents the canonical guest sequence (submit + ring + park-on-
/// completion-doorbell + drain). Never spins on the rings.
pub struct GuestRing {
    region: Region,
    g2h_write_fd: RawFd, // ring the host
    h2g_read_fd: RawFd,  // wait on completions
}

impl GuestRing {
    /// `g2h_write_fd` rings the host; `h2g_read_fd` is parked on for
    /// completions. (In T0 these come from the test; under QEMU they are
    /// the MSI-X-backed doorbell fds.)
    pub fn new(region: Region, g2h_write_fd: RawFd, h2g_read_fd: RawFd) -> Self {
        region.set_guest_status(1);
        GuestRing { region, g2h_write_fd, h2g_read_fd }
    }

    /// Stage a serialized FrameOp stream into the frame arena at
    /// `frame_off` (then reference it via `SubDesc.frame_off/frame_len`).
    pub fn stage_frame(&self, frame_off: u32, bytes: &[u8]) {
        self.region.stage_frame(frame_off, bytes);
    }

    /// Enqueue a submission descriptor and ring the host doorbell. Does
    /// not wait — fire-and-forget unless the caller later parks on a
    /// completion via [`wait_completions`].
    pub fn submit(&self, sub: &SubDesc) {
        let w = self.region.ctrl(C_SUB_WRITE).load(Ordering::Relaxed);
        let idx = (w % SUB_ENTRIES) as usize;
        let off = SUB_RING_OFFSET + idx * DESC_SIZE;
        let mut raw = [0u8; DESC_SIZE];
        sub.write_to(&mut raw);
        // SAFETY: idx < SUB_ENTRIES.
        unsafe { self.region.write_desc(off, &raw); }
        self.region.ctrl(C_SUB_WRITE).store(w.wrapping_add(1), Ordering::Release);
    }

    /// Ring the host→submission doorbell (call after one or more
    /// [`submit`]s to wake the host once for the batch).
    pub fn ring(&self) {
        let token: u64 = 1;
        // SAFETY: g2h_write_fd is a valid pipe write end held by the test.
        let _ = unsafe {
            libc::write(self.g2h_write_fd, &token as *const u64 as *const libc::c_void, 8)
        };
    }

    /// Park on the completion doorbell (a blocking read — no spin), then
    /// drain every completion currently visible. Returns them in order.
    pub fn wait_completions(&self) -> io::Result<Vec<CompDesc>> {
        let mut tok = [0u8; 4096];
        // SAFETY: read end held by the test.
        let n = unsafe {
            libc::read(self.h2g_read_fd, tok.as_mut_ptr() as *mut libc::c_void, tok.len())
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(self.drain_completions())
    }

    /// Drain the completion ring without blocking.
    pub fn drain_completions(&self) -> Vec<CompDesc> {
        let mut out = Vec::new();
        loop {
            let w = self.region.ctrl(C_COMP_WRITE).load(Ordering::Acquire);
            let r = self.region.ctrl(C_COMP_READ).load(Ordering::Relaxed);
            if r == w {
                break;
            }
            let idx = (r % COMP_ENTRIES) as usize;
            let off = COMP_RING_OFFSET + idx * DESC_SIZE;
            // SAFETY: idx < COMP_ENTRIES.
            let raw = unsafe { self.region.read_desc(off) };
            out.push(CompDesc::read_from(&raw));
            self.region.ctrl(C_COMP_READ).store(r.wrapping_add(1), Ordering::Release);
        }
        out
    }
}

/// Send a file descriptor over a connected `AF_UNIX` socket via
/// `SCM_RIGHTS` (with a 1-byte data payload, required for the ancillary
/// message to be delivered). This is the mechanism QEMU's ivshmem-server
/// protocol uses to hand the host the shared-memory fd and the per-vector
/// doorbell fds (T-real attach); it mirrors `fresco/ivshmem_server.rs`'s
/// `send_i64_with_fd`. The received fd is a fresh descriptor in the peer
/// referring to the same open file description.
pub fn send_fd(sock: RawFd, fd: RawFd) -> io::Result<()> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr() as *mut libc::c_void,
        iov_len: 1,
    };
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut cmsg_buf = vec![0u8; space];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = space as _;
    // SAFETY: msg.msg_control points at a buffer sized via CMSG_SPACE;
    // we write exactly one fd into the first cmsg.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::copy_nonoverlapping(&fd, libc::CMSG_DATA(cmsg) as *mut RawFd, 1);
    }
    let n = unsafe { libc::sendmsg(sock, &msg, 0) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Receive a single file descriptor sent via [`send_fd`] over a connected
/// `AF_UNIX` socket. Returns a fresh owned descriptor.
pub fn recv_fd(sock: RawFd) -> io::Result<RawFd> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr() as *mut libc::c_void,
        iov_len: 1,
    };
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut cmsg_buf = vec![0u8; space];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = space as _;
    // SAFETY: msg buffers are owned; recvmsg writes into them.
    let n = unsafe { libc::recvmsg(sock, &mut msg, 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: inspect the first ancillary header for our SCM_RIGHTS fd.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null()
            || (*cmsg).cmsg_level != libc::SOL_SOCKET
            || (*cmsg).cmsg_type != libc::SCM_RIGHTS
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "carillon: recv_fd got no SCM_RIGHTS ancillary data",
            ));
        }
        let mut fd: RawFd = -1;
        std::ptr::copy_nonoverlapping(libc::CMSG_DATA(cmsg) as *const RawFd, &mut fd, 1);
        if fd < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "carillon: recv_fd got an invalid descriptor",
            ));
        }
        Ok(fd)
    }
}

/// QEMU ivshmem client-protocol version (the value QEMU's
/// `ivshmem_recv_setup` expects first).
pub const IVSHMEM_PROTOCOL_VERSION: i64 = 0;

/// Send an `i64` (optionally with an fd via SCM_RIGHTS) over a connected
/// `AF_UNIX` socket — the message shape QEMU's ivshmem-server protocol
/// uses (8-byte payload + optional ancillary fd). Mirrors
/// `fresco/ivshmem_server.rs`'s `send_i64`/`send_i64_with_fd`.
fn send_i64(sock: RawFd, val: i64, fd: Option<RawFd>) -> io::Result<()> {
    let data = val.to_le_bytes();
    let mut iov = libc::iovec {
        iov_base: data.as_ptr() as *mut libc::c_void,
        iov_len: 8,
    };
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;

    let mut cmsg_buf;
    if let Some(fd) = fd {
        let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
        cmsg_buf = vec![0u8; space];
        msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = space as _;
        // SAFETY: control buffer sized via CMSG_SPACE; one fd written.
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
            std::ptr::copy_nonoverlapping(&fd, libc::CMSG_DATA(cmsg) as *mut RawFd, 1);
        }
    }
    let n = unsafe { libc::sendmsg(sock, &msg, 0) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// The host endpoint that QEMU's `ivshmem-doorbell` device attaches to:
/// a 2-peer ivshmem-server (host GPU endpoint + QEMU). It creates the
/// shared-memory region and the doorbell pipes, hands QEMU the right fds
/// over `AF_UNIX`/SCM_RIGHTS at connect, and exposes the host-side ends
/// so a serve-loop can wait on the guest→host doorbell and ring the
/// host→guest doorbell. Faithful port of `fresco/ivshmem_server.rs`.
///
/// **Verification status.** Construction (shm create + map + stamp, pipe
/// creation, socket bind) is unit-tested host-side. The QEMU handshake
/// (`send_init` ordering) is ported verbatim from the working reference
/// and is exercised end-to-end only against a real QEMU — i.e. in the
/// `run-vm.sh --carillon` VM session (T-real), where `Host::serve` is
/// wired onto the exposed fds + region.
pub struct IvshmemServer {
    region: Region,
    shmem_fd: RawFd, // a 2nd fd of the shm file, passed to QEMU
    server_read_fd: RawFd,  // guest→host doorbell (host waits)
    server_write_fd: RawFd, // (handed to QEMU; QEMU writes → guest rang)
    qemu_read_fd: RawFd,    // (handed to QEMU; QEMU's interrupt fd)
    qemu_write_fd: RawFd,   // host→guest doorbell (host writes → MSI-X)
    listener: std::os::unix::net::UnixListener,
    sock_path: std::path::PathBuf,
    qemu_connected: bool,
}

// SAFETY: the contained fds + mapping are a stable owned bundle.
unsafe impl Send for IvshmemServer {}

impl IvshmemServer {
    /// Create the shm region (stamped), the two doorbell pipes, and bind
    /// the `AF_UNIX` listen socket QEMU connects to.
    pub fn new(
        sock_path: &std::path::Path,
        shmem_path: &std::path::Path,
        shmem_size: usize,
    ) -> io::Result<Self> {
        let _ = std::fs::remove_file(sock_path);
        // Host's mapped + stamped view.
        let region = Region::create(shmem_path, shmem_size)?;
        // A second fd of the same file to hand QEMU (it maps it for BAR2).
        let shmem_fd = {
            let f = OpenOptions::new().read(true).write(true).open(shmem_path)?;
            use std::os::unix::io::IntoRawFd;
            f.into_raw_fd()
        };
        let (server_read_fd, server_write_fd) = make_pipe()?;
        let (qemu_read_fd, qemu_write_fd) = make_pipe()?;
        let listener = std::os::unix::net::UnixListener::bind(sock_path)?;
        listener.set_nonblocking(true)?;
        Ok(IvshmemServer {
            region,
            shmem_fd,
            server_read_fd,
            server_write_fd,
            qemu_read_fd,
            qemu_write_fd,
            listener,
            sock_path: sock_path.to_path_buf(),
            qemu_connected: false,
        })
    }

    /// The host's mapped view of the shared region.
    pub fn region(&self) -> &Region { &self.region }

    /// Guest→host doorbell read end — register this on a [`Waiter`] /
    /// kqueue; QEMU writes it when the guest rings.
    pub fn doorbell_read_fd(&self) -> RawFd { self.server_read_fd }

    /// Host→guest doorbell write end (what [`notify_peer`] writes).
    /// [`notify_peer`]: Self::notify_peer
    pub fn doorbell_write_fd(&self) -> RawFd { self.qemu_write_fd }

    /// Ring the guest (host→guest doorbell → MSI-X in the guest).
    pub fn notify_peer(&self) {
        write_token(self.qemu_write_fd);
    }

    /// The QEMU-side fd ends, for **host-side testing only** — a harness
    /// that plays QEMU's doorbell relay: write `0` to ring the host
    /// (guest→host), read `1` to observe a host→guest ring. Under a real
    /// QEMU these ends live in QEMU and are driven by the guest's BAR0
    /// doorbell / MSI-X delivery. Returns `(ring_host_fd, guest_recv_fd)`.
    pub fn loopback_qemu_fds(&self) -> (RawFd, RawFd) {
        (self.server_write_fd, self.qemu_read_fd)
    }

    /// Whether QEMU has connected + been initialised.
    pub fn has_peer(&self) -> bool { self.qemu_connected }

    /// Non-blocking accept; on first QEMU connection, run the ivshmem
    /// init handshake. Returns true once a peer is connected.
    pub fn try_accept(&mut self) -> io::Result<bool> {
        if self.qemu_connected {
            return Ok(true);
        }
        match self.listener.accept() {
            Ok((stream, _)) => {
                self.send_init(&stream)?;
                self.qemu_connected = true;
                Ok(true)
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// The QEMU ivshmem init sequence (order must match QEMU's
    /// `ivshmem_recv_setup`): version, this peer's id, peer-0 connect fd
    /// (QEMU writes it to ring the host), the shmem fd (msg=-1 terminates
    /// the sync setup loop), then our own interrupt fd. Ported verbatim
    /// from `fresco/ivshmem_server.rs`.
    fn send_init(&self, stream: &std::os::unix::net::UnixStream) -> io::Result<()> {
        use std::os::unix::io::AsRawFd;
        let s = stream.as_raw_fd();
        send_i64(s, IVSHMEM_PROTOCOL_VERSION, None)?;
        send_i64(s, 1, None)?; // this peer's id
        send_i64(s, 0, Some(self.server_write_fd))?; // peer 0 connect (guest→host)
        send_i64(s, -1, Some(self.shmem_fd))?; // shmem; terminates sync loop
        send_i64(s, 1, Some(self.qemu_read_fd))?; // our own interrupt fd
        Ok(())
    }
}

impl Drop for IvshmemServer {
    fn drop(&mut self) {
        // SAFETY: all fds owned by this server.
        unsafe {
            libc::close(self.shmem_fd);
            libc::close(self.server_read_fd);
            libc::close(self.server_write_fd);
            libc::close(self.qemu_read_fd);
            libc::close(self.qemu_write_fd);
        }
        let _ = std::fs::remove_file(&self.sock_path);
    }
}

fn make_pipe() -> io::Result<(RawFd, RawFd)> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: fds is a valid 2-element array.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((fds[0], fds[1]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptors_roundtrip_bytes() {
        let s = SubDesc { kind: 1, fence_id: 7, frame_off: 0x1000, frame_len: 256, flags: 3 };
        let mut raw = [0u8; DESC_SIZE];
        s.write_to(&mut raw);
        assert_eq!(SubDesc::read_from(&raw), s);

        let c = CompDesc { kind: 1, fence_id: 7, result: 0, readback_off: 0x1000, readback_len: 256 };
        let mut raw = [0u8; DESC_SIZE];
        c.write_to(&mut raw);
        assert_eq!(CompDesc::read_from(&raw), c);
    }

    #[test]
    fn control_header_stamped_and_validated() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("carillon-hdr-{}.shm", std::process::id()));
        let r = Region::create(&path, TOTAL_SIZE).unwrap();
        r.validate_header().unwrap();
        assert_eq!(r.host_page_size_field(), host_page_size());
        drop(r);
        let _ = std::fs::remove_file(&path);
    }

    /// The SCM_RIGHTS mechanism QEMU's ivshmem-server uses to hand the
    /// host its doorbell fds: pass a pipe's write end over a socketpair
    /// and confirm the received descriptor drives the original pipe.
    #[test]
    fn scm_rights_passes_a_working_fd() {
        let mut sv = [0 as RawFd; 2];
        assert_eq!(
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sv.as_mut_ptr()) },
            0
        );
        let mut pfd = [0 as RawFd; 2];
        assert_eq!(unsafe { libc::pipe(pfd.as_mut_ptr()) }, 0);
        let (pipe_r, pipe_w) = (pfd[0], pfd[1]);

        // Send the pipe's write end across the socketpair.
        send_fd(sv[0], pipe_w).unwrap();
        let recv_w = recv_fd(sv[1]).unwrap();
        assert!(recv_w >= 0 && recv_w != pipe_w, "received a fresh descriptor");

        // Writing through the received fd must surface on the original
        // pipe's read end — same open file description.
        let token: u64 = 0xABCD;
        let n = unsafe {
            libc::write(recv_w, &token as *const u64 as *const libc::c_void, 8)
        };
        assert_eq!(n, 8);
        let mut got: u64 = 0;
        let m = unsafe {
            libc::read(pipe_r, &mut got as *mut u64 as *mut libc::c_void, 8)
        };
        assert_eq!(m, 8);
        assert_eq!(got, token, "bytes written via the passed fd reached the pipe");

        unsafe {
            libc::close(sv[0]);
            libc::close(sv[1]);
            libc::close(pipe_r);
            libc::close(pipe_w);
            libc::close(recv_w);
        }
    }

    /// The shmem fd QEMU passes is mapped via `Region::from_raw_fd`: a
    /// host stamps a region through one fd, a second fd of the same file
    /// (the "received" one) maps it and reads the control header.
    #[test]
    fn region_maps_from_a_received_fd() {
        use std::os::unix::io::IntoRawFd;
        let dir = std::env::temp_dir();
        let path = dir.join(format!("carillon-fd-{}.shm", std::process::id()));
        let host = Region::create(&path, TOTAL_SIZE).unwrap();
        host.validate_header().unwrap();

        // Open a *second* descriptor of the same file and map it as if it
        // had arrived over SCM_RIGHTS.
        let f = std::fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
        let raw = f.into_raw_fd();
        // SAFETY: raw is a valid owned fd of a TOTAL_SIZE-byte file.
        let mapped = unsafe { Region::from_raw_fd(raw, TOTAL_SIZE).unwrap() };
        mapped.validate_header().unwrap();
        assert_eq!(mapped.host_page_size_field(), host_page_size());

        drop(host);
        drop(mapped);
        let _ = std::fs::remove_file(&path);
    }

    /// The ivshmem-server host endpoint constructs cleanly: shm created +
    /// mapped + stamped, doorbell pipes valid, listen socket bound, no
    /// peer yet. (The QEMU handshake is verified in the VM session.)
    #[test]
    fn ivshmem_server_constructs() {
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let sock = dir.join(format!("carillon-srv-{pid}.sock"));
        let shm = dir.join(format!("carillon-srv-{pid}.shm"));
        let mut srv = IvshmemServer::new(&sock, &shm, TOTAL_SIZE).unwrap();

        srv.region().validate_header().unwrap();
        assert_eq!(srv.region().host_page_size_field(), host_page_size());
        assert!(srv.doorbell_read_fd() >= 0);
        assert!(!srv.has_peer());
        // No QEMU is connecting, so accept is WouldBlock → false.
        assert!(!srv.try_accept().unwrap());
        // Ringing the (unconnected) guest doorbell must not error.
        srv.notify_peer();

        drop(srv);
        let _ = std::fs::remove_file(&shm);
    }

    /// Drive `serve_ivshmem` end-to-end host-side by playing QEMU's
    /// doorbell relay: a guest opens the server's shm, stages + submits a
    /// frame, "rings" by writing the server-side relay fd, and reads back
    /// the completion when the host rings via notify_peer. Proves the
    /// server serve-loop (region + doorbell + notify) without QEMU; only
    /// the send_init handshake + real MSI-X remain for the VM session.
    #[test]
    fn serve_ivshmem_round_trips_a_frame() {
        use std::thread;

        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let sock = dir.join(format!("carillon-srvloop-{pid}.sock"));
        let shm = dir.join(format!("carillon-srvloop-{pid}.shm"));
        let server = IvshmemServer::new(&sock, &shm, TOTAL_SIZE).unwrap();

        // Guest mapping + the QEMU-relay fds (host-side stand-in for QEMU).
        let (ring_host_fd, guest_recv_fd) = server.loopback_qemu_fds();
        let guest_region = Region::open(&shm, TOTAL_SIZE).unwrap();
        guest_region.validate_header().unwrap();
        let guest = GuestRing::new(guest_region, ring_host_fd, guest_recv_fd);

        // Out-of-band shutdown for the serve loop.
        let shutdown = Doorbell::new().unwrap();
        let shutdown_fd = shutdown.read_fd();

        thread::scope(|s| {
            let h = s.spawn(|| {
                serve_ivshmem(&server, shutdown_fd, |sub: &SubDesc, _b: &[u8]| CompDesc {
                    kind: CompDesc::KIND_FRAME_DONE,
                    fence_id: sub.fence_id,
                    result: 0,
                    readback_off: sub.frame_off,
                    readback_len: sub.frame_len,
                })
                .unwrap()
            });

            // Guest stages a frame, submits, rings the host.
            guest.stage_frame(0, &[0xAA; 32]);
            guest.submit(&SubDesc {
                kind: SubDesc::KIND_FRAME,
                fence_id: 42,
                frame_off: 0,
                frame_len: 32,
                flags: 0,
            });
            guest.ring();

            let comps = guest.wait_completions().unwrap();
            assert_eq!(comps.len(), 1);
            assert_eq!(comps[0].fence_id, 42);
            assert_eq!(comps[0].kind, CompDesc::KIND_FRAME_DONE);

            // Stop the serve loop and confirm exactly one frame ran in one wake.
            shutdown.signal();
            let (wakeups, frames) = h.join().unwrap();
            assert_eq!(frames, 1);
            assert_eq!(wakeups, 1, "one doorbell wake — no spin");
        });

        drop(server);
        let _ = std::fs::remove_file(&shm);
    }
}
