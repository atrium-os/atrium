//! Mail's account sync for IMAP (docs/spec/mail.md §6.1, §6.4).
//!
//! The session is the Arca mail courier's relay: it begins with
//! `* PREAUTH [CAPABILITY …]` and has no way to log in (§5.2). Sync reads the
//! server, hands what it fetched to a [`Sink`] (the engine, which parses in
//! jails and stores), and checkpoints after every page, so it resumes where
//! it stopped.
//!
//! One pass ([`Session::sync`]), in the order §6.4 fixes:
//!
//! 1. **Usable in seconds**: the folder list, then the inbox's newest
//!    [`STAGE1`] messages, header blocks only.
//! 2. **Headers for everything**: the rest of each folder, newest first, in
//!    pages of [`PAGE`]: the inbox, Sent, then the others. On Gmail only
//!    the inbox, Sent and All Mail are walked; the same message in several
//!    labels is one message by its `X-GM-MSGID` (and by `EMAILID` where a
//!    server has OBJECTID).
//! 3. **Recent text**: whole messages up to [`TEXT_MAX`] for the last
//!    [`Sink::text_wanted`] window, newest first, a few at a time.
//!
//! Resync of a folder already known: QRESYNC in `SELECT` (flag changes and
//! `VANISHED` in one round trip), else CONDSTORE's `CHANGEDSINCE`, else all
//! flags; expunges without QRESYNC by comparing UID lists.
//!
//! Read-only (M2): folders are opened with `EXAMINE`, so syncing never
//! marks anything read.

pub mod jmap;
pub mod remote;
pub mod setup;
pub mod submit;

use std::io::{Read, Write};

use mail_proto::imap::{Chunk, Cmd, Code, Decoder, Encoder, FetchItem, FetchWant, Limits, Response, Search, Set, Status, StoreOp};

/// Newest inbox messages listed first.
pub const STAGE1: usize = 200;
/// Headers per page after that.
pub const PAGE: usize = 500;
/// Whole messages fetched for the text window: larger ones wait to be opened.
pub const TEXT_MAX: u64 = 2 << 20;
/// Whole messages per fetch.
const TEXT_BATCH: usize = 20;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// The server's bytes were not IMAP or over a limit.
    Protocol(String),
    /// The courier said BYE (sign-in refused, Arca locked, server gone).
    Bye(String),
    /// `[THROTTLED]`, `[LIMIT]` and friends: back off (§6.4).
    Throttled,
    /// A command failed.
    No(String),
    /// The sink refused (the engine's store failed).
    Sink(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "connection: {e}"),
            Error::Protocol(e) => write!(f, "protocol: {e}"),
            Error::Bye(t) => write!(f, "server closed: {t}"),
            Error::Throttled => f.write_str("server asked to slow down"),
            Error::No(t) => write!(f, "refused: {t}"),
            Error::Sink(t) => write!(f, "store: {t}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self { Error::Io(e) }
}

pub type Result<T> = std::result::Result<T, Error>;

/// A folder's role from SPECIAL-USE (RFC 6154) or its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Role { Inbox, Sent, Drafts, Archive, Junk, Trash, All, Flagged, Other }

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemoteFolder {
    pub name: String,
    pub role: Role,
    pub selectable: bool,
}

/// Where a folder's sync stands, as the sink keeps it.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FolderState {
    pub uidvalidity: u32,
    /// Every UID ≥ this one (that the server had) has been fetched; 0 when
    /// the folder was never walked.
    pub synced_from: u32,
    pub uidnext: u32,
    pub highestmodseq: u64,
    pub exists: u32,
}

/// One message as fetched.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Fetched {
    pub uid: u32,
    pub flags: Vec<String>,
    pub size: u64,
    /// Seconds since the epoch, from INTERNALDATE.
    pub internal_date: Option<i64>,
    /// `EMAILID` or `X-GM-MSGID`: the same message in another folder.
    pub server_id: Option<String>,
    /// The header block, or the whole message when `full`.
    pub bytes: Vec<u8>,
    pub full: bool,
}

