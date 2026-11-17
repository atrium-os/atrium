//! A small spawner, forked before atrium-net enters capability mode, so
//! that a sandboxed helper (atrium-lease, atrium-probe) that dies can be
//! started again afterwards: capability mode forbids exec, and before this
//! a dead lease process made atrium-net exit (spec §10.1 known gap).
//!
//! The spawner keeps no state and reads one byte per request: which of the
//! programs fixed at fork time to start. It answers with one byte and, on
//! success, the parent's end of a fresh socketpair (SCM_RIGHTS); the
//! program gets the other end as fd 3 and nothing else of ours. Children
//! are reaped by the kernel (SIGCHLD ignored). The spawner exits when
//! atrium-net's end of its socket closes.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::os::unix::net::UnixStream;

pub struct Spawner {
    s: UnixStream,
    pid: libc::pid_t,
}

struct Prog {
    kind: u8,
    path: CString,
    argv: Vec<CString>,
}

fn close_from(n: RawFd) {
    #[cfg(target_os = "freebsd")]
    unsafe {
        libc::closefrom(n);
    }
    #[cfg(not(target_os = "freebsd"))]
    for fd in n..1024 {
        unsafe {
            libc::close(fd);
        }
    }
}

impl Spawner {
    /// Fork the spawner. `progs`: (request byte, program path, extra args).
    /// Call while the process is still single-threaded.
    pub fn start(progs: &[(u8, &str, &[&str])]) -> io::Result<Spawner> {
        let progs: Vec<Prog> = progs
            .iter()
            .map(|(kind, path, args)| {
                let path = CString::new(*path).map_err(|_| io::ErrorKind::InvalidInput)?;
                let mut argv = vec![path.clone()];
                for a in *args {
                    argv.push(CString::new(*a).map_err(|_| io::ErrorKind::InvalidInput)?);
                }
                Ok(Prog { kind: *kind, path, argv })
            })
            .collect::<io::Result<_>>()?;
        let (ours, theirs) = UnixStream::pair()?;
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(io::Error::last_os_error());
        }
        if pid == 0 {
            // The spawner: its socket as fd 3, stdio kept for messages.
            let fd = theirs.into_raw_fd();
            unsafe {
                if fd != 3 && libc::dup2(fd, 3) < 0 {
                    libc::_exit(1);
                }
                libc::signal(libc::SIGCHLD, libc::SIG_IGN);
            }
            close_from(4);
            serve(3, &progs);
        }
        drop(theirs);
        Ok(Spawner { s: ours, pid })
    }

    pub fn pid(&self) -> libc::pid_t {
        self.pid
    }

    /// Start program `kind`; our end of its socket.
    pub fn spawn(&self, kind: u8) -> io::Result<UnixStream> {
        let fd = self.s.as_raw_fd();
        if unsafe { libc::write(fd, (&kind as *const u8).cast(), 1) } != 1 {
            return Err(io::Error::last_os_error());
        }
        let (b, got) = recv_fd(fd)?;
        match (b, got) {
            (b'K', Some(f)) => Ok(unsafe { UnixStream::from_raw_fd(f) }),
            (_, f) => {
                if let Some(f) = f {
                    unsafe { libc::close(f) };
                }
                Err(io::Error::other("spawner could not start it"))
            }
        }
    }
}

