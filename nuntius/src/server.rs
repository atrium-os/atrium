//! ★ THE FETCHER'S END of a Nuntius channel (used by `nuntiusd`): requests
//! in, checked; events out, with `Fd`'s descriptor attached.

use crate::wire::{self, WireEvent};
use crate::{Event, Request};
use aqueduct::{flag, Connection};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

pub struct RequestsIn { conn: Connection }
pub struct EventsOut { conn: Connection }

/// The two halves of the fetcher's end.
pub fn channel(s: UnixStream) -> io::Result<(RequestsIn, EventsOut)> {
    let w = s.try_clone()?;
    Ok((RequestsIn { conn: Connection::wrap(s)? }, EventsOut { conn: Connection::wrap(w)? }))
}

impl RequestsIn {
    /// The next request; `Ok(None)` when the client hung up. A message out
    /// of shape is `InvalidData`: the fetcher ends the channel rather than
    /// guess (the client is not trusted).
    pub fn recv(&mut self) -> io::Result<Option<Request>> {
        let m = match self.conn.recv_message() {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        };
        if m.opcode_class != wire::CLASS || m.op != wire::OP_REQUEST || m.flags & (flag::IS_RESPONSE | flag::ASYNC_EVENT) != 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "nuntius: not a request"));
        }
        Request::decode(&m.payload).map(Some).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "nuntius: request out of shape"))
    }
}

impl EventsOut {
    pub fn send(&mut self, e: Event) -> io::Result<()> {
        let (w, fd) = WireEvent::from_event(e);
        match fd {
            Some(fd) => self.conn.send_message_with_fds(wire::CLASS, wire::OP_EVENT_FD, flag::ASYNC_EVENT, &w.encode(), &[fd.as_raw_fd()]),
            None => self.conn.send_message(wire::CLASS, wire::OP_EVENT, flag::ASYNC_EVENT, &w.encode()),
        }
    }
}