/// The engine, as sync sees it.
pub trait Sink {
    fn folders(&mut self, list: &[RemoteFolder]) -> std::result::Result<(), String>;
    fn state(&mut self, folder: &str) -> FolderState;
    /// UIDVALIDITY changed (or the folder is new): forget what it listed.
    fn reset(&mut self, folder: &str) -> std::result::Result<(), String>;
    /// UIDs the folder lists locally, ascending.
    fn known_uids(&mut self, folder: &str) -> Vec<u32>;
    /// A page of fetched messages; the sink parses and stores them.
    fn messages(&mut self, folder: &str, page: Vec<Fetched>) -> std::result::Result<(), String>;
    fn flags(&mut self, folder: &str, changes: &[(u32, Vec<String>)]) -> std::result::Result<(), String>;
    fn vanished(&mut self, folder: &str, uids: &[u32]) -> std::result::Result<(), String>;
    /// Save the folder's progress (after every page).
    fn checkpoint(&mut self, folder: &str, state: &FolderState) -> std::result::Result<(), String>;
    /// UIDs in the text window still without their text, newest first, at
    /// most `limit`, with sizes as listed.
    fn text_wanted(&mut self, folder: &str, limit: usize) -> Vec<(u32, u64)>;
    /// The person's changes due now, to make on the server first.
    fn changes(&mut self) -> Vec<Change> {
        Vec::new()
    }
    /// The server made change `id` (`ok`), or refused it.
    fn change_done(&mut self, id: u64, ok: bool) -> std::result::Result<(), String> {
        let _ = (id, ok);
        Ok(())
    }
    /// A sync state the account keeps (JMAP's Email state).
    fn server_state(&mut self, key: &str) -> Option<String> {
        let _ = key;
        None
    }
    fn set_server_state(&mut self, key: &str, value: &str) -> std::result::Result<(), String> {
        let _ = (key, value);
        Err("this store can't keep a server state".into())
    }
    /// Where the message with server id `id` is listed: (folder, UID).
    fn listed(&mut self, id: &str) -> Vec<(String, u32)> {
        let _ = id;
        Vec::new()
    }
    /// The server id of the message at `folder`/`uid`.
    fn server_id_at(&mut self, folder: &str, uid: u32) -> Option<String> {
        let _ = (folder, uid);
        None
    }
}

/// A change the person made, to make on the server (mail.md §6.3).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Change {
    pub id: u64,
    pub folder: String,
    pub uid: u32,
    pub what: What,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum What {
    /// IMAP system flags (`\Seen`, `\Flagged`) to add and remove.
    Flags { add: Vec<String>, remove: Vec<String> },
    Move { to: String },
    /// Delete for good.
    Expunge,
}

/// What one pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Report {
    pub folders: usize,
    pub headers: usize,
    pub texts: usize,
    pub flag_changes: usize,
    pub vanished: usize,
}

/// The IMAP session over the courier's socket.
pub struct Session<S: Read + Write> {
    s: S,
    dec: Decoder,
    enc: Encoder,
    caps: Vec<String>,
    qresync: bool,
    /// Untagged responses read while waiting for something else.
    pending: Vec<Response>,
}

fn has(caps: &[String], c: &str) -> bool {
    caps.iter().any(|x| x.eq_ignore_ascii_case(c))
}

impl<S: Read + Write> Session<S> {
    /// Read the courier's greeting: the session is already signed in.
    pub fn start(s: S) -> Result<Session<S>> {
        let mut me = Session { s, dec: Decoder::new(Limits::default()), enc: Encoder::new(), caps: Vec::new(), qresync: false, pending: Vec::new() };
        match me.read()? {
            Response::State { status: Status::PreAuth, code, .. } => {
                if let Some(Code::Capability(c)) = code {
                    me.caps = c;
                }
            }
            Response::State { status: Status::Bye, text, .. } => return Err(Error::Bye(text)),
            other => return Err(Error::Protocol(format!("greeting: {other:?}"))),
        }
        if me.caps.is_empty() {
            let cmd = me.enc.capability();
            me.run(cmd)?;
        }
        me.enc.set_capabilities(&me.caps);
        if has(&me.caps, "QRESYNC") {
            let cmd = me.enc.enable(&["QRESYNC"]);
            let (rs, _) = me.run(cmd)?;
            me.qresync = rs.iter().any(|r| matches!(r, Response::Enabled(e) if has(e, "QRESYNC")));
        }
        Ok(me)
    }

