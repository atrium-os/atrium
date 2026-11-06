//! The two ends of an embed, as apps use them (limen.md §11).
//!
//! Both are synchronous: a request waits for its reply, and events that
//! arrive meanwhile are queued for [`Parent::poll`] / [`Child::poll`].

use std::collections::VecDeque;
use std::io;
use std::path::Path;
use std::time::Duration;

use aqueduct::{flag, Connection};
use serde::Serialize;

use crate::proto::{self, op, ChildEvent, EmbedId, ParentEvent, Reply, WindowToken};

pub const DEFAULT_SOCKET: &str = "/var/run/atrium/limend.sock";

#[derive(Debug)]
pub enum Error {
    Refused(u8),
    Io(io::Error),
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self { Error::Io(e) }
}

struct Wire<E> {
    conn: Connection,
    queued: VecDeque<E>,
    ev_op: u16,
}

impl<E: for<'a> serde::Deserialize<'a>> Wire<E> {
    fn call<T: Serialize>(&mut self, opc: u16, req: &T) -> Result<Reply, Error> {
        let payload = postcard::to_stdvec(req).expect("encodes");
        self.conn.send_message(proto::CLASS, opc, flag::RESPONSE_EXPECTED, &payload)?;
        loop {
            let m = self.conn.recv_message_or_timeout(Duration::from_secs(5))?
                .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "limend did not answer"))?;
            if m.opcode_class != proto::CLASS {
                continue;
            }
            if m.flags & flag::IS_RESPONSE != 0 && m.op == opc {
                return postcard::from_bytes(&m.payload)
                    .map_err(|e| Error::Io(io::Error::new(io::ErrorKind::InvalidData, e)));
            }
            if m.op == self.ev_op {
                if let Ok(ev) = postcard::from_bytes(&m.payload) {
                    self.queued.push_back(ev);
                }
            }
        }
    }

    fn ok<T: Serialize>(&mut self, opc: u16, req: &T) -> Result<(), Error> {
        match self.call(opc, req)? {
            Reply::Refused { reason } => Err(Error::Refused(reason)),
            _ => Ok(()),
        }
    }

    fn poll(&mut self, wait: Duration) -> Result<Option<E>, Error> {
        if let Some(e) = self.queued.pop_front() {
            return Ok(Some(e));
        }
        loop {
            let Some(m) = self.conn.recv_message_or_timeout(wait)? else { return Ok(None) };
            if m.opcode_class == proto::CLASS && m.op == self.ev_op {
                if let Ok(ev) = postcard::from_bytes(&m.payload) {
                    return Ok(Some(ev));
                }
            }
        }
    }
}

/// The parent's end: one connection to limend for all of a window's embeds.
pub struct Parent(Wire<ParentEvent>);

impl Parent {
    /// `LIMEN_SOCKET`, else [`DEFAULT_SOCKET`].
    pub fn connect() -> Result<Self, Error> {
        let p = std::env::var("LIMEN_SOCKET").unwrap_or_else(|_| DEFAULT_SOCKET.into());
        Self::at(p)
    }

    pub fn at(path: impl AsRef<Path>) -> Result<Self, Error> {
        Ok(Self::wrap(Connection::connect(path)?))
    }

    pub fn wrap(conn: Connection) -> Self {
        Parent(Wire { conn, queued: VecDeque::new(), ev_op: op::EV_PARENT })
    }

    /// Ask for a `role` embed in the window `window` names (its frescod
    /// embed token).
    pub fn request(&mut self, role: &str, window: WindowToken) -> Result<EmbedId, Error> {
        match self.0.call(op::REQUEST, &proto::Request { role: role.into(), window })? {
            Reply::Embed { embed } => Ok(embed),
            Reply::Refused { reason } => Err(Error::Refused(reason)),
            Reply::Ok => Err(Error::Refused(proto::refusal::MALFORMED)),
        }
    }

    /// Where the slot is, in the parent window's surface px; `None` hides it.
    pub fn place(&mut self, embed: EmbedId, rect: Option<[f32; 4]>) -> Result<(), Error> {
        self.0.ok(op::PLACE, &proto::Place { embed, rect })
    }

    pub fn set_visible(&mut self, embed: EmbedId, visible: bool) -> Result<(), Error> {
        self.0.ok(op::SET_VISIBLE, &proto::SetVisible { embed, visible })
    }

    /// A role message, already encoded with the role's types.
    pub fn send(&mut self, embed: EmbedId, bytes: Vec<u8>) -> Result<(), Error> {
        self.0.ok(op::SEND, &proto::Send { embed, bytes })
    }

    pub fn detach(&mut self, embed: EmbedId) -> Result<(), Error> {
        self.0.ok(op::DETACH, &proto::Detach { embed })
    }

    pub fn poll(&mut self, wait: Duration) -> Result<Option<ParentEvent>, Error> {
        self.0.poll(wait)
    }
}

/// The child's end, or a system provider's (which serves many embeds on
/// one connection; every call names its embed).
pub struct Child(Wire<ChildEvent>);

impl Child {
    /// A child limend started: its channel is the descriptor in `LIMEN_FD`
    /// (portcullisd's embed mode). `None` when not started as an embed.
    pub fn inherited() -> Option<Result<Self, Error>> {
        use std::os::fd::FromRawFd;
        let fd: i32 = std::env::var("LIMEN_FD").ok()?.parse().ok()?;
        // SAFETY: portcullisd hands this descriptor to the child for limend
        // and nothing else owns it.
        let s = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
        Some(Connection::wrap(s).map(Self::wrap).map_err(Error::Io))
    }

    /// A system provider connecting to limend's socket.
    pub fn provider(path: impl AsRef<Path>, role: &str) -> Result<Self, Error> {
        let mut c = Self::wrap(Connection::connect(path)?);
        c.provide_role(role)?;
        Ok(c)
    }

    /// Offer `role` on this connection (system providers only).
    pub fn provide_role(&mut self, role: &str) -> Result<(), Error> {
        self.0.ok(op::PROVIDE, &proto::Provide { role: role.into() })
    }

    pub fn wrap(conn: Connection) -> Self {
        Child(Wire { conn, queued: VecDeque::new(), ev_op: op::EV_CHILD })
    }

    /// The child's window for `embed` is open; `window` is its embed token.
    pub fn ready(&mut self, embed: EmbedId, window: WindowToken) -> Result<(), Error> {
        self.0.ok(op::READY, &proto::Ready { embed, window })
    }

    pub fn emit(&mut self, embed: EmbedId, bytes: Vec<u8>) -> Result<(), Error> {
        self.0.ok(op::EMIT, &proto::Emit { embed, bytes })
    }

    pub fn focus_out(&mut self, embed: EmbedId, forward: bool) -> Result<(), Error> {
        self.0.ok(op::FOCUS_OUT, &proto::FocusOut { embed, forward })
    }

    pub fn poll(&mut self, wait: Duration) -> Result<Option<ChildEvent>, Error> {
        self.0.poll(wait)
    }
}
