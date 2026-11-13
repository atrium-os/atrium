//! ★ A FETCHER OF THE APP'S OWN (nuntius.md §2, step 4): portcullisd's
//! `OpenFetcher`. The app's installed manifest must hold `net-fetch`
//! (portcullisd reads it; nothing the app sends counts). portcullisd starts
//! `org.atrium.nuntius` in a one-shot jail of its own, as the fetcher's own
//! uid, with one end of a socket pair as its standard input and output; the
//! other end is the app's [`Client`]. The fetcher lives while [`Fetcher`]
//! holds the request's connection: dropping it hangs up, and portcullisd
//! removes the jail.

use crate::Client;
use portcullis_ipc::{read_response, round_trip, send_fds, write_request, Request, Response};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// How the fetcher ended, as portcullisd reports it.
#[derive(Default)]
struct End { done: AtomicBool, failure: std::sync::Mutex<Option<String>> }
impl End {
    fn done(&self) -> bool { self.done.load(Ordering::Acquire) }
    fn failure(&self) -> Option<String> { self.failure.lock().unwrap().clone() }
}

/// A running fetcher: its channel, and the connection that keeps it alive.
pub struct Fetcher {
    pub client: Client,
    control: UnixStream,
    exited: Arc<End>,
}

impl Fetcher {
    /// Whether portcullisd has reported the fetcher's end.
    pub fn exited(&self) -> bool { self.exited.done() }
    /// Why the fetcher never ran, when portcullisd refused it after the
    /// handoff (a missing racct, a jail that could not be built): the app
    /// sees its channel close, and this says why.
    pub fn failure(&self) -> Option<String> { self.exited.failure() }
    /// The channel, keeping the fetcher alive as long as the returned guard.
    pub fn into_parts(self) -> (Client, FetcherGuard) {
        let this = std::mem::ManuallyDrop::new(self);
        // SAFETY: `this` is never dropped, so each field is moved out once.
        let (client, control, exited) = unsafe { (std::ptr::read(&this.client), std::ptr::read(&this.control), std::ptr::read(&this.exited)) };
        (client, FetcherGuard { _control: control, exited })
    }
}

/// Holds a fetcher's jail open; dropping it ends the fetcher.
pub struct FetcherGuard { _control: UnixStream, exited: Arc<End> }
impl FetcherGuard {
    pub fn exited(&self) -> bool { self.exited.done() }
    /// See [`Fetcher::failure`].
    pub fn failure(&self) -> Option<String> { self.exited.failure() }
}

// ★ Shut the connection down, not just drop it: the thread waiting for
// portcullisd's answer holds a clone of it, so a plain drop would never hang
// up, and the fetcher would outlive the app's use of it.
impl Drop for FetcherGuard { fn drop(&mut self) { let _ = self._control.shutdown(std::net::Shutdown::Both); } }
impl Drop for Fetcher { fn drop(&mut self) { let _ = self.control.shutdown(std::net::Shutdown::Both); } }

fn other(why: String) -> io::Error { io::Error::other(format!("nuntius: {why}")) }

/// Ask portcullisd (at its usual socket) for a fetcher.
pub fn open() -> io::Result<Fetcher> {
    let path = portcullis_ipc::resolve_socket_path()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "nuntius: portcullisd's socket is not reachable (does the manifest hold net-fetch?)"))?;
    open_at(UnixStream::connect(path)?)
}

/// [`open`] over an already connected portcullisd socket.
pub fn open_at(mut s: UnixStream) -> io::Result<Fetcher> {
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    match round_trip(&mut s, &Request::Hello { version: portcullis_ipc::PROTO_VERSION })? {
        Response::Hello { .. } => {}
        r => return Err(other(format!("portcullisd hello: {r:?}"))),
    }
    write_request(&mut s, &Request::OpenFetcher)?;
    match read_response(&mut s)? {
        Response::ReadyForFds => {}
        Response::LaunchFailed { stage, message } => return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("nuntius: {stage}: {message}"))),
        r => return Err(other(format!("portcullisd: {r:?}"))),
    }
    let (mine, theirs) = UnixStream::pair()?;
    // The channel is the fetcher's stdin and stdout; its stderr goes nowhere
    // the app reads, unless the app is being debugged (NUNTIUS_FETCHER_STDERR=inherit:
    // the fetcher's diagnostics on the app's own stderr).
    let null = std::fs::OpenOptions::new().write(true).open("/dev/null")?;
    let err = if std::env::var_os("NUNTIUS_FETCHER_STDERR").is_some_and(|v| v == "inherit") { 2 } else { null.as_raw_fd() };
    send_fds(&s, &[theirs.as_raw_fd(), theirs.as_raw_fd(), err])?;
    drop(theirs);
    s.set_read_timeout(None)?;
    let exited = Arc::new(End::default());
    let (mut r, e) = (s.try_clone()?, exited.clone());
    std::thread::Builder::new().name("nuntius-launch".into()).spawn(move || {
        // ★ Recorded BEFORE `done`, so a reader that sees the end sees why.
        if let Ok(Response::LaunchFailed { stage, message }) = read_response(&mut r) {
            *e.failure.lock().unwrap() = Some(format!("{stage}: {message}"));
        }
        e.done.store(true, Ordering::Release);
    })?;
    Ok(Fetcher { client: Client::from_stream(mine)?, control: s, exited })
}
