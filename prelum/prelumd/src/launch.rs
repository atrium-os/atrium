//! Starting workers (printing.md §6.1, §6.3). In Atrium they run through
//! jaild ([`crate::jail`], FreeBSD); [`Direct`] starts the same binaries as
//! plain children, for tests and development off-target.
//!
//! A worker's stdin and stdout are one end of a socket pair (prelum-ipp), or
//! the document and the socket (prelum-render); prelumd keeps the other end.

use std::io;
use std::net::SocketAddr;
use std::os::fd::OwnedFd;
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
}
