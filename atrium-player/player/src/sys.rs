//! The supervisor's and worker's OS plumbing: framed sends with descriptors,
//! anonymous shared memory, and mappings. The only `unsafe` in this crate.
#![allow(unsafe_code)]

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

use crate::wire::{FDS_MAX, FRAME_MAX};

/// Send one frame with `fds` attached to its header.
pub fn send_frame(sock: &UnixStream, payload: &[u8], fds: &[RawFd]) -> io::Result<()> {
    assert!(fds.len() <= FDS_MAX && payload.len() <= FRAME_MAX);
    let mut hdr = [0u8; 8];
    hdr[..4].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    hdr[4..].copy_from_slice(&(fds.len() as u32).to_le_bytes());
    let mut iov = libc::iovec { iov_base: hdr.as_mut_ptr() as *mut _, iov_len: hdr.len() };
    let mut cmsgbuf = [0u8; 256];
    // SAFETY: msghdr is plain data; every pointer in it points at a live
    // local above, and the control buffer is big enough for FDS_MAX fds.
    unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        if !fds.is_empty() {
            let len = std::mem::size_of_val(fds) as u32;
            msg.msg_control = cmsgbuf.as_mut_ptr() as *mut _;
            msg.msg_controllen = libc::CMSG_SPACE(len) as _;
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(len) as _;
            std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(c) as *mut RawFd, fds.len());
        }
        let n = libc::sendmsg(sock.as_raw_fd(), &msg, 0);
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if (n as usize) < hdr.len() {
            (&*sock).write_all(&hdr[n as usize..])?;
        }
    }
    (&*sock).write_all(payload)
}

/// Receive one frame: (payload, descriptors). `Ok(None)` at end of stream.
pub fn recv_frame(sock: &UnixStream) -> io::Result<Option<(Vec<u8>, Vec<OwnedFd>)>> {
    let mut hdr = [0u8; 8];
    let mut fds = Vec::new();
    let mut iov = libc::iovec { iov_base: hdr.as_mut_ptr() as *mut _, iov_len: hdr.len() };
    let mut cmsgbuf = [0u8; 256];
    // SAFETY: as in send_frame; received descriptors are taken into OwnedFd
    // once each.
    let n = unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsgbuf.as_mut_ptr() as *mut _;
        msg.msg_controllen = cmsgbuf.len() as _;
        let n = libc::recvmsg(sock.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC);
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(c) as *const RawFd;
                let count = ((*c).cmsg_len as usize - (data as usize - c as usize)) / std::mem::size_of::<RawFd>();
                for i in 0..count {
                    fds.push(OwnedFd::from_raw_fd(*data.add(i)));
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
        n as usize
    };
    if n == 0 {
        return Ok(None);
    }
    if n < hdr.len() {
        (&*sock).read_exact(&mut hdr[n..])?;
    }
    let len = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
    let nfds = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]) as usize;
    if len > FRAME_MAX || nfds != fds.len() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad frame header"));
    }
    let mut payload = vec![0u8; len];
    (&*sock).read_exact(&mut payload)?;
    Ok(Some((payload, fds)))
}

/// An anonymous shared-memory object of `len` bytes.
pub fn shm_anon(len: usize) -> io::Result<OwnedFd> {
    // SAFETY: plain syscalls; the descriptor is owned on success.
    unsafe {
        #[cfg(target_os = "linux")]
        let fd = libc::memfd_create(c"atrium-player".as_ptr(), libc::MFD_CLOEXEC);
        #[cfg(target_os = "freebsd")]
        let fd = libc::shm_open(libc::SHM_ANON, libc::O_RDWR | libc::O_CLOEXEC, 0o600);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = OwnedFd::from_raw_fd(fd);
        if libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(fd)
    }
}

/// A shared read-write mapping of a whole shm object, unmapped on drop.
pub struct Map {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: the mapping is plain memory; concurrent access through it goes
// through atomics (the ring) or ownership hand-off over the socket (buffers).
unsafe impl Send for Map {}

impl Map {
    /// Map `fd`, checking it is exactly `len` bytes.
    pub fn new(fd: &OwnedFd, len: usize) -> io::Result<Map> {
        // SAFETY: fstat into a zeroed struct; mmap of a size we just checked.
        unsafe {
            let mut st: libc::stat = std::mem::zeroed();
            if libc::fstat(fd.as_raw_fd(), &mut st) != 0 {
                return Err(io::Error::last_os_error());
            }
            if st.st_size as u64 != len as u64 || len == 0 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "shared memory of the wrong size"));
            }
            let p = libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd.as_raw_fd(), 0);
            if p == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
            Ok(Map { ptr: p as *mut u8, len })
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The bytes. The other process may write them at any time, so a
    /// reader treats what it sees as hostile (fixed-size pixels, or ring
    /// fields it range-checks).
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: valid mapping of len bytes for our lifetime.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    pub fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, exclusive through &mut self in this process.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }

    /// An atomic u64 at byte offset `off` (8-aligned, in bounds).
    pub fn atomic_u64(&self, off: usize) -> &std::sync::atomic::AtomicU64 {
        assert!(off % 8 == 0 && off + 8 <= self.len);
        // SAFETY: aligned (mmap is page-aligned, off is 8-aligned), in
        // bounds, and only ever accessed atomically by both processes.
        unsafe { &*(self.ptr.add(off) as *const std::sync::atomic::AtomicU64) }
    }
}