    pub fn capabilities(&self) -> &[String] { &self.caps }

    /// The socket under the session (to set its read timeout).
    pub fn socket(&self) -> &S { &self.s }

    fn read(&mut self) -> Result<Response> {
        loop {
            if let Some(r) = self.dec.next() {
                return r.map_err(|e| Error::Protocol(format!("{e:?}")));
            }
            let mut b = [0u8; 64 * 1024];
            let n = self.s.read(&mut b)?;
            if n == 0 {
                return Err(Error::Io(std::io::ErrorKind::UnexpectedEof.into()));
            }
            self.dec.feed(&b[..n]);
        }
    }

    /// Send a command; collect untagged responses until its tagged one.
    fn run(&mut self, cmd: Cmd) -> Result<(Vec<Response>, Option<Code>)> {
        let mut out = std::mem::take(&mut self.pending);
        for c in cmd.chunks {
            match c {
                Chunk::Send(b) => self.s.write_all(&b)?,
                Chunk::AwaitContinue => {
                    self.s.flush()?;
                    loop {
                        match self.read()? {
                            Response::Continue(_) => break,
                            Response::Tagged { tag, status, code, text } if tag == cmd.tag => return Self::done(status, code, text, out),
                            r => out.push(r),
                        }
                    }
                }
            }
        }
        self.s.flush()?;
        loop {
            match self.read()? {
                Response::Tagged { tag, status, code, text } if tag == cmd.tag => return Self::done(status, code, text, out),
                Response::State { status: Status::Bye, text, .. } => return Err(Error::Bye(text)),
                Response::Capability(c) => {
                    self.caps = c;
                }
                r => out.push(r),
            }
        }
    }

    fn done(status: Status, code: Option<Code>, text: String, out: Vec<Response>) -> Result<(Vec<Response>, Option<Code>)> {
        match status {
            Status::Ok => Ok((out, code)),
            _ if code == Some(Code::Throttled) => Err(Error::Throttled),
            _ => Err(Error::No(text)),
        }
    }

    /// The folder list, with roles.
    pub fn folders(&mut self) -> Result<Vec<RemoteFolder>> {
        let cmd = self.enc.list(false);
        let (rs, _) = self.run(cmd)?;
        let mut out = Vec::new();
        for r in rs {
            let Response::List { flags, name, .. } = r else { continue };
            let f = |x: &str| has(&flags, x);
            let role = if name.eq_ignore_ascii_case("INBOX") {
                Role::Inbox
            } else if f("\\Sent") {
                Role::Sent
            } else if f("\\Drafts") {
                Role::Drafts
            } else if f("\\Archive") {
                Role::Archive
            } else if f("\\Junk") {
                Role::Junk
            } else if f("\\Trash") {
                Role::Trash
            } else if f("\\All") {
                Role::All
            } else if f("\\Flagged") {
                Role::Flagged
            } else {
                Role::Other
            };
            let selectable = !(f("\\Noselect") || f("\\NonExistent"));
            out.push(RemoteFolder { name, role, selectable });
        }
        out.sort_by_key(|f| (f.role != Role::Inbox, f.role != Role::Sent, f.role != Role::All, f.name.clone()));
        Ok(out)
    }

    /// Which folders a pass walks: on Gmail, the inbox, Sent and All Mail
    /// (labels are views of All Mail); elsewhere every selectable folder.
    fn walk(&self, all: &[RemoteFolder]) -> Vec<RemoteFolder> {
        let gmail = has(&self.caps, "X-GM-EXT-1") && all.iter().any(|f| f.role == Role::All);
        all.iter()
            .filter(|f| f.selectable && (!gmail || matches!(f.role, Role::Inbox | Role::Sent | Role::All)))
            .cloned()
            .collect()
    }