fn serve(s: RawFd, progs: &[Prog]) -> ! {
    loop {
        let mut k = 0u8;
        let n = unsafe { libc::read(s, (&mut k as *mut u8).cast(), 1) };
        if n == 0 || (n < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted) {
            unsafe { libc::_exit(0) };
        }
        if n != 1 {
            continue;
        }
        let Some(p) = progs.iter().find(|p| p.kind == k) else {
            let _ = send_fd(s, b'E', None);
            continue;
        };
        let mut sv = [0 as RawFd; 2];
        if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sv.as_mut_ptr()) } != 0 {
            let _ = send_fd(s, b'E', None);
            continue;
        }
        let mut argv: Vec<*const libc::c_char> = p.argv.iter().map(|a| a.as_ptr()).collect();
        argv.push(std::ptr::null());
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            unsafe {
                // Default SIGCHLD for the program; its socket as fd 3.
                libc::signal(libc::SIGCHLD, libc::SIG_DFL);
                if libc::dup2(sv[1], 3) < 0 {
                    libc::_exit(127);
                }
            }
            close_from(4);
            unsafe {
                libc::execv(p.path.as_ptr(), argv.as_ptr());
                libc::_exit(127);
            }
        }
        unsafe { libc::close(sv[1]) };
        if pid < 0 {
            let _ = send_fd(s, b'E', None);
        } else {
            let _ = send_fd(s, b'K', Some(sv[0]));
        }
        unsafe { libc::close(sv[0]) };
    }
}

const CMSG_SPACE: usize = 32;

fn send_fd(s: RawFd, b: u8, fd: Option<RawFd>) -> io::Result<()> {
    let mut byte = b;
    let mut iov = libc::iovec { iov_base: (&mut byte as *mut u8).cast(), iov_len: 1 };
    let mut cbuf = [0u64; CMSG_SPACE / 8];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if let Some(fd) = fd {
        unsafe {
            msg.msg_control = cbuf.as_mut_ptr().cast();
            msg.msg_controllen = libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) as _;
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(c) as *mut RawFd, fd);
        }
    }
    if unsafe { libc::sendmsg(s, &msg, 0) } != 1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn recv_fd(s: RawFd) -> io::Result<(u8, Option<RawFd>)> {
    let mut byte = 0u8;
    let mut iov = libc::iovec { iov_base: (&mut byte as *mut u8).cast(), iov_len: 1 };
    let mut cbuf = [0u64; CMSG_SPACE / 8];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr().cast();
    msg.msg_controllen = CMSG_SPACE as _;
    let n = unsafe { libc::recvmsg(s, &mut msg, 0) };
    if n != 1 {
        return Err(if n == 0 { io::ErrorKind::UnexpectedEof.into() } else { io::Error::last_os_error() });
    }
    let mut fd = None;
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        if !c.is_null() && (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
            fd = Some(std::ptr::read_unaligned(libc::CMSG_DATA(c) as *const RawFd));
        }
    }
    Ok((byte, fd))
}

#[cfg(test)]
mod tests {
    use super::Spawner;
    use std::io::{Read, Write};

    #[test]
    fn spawns_again_and_again() {
        // A program that answers on fd 3 and then exits; and an unknown kind.
        let sp = Spawner::start(&[(b'L', "/bin/sh", &["-c", "read x <&3; echo \"got $x\" >&3"])]).unwrap();
        for i in 0..5 {
            let mut s = sp.spawn(b'L').unwrap();
            writeln!(s, "{i}").unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).unwrap();
            assert_eq!(out, format!("got {i}\n"));
        }
        assert!(sp.spawn(b'X').is_err());
        // The child sees only fds 0-3.
        let sp2 = Spawner::start(&[(b'L', "/bin/sh", &["-c", "ls /dev/fd/ | grep -c '^[0-9]' >&3"])]).unwrap();
        let mut s = sp2.spawn(b'L').unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        // 0, 1, 2, 3 and the directory ls is reading.
        let n: u32 = out.trim().parse().unwrap();
        assert!(n <= 5, "fds: {n}");
    }

    #[test]
    fn spawner_exits_when_dropped() {
        let sp = Spawner::start(&[(b'L', "/bin/true", &[])]).unwrap();
        let pid = sp.pid();
        drop(sp);
        let mut st = 0;
        let r = unsafe { libc::waitpid(pid, &mut st, 0) };
        assert_eq!(r, pid);
        assert!(libc::WIFEXITED(st) && libc::WEXITSTATUS(st) == 0);
    }
}
