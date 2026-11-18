//! Starting workers (printing.md §6.1, §6.3). In Atrium they run through
//! jaild ([`crate::jail`], FreeBSD); [`Direct`] starts the same binaries as
//! plain children, for tests and development off-target.
//!
//! A worker's stdin and stdout are one end of a socket pair (prelum-ipp), or
//! the document and the socket (prelum-render); prelumd keeps the other end.

use std::io;
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

/// A running worker: prelumd's end of its socket, and a way to stop it.
pub struct Proc {
    pub sock: UnixStream,
    pub stop: Stopper,
}

/// Ends a worker; cheap to clone and safe to call twice.
#[derive(Clone)]
pub struct Stopper(Arc<dyn Fn() + Send + Sync>);

impl Stopper {
    pub fn new(f: impl Fn() + Send + Sync + 'static) -> Stopper {
        Stopper(Arc::new(f))
    }
    pub fn stop(&self) {
        (self.0)()
    }
}

pub trait Launcher: Send + Sync {
    /// A `prelum-ipp` whose network reaches only `printer`.
    fn ipp(&self, printer: SocketAddr) -> io::Result<Proc>;
    /// A `prelum-render` reading `pdf`, with these arguments.
    fn render(&self, pdf: OwnedFd, args: &[String]) -> io::Result<Proc>;
    /// A `prelum-browse` with no network; prelumd then hands it the mDNS
    /// sockets over its socket ([`send_fds`]).
    fn browse(&self, args: &[String]) -> io::Result<Proc>;
}

/// One byte carrying `fds` (`SCM_RIGHTS`): how prelum-browse gets atrium-net's
/// mDNS sockets.
pub fn send_fds(s: &UnixStream, fds: &[&OwnedFd]) -> io::Result<()> {
    let raw: Vec<RawFd> = fds.iter().map(|f| f.as_raw_fd()).collect();
    let mut byte = 0u8;
    let mut iov = libc::iovec { iov_base: (&mut byte as *mut u8).cast(), iov_len: 1 };
    let mut cbuf = [0u64; 8];
    let len = std::mem::size_of_val(&raw[..]) as u32;
    // SAFETY: a zeroed msghdr is valid; the control buffer (64 bytes) holds
    // one header and up to 8 descriptors, more than are ever sent here.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    if raw.is_empty() || raw.len() > 8 {
        return Err(io::Error::other("1 to 8 descriptors"));
    }
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr().cast();
    // SAFETY: CMSG_SPACE only computes a size.
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(len) } as _;
    // SAFETY: the header fits in cbuf (see above); data is written within it.
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(len) as _;
        std::ptr::copy_nonoverlapping(raw.as_ptr().cast::<u8>(), libc::CMSG_DATA(c), len as usize);
    }
    // SAFETY: msg points at live buffers of the sizes given.
    if unsafe { libc::sendmsg(s.as_raw_fd(), &msg, 0) } != 1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Plain children: no jail. Development and the Linux tests only.
pub struct Direct {
    pub bin_dir: PathBuf,
}

fn spawn(cmd: &mut Command) -> io::Result<Stopper> {
    let child: Arc<Mutex<Child>> = Arc::new(Mutex::new(cmd.spawn()?));
    let c = child.clone();
    // Reap it whenever it ends.
    std::thread::spawn(move || loop {
        match c.lock().unwrap().try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => {}
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    });
    Ok(Stopper::new(move || {
        let _ = child.lock().unwrap().kill();
    }))
}

impl Launcher for Direct {
    fn ipp(&self, _printer: SocketAddr) -> io::Result<Proc> {
        let (ours, theirs) = UnixStream::pair()?;
        let theirs: OwnedFd = theirs.into();
        let stop = spawn(
            Command::new(self.bin_dir.join("atrium-prelum-ipp"))
                .stdin(Stdio::from(theirs.try_clone()?))
                .stdout(Stdio::from(theirs))
                .stderr(Stdio::inherit()),
        )?;
        Ok(Proc { sock: ours, stop })
    }

    fn render(&self, pdf: OwnedFd, args: &[String]) -> io::Result<Proc> {
        let (ours, theirs) = UnixStream::pair()?;
        let theirs: OwnedFd = theirs.into();
        let stop = spawn(
            Command::new(self.bin_dir.join("atrium-prelum-render"))
                .args(args)
                .stdin(Stdio::from(pdf))
                .stdout(Stdio::from(theirs))
                .stderr(Stdio::inherit()),
        )?;
        Ok(Proc { sock: ours, stop })
    }

    fn browse(&self, args: &[String]) -> io::Result<Proc> {
        let (ours, theirs) = UnixStream::pair()?;
        let theirs: OwnedFd = theirs.into();
        let stop = spawn(
            Command::new(self.bin_dir.join("atrium-prelum-browse"))
                .args(args)
                .stdin(Stdio::from(theirs.try_clone()?))
                .stdout(Stdio::from(theirs))
                .stderr(Stdio::inherit()),
        )?;
        Ok(Proc { sock: ours, stop })
    }
}
