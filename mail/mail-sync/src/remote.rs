//! IMAP sync in a process of its own (mail.md §4): the `sync` worker runs
//! the [`Session`] and parses every byte the server sends; the engine keeps
//! the store and the courier. The worker has no capability at all: the
//! courier's bytes reach it relayed over its standard input and output,
//! and what it would ask of the store ([`Sink`]) it asks the engine there
//! too. So a server that breaks the IMAP parser holds a process that sees
//! one account's session and can only ask the store what sync may ask.
//!
//! The wire, both ways: `u8 kind | u32 LE length | payload`.
//! - `WIRE`: courier bytes (engine → worker: from the server; worker →
//!   engine: to it).
//! - `EOF`, `TIMEOUT` (engine → worker, empty): the courier ended; it was
//!   quiet past the call's wait (an `IDLE` renews, anything else fails).
//! - `CALL` (engine → worker) and `REPLY` (worker → engine): a [`Call`] and
//!   its [`Reply`].
//! - `SINK` (worker → engine) and `ANSWER` (engine → worker): a store call
//!   made while a call runs, and its answer.
//!
//! Payloads are postcard.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{Change, Error, Fetched, FolderState, RemoteFolder, Report, Result, Session, Sink};

pub const WIRE: u8 = 0;
pub const EOF: u8 = 1;
pub const TIMEOUT: u8 = 2;
pub const CALL: u8 = 3;
pub const REPLY: u8 = 4;
pub const SINK: u8 = 5;
pub const ANSWER: u8 = 6;

/// The largest frame: a page of whole messages (20 × 2 MiB) and room.
pub const FRAME_MAX: usize = 64 << 20;

/// What the engine asks the worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Call {
    /// Read the greeting; answers [`Reply::Started`].
    Start,
    Sync,
    Append { folder: String, flags: Vec<String>, message: Vec<u8> },
    Idle { folder: String },
    Logout,
    /// Send one message on a submission session (instead of `Start`; the
    /// worker ends after it). Answers [`Reply::Submitted`].
    Submit { from: String, rcpts: Vec<String>, bytes: Vec<u8>, hold_for: Option<u64> },
    /// A JMAP account (instead of `Start`): read its session at `url`, the
    /// HTTP asked of the engine ([`SinkCall::Get`], [`SinkCall::Post`]).
    /// Answers [`Reply::JmapReady`].
    JmapConnect { url: String },
    JmapSync,
    JmapSend { message: Vec<u8>, from: String, rcpts: Vec<String>, send_at: Option<i64> },
    /// Watch the account's event source; the stream is asked of the engine
    /// ([`SinkCall::Events`]), each event's verdict told to it
    /// ([`SinkCall::Pushed`]). Answers [`Reply::Watched`].
    JmapWatch,
    /// Search the IMAP server for messages with every word (§11.2);
    /// answers [`Reply::Found`].
    Search { query: crate::ServerQuery, limit: u32 },
    /// The same on a JMAP account.
    JmapSearch { query: crate::ServerQuery, limit: u32 },
    /// Store a message in a folder, seen (an import, §6.5); answers
    /// [`Reply::JmapSent`] with its id.
    JmapImport { folder: String, message: Vec<u8> },
    /// The vacation reply, set first if given; answers [`Reply::Vacation`].
    JmapVacation(Option<crate::jmap::Vacation>),
    /// Put Mail's filters on a JMAP account's server (RFC 9661); answers
    /// [`Reply::JmapSieved`].
    JmapSieve(crate::sieve::Plan),
    /// Put Mail's filters on a ManageSieve session (instead of `Start`;
    /// the worker ends after it). Answers [`Reply::Sieved`].
    Sieve(crate::sieve::Plan),
}

/// What [`Error`] was, across the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    Io,
    Protocol,
    Bye,
    Throttled,
    No,
    Sink,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reply {
    Started { caps: Vec<String> },
    Synced(Report),
    Appended(Option<u32>),
    Idled(bool),
    LoggedOut,
    Submitted(std::result::Result<crate::submit::Sent, crate::submit::SendError>),
    /// The session's `maxDelayedSend` (seconds).
    /// `filters`: what [`crate::jmap::Jmap::filters`] says.
    JmapReady { max_delayed_send: u64, filters: crate::jmap::Filters },
    JmapSent(std::result::Result<String, String>),
    Vacation(std::result::Result<Option<crate::jmap::Vacation>, String>),
    Watched(std::result::Result<crate::jmap::Pushed, String>),
    Sieved(std::result::Result<crate::sieve::Sieved, String>),
    JmapSieved(std::result::Result<Option<crate::sieve::Sieved>, String>),
    /// How many messages a server search found.
    Found(u32),
    Failed(Kind, String),
}

/// A [`Sink`] call, from the worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SinkCall {
    Folders(Vec<RemoteFolder>),
    State(String),
    Reset(String),
    KnownUids(String),
    Messages(String, Vec<Fetched>),
    Flags(String, Vec<(u32, Vec<String>)>),
    Vanished(String, Vec<u32>),
    Checkpoint(String, FolderState),
    TextWanted(String, u32),
    Changes,
    ChangeDone(u64, bool),
    ServerState(String),
    SetServerState(String, String),
    Listed(String),
    ServerIdAt(String, u32),
    /// JMAP's HTTP, which the engine does through Arca's courier.
    Get { url: String, max: u64 },
    Post { url: String, content_type: String, body: Vec<u8>, max: u64 },
    /// Open `url` as an event stream: the engine answers
    /// [`SinkAnswer::Event`] for each event, then [`SinkAnswer::EventsEnd`].
    Events { url: String },
    /// An event was news (`true`) or a sign of life; answered
    /// [`SinkAnswer::Go`]: whether to keep watching.
    Pushed(bool),
    /// [`Sink::has_text`]; answered [`SinkAnswer::Go`].
    HasText(String, u32),
    WantsSnoozed,
    SnoozeMoves,
    JmapDestroyed(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SinkAnswer {
    Done(std::result::Result<(), String>),
    State(FolderState),
    Uids(Vec<u32>),
    Wanted(Vec<(u32, u64)>),
    Changes(Vec<Change>),
    Text(Option<String>),
    Listed(Vec<(String, u32)>),
    Body(std::result::Result<Vec<u8>, String>),
    Event(Vec<u8>),
    EventsEnd(std::result::Result<(), String>),
    Go(bool),
    WantsSnoozed(std::result::Result<bool, String>),
    SnoozeMoves(std::result::Result<Vec<crate::SnoozeMove>, String>),
}

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_string())
}

