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
//!
//! [`ready`] tells the supervisor the service now serves (fast-boot.md §6.1),
//! for services whose manifest has `[ready] notify = true`: the ones other
//! services are `after`. Without a readiness pipe it does nothing.
//!
//! [`connect`] is the client side (fast-boot.md §7): connect to a service's
//! socket, and if it is not there yet, wait for it without polling (kqueue
//! EVFILT_VNODE on its directory on FreeBSD and macOS) until a deadline.

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

/// Bind a listening socket at `path` for someone else to serve on (the
/// `atrium-bind` helper): replaces a stale socket, but never a live one or a
/// file of another kind; then owner, group and mode.
pub fn bind_socket(path: &Path, mode: u32, owner: Option<u32>, group: Option<u32>) -> io::Result<UnixListener> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    if let Ok(md) = std::fs::symlink_metadata(path) {
        if !md.file_type().is_socket() {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "a file that is not a socket is in the way"));
        }
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return Err(io::Error::new(io::ErrorKind::AddrInUse, "a running daemon already serves it"));
        }
        std::fs::remove_file(path)?;
    }
    let l = UnixListener::bind(path)?;
    if owner.is_some() || group.is_some() {
        std::os::unix::fs::chown(path, owner, group)?;
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(l)
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

/// Connect to the Unix socket at `path`, waiting up to `deadline` for it to
/// appear or start accepting. Between attempts it sleeps on a kqueue
/// EVFILT_VNODE watch of the socket's directory (or the nearest directory
/// that exists), so it wakes the moment the service binds, and never longer
/// than 250 ms in case the service only started listening. `None` waits
/// forever. Errors other than "not there" / "refused" return at once; when
/// the deadline passes, the last error is returned.
pub fn connect(path: impl AsRef<Path>, deadline: Option<std::time::Duration>)
    -> io::Result<std::os::unix::net::UnixStream>
{
    use std::time::{Duration, Instant};
    let path = path.as_ref();
    let start = Instant::now();
    loop {
        match std::os::unix::net::UnixStream::connect(path) {
            Ok(s) => return Ok(s),
            Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused) => {
                let left = match deadline {
                    Some(d) => match d.checked_sub(start.elapsed()) {
                        Some(l) if !l.is_zero() => l,
                        _ => return Err(e),
                    },
                    None => Duration::from_millis(250),
                };
                wait_for_change(path, left.min(Duration::from_millis(250)));
            }
            Err(e) => return Err(e),
        }
    }
}

/// For long-lived subscribers between connections (fast-boot.md §7): wait
/// until the socket at `path` exists again, on a watch of its directory,
/// for at most `max`; then the caller reconnects. A supervisor-bound socket
/// never goes away, so this returns at once and the caller's connect waits
/// in the backlog until the service is back.
pub fn wait_for(path: impl AsRef<Path>, max: std::time::Duration) {
    let path = path.as_ref();
    let start = std::time::Instant::now();
    while !path.exists() {
        let Some(left) = max.checked_sub(start.elapsed()).filter(|l| !l.is_zero()) else { return };
        wait_for_change(path, left.min(std::time::Duration::from_millis(250)));
    }
}

/// Reconnect pacing for a subscriber whose service went away: starts at
/// 100 ms, doubles to `cap`, back to the start after a session that lasted.
pub struct Backoff { next: std::time::Duration, cap: std::time::Duration }

impl Backoff {
    pub fn new(cap: std::time::Duration) -> Backoff {
        Backoff { next: std::time::Duration::from_millis(100), cap }
    }
    /// A session ended after `lasted`: wait for the socket (`wait_for`)
    /// and pace the next attempt.
    pub fn after(&mut self, path: impl AsRef<Path>, lasted: std::time::Duration) {
        if lasted >= std::time::Duration::from_secs(10) {
            self.next = std::time::Duration::from_millis(100);
        }
        let start = std::time::Instant::now();
        wait_for(&path, self.cap);
        if let Some(rest) = self.next.checked_sub(start.elapsed()) {
            std::thread::sleep(rest);
        }
        self.next = (self.next * 2).min(self.cap);
    }
}

