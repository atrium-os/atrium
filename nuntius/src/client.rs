//! ★ THE CLIENT'S END of a Nuntius channel. [`Client::from_stream`] takes
//! the socket portcullisd's launch handed over (or, in tests, one end of a
//! socket pair); [`Client::split`] gives a sending half and a receiving
//! half, so one thread can wait for events while another opens and
//! cancels.

use crate::wire::{self, WireEvent};
use crate::{Event, Open, Request};
use aqueduct::{flag, Connection};
use std::io;
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// Sends requests.
pub struct Requests { conn: Connection }
/// Receives events.
pub struct Events { conn: Connection }

/// Both halves.
pub struct Client { pub requests: Requests, pub events: Events }

impl Client {
    pub fn from_stream(s: UnixStream) -> io::Result<Client> {
        let r = s.try_clone()?;
        Ok(Client { requests: Requests { conn: Connection::wrap(s)? }, events: Events { conn: Connection::wrap(r)? } })
    }

    pub fn split(self) -> (Requests, Events) { (self.requests, self.events) }

    pub fn send(&mut self, r: &Request) -> io::Result<()> { self.requests.send(r) }
    pub fn open(&mut self, o: Open) -> io::Result<()> { self.requests.send(&Request::Open(o)) }
    pub fn cancel(&mut self, id: u32) -> io::Result<()> { self.requests.send(&Request::Cancel { id }) }
    pub fn budget(&mut self, bytes: u64) -> io::Result<()> { self.requests.send(&Request::Budget { bytes }) }
    pub fn recv(&mut self) -> io::Result<Event> { self.events.recv() }
    pub fn recv_timeout(&mut self, t: Duration) -> io::Result<Option<Event>> { self.events.recv_timeout(t) }
}

impl Requests {
    /// Refused locally (`InvalidInput`) if the fetcher would refuse it.
    pub fn send(&mut self, r: &Request) -> io::Result<()> {
        if !r.valid() { return Err(io::Error::new(io::ErrorKind::InvalidInput, "request over the wire's limits")) }
        self.conn.send_message(wire::CLASS, wire::OP_REQUEST, 0, &r.encode())
    }
}

fn bad(why: &str) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, format!("nuntius: {why}")) }

impl Events {
    /// The next event, blocking. The channel closed is `UnexpectedEof`; a
    /// message out of shape is `InvalidData` (the fetcher is not trusted
    /// to be well-behaved either).
    pub fn recv(&mut self) -> io::Result<Event> {
        let m = self.conn.recv_message()?;
        self.event(m)
    }

    /// The next event, or `None` after `t` with none.
    pub fn recv_timeout(&mut self, t: Duration) -> io::Result<Option<Event>> {
        match self.conn.recv_message_or_timeout(t)? {
            Some(m) => self.event(m).map(Some),
            None => Ok(None),
        }
    }

    fn event(&mut self, m: aqueduct::Message) -> io::Result<Event> {
        if m.opcode_class != wire::CLASS || m.flags & flag::ASYNC_EVENT == 0 { return Err(bad("not an event")) }
        let e = WireEvent::decode(&m.payload).ok_or_else(|| bad("event out of shape"))?;
        let fd = match (m.op, &e) {
            (wire::OP_EVENT_FD, WireEvent::Fd { .. }) => Some(self.conn.take_fds(1).and_then(|mut v| v.pop()).ok_or_else(|| bad("Fd without a descriptor"))?),
            (wire::OP_EVENT, WireEvent::Fd { .. }) | (wire::OP_EVENT_FD, _) => return Err(bad("Fd and its op disagree")),
            (wire::OP_EVENT, _) => None,
            _ => return Err(bad("unknown op")),
        };
        e.into_event(fd).ok_or_else(|| bad("Fd without a descriptor"))
    }
}