pub fn write_frame(w: &mut (impl Write + ?Sized), kind: u8, payload: &[u8]) -> io::Result<()> {
    if payload.len() > FRAME_MAX {
        return Err(bad("frame too large"));
    }
    let mut head = [0u8; 5];
    head[0] = kind;
    head[1..].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    w.write_all(&head)?;
    w.write_all(payload)?;
    w.flush()
}

/// One frame; `Ok(None)` at a clean end.
pub fn read_frame(r: &mut (impl Read + ?Sized)) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut head = [0u8; 5];
    match r.read_exact(&mut head[..1]) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    r.read_exact(&mut head[1..])?;
    let n = u32::from_le_bytes(head[1..].try_into().unwrap()) as usize;
    if n > FRAME_MAX || head[0] > ANSWER {
        return Err(bad("frame"));
    }
    let mut b = vec![0u8; n];
    r.read_exact(&mut b)?;
    Ok(Some((head[0], b)))
}

fn enc<T: Serialize>(v: &T) -> io::Result<Vec<u8>> {
    postcard::to_stdvec(v).map_err(|_| bad("encode"))
}

fn dec<'a, T: Deserialize<'a>>(b: &'a [u8]) -> io::Result<T> {
    postcard::from_bytes(b).map_err(|_| bad("decode"))
}

fn kind_of(e: &Error) -> (Kind, String) {
    match e {
        Error::Io(e) => (Kind::Io, e.to_string()),
        Error::Protocol(t) => (Kind::Protocol, t.clone()),
        Error::Bye(t) => (Kind::Bye, t.clone()),
        Error::Throttled => (Kind::Throttled, String::new()),
        Error::No(t) => (Kind::No, t.clone()),
        Error::Sink(t) => (Kind::Sink, t.clone()),
    }
}

fn error_of(k: Kind, t: String) -> Error {
    match k {
        Kind::Io => Error::Io(io::Error::other(t)),
        Kind::Protocol => Error::Protocol(t),
        Kind::Bye => Error::Bye(t),
        Kind::Throttled => Error::Throttled,
        Kind::No => Error::No(t),
        Kind::Sink => Error::Sink(t),
    }
}

// ── the worker ──────────────────────────────────────────────────────

/// The worker's one channel to the engine, shared by the session's
/// socket ([`Tunnel`]) and the store ([`RemoteSink`]).
struct Chan<R: Read, W: Write> {
    r: R,
    w: W,
    /// Server bytes that came while a store answer was awaited.
    wire: VecDeque<u8>,
    /// The courier ended / a wait passed, seen while awaiting an answer.
    eof: bool,
    timeouts: usize,
}

impl<R: Read, W: Write> Chan<R, W> {
    /// The next frame that isn't server bytes or a courier event (those
    /// are kept for the tunnel).
    fn next_other(&mut self) -> io::Result<(u8, Vec<u8>)> {
        loop {
            let (k, b) = read_frame(&mut self.r)?.ok_or_else(|| bad("the engine went away"))?;
            match k {
                WIRE => self.wire.extend(b),
                EOF => self.eof = true,
                TIMEOUT => self.timeouts += 1,
                _ => return Ok((k, b)),
            }
        }
    }
}

type Shared<R, W> = std::rc::Rc<std::cell::RefCell<Chan<R, W>>>;

/// The session's socket, inside the worker.
struct Tunnel<R: Read, W: Write>(Shared<R, W>);

impl<R: Read, W: Write> Read for Tunnel<R, W> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut c = self.0.borrow_mut();
        loop {
            if !c.wire.is_empty() {
                let n = buf.len().min(c.wire.len());
                for (i, b) in c.wire.drain(..n).enumerate() {
                    buf[i] = b;
                }
                return Ok(n);
            }
            if c.eof {
                return Ok(0);
            }
            if c.timeouts > 0 {
                c.timeouts -= 1;
                return Err(io::ErrorKind::TimedOut.into());
            }
            let (k, b) = read_frame(&mut c.r)?.ok_or_else(|| bad("the engine went away"))?;
            match k {
                WIRE => c.wire.extend(b),
                EOF => c.eof = true,
                TIMEOUT => c.timeouts += 1,
                _ => return Err(bad("a call while the server was awaited")),
            }
        }
    }
}

impl<R: Read, W: Write> Write for Tunnel<R, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = buf.len().min(64 * 1024);
        write_frame(&mut self.0.borrow_mut().w, WIRE, &buf[..n])?;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.borrow_mut().w.flush()
    }
}

/// The store, inside the worker: every call is asked of the engine.
struct RemoteSink<R: Read, W: Write>(Shared<R, W>);

