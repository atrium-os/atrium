//! fenestrad as `_fenestra` (fenestra.md §11.3).
//!
//! What needs root is done first, as root: the link to frescod is opened
//! (frescod checks the peer's uid when it connects) and the mint socket is
//! bound. Then fenestrad drops to its own user ([`drop_to`]) and everything
//! that reads a peer's bytes (mint requests, frescod's records, sessions'
//! records) runs there.
//!
//! Session jails need jaild, which takes only root callers. With `--jaild`,
//! fenestrad first forks ([`jailer::split`]): the root side keeps nothing
//! but jaild and a socket to the unprivileged side, and answers two fixed
//! 32-byte requests on it, `START` (with the session's link and mint
//! sockets attached) and `STOP PORT`. The session's arguments, its jail's
//! network and its uid are the root side's own; the unprivileged side can
//! ask for a jail but not say what goes in it, and it may hold at most
//! [`jailer::MAX_RUNNING`] at once. The root side ends with the other.

use std::ffi::CString;

/// A system user to run as.
#[derive(Clone, Debug)]
pub struct User {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
}

/// `name` from the password database. Call before any thread starts
/// (getpwnam is not thread-safe).
pub fn lookup(name: &str) -> Result<User, String> {
    let c = CString::new(name).map_err(|_| format!("{name}: not a user name"))?;
    // SAFETY: getpwnam with a NUL-terminated name; the record it returns is
    // read at once, before any other call that could reuse its buffer.
    let pw = unsafe { libc::getpwnam(c.as_ptr()) };
    if pw.is_null() {
        return Err(format!("no user {name}"));
    }
    // SAFETY: pw is non-null and points at a passwd record from libc.
    let (uid, gid) = unsafe { ((*pw).pw_uid, (*pw).pw_gid) };
    if uid == 0 || gid == 0 {
        return Err(format!("{name} is root"));
    }
    Ok(User { name: name.into(), uid, gid })
}

/// Become `u` for good: only its own group, then its gid, then its uid,
/// and check from the kernel that every id took and root is out of reach.
pub fn drop_to(u: &User) -> Result<(), String> {
    let g = u.gid as libc::gid_t;
    // SAFETY: plain syscalls on this process's credentials; setgroups reads
    // one gid from `g`.
    unsafe {
        if libc::setgroups(1, &g) != 0 {
            return Err(format!("setgroups: {}", std::io::Error::last_os_error()));
        }
        if libc::setgid(g) != 0 {
            return Err(format!("setgid: {}", std::io::Error::last_os_error()));
        }
        if libc::setuid(u.uid) != 0 {
            return Err(format!("setuid: {}", std::io::Error::last_os_error()));
        }
    }
    // SAFETY: getters of this process's credentials.
    let (ruid, euid, rgid, egid) = unsafe { (libc::getuid(), libc::geteuid(), libc::getgid(), libc::getegid()) };
    if (ruid, euid, rgid, egid) != (u.uid, u.uid, g, g) {
        return Err(format!("still uid {ruid}/{euid} gid {rgid}/{egid} after the drop"));
    }
    let mut groups = [0 as libc::gid_t; 64];
    // SAFETY: getgroups writes at most groups.len() gids.
    let n = unsafe { libc::getgroups(groups.len() as libc::c_int, groups.as_mut_ptr()) };
    if n < 0 || groups[..n as usize].iter().any(|&x| x != g) {
        return Err("supplementary groups left after the drop".into());
    }
    // SAFETY: an attempt that must fail.
    if unsafe { libc::setuid(0) } == 0 {
        return Err("root came back after the drop".into());
    }
    Ok(())
}

#[cfg(target_os = "freebsd")]
pub mod jailer {
    use std::collections::HashMap;
    use std::io;
    use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
    use std::sync::Mutex;

    use crate::jail::{Jails, Running};

    /// Every request and answer is exactly this long, NUL-padded.
    const MSG: usize = 32;
    /// Session jails the unprivileged side may hold at once: one with
    /// control, and one more while a take-over replaces it.
    pub const MAX_RUNNING: usize = 2;

    fn message(text: &str) -> [u8; MSG] {
        let mut m = [0u8; MSG];
        let b = text.as_bytes();
        m[..b.len().min(MSG)].copy_from_slice(&b[..b.len().min(MSG)]);
        m
    }

    fn text(m: &[u8]) -> Option<&str> {
        let end = m.iter().position(|&b| b == 0).unwrap_or(m.len());
        if m[end..].iter().any(|&b| b != 0) {
            return None;
        }
        std::str::from_utf8(&m[..end]).ok()
    }

    /// The unprivileged side's handle on the root side.
    pub struct Jailer {
        sock: Mutex<OwnedFd>,
    }

    impl Jailer {
        fn ask(&self, req: &str, fds: &[BorrowedFd<'_>]) -> io::Result<String> {
            let s = self.sock.lock().unwrap_or_else(|e| e.into_inner());
            let raw: Vec<i32> = fds.iter().map(|f| f.as_raw_fd()).collect();
            atrium_fdpass::send(s.as_raw_fd(), &message(req), &raw)?;
            let mut buf = [0u8; MSG];
            let r = atrium_fdpass::recv(s.as_raw_fd(), &mut buf, 0)?;
            if r.n != MSG {
                return Err(io::Error::other("the jailer went"));
            }
            text(&buf).map(str::to_string).ok_or_else(|| io::Error::other("a garbled answer from the jailer"))
        }

        /// A session in a new jail; its port.
        pub fn start(&self, link: BorrowedFd<'_>, mint: BorrowedFd<'_>) -> io::Result<u16> {
            let a = self.ask("START", &[link, mint])?;
            match a.strip_prefix("OK ").and_then(|p| p.parse().ok()) {
                Some(port) => Ok(port),
                None => Err(io::Error::other(format!("jailer: {a}"))),
            }
        }

        pub fn stop(&self, port: u16) {
            let _ = self.ask(&format!("STOP {port}"), &[]);
        }
    }