    fn header_items(&self) -> Vec<FetchWant> {
        let mut v = vec![FetchWant::Uid, FetchWant::Flags, FetchWant::Size, FetchWant::InternalDate,
                         FetchWant::BodyPeek { section: "HEADER".into(), partial: None }];
        if has(&self.caps, "X-GM-EXT-1") {
            v.push(FetchWant::GmMsgId);
        } else if has(&self.caps, "OBJECTID") {
            v.push(FetchWant::EmailId);
        }
        v
    }

    fn fetched(&self, rs: Vec<Response>, full: bool) -> (Vec<Fetched>, Vec<(u32, Vec<String>)>) {
        let mut msgs = Vec::new();
        let mut flags_only = Vec::new();
        for r in rs {
            let Response::Fetch { items, .. } = r else { continue };
            let mut f = Fetched { uid: 0, flags: Vec::new(), size: 0, internal_date: None, server_id: None, bytes: Vec::new(), full };
            let mut got_body = false;
            let mut got_flags = false;
            for it in items {
                match it {
                    FetchItem::Uid(u) => f.uid = u,
                    FetchItem::Flags(fl) => { f.flags = fl; got_flags = true }
                    FetchItem::Size(n) => f.size = n,
                    FetchItem::InternalDate(d) => f.internal_date = internal_date(&d),
                    FetchItem::EmailId(e) if !e.is_empty() => f.server_id = Some(format!("emailid:{e}")),
                    FetchItem::GmMsgId(g) if g != 0 => f.server_id = Some(format!("gm:{g}")),
                    FetchItem::Body { data: Some(d), .. } => { f.bytes = d; got_body = true }
                    _ => {}
                }
            }
            if f.uid == 0 {
                continue;
            }
            if got_body {
                msgs.push(f);
            } else if got_flags {
                flags_only.push((f.uid, f.flags));
            }
        }
        (msgs, flags_only)
    }

    /// One pass over the account.
    pub fn sync(&mut self, sink: &mut dyn Sink) -> Result<Report> {
        let mut report = Report::default();
        let all = self.folders()?;
        sink.folders(&all).map_err(Error::Sink)?;
        // The person's changes first, so the walk below sees their result.
        self.changes(sink)?;
        let walk = self.walk(&all);
        report.folders = walk.len();
        // Stage 1: the inbox, newest first, one small page.
        if let Some(inbox) = walk.iter().find(|f| f.role == Role::Inbox) {
            self.folder(&inbox.name, sink, Some(STAGE1), &mut report)?;
        }
        // Stage 2: headers for everything.
        for f in &walk {
            self.folder(&f.name, sink, None, &mut report)?;
        }
        // Stage 3: recent text.
        for f in &walk {
            self.texts(&f.name, sink, &mut report)?;
        }
        Ok(report)
    }

    /// Put a message in `folder` (a sent copy, a draft); its new UID when
    /// the server says (UIDPLUS).
    pub fn append(&mut self, folder: &str, flags: &[&str], message: &[u8]) -> Result<Option<u32>> {
        let cmd = self.enc.append(folder, flags, message);
        let (_, code) = self.run(cmd)?;
        Ok(match code {
            Some(Code::AppendUid { uids, .. }) => uids.parse().ok(),
            _ => None,
        })
    }

