//! The OS side of the split: the record channel (`SOCK_SEQPACKET`, fds by
//! `SCM_RIGHTS`) and the sealed shared memory the frame slots live in.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use crate::{Msg, Refused, MSG_BYTES};

/// Most fds one record carries (a window's two slots).
pub const MAX_FDS: usize = 2;

/// One end of a broker ⇄ renderer channel.
pub struct Chan(OwnedFd);

/// What [`Chan::recv`] got.
pub enum Recv {
    /// A record and the fds that came with it.
    Msg(Msg, Vec<OwnedFd>),
    /// A record that did not parse, or arrived truncated (its fds, if any,
    /// are already closed). The broker drops a renderer that sends one.
    Refused(Refused),
    /// The other end closed.
    Eof,
}

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(r) }
}

impl Chan {
    /// A connected pair.
    pub fn pair() -> io::Result<(Chan, Chan)> {
        let mut fds = [0; 2];
        // SAFETY: socketpair writes two fds into `fds`.
        cvt(unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0, fds.as_mut_ptr()) })?;
        // SAFETY: both fds are fresh and ours.
        Ok(unsafe { (Chan(OwnedFd::from_raw_fd(fds[0])), Chan(OwnedFd::from_raw_fd(fds[1]))) })
    }

    pub fn from_fd(fd: OwnedFd) -> Chan {
        Chan(fd)
    }

    pub fn into_fd(self) -> OwnedFd {
        self.0
    }

    /// End both directions: the other side reads end-of-file, and a
    /// thread blocked in [`Chan::recv`] here returns `Eof`.
    pub fn shutdown(&self) {
        // SAFETY: shutdown on our own socket.
        unsafe {
            libc::shutdown(self.0.as_raw_fd(), libc::SHUT_RDWR);
        }
    }

    pub fn try_clone(&self) -> io::Result<Chan> {
        Ok(Chan(self.0.try_clone()?))
    }

    /// Send one record, with `fds` attached.
    pub fn send(&self, m: &Msg, fds: &[RawFd]) -> io::Result<()> {
        assert!(fds.len() <= MAX_FDS);
        let rec = m.encode();
        let mut iov = libc::iovec { iov_base: rec.as_ptr() as *mut _, iov_len: rec.len() };
        let mut cbuf = [0u64; 8];
        // SAFETY: msghdr is plain data; every pointer in it outlives the call.
        let n = unsafe {
            let mut h: libc::msghdr = std::mem::zeroed();
            h.msg_iov = &mut iov;
            h.msg_iovlen = 1;
            if !fds.is_empty() {
                let len = libc::CMSG_SPACE((fds.len() * 4) as u32) as usize;
                h.msg_control = cbuf.as_mut_ptr() as *mut _;
                h.msg_controllen = len as _;
                let c = libc::CMSG_FIRSTHDR(&h);
                (*c).cmsg_level = libc::SOL_SOCKET;
                (*c).cmsg_type = libc::SCM_RIGHTS;
                (*c).cmsg_len = libc::CMSG_LEN((fds.len() * 4) as u32) as _;
                std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(c) as *mut RawFd, fds.len());
            }
            libc::sendmsg(self.0.as_raw_fd(), &h, libc::MSG_NOSIGNAL)
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n as usize != MSG_BYTES {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "short record"));
        }
        Ok(())
    }

    /// Wait for one record.
    pub fn recv(&self) -> io::Result<Recv> {
        let mut rec = [0u8; MSG_BYTES];
        let mut iov = libc::iovec { iov_base: rec.as_mut_ptr() as *mut _, iov_len: rec.len() };
        let mut cbuf = [0u64; 8];
        let mut fds = Vec::new();
        // SAFETY: as in `send`; the kernel fills `rec` and `cbuf`.
        let (n, flags) = unsafe {
            let mut h: libc::msghdr = std::mem::zeroed();
            h.msg_iov = &mut iov;
            h.msg_iovlen = 1;
            h.msg_control = cbuf.as_mut_ptr() as *mut _;
            h.msg_controllen = std::mem::size_of_val(&cbuf) as _;
            let n = loop {
                let n = libc::recvmsg(self.0.as_raw_fd(), &mut h, libc::MSG_CMSG_CLOEXEC);
                if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                break n;
            };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            // Take ownership of every fd that arrived, whatever happens next.
            let mut c = libc::CMSG_FIRSTHDR(&h);
            while !c.is_null() {
                if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                    let data = libc::CMSG_DATA(c) as *const RawFd;
                    let len = (*c).cmsg_len as usize - (data as usize - c as usize);
                    for i in 0..len / 4 {
                        fds.push(OwnedFd::from_raw_fd(std::ptr::read_unaligned(data.add(i))));
                    }
                }
                c = libc::CMSG_NXTHDR(&h, c);
            }
            (n as usize, h.msg_flags)
        };
        if n == 0 && fds.is_empty() {
            return Ok(Recv::Eof);
        }
        if flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 || n != MSG_BYTES {
            return Ok(Recv::Refused(Refused("record truncated or wrong size")));
        }
        if fds.len() > MAX_FDS {
            return Ok(Recv::Refused(Refused("too many fds")));
        }
        Ok(match Msg::decode(&rec) {
            Ok(m) => Recv::Msg(m, fds),
            Err(e) => Recv::Refused(e),
        })
    }
}