impl<R: Read, W: Write> RemoteSink<R, W> {
    fn ask(&mut self, call: SinkCall) -> std::result::Result<SinkAnswer, String> {
        let mut c = self.0.borrow_mut();
        let b = enc(&call).map_err(|e| e.to_string())?;
        write_frame(&mut c.w, SINK, &b).map_err(|e| e.to_string())?;
        let (k, b) = c.next_other().map_err(|e| e.to_string())?;
        if k != ANSWER {
            return Err("not an answer".into());
        }
        dec(&b).map_err(|e| e.to_string())
    }
    /// The engine's next answer, unasked (an event stream's).
    fn next(&mut self) -> std::result::Result<SinkAnswer, String> {
        let (k, b) = self.0.borrow_mut().next_other().map_err(|e| e.to_string())?;
        if k != ANSWER {
            return Err("not an answer".into());
        }
        dec(&b).map_err(|e| e.to_string())
    }
    fn done(&mut self, call: SinkCall) -> std::result::Result<(), String> {
        match self.ask(call)? {
            SinkAnswer::Done(r) => r,
            _ => Err("a wrong answer".into()),
        }
    }
}

impl<R: Read, W: Write> Sink for RemoteSink<R, W> {
    fn folders(&mut self, list: &[RemoteFolder]) -> std::result::Result<(), String> {
        self.done(SinkCall::Folders(list.to_vec()))
    }
    fn state(&mut self, folder: &str) -> FolderState {
        match self.ask(SinkCall::State(folder.into())) {
            Ok(SinkAnswer::State(s)) => s,
            _ => FolderState::default(),
        }
    }
    fn reset(&mut self, folder: &str) -> std::result::Result<(), String> {
        self.done(SinkCall::Reset(folder.into()))
    }
    fn known_uids(&mut self, folder: &str) -> Vec<u32> {
        match self.ask(SinkCall::KnownUids(folder.into())) {
            Ok(SinkAnswer::Uids(u)) => u,
            _ => Vec::new(),
        }
    }
    fn messages(&mut self, folder: &str, page: Vec<Fetched>) -> std::result::Result<(), String> {
        self.done(SinkCall::Messages(folder.into(), page))
    }
    fn flags(&mut self, folder: &str, changes: &[(u32, Vec<String>)]) -> std::result::Result<(), String> {
        self.done(SinkCall::Flags(folder.into(), changes.to_vec()))
    }
    fn vanished(&mut self, folder: &str, uids: &[u32]) -> std::result::Result<(), String> {
        self.done(SinkCall::Vanished(folder.into(), uids.to_vec()))
    }
    fn checkpoint(&mut self, folder: &str, state: &FolderState) -> std::result::Result<(), String> {
        self.done(SinkCall::Checkpoint(folder.into(), state.clone()))
    }
    fn text_wanted(&mut self, folder: &str, limit: usize) -> Vec<(u32, u64)> {
        match self.ask(SinkCall::TextWanted(folder.into(), limit.min(u32::MAX as usize) as u32)) {
            Ok(SinkAnswer::Wanted(w)) => w,
            _ => Vec::new(),
        }
    }
    fn changes(&mut self) -> Vec<Change> {
        match self.ask(SinkCall::Changes) {
            Ok(SinkAnswer::Changes(c)) => c,
            _ => Vec::new(),
        }
    }
    fn change_done(&mut self, id: u64, ok: bool) -> std::result::Result<(), String> {
        self.done(SinkCall::ChangeDone(id, ok))
    }
    fn wants_snoozed(&mut self) -> std::result::Result<bool, String> {
        match self.ask(SinkCall::WantsSnoozed).map_err(|e| e.to_string())? {
            SinkAnswer::WantsSnoozed(value) => value,
            _ => Err("bad snooze mailbox reply".into()),
        }
    }

    fn snooze_moves(&mut self) -> std::result::Result<Vec<crate::SnoozeMove>, String> {
        match self.ask(SinkCall::SnoozeMoves).map_err(|e| e.to_string())? {
            SinkAnswer::SnoozeMoves(value) => value,
            _ => Err("bad snooze moves reply".into()),
        }
    }

    fn server_state(&mut self, key: &str) -> Option<String> {
        match self.ask(SinkCall::ServerState(key.into())) {
            Ok(SinkAnswer::Text(t)) => t,
            _ => None,
        }
    }
    fn set_server_state(&mut self, key: &str, value: &str) -> std::result::Result<(), String> {
        self.done(SinkCall::SetServerState(key.into(), value.into()))
    }
    fn jmap_destroyed(&mut self, ids: &[String]) -> std::result::Result<(), String> {
        self.done(SinkCall::JmapDestroyed(ids.to_vec()))
    }
    fn listed(&mut self, id: &str) -> Vec<(String, u32)> {
        match self.ask(SinkCall::Listed(id.into())) {
            Ok(SinkAnswer::Listed(l)) => l,
            _ => Vec::new(),
        }
    }
    fn server_id_at(&mut self, folder: &str, uid: u32) -> Option<String> {
        match self.ask(SinkCall::ServerIdAt(folder.into(), uid)) {
            Ok(SinkAnswer::Text(t)) => t,
            _ => None,
        }
    }
    fn has_text(&mut self, folder: &str, uid: u32) -> bool {
        matches!(self.ask(SinkCall::HasText(folder.into(), uid)), Ok(SinkAnswer::Go(true)))
    }
}

/// JMAP's HTTP, inside the worker: asked of the engine.
struct RemoteHttp<R: Read, W: Write>(Shared<R, W>);

