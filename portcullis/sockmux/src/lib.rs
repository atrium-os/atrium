//! sockmux — serve many unix-socket clients from one thread.
//!
//! Atrium's privileged brokers (jaild, atrium-volumes, portcullisd-daemon)
//! are deliberately single-threaded: requests are cheap, state is `&mut`, and
//! no async runtime belongs in the smallest-TCB tier. They used to get there
//! by serving each connection TO COMPLETION before accepting the next, which
//! fails the moment any client holds a connection open — and the portcullisd
//! bootstrap holds its jaild connection for its whole supervisor lifetime, so
//! the aqueduct attach smoke timed out at every boot waiting for jaild.
//!
//! [`Mux`] keeps every connection on one kqueue instead. Reads are
//! non-blocking and buffered per connection (the [`Session`] owns the
//! buffer and knows its framing), and each round hands the caller at most one
//! ready request per connection. A client that idles, stalls mid-frame, or
//! pipelines requests costs nobody else anything.
//!
//! Replies stay blocking — the socket is non-blocking only inside
//! [`Session::fill`] — bounded by [`SEND_TIMEOUT`], so a client that stops
//! reading can stall the loop for at most that long before its send fails.
//!
//! ```ignore
//! let mut mux = Mux::new(listener)?;
//! loop {
//!     for fd in mux.next_round(&mut |stream| admit(stream))? {
//!         let Some(session) = mux.session_mut(fd) else { continue };
//!         if serve_one(session, &mut state).is_err() {
//!             mux.close(fd);
//!         }
//!     }
//! }
//! ```

#![deny(unsafe_code)]

mod fdrecv;
mod framed;
mod kqueue;

pub use framed::LengthPrefixed;
pub use kqueue::Kqueue;

use std::collections::HashMap;
use std::io::{self, ErrorKind};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::Duration;

use log::{error, warn};

/// How long a blocking reply may wait on a client that is not reading.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// One client connection as the multiplexer sees it. The implementation owns
/// the socket and the bytes read from it but not yet served.
pub trait Session {
    /// The connection's socket fd.
    fn fd(&self) -> RawFd;

    /// Switch the socket's O_NONBLOCK.
    fn set_nonblocking(&self, nb: bool) -> io::Result<()>;

    /// Read whatever is available. Called with the socket non-blocking, so
    /// a read that would block must end the call with `Ok(true)`, not an
    /// error. Returns `Ok(false)` once the peer has closed its side. May stop
    /// early once a complete request is buffered; the rest waits in the
    /// socket (the kqueue is level-triggered).
    fn fill(&mut self) -> io::Result<bool>;

    /// Whether a complete request (or a framing error to report) is
    /// buffered and ready to serve.
    fn ready(&self) -> bool;
}

struct Entry<S> {
    session: S,
    eof:     bool,
}

/// A listener plus its client sessions, multiplexed over one kqueue.
pub struct Mux<S: Session> {
    kq:       Kqueue,
    listener: UnixListener,
    sessions: HashMap<RawFd, Entry<S>>,
    /// Round-robin order: accept order, rotated each round.
    order:    Vec<RawFd>,
}

impl<S: Session> Mux<S> {
    pub fn new(listener: UnixListener) -> io::Result<Self> {
        let kq = Kqueue::new()?;
        listener.set_nonblocking(true)?;
        kq.add_read(listener.as_raw_fd())?;
        Ok(Mux { kq, listener, sessions: HashMap::new(), order: Vec::new() })
    }

    /// Number of open client sessions.
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Block until at least one session has a request ready, accepting new
    /// connections meanwhile, and return the ready sessions' fds in
    /// round-robin order. Serve at most one request from each, then call
    /// again.
    ///
    /// `admit` receives each accepted stream (already given
    /// [`SEND_TIMEOUT`]) and returns the session to serve it with, or `None`
    /// to refuse — it may write a refusal first; the stream is closed when
    /// dropped.
    pub fn next_round(
        &mut self,
        admit: &mut dyn FnMut(UnixStream) -> Option<S>,
    ) -> io::Result<Vec<RawFd>> {
        let listen_fd = self.listener.as_raw_fd();
        loop {
            /* Always look at the kqueue, blocking only when nothing is
             * already buffered: a client whose one read delivered several
             * pipelined frames must not keep new connections and other
             * clients' bytes waiting for as many rounds. */
            self.reap();
            let any_ready = self.sessions.values().any(|e| e.session.ready());
            for fd in self.kq.wait(!any_ready)? {
                if fd == listen_fd {
                    self.accept_ready(admit);
                } else {
                    self.fill(fd);
                }
            }
            self.reap();
            let ready: Vec<RawFd> = self.order.iter().copied()
                .filter(|fd| self.sessions.get(fd).is_some_and(|e| e.session.ready()))
                .collect();
            if !ready.is_empty() {
                if self.order.len() > 1 {
                    self.order.rotate_left(1);
                }
                return Ok(ready);
            }
        }
    }

    pub fn session_mut(&mut self, fd: RawFd) -> Option<&mut S> {
        self.sessions.get_mut(&fd).map(|e| &mut e.session)
    }

    /// Close a session (drops it, closing its socket).
    pub fn close(&mut self, fd: RawFd) {
        self.sessions.remove(&fd);
        self.order.retain(|f| *f != fd);
    }

    /// Drop sessions whose peer closed and that have nothing left to serve.
    fn reap(&mut self) {
        let done: Vec<RawFd> = self.sessions.iter()
            .filter(|(_, e)| e.eof && !e.session.ready())
            .map(|(fd, _)| *fd)
            .collect();
        for fd in done {
            self.close(fd);
        }
    }

    fn fill(&mut self, fd: RawFd) {
        let Some(e) = self.sessions.get_mut(&fd) else { return };
        let res = e.session.set_nonblocking(true)
            .and_then(|_| e.session.fill())
            .and_then(|open| e.session.set_nonblocking(false).map(|_| open));
        match res {
            Ok(true) => {}
            Ok(false) => {
                e.eof = true;
                /* A level-triggered EOF would fire on every wait. */
                let _ = self.kq.remove_read(fd);
            }
            Err(err) => {
                warn!("connection closed with error: {err}");
                self.close(fd);
            }
        }
    }

    fn accept_ready(&mut self, admit: &mut dyn FnMut(UnixStream) -> Option<S>) {
        loop {
            let stream = match self.listener.accept() {
                Ok((s, _)) => s,
                Err(e) if e.kind() == ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => {
                    /* ECONNABORTED, EMFILE, ...: keep serving the clients
                     * we have; the listener fires again. */
                    error!("accept: {e}");
                    return;
                }
            };
            /* The accepted socket does not inherit the listener's
             * O_NONBLOCK on every platform; make it explicit. */
            if let Err(e) = stream.set_nonblocking(false)
                .and_then(|_| stream.set_write_timeout(Some(SEND_TIMEOUT)))
            {
                warn!("connection setup failed: {e}");
                continue;
            }
            let Some(session) = admit(stream) else { continue };
            let fd = session.fd();
            if let Err(e) = self.kq.add_read(fd) {
                warn!("connection setup failed: {e}");
                continue;
            }
            self.sessions.insert(fd, Entry { session, eof: false });
            self.order.push(fd);
        }
    }
}
