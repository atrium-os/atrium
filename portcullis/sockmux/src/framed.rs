//! A [`Session`] for the u32-LE length-prefixed framing jaild and
//! atrium-volumes speak.

use std::io::{self, ErrorKind, Read};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;

use crate::Session;

/// A connection speaking `[u32 LE length][body]` frames.
pub struct LengthPrefixed {
    stream:    UnixStream,
    buf:       Vec<u8>,
    max_frame: u32,
    eof:       bool,
}

impl LengthPrefixed {
    pub fn new(stream: UnixStream, max_frame: u32) -> Self {
        LengthPrefixed { stream, buf: Vec::new(), max_frame, eof: false }
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
    pub fn take_frame(&mut self) -> io::Result<Option<Vec<u8>>> {
        match self.frame_len()? {
            Some(total) => {
                let body = self.buf[4..total].to_vec();
                self.buf.drain(..total);
                Ok(Some(body))
            }
            None => Ok(None),
        }
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
            match self.stream.read(&mut chunk) {
                Ok(0) => {
                    self.eof = true;
                    /* A peer that closed mid-frame gets no answer. */
                    self.buf.clear();
                    return Ok(false);
                }
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
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