impl<R: Read, W: Write> crate::jmap::Http for RemoteHttp<R, W> {
    fn get(&mut self, url: &str, max: usize) -> std::result::Result<Vec<u8>, String> {
        match RemoteSink(self.0.clone()).ask(SinkCall::Get { url: url.into(), max: max as u64 })? {
            SinkAnswer::Body(b) => b.and_then(|b| if b.len() > max { Err("the answer is too large".into()) } else { Ok(b) }),
            _ => Err("a wrong answer".into()),
        }
    }
    fn post(&mut self, url: &str, content_type: &str, body: &[u8], max: usize) -> std::result::Result<Vec<u8>, String> {
        match RemoteSink(self.0.clone()).ask(SinkCall::Post { url: url.into(), content_type: content_type.into(), body: body.to_vec(), max: max as u64 })? {
            SinkAnswer::Body(b) => b.and_then(|b| if b.len() > max { Err("the answer is too large".into()) } else { Ok(b) }),
            _ => Err("a wrong answer".into()),
        }
    }
    fn events(&mut self, url: &str, on_event: &mut dyn FnMut(&[u8]) -> bool) -> std::result::Result<(), String> {
        let mut a = RemoteSink(self.0.clone());
        let mut next = a.ask(SinkCall::Events { url: url.into() })?;
        loop {
            match next {
                // The verdict reaches the engine through the watcher's
                // `Pushed`; a `false` ends its stream, then `EventsEnd`.
                SinkAnswer::Event(raw) => {
                    on_event(&raw);
                }
                SinkAnswer::EventsEnd(r) => return r,
                _ => return Err("a wrong answer".into()),
            }
            next = a.next()?;
        }
    }
}

/// The `sync` worker's main: serve calls on `r`/`w` until the engine
/// closes them or after `Logout`.
pub fn serve<R: Read, W: Write>(r: R, w: W) -> io::Result<()> {
    let chan: Shared<R, W> = std::rc::Rc::new(std::cell::RefCell::new(Chan { r, w, wire: VecDeque::new(), eof: false, timeouts: 0 }));
    let mut session: Option<Session<Tunnel<R, W>>> = None;
    let mut jmap: Option<crate::jmap::Client<RemoteHttp<R, W>>> = None;
    loop {
        let next = {
            let mut c = chan.borrow_mut();
            match read_frame(&mut c.r)? {
                None => return Ok(()),
                Some((WIRE, b)) => {
                    c.wire.extend(b);
                    continue;
                }
                Some((EOF, _)) => {
                    c.eof = true;
                    continue;
                }
                Some((TIMEOUT, _)) => continue,
                Some((CALL, b)) => {
                    // A wait that passed between calls is no one's.
                    c.timeouts = 0;
                    dec::<Call>(&b)?
                }
                Some(_) => return Err(bad("not a call")),
            }
        };
        // JMAP calls, for a worker that connected to a JMAP account.
        let next = match next {
            Call::JmapConnect { url } if session.is_none() && jmap.is_none() => {
                let reply = match crate::jmap::Client::connect(RemoteHttp(chan.clone()), &url) {
                    Ok(c) => {
                        let r = Reply::JmapReady { max_delayed_send: c.session.max_delayed_send, filters: crate::jmap::Jmap::filters(&c) };
                        jmap = Some(c);
                        r
                    }
                    Err(e) => Reply::Failed(Kind::Protocol, e.0),
                };
                write_frame(&mut chan.borrow_mut().w, REPLY, &enc(&reply)?)?;
                continue;
            }
            Call::JmapSync | Call::JmapSend { .. } | Call::JmapWatch | Call::JmapSearch { .. } | Call::JmapImport { .. } | Call::JmapVacation(_) | Call::JmapSieve(_) => {
                let reply = match (next, jmap.as_mut()) {
                    (Call::JmapWatch, Some(c)) => {
                        let mut a = RemoteSink(chan.clone());
                        let mut on = |news: bool| matches!(a.ask(SinkCall::Pushed(news)), Ok(SinkAnswer::Go(true)));
                        Reply::Watched(c.watch(&mut on).map_err(|e| e.0))
                    }
                    (_, None) => Reply::Failed(Kind::Protocol, "not connected".into()),
                    (Call::JmapSync, Some(c)) => c.sync(&mut RemoteSink(chan.clone())).map(Reply::Synced).unwrap_or_else(|e| Reply::Failed(Kind::Protocol, e.0)),
                    (Call::JmapSend { message, from, rcpts, send_at }, Some(c)) => Reply::JmapSent(c.send(&message, &from, &rcpts, send_at).map_err(|e| e.0)),
                    (Call::JmapImport { folder, message }, Some(c)) => Reply::JmapSent(c.import(&folder, &message).map_err(|e| e.0)),
                    (Call::JmapVacation(set), Some(c)) => Reply::Vacation(c.vacation(set.as_ref()).map_err(|e| e.0)),
                    (Call::JmapSieve(plan), Some(c)) => Reply::JmapSieved(c.sieve(&plan).map_err(|e| e.0)),
                    (Call::JmapSearch { query, limit }, Some(c)) => c.search_server(&query, &mut RemoteSink(chan.clone()), (limit as usize).min(crate::SERVER_HITS)).map(|n| Reply::Found(n as u32)).unwrap_or_else(|e| Reply::Failed(Kind::Protocol, e.0)),
                    _ => unreachable!(),
                };
                write_frame(&mut chan.borrow_mut().w, REPLY, &enc(&reply)?)?;
                continue;
            }
            Call::JmapConnect { .. } => {
                write_frame(&mut chan.borrow_mut().w, REPLY, &enc(&Reply::Failed(Kind::Protocol, "already started".into()))?)?;
                continue;
            }
            other => other,
        };
        let reply = match (next, session.as_mut()) {
            (Call::Start, None) => match Session::start(Tunnel(chan.clone())) {
                Ok(s) => {
                    let caps = s.capabilities().to_vec();
                    session = Some(s);
                    Reply::Started { caps }
                }
                Err(e) => {
                    let (k, t) = kind_of(&e);
                    Reply::Failed(k, t)
                }
            },
            (Call::Submit { from, rcpts, bytes, hold_for }, None) => {
                Reply::Submitted(crate::submit::submit(Tunnel(chan.clone()), &crate::submit::Outgoing { from: &from, rcpts: &rcpts, bytes: &bytes, hold_for }))
            }
            (Call::Sieve(plan), None) => Reply::Sieved(crate::sieve::apply(Tunnel(chan.clone()), &plan)),
            (Call::Start, Some(_)) | (Call::Submit { .. } | Call::Sieve(_), Some(_)) => Reply::Failed(Kind::Protocol, "already started".into()),
            (Call::JmapConnect { .. } | Call::JmapSync | Call::JmapSend { .. } | Call::JmapWatch | Call::JmapSearch { .. } | Call::JmapImport { .. } | Call::JmapVacation(_) | Call::JmapSieve(_), _) => unreachable!(),
            (_, None) => Reply::Failed(Kind::Protocol, "not started".into()),
            (Call::Sync, Some(s)) => s.sync(&mut RemoteSink(chan.clone())).map(Reply::Synced).unwrap_or_else(|e| fail(&e)),
            (Call::Append { folder, flags, message }, Some(s)) => {
                let f: Vec<&str> = flags.iter().map(String::as_str).collect();
                s.append(&folder, &f, &message).map(Reply::Appended).unwrap_or_else(|e| fail(&e))
            }
            (Call::Idle { folder }, Some(s)) => s.idle(&folder).map(Reply::Idled).unwrap_or_else(|e| fail(&e)),
            (Call::Search { query, limit }, Some(s)) => s.search_server(&query, &mut RemoteSink(chan.clone()), (limit as usize).min(crate::SERVER_HITS)).map(|n| Reply::Found(n as u32)).unwrap_or_else(|e| fail(&e)),
            (Call::Logout, Some(_)) => {
                let _ = session.take().unwrap().logout();
                Reply::LoggedOut
            }
        };
        let done = matches!(reply, Reply::LoggedOut | Reply::Submitted(_) | Reply::Sieved(_));
        let b = enc(&reply)?;
        write_frame(&mut chan.borrow_mut().w, REPLY, &b)?;
        if done {
            return Ok(());
        }
    }
}