/// Sleep until something changes in the directory that holds (or would
/// hold) `path`, or `max` passes.
#[cfg(any(target_os = "freebsd", target_os = "macos"))]
fn wait_for_change(path: &Path, max: std::time::Duration) {
    use std::os::fd::AsRawFd;
    let mut dir = path.parent();
    while let Some(d) = dir {
        if d.is_dir() { break; }
        dir = d.parent();
    }
    let Some(dir) = dir else { std::thread::sleep(max); return };
    let Ok(df) = std::fs::File::open(dir) else { std::thread::sleep(max); return };
    // SAFETY: a kqueue, one change list entry on a descriptor we hold open
    // for the call, and a timespec on the stack; the kqueue fd is closed.
    #[allow(unsafe_code)]
    unsafe {
        let kq = libc::kqueue();
        if kq < 0 { std::thread::sleep(max); return; }
        let mut ev: libc::kevent = std::mem::zeroed();
        ev.ident = df.as_raw_fd() as libc::uintptr_t;
        ev.filter = libc::EVFILT_VNODE;
        ev.flags = libc::EV_ADD | libc::EV_CLEAR;
        ev.fflags = libc::NOTE_WRITE | libc::NOTE_EXTEND | libc::NOTE_LINK;
        let ts = libc::timespec { tv_sec: max.as_secs() as _, tv_nsec: max.subsec_nanos() as _ };
        let mut out: libc::kevent = std::mem::zeroed();
        // Register, then a race check: the socket may have appeared before the
        // watch was in place.
        if libc::kevent(kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) == 0 && !path.exists() {
            libc::kevent(kq, std::ptr::null(), 0, &mut out, 1, &ts);
        } else if path.exists() {
            // Present but refused: the service is between bind and listen.
            std::thread::sleep(std::time::Duration::from_millis(5).min(max));
        }
        libc::close(kq);
    }
}

#[cfg(not(any(target_os = "freebsd", target_os = "macos")))]
fn wait_for_change(path: &Path, max: std::time::Duration) {
    let _ = path;
    std::thread::sleep(max.min(std::time::Duration::from_millis(20)));
}

/// Report readiness: write "READY\n" to `$ATRIUM_READY_FD` and close it.
/// Call once the service serves (its listeners are taken and its state is
/// loaded). A no-op when there is no readiness pipe (run by hand, in tests,
/// or a manifest without `[ready] notify`), and only the first call writes.
pub fn ready() -> io::Result<()> {
    use std::io::Write;
    static DONE: Mutex<bool> = Mutex::new(false);
    let mut done = DONE.lock().unwrap_or_else(|e| e.into_inner());
    if *done { return Ok(()); }
    *done = true;
    let Some(fd) = std::env::var("ATRIUM_READY_FD").ok().and_then(|v| v.parse::<i32>().ok()) else {
        return Ok(());
    };
    if fd < 3 { return Ok(()); }
    // SAFETY: jaild put the pipe's write end at this fd for us, and only
    // this function (guarded by DONE) takes it.
    #[allow(unsafe_code)]
    let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
    f.write_all(b"READY\n")
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

    #[test]
    fn ready_writes_once_to_the_pipe() {
        use std::io::Read;
        let mut fds = [0i32; 2];
        #[allow(unsafe_code)]
        unsafe {
            assert_eq!(libc::pipe(fds.as_mut_ptr()), 0);
            assert_eq!(libc::dup2(fds[1], 302), 302);
            libc::close(fds[1]);
        }
        std::env::set_var("ATRIUM_READY_FD", "302");
        ready().unwrap();
        ready().unwrap(); // second call: nothing more, fd already closed
        #[allow(unsafe_code)]
        let mut r = unsafe { std::fs::File::from_raw_fd(fds[0]) };
        let mut got = String::new();
        r.read_to_string(&mut got).unwrap(); // EOF: ready() closed the write end
        assert_eq!(got, "READY\n");
    }

    #[test]
    fn connect_waits_for_a_late_bind_and_honours_its_deadline() {
        let d = std::env::temp_dir().join(format!("atrium-listen-c-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let p = d.join("sub").join("late.sock");
        let e = connect(&p, Some(std::time::Duration::from_millis(60))).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        let p2 = p.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            std::fs::create_dir_all(p2.parent().unwrap()).unwrap();
            let l = UnixListener::bind(&p2).unwrap();
            let _ = l.accept();
        });
        let start = std::time::Instant::now();
        connect(&p, Some(std::time::Duration::from_secs(5))).unwrap();
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
        t.join().unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn bind_socket_replaces_stale_but_not_live_or_files() {
        use std::os::unix::fs::PermissionsExt;
        let d = std::env::temp_dir().join(format!("atrium-listen-b-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("x.sock");
        let live = bind_socket(&p, 0o640, None, None).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o640);
        assert_eq!(bind_socket(&p, 0o600, None, None).unwrap_err().kind(), io::ErrorKind::AddrInUse);
        drop(live); // now stale
        bind_socket(&p, 0o600, None, None).unwrap();
        let f = d.join("plain");
        std::fs::write(&f, b"x").unwrap();
        assert_eq!(bind_socket(&f, 0o600, None, None).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        let _ = std::fs::remove_dir_all(&d);
    }
}