    /// Fork. The child returns, as the side that will drop to `_fenestra`;
    /// the parent stays root, serves it with `jails` (made after the fork,
    /// so its threads are the parent's alone) and exits when it goes.
    /// Call before any thread starts.
    pub fn split(make: impl FnOnce() -> Jails, args: Vec<String>) -> Jailer {
        let mut sv = [0i32; 2];
        // SAFETY: socketpair writes two descriptors into `sv`.
        if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0, sv.as_mut_ptr()) } != 0 {
            crate::die(format!("socketpair: {}", io::Error::last_os_error()));
        }
        // SAFETY: both are fresh descriptors of ours, each owned once.
        let (parent, child) = unsafe { (OwnedFd::from_raw_fd(sv[0]), OwnedFd::from_raw_fd(sv[1])) };
        // SAFETY: no thread has started (the caller's promise), so the
        // child may run ordinary code after fork.
        let pid = unsafe { libc::fork() };
        match pid {
            -1 => crate::die(format!("fork: {}", io::Error::last_os_error())),
            0 => {
                drop(parent);
                Jailer { sock: Mutex::new(child) }
            }
            _ => {
                drop(child);
                serve(parent.as_fd(), &make(), &args);
                let mut st = 0;
                // SAFETY: reaps our own child.
                unsafe { libc::waitpid(pid, &mut st, 0) };
                std::process::exit(0)
            }
        }
    }

    /// The root side: one request at a time until the other side goes.
    fn serve(sock: BorrowedFd<'_>, jails: &Jails, args: &[String]) {
        let mut running: HashMap<u16, Running> = HashMap::new();
        loop {
            let mut buf = [0u8; MSG];
            let got = match atrium_fdpass::recv(sock.as_raw_fd(), &mut buf, 2) {
                Ok(r) if r.n == MSG => r,
                Ok(r) if r.n == 0 => break,
                Ok(_) => {
                    eprintln!("fenestrad[jailer]: a short request; ending");
                    break;
                }
                Err(e) => {
                    eprintln!("fenestrad[jailer]: {e}; ending");
                    break;
                }
            };
            running.retain(|_, r| r.alive());
            let answer = match (text(&buf), got.fds.as_slice()) {
                (Some("START"), [link, mint]) if running.len() < MAX_RUNNING => match jails.start(link.as_fd(), mint.as_fd(), args) {
                    Ok(r) => {
                        let port = r.port;
                        running.insert(port, r);
                        format!("OK {port}")
                    }
                    Err(e) => {
                        eprintln!("fenestrad[jailer]: {e}");
                        "ERR the jail did not start".into()
                    }
                },
                (Some("START"), [_, _]) => "ERR too many sessions".into(),
                (Some(t), []) if t.starts_with("STOP ") => match t[5..].parse::<u16>().ok().and_then(|p| running.remove(&p)) {
                    Some(r) => {
                        r.kill();
                        "OK".into()
                    }
                    None => "ERR no such session".into(),
                },
                _ => {
                    eprintln!("fenestrad[jailer]: a malformed request; ending");
                    break;
                }
            };
            drop(got);
            if atrium_fdpass::send(sock.as_raw_fd(), &message(&answer), &[]).is_err() {
                break;
            }
        }
        for r in running.values() {
            r.kill();
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn messages_are_fixed_and_padded() {
            let m = message("STOP 24300");
            assert_eq!(text(&m), Some("STOP 24300"));
            let mut bad = m;
            bad[MSG - 1] = b'x';
            assert_eq!(text(&bad), None, "bytes after the NUL");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_and_unknown_users_are_refused() {
        assert!(lookup("root").is_err());
        assert!(lookup("no-such-user-here").is_err());
        assert!(lookup("bad\0name").is_err());
    }

    /// The drop in a forked child (it is for good): every id is the
    /// user's, and setuid(0) fails after it. Needs root; skipped without.
    #[test]
    fn the_drop_takes_and_holds() {
        // SAFETY: a getter.
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("skipped: not root");
            return;
        }
        let u = lookup("nobody").expect("a nobody user");
        // SAFETY: the child only makes syscalls and exits; the test
        // harness's other threads are not touched in it.
        match unsafe { libc::fork() } {
            0 => {
                let code = match drop_to(&u) {
                    // SAFETY: getters.
                    Ok(()) if unsafe { libc::getuid() } == u.uid && unsafe { libc::setgid(0) } != 0 => 0,
                    _ => 1,
                };
                // SAFETY: leave the child without running the harness's
                // destructors.
                unsafe { libc::_exit(code) }
            }
            pid => {
                let mut st = 0;
                // SAFETY: reaps our own child.
                unsafe { libc::waitpid(pid, &mut st, 0) };
                assert!(libc::WIFEXITED(st) && libc::WEXITSTATUS(st) == 0, "the drop did not hold");
            }
        }
    }
}