fn fail(e: &Error) -> Reply {
    let (k, t) = kind_of(e);
    Reply::Failed(k, t)
}

// ── the engine's side ───────────────────────────────────────────────

/// What the engine does with an IMAP session, here or in the worker.
pub trait Imap {
    fn capabilities(&self) -> &[String];
    fn sync(&mut self, sink: &mut dyn Sink) -> Result<Report>;
    fn append(&mut self, folder: &str, flags: &[&str], message: &[u8]) -> Result<Option<u32>>;
    /// `IDLE` on `folder` until a change or `wait` passes.
    fn idle(&mut self, folder: &str, wait: Duration) -> Result<bool>;
    fn logout(&mut self) -> Result<()>;
    /// Search the server (§11.2); see [`Session::search_server`].
    fn search(&mut self, query: &crate::ServerQuery, sink: &mut dyn Sink, limit: usize) -> Result<usize>;
}

/// The session here, over the courier's socket (tests, and Mail without
/// a `sync` worker).
pub struct Local {
    s: Option<Session<std::os::unix::net::UnixStream>>,
    caps: Vec<String>,
}

impl Local {
    pub fn start(courier: std::os::unix::net::UnixStream) -> Result<Local> {
        let s = Session::start(courier)?;
        Ok(Local { caps: s.capabilities().to_vec(), s: Some(s) })
    }
    fn s(&mut self) -> Result<&mut Session<std::os::unix::net::UnixStream>> {
        self.s.as_mut().ok_or_else(|| Error::Protocol("logged out".into()))
    }
}

impl Imap for Local {
    fn capabilities(&self) -> &[String] {
        &self.caps
    }
    fn sync(&mut self, sink: &mut dyn Sink) -> Result<Report> {
        self.s()?.sync(sink)
    }
    fn append(&mut self, folder: &str, flags: &[&str], message: &[u8]) -> Result<Option<u32>> {
        self.s()?.append(folder, flags, message)
    }
    fn search(&mut self, query: &crate::ServerQuery, sink: &mut dyn Sink, limit: usize) -> Result<usize> {
        self.s()?.search_server(query, sink, limit)
    }
    fn idle(&mut self, folder: &str, wait: Duration) -> Result<bool> {
        let s = self.s()?;
        s.socket().set_read_timeout(Some(wait))?;
        s.idle(folder)
    }
    fn logout(&mut self) -> Result<()> {
        match self.s.take() {
            Some(s) => s.logout(),
            None => Ok(()),
        }
    }
}

/// How long the engine waits on a quiet server outside `IDLE`.
pub const QUIET: Duration = Duration::from_secs(120);

/// The session in the `sync` worker: the engine relays the courier's
/// bytes and answers the store calls.
pub struct Remote<I: Write + AsFd, O: Read + AsFd> {
    /// The IMAP or submission courier; none for JMAP, whose HTTP the
    /// worker asks of `http`.
    courier: Option<std::os::unix::net::UnixStream>,
    http: Option<Box<dyn crate::jmap::Http>>,
    to_worker: I,
    from_worker: O,
    caps: Vec<String>,
    gone: bool,
    max_delayed_send: u64,
    /// A JMAP account's: what its server keeps.
    filters: crate::jmap::Filters,
}

impl<I: Write + AsFd, O: Read + AsFd> Remote<I, O> {
    /// Start the session in the worker (`to_worker` is its standard input,
    /// `from_worker` its output) over `courier`.
    pub fn start(courier: std::os::unix::net::UnixStream, to_worker: I, from_worker: O) -> Result<Remote<I, O>> {
        let mut r = Remote { courier: Some(courier), http: None, to_worker, from_worker, caps: Vec::new(), gone: false, max_delayed_send: 0, filters: Default::default() };
        match r.call(&Call::Start, None, QUIET)? {
            Reply::Started { caps } => r.caps = caps,
            other => return Err(Error::Protocol(format!("the worker answered {other:?}"))),
        }
        Ok(r)
    }