impl Drop for Map {
    fn drop(&mut self) {
        // SAFETY: unmapping our own mapping once.
        unsafe {
            libc::munmap(self.ptr as *mut _, self.len);
        }
    }
}

/// Let the child `cmd` starts inherit `fd`, and only that child: the flag
/// is cleared after the fork, so workers started at the same time from
/// other threads don't get each other's sockets.
pub fn pass_fd(cmd: &mut std::process::Command, fd: RawFd) {
    use std::os::unix::process::CommandExt;
    // SAFETY: the closure runs in the child between fork and exec and calls
    // only fcntl, which is async-signal-safe.
    unsafe {
        cmd.pre_exec(move || {
            let f = libc::fcntl(fd, libc::F_GETFD);
            if f < 0 || libc::fcntl(fd, libc::F_SETFD, f & !libc::FD_CLOEXEC) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Take a descriptor number given on the command line.
///
/// # Safety
/// The caller must own `fd` (it was inherited for this purpose).
pub unsafe fn adopt(fd: RawFd) -> OwnedFd {
    unsafe { OwnedFd::from_raw_fd(fd) }
}

/// Cap the process's address space (worker setup, before the sandbox).
pub fn limit_memory(bytes: u64) -> io::Result<()> {
    let r = libc::rlimit { rlim_cur: bytes as _, rlim_max: bytes as _ };
    // SAFETY: plain syscall.
    if unsafe { libc::setrlimit(libc::RLIMIT_AS, &r) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Enter Capsicum capability mode (FreeBSD). Elsewhere a no-op that says so.
pub fn enter_sandbox() -> io::Result<bool> {
    #[cfg(target_os = "freebsd")]
    {
        extern "C" {
            fn cap_enter() -> libc::c_int;
        }
        // SAFETY: plain syscall; irreversible for this process.
        if unsafe { cap_enter() } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(true)
    }
    #[cfg(not(target_os = "freebsd"))]
    Ok(false)
}

/// Try to leave the sandbox three ways; a bit set per refusal (see
/// `Record::Probe`).
pub fn probe_sandbox() -> u32 {
    let mut refused = 0;
    // SAFETY: plain syscalls; a child, if one is made, exits at once.
    unsafe {
        let fd = libc::open(c"/etc/passwd".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
        if fd < 0 {
            refused |= 1;
        } else {
            libc::close(fd);
        }
        let s = libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        if s < 0 {
            refused |= 2;
        } else {
            let mut sa: libc::sockaddr_in = std::mem::zeroed();
            sa.sin_family = libc::AF_INET as _;
            sa.sin_port = 80u16.to_be();
            sa.sin_addr.s_addr = u32::from(std::net::Ipv4Addr::LOCALHOST).to_be();
            if libc::connect(s, &sa as *const _ as *const libc::sockaddr, std::mem::size_of_val(&sa) as _) != 0
                && io::Error::last_os_error().raw_os_error() != Some(libc::ECONNREFUSED)
            {
                refused |= 2;
            }
            libc::close(s);
        }
        let pid = libc::fork();
        if pid < 0 {
            refused |= 4;
        } else if pid == 0 {
            libc::_exit(0);
        } else {
            let mut st = 0;
            libc::waitpid(pid, &mut st, 0);
        }
    }
    refused
}

/// Kill and reap a child.
pub fn kill(pid: u32) {
    // SAFETY: plain syscall on our own child.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
}

/// What a descriptor handed to the worker may be used for.
#[derive(Clone, Copy, Debug)]
pub enum Rights {
    /// The `Load` input: read, seek, fstat.
    Input,
    /// Shared memory: map read-write, fstat.
    Shm,
    /// The worker's socket: read, write, poll.
    Socket,
}

/// A duplicate of `fd` that can do only `rights` (FreeBSD Capsicum
/// `cap_rights_limit`), for passing to the worker: rights travel with a
/// descriptor, so the worker can't widen them. Elsewhere a plain duplicate.
pub fn restricted(fd: &OwnedFd, rights: Rights) -> io::Result<OwnedFd> {
    let dup = fd.try_clone()?;
    limit(&dup, rights)?;
    Ok(dup)
}

/// Narrow `fd` itself to `rights`.
pub fn limit(fd: &OwnedFd, rights: Rights) -> io::Result<()> {
    #[cfg(target_os = "freebsd")]
    {
        let bits = match rights {
            Rights::Input => [libc::CAP_READ, libc::CAP_SEEK, libc::CAP_FSTAT],
            Rights::Shm => [libc::CAP_MMAP_RW, libc::CAP_FSTAT, libc::CAP_FSTAT],
            Rights::Socket => [libc::CAP_READ, libc::CAP_WRITE, libc::CAP_EVENT],
        };
        // SAFETY: __cap_rights_init takes a zero-terminated list of rights
        // into a cap_rights_t we own; cap_rights_limit only reads it.
        unsafe {
            let mut r: libc::cap_rights_t = std::mem::zeroed();
            libc::__cap_rights_init(libc::CAP_RIGHTS_VERSION, &mut r, bits[0], bits[1], bits[2], 0u64);
            if libc::cap_rights_limit(fd.as_raw_fd(), &r) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    #[cfg(not(target_os = "freebsd"))]
    let _ = (fd, rights);
    Ok(())
}

/// Whether `sock` has something to read within `ms` milliseconds.
pub fn readable(sock: &UnixStream, ms: i32) -> bool {
    let mut p = libc::pollfd { fd: sock.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    // SAFETY: one pollfd we own.
    unsafe { libc::poll(&mut p, 1, ms) > 0 }
}