    /// Make the person's due changes on the server, folder by folder. A
    /// change the server refuses is reported so the index goes back; a
    /// lost connection leaves the rest for the next pass.
    fn changes(&mut self, sink: &mut dyn Sink) -> Result<()> {
        let mut todo = sink.changes();
        if todo.is_empty() {
            return Ok(());
        }
        todo.sort_by(|a, b| a.folder.cmp(&b.folder).then(a.id.cmp(&b.id)));
        let uidplus = has(&self.caps, "UIDPLUS");
        let moves = has(&self.caps, "MOVE");
        let mut open: Option<String> = None;
        for c in todo {
            if open.as_deref() != Some(c.folder.as_str()) {
                let cmd = self.enc.select(&c.folder, false, None, false);
                match self.run(cmd) {
                    Ok(_) => open = Some(c.folder.clone()),
                    Err(Error::No(_)) => {
                        sink.change_done(c.id, false).map_err(Error::Sink)?;
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
            let set = Set::one(c.uid);
            let r = match &c.what {
                What::Flags { add, remove } => {
                    let mut r = Ok(());
                    for (op, flags) in [(StoreOp::Add, add), (StoreOp::Remove, remove)] {
                        if r.is_ok() && !flags.is_empty() {
                            let f: Vec<&str> = flags.iter().map(String::as_str).collect();
                            let cmd = self.enc.uid_store(&set, op, &f, None);
                            r = self.run(cmd).map(|_| ());
                        }
                    }
                    r
                }
                What::Move { to } if moves => {
                    let cmd = self.enc.uid_move(&set, to);
                    self.run(cmd).map(|_| ())
                }
                // Without MOVE: copy, then delete only this UID (UIDPLUS);
                // a plain EXPUNGE could take others' \Deleted messages.
                What::Move { to } if uidplus => {
                    let cmd = self.enc.uid_copy(&set, to);
                    self.run(cmd).and_then(|_| self.expunge_one(&set))
                }
                What::Expunge if uidplus => self.expunge_one(&set),
                _ => Err(Error::No("the server can't move or delete one message alone (no MOVE, no UIDPLUS)".into())),
            };
            match r {
                Ok(()) => sink.change_done(c.id, true).map_err(Error::Sink)?,
                Err(Error::No(_)) => sink.change_done(c.id, false).map_err(Error::Sink)?,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn expunge_one(&mut self, set: &Set) -> Result<()> {
        let cmd = self.enc.uid_store(set, StoreOp::Add, &["\\Deleted"], None);
        self.run(cmd)?;
        let cmd = self.enc.uid_expunge(set);
        self.run(cmd).map(|_| ())
    }

    /// Open a folder and bring it up to date; with `first_page`, stop after
    /// that many older messages (stage 1).
    fn folder(&mut self, name: &str, sink: &mut dyn Sink, first_page: Option<usize>, report: &mut Report) -> Result<()> {
        let mut st = sink.state(name);
        let condstore = has(&self.caps, "CONDSTORE") || self.qresync;
        let known_for_qresync = self.qresync && st.uidvalidity != 0 && st.highestmodseq != 0;
        let cmd = self.enc.select(name, true, known_for_qresync.then_some((st.uidvalidity, st.highestmodseq, None)), condstore && !known_for_qresync);
        let (rs, _) = self.run(cmd)?;
        let (mut validity, mut uidnext, mut modseq, mut exists, mut nomodseq) = (0, 0, 0, 0, false);
        let mut vanished = Vec::new();
        let mut changed: Vec<Response> = Vec::new();
        for r in rs {
            match r {
                Response::State { code: Some(Code::UidValidity(v)), .. } => validity = v,
                Response::State { code: Some(Code::UidNext(n)), .. } => uidnext = n,
                Response::State { code: Some(Code::HighestModSeq(m)), .. } => modseq = m,
                Response::State { code: Some(Code::NoModSeq), .. } => nomodseq = true,
                Response::Exists(n) => exists = n,
                Response::Vanished { uids, .. } => vanished.extend(expand(&uids)),
                r @ Response::Fetch { .. } => changed.push(r),
                _ => {}
            }
        }
        if validity != st.uidvalidity {
            sink.reset(name).map_err(Error::Sink)?;
            st = FolderState { uidvalidity: validity, ..FolderState::default() };
            vanished.clear();
            changed.clear();
        }
        st.exists = exists;
        let had = st.synced_from != 0 || st.uidnext != 0;

        // What changed among the known messages.
        if had {
            if known_for_qresync {
                let (_, fl) = self.fetched(changed, false);
                report.flag_changes += fl.len();
                sink.flags(name, &fl).map_err(Error::Sink)?;
            } else if condstore && !nomodseq && st.highestmodseq != 0 {
                if st.highestmodseq != modseq {
                    let cmd = self.enc.uid_fetch(&Set::from(1), &[FetchWant::Uid, FetchWant::Flags], Some(st.highestmodseq), false);
                    let (rs, _) = self.run(cmd)?;
                    let (_, fl) = self.fetched(rs, false);
                    report.flag_changes += fl.len();
                    sink.flags(name, &fl).map_err(Error::Sink)?;
                }
            } else {
                let cmd = self.enc.uid_fetch(&Set::from(1), &[FetchWant::Uid, FetchWant::Flags], None, false);
                let (rs, _) = self.run(cmd)?;
                let (_, fl) = self.fetched(rs, false);
                report.flag_changes += fl.len();
                sink.flags(name, &fl).map_err(Error::Sink)?;
            }
        }

        // Which UIDs the server has (needed for new ones, the walk, and
        // expunges without QRESYNC).
        let server_uids: Vec<u32> = if exists == 0 {
            Vec::new()
        } else {
            let cmd = self.enc.uid_search(&Search::All);
            let (rs, _) = self.run(cmd)?;
            let mut v: Vec<u32> = rs.into_iter().flat_map(|r| match r { Response::Search { ids, .. } => ids, _ => Vec::new() }).collect();
            v.sort_unstable();
            v
        };
        if had && !known_for_qresync {
            let known = sink.known_uids(name);
            let gone: Vec<u32> = known.iter().copied().filter(|u| server_uids.binary_search(u).is_err()).collect();
            vanished.extend(gone);
        }
        if !vanished.is_empty() {
            vanished.sort_unstable();
            vanished.dedup();
            report.vanished += vanished.len();
            sink.vanished(name, &vanished).map_err(Error::Sink)?;
        }

        // New since last time, then the walk down, newest first.
        let new_from = st.uidnext.max(1);
        let mut todo: Vec<u32> = server_uids.iter().copied().filter(|u| had && *u >= new_from).collect();
        let older: Vec<u32> = server_uids.iter().copied().filter(|u| st.synced_from == 0 || *u < st.synced_from).filter(|u| !(had && *u >= new_from)).collect();
        todo.extend(older);
        todo.sort_unstable_by(|a, b| b.cmp(a));
        todo.dedup();
        let limit = first_page.unwrap_or(usize::MAX);
        let items = self.header_items();
        let mut done = 0;
        for page in todo.chunks(first_page.unwrap_or(PAGE).min(PAGE)) {
            if done >= limit {
                break;
            }
            let cmd = self.enc.uid_fetch(&Set::of(page), &items, None, false);
            let (rs, _) = self.run(cmd)?;
            let (msgs, _) = self.fetched(rs, false);
            done += page.len();
            report.headers += msgs.len();
            sink.messages(name, msgs).map_err(Error::Sink)?;
            let lowest = *page.iter().min().unwrap();
            // Checkpoint: everything from `lowest` up is here (new UIDs were
            // fetched first, and pages go down).
            if st.synced_from == 0 || lowest < st.synced_from {
                st.synced_from = lowest;
            }
            st.uidnext = st.uidnext.max(page.iter().max().unwrap() + 1);
            sink.checkpoint(name, &st).map_err(Error::Sink)?;
        }
        if first_page.is_none() || todo.len() <= limit {
            // The whole folder is walked.
            st.synced_from = 1;
        }
        st.uidnext = st.uidnext.max(uidnext);
        if modseq != 0 {
            st.highestmodseq = modseq;
        }
        st.uidvalidity = validity;
        sink.checkpoint(name, &st).map_err(Error::Sink)?;
        Ok(())
    }

    /// Stage 3 for one folder: whole messages for the text window.
    fn texts(&mut self, name: &str, sink: &mut dyn Sink, report: &mut Report) -> Result<()> {
        let want: Vec<u32> = sink.text_wanted(name, 10_000).into_iter().filter(|(_, size)| *size <= TEXT_MAX).map(|(u, _)| u).collect();
        if want.is_empty() {
            return Ok(());
        }
        let cmd = self.enc.select(name, true, None, false);
        self.run(cmd)?;
        let mut items = vec![FetchWant::Uid, FetchWant::Flags, FetchWant::Size, FetchWant::InternalDate, FetchWant::BodyPeek { section: String::new(), partial: None }];
        if has(&self.caps, "X-GM-EXT-1") {
            items.push(FetchWant::GmMsgId);
        } else if has(&self.caps, "OBJECTID") {
            items.push(FetchWant::EmailId);
        }
        for batch in want.chunks(TEXT_BATCH) {
            let cmd = self.enc.uid_fetch(&Set::of(batch), &items, None, false);
            let (rs, _) = self.run(cmd)?;
            let (msgs, _) = self.fetched(rs, true);
            report.texts += msgs.len();
            sink.messages(name, msgs).map_err(Error::Sink)?;
        }
        Ok(())
    }

    /// Wait in `IDLE` on `folder` until the server reports a change or
    /// `timeout` passes (the caller sets the socket's read timeout to it).
    /// `true`: something changed, sync again.
    pub fn idle(&mut self, folder: &str) -> Result<bool> {
        let cmd = self.enc.select(folder, true, None, false);
        self.run(cmd)?;
        let cmd = self.enc.idle();
        let tag = cmd.tag.clone();
        for c in cmd.chunks {
            if let Chunk::Send(b) = c {
                self.s.write_all(&b)?;
            }
        }
        self.s.flush()?;
        // The continuation, then wait.
        let changed = loop {
            match self.read() {
                Ok(Response::Continue(_)) => continue,
                Ok(Response::Exists(_) | Response::Expunge(_) | Response::Vanished { .. } | Response::Fetch { .. }) => break true,
                Ok(Response::State { status: Status::Bye, text, .. }) => return Err(Error::Bye(text)),
                Ok(_) => continue,
                Err(Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => break false,
                Err(e) => return Err(e),
            }
        };
        self.s.write_all(&Encoder::done())?;
        self.s.flush()?;
        loop {
            match self.read()? {
                Response::Tagged { tag: t, .. } if t == tag => return Ok(changed),
                r => self.pending.push(r),
            }
        }
    }

    pub fn logout(mut self) -> Result<()> {
        let cmd = self.enc.logout();
        let _ = self.run(cmd);
        Ok(())
    }
}

/// `1:3,7,9:10` → its UIDs (bounded: a hostile range can't make us allocate
/// four billion entries).
pub fn expand(set: &str) -> Vec<u32> {
    let mut out = Vec::new();
    for part in set.split(',') {
        let (a, b) = match part.split_once(':') {
            Some((a, b)) => (a.parse::<u32>().ok(), b.parse::<u32>().ok()),
            None => (part.parse().ok(), part.parse().ok()),
        };
        let (Some(a), Some(b)) = (a, b) else { continue };
        let (lo, hi) = (a.min(b), a.max(b));
        if hi - lo > 1_000_000 || out.len() > 4_000_000 {
            continue;
        }
        out.extend(lo..=hi);
    }
    out
}

/// INTERNALDATE `17-Jul-1996 02:44:25 -0700` → seconds since the epoch.
pub fn internal_date(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, rest) = s.split_once(' ')?;
    let (time, zone) = rest.trim().split_once(' ')?;
    let mut d = date.trim().split('-');
    let day: i64 = d.next()?.trim().parse().ok()?;
    let mon = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"]
        .iter()
        .position(|m| d.clone().next().is_some_and(|x| x.eq_ignore_ascii_case(m)))? as i64 + 1;
    d.next();
    let year: i64 = d.next()?.parse().ok()?;
    let mut t = time.split(':');
    let (h, m, sec): (i64, i64, i64) = (t.next()?.parse().ok()?, t.next()?.parse().ok()?, t.next()?.parse().ok()?);
    let z = zone.trim();
    if z.len() != 5 {
        return None;
    }
    let sign = if z.starts_with('-') { -1 } else { 1 };
    let zh: i64 = z[1..3].parse().ok()?;
    let zm: i64 = z[3..5].parse().ok()?;
    // Days from civil (Howard Hinnant's algorithm).
    let y = if mon <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (mon + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + m * 60 + sec - sign * (zh * 3600 + zm * 60))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_and_sets() {
        assert_eq!(internal_date("17-Jul-1996 02:44:25 -0700"), Some(837_596_665));
        assert_eq!(internal_date(" 1-Jan-2026 00:00:00 +0000"), Some(1_767_225_600));
        assert_eq!(expand("1:3,7,10:9"), vec![1, 2, 3, 7, 9, 10]);
        assert_eq!(expand("1:4000000000").len(), 0);
    }
}