    /// Send `o` on a submission `courier` in the worker (one message, then
    /// the worker ends).
    pub fn submit(courier: std::os::unix::net::UnixStream, to_worker: I, from_worker: O, o: &crate::submit::Outgoing<'_>) -> std::result::Result<crate::submit::Sent, crate::submit::SendError> {
        use crate::submit::SendError;
        let mut r = Remote { courier: Some(courier), http: None, to_worker, from_worker, caps: Vec::new(), gone: false, max_delayed_send: 0, filters: Default::default() };
        let c = Call::Submit { from: o.from.into(), rcpts: o.rcpts.to_vec(), bytes: o.bytes.to_vec(), hold_for: o.hold_for };
        match r.call(&c, None, QUIET) {
            Ok(Reply::Submitted(x)) => x,
            Ok(Reply::Failed(_, t)) => Err(SendError::Failed { why: t, transient: true }),
            Ok(other) => Err(SendError::Failed { why: format!("the worker answered {other:?}"), transient: true }),
            Err(e) => Err(SendError::Failed { why: format!("the sync worker: {e}"), transient: true }),
        }
    }

    /// Put Mail's filters on a ManageSieve `courier` in the worker (then
    /// the worker ends).
    pub fn sieve(courier: std::os::unix::net::UnixStream, to_worker: I, from_worker: O, plan: &crate::sieve::Plan) -> std::result::Result<crate::sieve::Sieved, String> {
        let mut r = Remote { courier: Some(courier), http: None, to_worker, from_worker, caps: Vec::new(), gone: false, max_delayed_send: 0, filters: Default::default() };
        match r.call(&Call::Sieve(plan.clone()), None, QUIET) {
            Ok(Reply::Sieved(x)) => x,
            Ok(Reply::Failed(_, t)) => Err(t),
            Ok(other) => Err(format!("the worker answered {other:?}")),
            Err(e) => Err(format!("the sync worker: {e}")),
        }
    }

    /// A JMAP account in the worker: its session at `url`, the HTTP done
    /// here with `http` (the courier's).
    pub fn connect_jmap(http: Box<dyn crate::jmap::Http>, to_worker: I, from_worker: O, url: &str) -> Result<Remote<I, O>> {
        let mut r = Remote { courier: None, http: Some(http), to_worker, from_worker, caps: Vec::new(), gone: true, max_delayed_send: 0, filters: Default::default() };
        match r.call(&Call::JmapConnect { url: url.into() }, None, QUIET)? {
            Reply::JmapReady { max_delayed_send, filters } => {
                r.max_delayed_send = max_delayed_send;
                r.filters = filters;
            }
            Reply::Failed(k, t) => return Err(error_of(k, t)),
            other => return Err(Error::Protocol(format!("the worker answered {other:?}"))),
        }
        Ok(r)
    }

    /// Watch a JMAP account's event source in the worker: the stream comes
    /// from this side's HTTP, its events go to the worker, and `on` hears
    /// the worker's verdict on each (see [`crate::jmap::Client::watch`]).
    pub fn watch_jmap(&mut self, on: &mut dyn FnMut(bool) -> bool) -> Result<crate::jmap::Pushed> {
        let Remote { to_worker, from_worker, http, .. } = self;
        let Some(http) = http.as_mut() else { return Err(Error::Protocol("not a JMAP worker".into())) };
        write_frame(to_worker, CALL, &enc(&Call::JmapWatch)?)?;
        let next = |from: &mut O| -> Result<(u8, Vec<u8>)> { read_frame(from)?.ok_or_else(|| Error::Protocol("the sync worker ended".into())) };
        loop {
            let (k, b) = next(from_worker)?;
            match (k, if k == SINK { Some(dec::<SinkCall>(&b)?) } else { None }) {
                (REPLY, _) => {
                    return match dec::<Reply>(&b)? {
                        Reply::Watched(r) => r.map_err(Error::Protocol),
                        Reply::Failed(k, t) => Err(error_of(k, t)),
                        other => Err(Error::Protocol(format!("the worker answered {other:?}"))),
                    };
                }
                (SINK, Some(SinkCall::Events { url })) => {
                    let mut broken: Option<Error> = None;
                    let r = http.events(&url, &mut |raw| {
                        let go = (|| -> Result<bool> {
                            write_frame(to_worker, ANSWER, &enc(&SinkAnswer::Event(raw.to_vec()))?)?;
                            let (k, b) = next(from_worker)?;
                            match (k, dec::<SinkCall>(&b)) {
                                (SINK, Ok(SinkCall::Pushed(news))) => {
                                    let go = on(news);
                                    write_frame(to_worker, ANSWER, &enc(&SinkAnswer::Go(go))?)?;
                                    Ok(go)
                                }
                                _ => Err(Error::Protocol("the worker didn't judge an event".into())),
                            }
                        })();
                        go.unwrap_or_else(|e| {
                            broken = Some(e);
                            false
                        })
                    });
                    if let Some(e) = broken {
                        return Err(e);
                    }
                    write_frame(to_worker, ANSWER, &enc(&SinkAnswer::EventsEnd(r))?)?;
                }
                (SINK, Some(SinkCall::Get { .. } | SinkCall::Post { .. })) => write_frame(to_worker, ANSWER, &enc(&SinkAnswer::Body(Err("not while watching".into())))?)?,
                (SINK, Some(_)) => return Err(Error::Protocol("a store call while watching".into())),
                _ => return Err(Error::Protocol("a frame the worker doesn't send".into())),
            }
        }
    }

