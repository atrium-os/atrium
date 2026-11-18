//! A service's listening sockets (docs/spec/fast-boot.md §5).
//!
//! At boot the supervisor (atrium-portcullisd-bootstrap) binds each service's
//! `[[sockets]]` before it starts anything, and jaild hands them to the
//! service at fd 3, 4, … with `ATRIUM_LISTEN_FDS=n` and
//! `ATRIUM_LISTEN_PATHS=p1:p2:…` (each socket's path inside the jail). So
//! clients can connect before the service is up (they wait in the backlog)
//! and keep the same socket across a restart.
//!
//! [`listener`] gives the service the inherited socket for a path if it has
//! one, and otherwise binds the path itself, so the same code runs under the
//! supervisor, by hand, in tests and on macOS.

#![deny(unsafe_code)]

use std::io;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::Mutex;

/// Descriptors already handed out, so each is taken at most once.
static TAKEN: Mutex<Vec<i32>> = Mutex::new(Vec::new());

/// How the listener came about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Handed over by the supervisor: already bound, owned and moded by it.
    Inherited,
    /// Bound here (a stale socket at the path was replaced). The caller sets
    /// its mode as before.
    Bound,
}

/// The listening socket for `path`: the supervisor's, if it handed one over
/// for this path, else a fresh bind (removing a stale socket first).
pub fn listener(path: impl AsRef<Path>) -> io::Result<(UnixListener, Origin)> {
    let path = path.as_ref();
    if let Some(fd) = inherited(path, std::env::var("ATRIUM_LISTEN_FDS").ok().as_deref(),
                                std::env::var("ATRIUM_LISTEN_PATHS").ok().as_deref())
    {
        return Ok((UnixListener::from(fd), Origin::Inherited));
    }
    let _ = std::fs::remove_file(path);
    Ok((UnixListener::bind(path)?, Origin::Bound))
}

fn inherited(path: &Path, fds: Option<&str>, paths: Option<&str>) -> Option<OwnedFd> {
    inherited_from(3, path, fds, paths)
}

/// `first` is 3 in a service; tests use a high slot nothing else owns.
fn inherited_from(first: i32, path: &Path, fds: Option<&str>, paths: Option<&str>) -> Option<OwnedFd> {
    let n: usize = fds?.parse().ok()?;
    let i = paths?.split(':').take(n).position(|p| Path::new(p) == path)?;
    let fd = first + i as i32;
    let mut taken = TAKEN.lock().unwrap_or_else(|e| e.into_inner());
    if taken.contains(&fd) || !is_listening_socket(fd) {
        return None;
    }
    taken.push(fd);
    set_cloexec(fd);
    // SAFETY: jaild placed a listening socket at this slot for us, we checked
    // it is one, and TAKEN makes sure only one owner is ever created.
    #[allow(unsafe_code)]
    Some(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[allow(unsafe_code)]
fn is_listening_socket(fd: i32) -> bool {
    // SAFETY: fstat/getsockopt write into locals sized as passed.
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(fd, &mut st) != 0 || st.st_mode & libc::S_IFMT != libc::S_IFSOCK {
            return false;
        }
        let mut v: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_ACCEPTCONN,
                         (&mut v as *mut libc::c_int).cast(), &mut len) == 0 && v != 0
    }
}

/// The descriptor was dup2'd into place without close-on-exec; anything the
/// service starts must not inherit its listening socket.
#[allow(unsafe_code)]
fn set_cloexec(fd: i32) {
    // SAFETY: fcntl on a descriptor this process holds.
    unsafe {
        let f = libc::fcntl(fd, libc::F_GETFD);
        if f >= 0 {
            libc::fcntl(fd, libc::F_SETFD, f | libc::FD_CLOEXEC);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::{AsRawFd, IntoRawFd};

    #[test]
    fn binds_when_nothing_was_handed_over() {
        let d = std::env::temp_dir().join(format!("atrium-listen-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        let p = d.join("a.sock");
        std::fs::write(&p, b"").ok(); // stale file in the way
        let _ = std::fs::remove_file(&p);
        let (_l, o) = listener(&p).unwrap();
        assert_eq!(o, Origin::Bound);
        std::os::unix::net::UnixStream::connect(&p).unwrap();
    }

    #[test]
    fn takes_the_inherited_socket_once_and_only_if_it_listens() {
        let d = std::env::temp_dir().join(format!("atrium-listen-i-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        let host = d.join("h.sock");
        let _ = std::fs::remove_file(&host);
        let l = UnixListener::bind(&host).unwrap();
        // Put it in the second slot, as jaild would (slots 300, 301 here,
        // where no other test's descriptor can be).
        let raw = l.into_raw_fd();
        #[allow(unsafe_code)]
        unsafe { assert!(libc::dup2(raw, 301) == 301); libc::close(raw); }
        let names = Some("/in/a.sock:/in/b.sock");
        // Slot 300 holds nothing: refused, not adopted.
        assert!(inherited_from(300, Path::new("/in/a.sock"), Some("2"), names).is_none());
        let fd = inherited_from(300, Path::new("/in/b.sock"), Some("2"), names).unwrap();
        assert_eq!(fd.as_raw_fd(), 301);
        assert!(inherited_from(300, Path::new("/in/b.sock"), Some("2"), names).is_none());
        assert!(inherited_from(300, Path::new("/in/c.sock"), Some("2"), names).is_none());
        // Only the first n entries count.
        assert!(inherited_from(300, Path::new("/in/b.sock"), Some("1"), names).is_none());
        let l = UnixListener::from(fd);
        let _c = std::os::unix::net::UnixStream::connect(&host).unwrap();
        l.accept().unwrap();
    }
}
