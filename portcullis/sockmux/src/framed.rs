//! A [`Session`] for the u32-LE length-prefixed framing jaild and
//! atrium-volumes speak.

use std::io::{self, ErrorKind};
use std::os::unix::io::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

use crate::{fdrecv, Session};

/// A connection speaking `[u32 LE length][body]` frames, optionally carrying
/// descriptors (SCM_RIGHTS) on a frame.
pub struct LengthPrefixed {
    stream:    UnixStream,
    buf:       Vec<u8>,
    max_frame: u32,
    /// How many descriptors one frame may carry. 0 (the default) means a peer
    /// that sends any is broken, and the session is closed.
    max_fds:   usize,
    /// Received descriptors, each tagged with the offset in `buf` of the first
    /// byte that arrived with it. ★ By OFFSET, so a descriptor belongs to the
    /// frame containing that byte — a client that pipelines a plain request
    /// and then one carrying descriptors cannot have them attached to the
    /// wrong request.
    fds:       Vec<(usize, OwnedFd)>,
    eof:       bool,
}

impl LengthPrefixed {
    pub fn new(stream: UnixStream, max_frame: u32) -> Self {
        LengthPrefixed { stream, buf: Vec::new(), max_frame, max_fds: 0, fds: Vec::new(), eof: false }
    }

    /// Accept up to `max_fds` descriptors per frame (see [`Self::take_frame_with_fds`]).
    pub fn with_fds(mut self, max_fds: usize) -> Self {
        self.max_fds = max_fds;
        self
    }

    /// The socket, for writing replies. It is in blocking mode outside
    /// [`Session::fill`].
    pub fn stream(&self) -> &UnixStream {
        &self.stream
    }

    /// Total length (header + body) of the first buffered frame, if complete.
    fn frame_len(&self) -> io::Result<Option<usize>> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]);
        if len > self.max_frame {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                format!("frame too large: {len} > {}", self.max_frame),
            ));
        }
        let total = 4 + len as usize;
        Ok((self.buf.len() >= total).then_some(total))
    }

    /// Take the next complete frame's body. `Ok(None)` if none is buffered;
    /// `Err` for an oversized frame (close the session).
    ///
    /// ★ For a connection that takes no descriptors. If the frame arrived WITH
    /// some, this is an error, not a silent drop: a caller that sent a pipe
    /// expects it to be used, and quietly closing it would leave that caller
    /// waiting on a peer that will never speak.
    pub fn take_frame(&mut self) -> io::Result<Option<Vec<u8>>> {
        match self.take_frame_with_fds()? {
            Some((_, fds)) if !fds.is_empty() => Err(io::Error::new(
                ErrorKind::InvalidData,
                format!("{} descriptor(s) sent with a request that takes none", fds.len()),
            )),
            Some((body, _)) => Ok(Some(body)),
            None => Ok(None),
        }
    }

    /// Take the next complete frame's body and the descriptors that rode with
    /// it (in the order sent). The caller owns them; dropping one closes it.
    pub fn take_frame_with_fds(&mut self) -> io::Result<Option<(Vec<u8>, Vec<OwnedFd>)>> {
        let Some(total) = self.frame_len()? else { return Ok(None) };
        let body = self.buf[4..total].to_vec();
        self.buf.drain(..total);
        let (mine, rest): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.fds).into_iter().partition(|(off, _)| *off < total);
        self.fds = rest.into_iter().map(|(off, fd)| (off - total, fd)).collect();
        Ok(Some((body, mine.into_iter().map(|(_, fd)| fd).collect())))
    }
}

impl Session for LengthPrefixed {
    fn fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    fn set_nonblocking(&self, nb: bool) -> io::Result<()> {
        self.stream.set_nonblocking(nb)
    }

    fn fill(&mut self) -> io::Result<bool> {
        let mut chunk = [0u8; 16 * 1024];
        loop {
            match self.frame_len() {
                Ok(Some(_)) | Err(_) => return Ok(true),   // serve/report it first
                Ok(None) => {}
            }
            match fdrecv::recv(self.stream.as_raw_fd(), &mut chunk, self.max_fds) {
                Ok(r) if r.control_truncated => {
                    return Err(io::Error::new(
                        ErrorKind::InvalidData,
                        if self.max_fds == 0 {
                            "peer sent descriptors on a connection that takes none".to_string()
                        } else {
                            format!("peer sent more than {} descriptors at once", self.max_fds)
                        },
                    ));
                }
                Ok(r) if r.n == 0 && r.fds.is_empty() => {
                    self.eof = true;
                    /* A peer that closed mid-frame gets no answer. */
                    self.buf.clear();
                    self.fds.clear();
                    return Ok(false);
                }
                Ok(r) => {
                    // ★ Bounded per connection, not just per recvmsg: without
                    // this a peer could drip descriptors in many small sends
                    // and exhaust the broker's fd table — and the broker is
                    // the TCB.
                    if self.fds.len() + r.fds.len() > self.max_fds {
                        return Err(io::Error::new(
                            ErrorKind::InvalidData,
                            if self.max_fds == 0 {
                                "peer sent descriptors on a connection that takes none".to_string()
                            } else {
                                format!("more than {} descriptors pending on one connection",
                                        self.max_fds)
                            },
                        ));
                    }
                    let at = self.buf.len();
                    self.fds.extend(r.fds.into_iter().map(|fd| (at, fd)));
                    self.buf.extend_from_slice(&chunk[..r.n]);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(true),
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }

    fn ready(&self) -> bool {
        !matches!(self.frame_len(), Ok(None))
    }
}