    /// One call: relay until its reply.
    fn call(&mut self, call: &Call, mut sink: Option<&mut dyn Sink>, wait: Duration) -> Result<Reply> {
        let b = enc(call)?;
        write_frame(&mut self.to_worker, CALL, &b)?;
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let courier_fd = self.courier.as_ref().map_or(-1, |c| c.as_raw_fd());
            let mut fds = [
                libc::pollfd { fd: self.from_worker.as_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: courier_fd, events: if self.gone { 0 } else { libc::POLLIN }, revents: 0 },
            ];
            // A JMAP worker's wait is the HTTP's, not a courier's.
            let ms = if self.courier.is_some() { wait.as_millis().min(i32::MAX as u128) as i32 } else { -1 };
            let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, ms) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e.into());
            }
            if n == 0 {
                write_frame(&mut self.to_worker, TIMEOUT, &[])?;
                continue;
            }
            if fds[0].revents != 0 {
                let (k, b) = read_frame(&mut self.from_worker)?.ok_or_else(|| Error::Protocol("the sync worker ended".into()))?;
                match k {
                    WIRE => match self.courier.as_mut() {
                        Some(c) => c.write_all(&b)?,
                        None => return Err(Error::Protocol("server bytes for a JMAP worker".into())),
                    },
                    REPLY => return Ok(dec(&b)?),
                    SINK => {
                        let a = match dec::<SinkCall>(&b)? {
                            SinkCall::Get { url, max } => match self.http.as_mut() {
                                Some(h) => SinkAnswer::Body(h.get(&url, max.min(FRAME_MAX as u64) as usize)),
                                None => return Err(Error::Protocol("HTTP asked of an IMAP worker".into())),
                            },
                            SinkCall::Post { url, content_type, body, max } => match self.http.as_mut() {
                                Some(h) => SinkAnswer::Body(h.post(&url, &content_type, &body, max.min(FRAME_MAX as u64) as usize)),
                                None => return Err(Error::Protocol("HTTP asked of an IMAP worker".into())),
                            },
                            c => {
                                let Some(s) = sink.as_deref_mut() else { return Err(Error::Protocol("a store call outside sync".into())) };
                                answer(s, c)
                            }
                        };
                        write_frame(&mut self.to_worker, ANSWER, &enc(&a)?)?;
                    }
                    _ => return Err(Error::Protocol("a frame the worker doesn't send".into())),
                }
                continue;
            }
            if fds[1].revents != 0 {
                let Some(c) = self.courier.as_mut() else { continue };
                let sent = match c.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        self.gone = true;
                        write_frame(&mut self.to_worker, EOF, &[])
                    }
                    Ok(n) => write_frame(&mut self.to_worker, WIRE, &buf[..n]),
                };
                // The worker may have answered and ended while the server's
                // last bytes (or its close) came in: its reply is still to
                // be read, so a closed pipe here isn't the error.
                match sent {
                    Err(e) if e.kind() == io::ErrorKind::BrokenPipe => self.gone = true,
                    r => r?,
                }
            }
        }
    }

    fn reply<T>(&mut self, call: &Call, sink: Option<&mut dyn Sink>, wait: Duration, f: impl FnOnce(Reply) -> Option<T>) -> Result<T> {
        match self.call(call, sink, wait)? {
            Reply::Failed(k, t) => Err(error_of(k, t)),
            r => f(r.clone()).ok_or_else(|| Error::Protocol(format!("the worker answered {r:?}"))),
        }
    }
}

/// The engine's answer to a store call.
fn answer(s: &mut dyn Sink, c: SinkCall) -> SinkAnswer {
    match c {
        SinkCall::Folders(l) => SinkAnswer::Done(s.folders(&l)),
        SinkCall::State(f) => SinkAnswer::State(s.state(&f)),
        SinkCall::Reset(f) => SinkAnswer::Done(s.reset(&f)),
        SinkCall::KnownUids(f) => SinkAnswer::Uids(s.known_uids(&f)),
        SinkCall::Messages(f, p) => SinkAnswer::Done(s.messages(&f, p)),
        SinkCall::Flags(f, c) => SinkAnswer::Done(s.flags(&f, &c)),
        SinkCall::Vanished(f, u) => SinkAnswer::Done(s.vanished(&f, &u)),
        SinkCall::Checkpoint(f, st) => SinkAnswer::Done(s.checkpoint(&f, &st)),
        SinkCall::TextWanted(f, n) => SinkAnswer::Wanted(s.text_wanted(&f, n as usize)),
        SinkCall::Changes => SinkAnswer::Changes(s.changes()),
        SinkCall::ChangeDone(id, ok) => SinkAnswer::Done(s.change_done(id, ok)),
        SinkCall::ServerState(k) => SinkAnswer::Text(s.server_state(&k)),
        SinkCall::SetServerState(k, v) => SinkAnswer::Done(s.set_server_state(&k, &v)),
        SinkCall::JmapDestroyed(ids) => SinkAnswer::Done(s.jmap_destroyed(&ids)),
        SinkCall::Listed(id) => SinkAnswer::Listed(s.listed(&id)),
        SinkCall::ServerIdAt(f, u) => SinkAnswer::Text(s.server_id_at(&f, u)),
        SinkCall::Get { .. } | SinkCall::Post { .. } => SinkAnswer::Body(Err("no HTTP here".into())),
        SinkCall::Events { .. } => SinkAnswer::EventsEnd(Err("not watching".into())),
        SinkCall::Pushed(_) => SinkAnswer::Go(false),
        SinkCall::WantsSnoozed => SinkAnswer::WantsSnoozed(s.wants_snoozed()),
        SinkCall::SnoozeMoves => SinkAnswer::SnoozeMoves(s.snooze_moves()),
        SinkCall::HasText(f, u) => SinkAnswer::Go(s.has_text(&f, u)),
    }
}