impl AsRawFd for Chan {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

/// A shared memory object mapped read-write. The broker creates it sealed
/// against shrinking and growing, so a renderer holding the fd cannot
/// truncate it under the broker's mapping (which would fault the broker),
/// and checks the seals of any it is handed.
pub struct Shm {
    fd: OwnedFd,
    ptr: *mut u8,
    len: usize,
}

// SAFETY: the mapping is plain shared memory; access goes through
// `as_slice`/`as_mut_slice`, whose callers treat the contents as racy.
unsafe impl Send for Shm {}

const SEALS: libc::c_int = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;

impl Shm {
    /// A new sealed object of `len` bytes.
    pub fn create(len: usize) -> io::Result<Shm> {
        // SAFETY: memfd_create takes a NUL-terminated name.
        let fd = cvt(unsafe { libc::memfd_create(c"fresco-slot".as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) })?;
        // SAFETY: fresh fd, ours.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: plain syscalls on our fd.
        unsafe {
            cvt(libc::ftruncate(fd.as_raw_fd(), len as libc::off_t))?;
            cvt(libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, SEALS))?;
        }
        Shm::map(fd, len)
    }

    /// Map an object someone handed us; it must be exactly `len` bytes and
    /// sealed against resizing.
    pub fn open(fd: OwnedFd, len: usize) -> io::Result<Shm> {
        // SAFETY: fstat and fcntl on an fd we own.
        let (size, seals) = unsafe {
            let mut st: libc::stat = std::mem::zeroed();
            cvt(libc::fstat(fd.as_raw_fd(), &mut st))?;
            (st.st_size as u64, cvt(libc::fcntl(fd.as_raw_fd(), libc::F_GET_SEALS))?)
        };
        if size != len as u64 || seals & SEALS != SEALS {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "shm: wrong size or not sealed"));
        }
        Shm::map(fd, len)
    }

    fn map(fd: OwnedFd, len: usize) -> io::Result<Shm> {
        // SAFETY: a shared mapping of `len` bytes of `fd`; unmapped in Drop.
        let p = unsafe {
            libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd.as_raw_fd(), 0)
        };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Shm { fd, ptr: p as *mut u8, len })
    }

    pub fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The bytes. Another process may write them at any time: read them
    /// once into memory of your own before checking anything
    /// ([`crate::slot::read`] does).
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: the mapping is `len` bytes for as long as `self` lives.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as above.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for Shm {
    fn drop(&mut self) {
        // SAFETY: our own mapping.
        unsafe {
            libc::munmap(self.ptr as *mut _, self.len);
        }
    }
}