impl<I: Write + AsFd, O: Read + AsFd> Imap for Remote<I, O> {
    fn capabilities(&self) -> &[String] {
        &self.caps
    }
    fn sync(&mut self, sink: &mut dyn Sink) -> Result<Report> {
        self.reply(&Call::Sync, Some(sink), QUIET, |r| match r {
            Reply::Synced(x) => Some(x),
            _ => None,
        })
    }
    fn append(&mut self, folder: &str, flags: &[&str], message: &[u8]) -> Result<Option<u32>> {
        let c = Call::Append { folder: folder.into(), flags: flags.iter().map(|f| f.to_string()).collect(), message: message.to_vec() };
        self.reply(&c, None, QUIET, |r| match r {
            Reply::Appended(x) => Some(x),
            _ => None,
        })
    }
    fn idle(&mut self, folder: &str, wait: Duration) -> Result<bool> {
        self.reply(&Call::Idle { folder: folder.into() }, None, wait, |r| match r {
            Reply::Idled(x) => Some(x),
            _ => None,
        })
    }
    fn logout(&mut self) -> Result<()> {
        self.reply(&Call::Logout, None, QUIET, |r| (r == Reply::LoggedOut).then_some(()))
    }
    fn search(&mut self, query: &crate::ServerQuery, sink: &mut dyn Sink, limit: usize) -> Result<usize> {
        let c = Call::Search { query: query.clone(), limit: limit.min(u32::MAX as usize) as u32 };
        self.reply(&c, Some(sink), QUIET, |r| match r {
            Reply::Found(n) => Some(n as usize),
            _ => None,
        })
    }
}

impl<I: Write + AsFd, O: Read + AsFd> crate::jmap::Jmap for Remote<I, O> {
    fn sync(&mut self, sink: &mut dyn Sink) -> crate::jmap::Result<Report> {
        match self.call(&Call::JmapSync, Some(sink), QUIET) {
            Ok(Reply::Synced(r)) => Ok(r),
            Ok(Reply::Failed(_, t)) => Err(crate::jmap::Error(t)),
            Ok(other) => Err(crate::jmap::Error(format!("the worker answered {other:?}"))),
            Err(e) => Err(crate::jmap::Error(format!("the sync worker: {e}"))),
        }
    }
    fn send(&mut self, message: &[u8], from: &str, rcpts: &[String], send_at: Option<i64>) -> crate::jmap::Result<String> {
        let c = Call::JmapSend { message: message.to_vec(), from: from.into(), rcpts: rcpts.to_vec(), send_at };
        match self.call(&c, None, QUIET) {
            Ok(Reply::JmapSent(r)) => r.map_err(crate::jmap::Error),
            Ok(Reply::Failed(_, t)) => Err(crate::jmap::Error(t)),
            Ok(other) => Err(crate::jmap::Error(format!("the worker answered {other:?}"))),
            Err(e) => Err(crate::jmap::Error(format!("the sync worker: {e}"))),
        }
    }
    fn watch(&mut self, on: &mut dyn FnMut(bool) -> bool) -> crate::jmap::Result<crate::jmap::Pushed> {
        self.watch_jmap(on).map_err(|e| crate::jmap::Error(e.to_string()))
    }
    fn search(&mut self, query: &crate::ServerQuery, sink: &mut dyn Sink, limit: usize) -> crate::jmap::Result<usize> {
        let c = Call::JmapSearch { query: query.clone(), limit: limit.min(u32::MAX as usize) as u32 };
        match self.call(&c, Some(sink), QUIET) {
            Ok(Reply::Found(n)) => Ok(n as usize),
            Ok(Reply::Failed(_, t)) => Err(crate::jmap::Error(t)),
            Ok(other) => Err(crate::jmap::Error(format!("the worker answered {other:?}"))),
            Err(e) => Err(crate::jmap::Error(format!("the sync worker: {e}"))),
        }
    }
    fn can_hold(&self, secs: i64) -> bool {
        secs > 0 && (secs as u64) <= self.max_delayed_send
    }
    fn filters(&self) -> crate::jmap::Filters {
        self.filters
    }
    fn sieve(&mut self, plan: &crate::sieve::Plan) -> crate::jmap::Result<Option<crate::sieve::Sieved>> {
        match self.call(&Call::JmapSieve(plan.clone()), None, QUIET) {
            Ok(Reply::JmapSieved(r)) => r.map_err(crate::jmap::Error),
            Ok(Reply::Failed(_, t)) => Err(crate::jmap::Error(t)),
            Ok(other) => Err(crate::jmap::Error(format!("the worker answered {other:?}"))),
            Err(e) => Err(crate::jmap::Error(format!("the sync worker: {e}"))),
        }
    }
    fn vacation(&mut self, set: Option<&crate::jmap::Vacation>) -> crate::jmap::Result<Option<crate::jmap::Vacation>> {
        match self.call(&Call::JmapVacation(set.cloned()), None, QUIET) {
            Ok(Reply::Vacation(r)) => r.map_err(crate::jmap::Error),
            Ok(Reply::Failed(_, t)) => Err(crate::jmap::Error(t)),
            Ok(other) => Err(crate::jmap::Error(format!("the worker answered {other:?}"))),
            Err(e) => Err(crate::jmap::Error(format!("the sync worker: {e}"))),
        }
    }
    fn import(&mut self, folder: &str, message: &[u8]) -> crate::jmap::Result<String> {
        let c = Call::JmapImport { folder: folder.into(), message: message.to_vec() };
        match self.call(&c, None, QUIET) {
            Ok(Reply::JmapSent(r)) => r.map_err(crate::jmap::Error),
            Ok(Reply::Failed(_, t)) => Err(crate::jmap::Error(t)),
            Ok(other) => Err(crate::jmap::Error(format!("the worker answered {other:?}"))),
            Err(e) => Err(crate::jmap::Error(format!("the sync worker: {e}"))),
        }
    }
}
