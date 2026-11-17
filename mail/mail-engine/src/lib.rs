//! Mail's engine (docs/spec/mail.md §4, §6).
//!
//! One per Mail instance: it owns the store, takes what mail-sync fetched,
//! has every message parsed in a parse jail, checks what the jail says
//! (mail-schema `check`), and keeps only that. It has no network and parses
//! nothing itself.
//!
//! - [`Parser`]: runs `mail-parse-jail`, one jail per batch of one account's
//!   messages (≤ 64 messages or 32 MiB, §4), one jail per message opened.
//!   A jail that sends an invalid frame is killed and its batch counts as
//!   refused.
//! - [`AccountSink`]: mail-sync's [`Sink`] for one account: folders and their
//!   checkpoints, pages of messages through the parser into the store,
//!   flags, expunges, and which messages still want their text.
//! - [`Engine::open`]: a message for reading, parsed alone in its own jail.

use std::io::{BufReader, BufWriter};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

pub mod attachments;
pub mod compose;
pub mod drafts;
pub mod pictures;
pub mod send;

use mail_schema::{self as wire, Caps, FromJail, Opened, Summary, ToJail};
use mail_store::{flag, AccountId, Folder, FolderId, Incoming, OpKind, Role, Store};
use mail_sync::{FolderState, Sink};

/// How the engine starts a parse jail. On Atrium jaild starts it with no
/// network and no files (§4); on a development host it is the binary alone.
#[derive(Debug, Clone)]
pub struct Parser {
    pub program: PathBuf,
}

/// The text window of the staged first sync (§6.4): 90 days.
pub const TEXT_WINDOW: i64 = 90 * 86_400;

#[derive(Debug)]
pub enum Error {
    Store(mail_store::Error),
    /// The jail could not be started or sent an invalid frame.
    Jail(String),
    NotFound,
    /// An action this account can't do, said why.
    Refused(&'static str),
    /// The draft can't be written as a message (a bad address, too big).
    Compose(String),
    /// The server said no (filing in Sent).
    Sync(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Store(e) => write!(f, "{e}"),
            Error::Jail(e) => write!(f, "parse jail: {e}"),
            Error::NotFound => f.write_str("no such message"),
            Error::Refused(why) => f.write_str(why),
            Error::Compose(e) => f.write_str(e),
            Error::Sync(e) => write!(f, "server: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<mail_store::Error> for Error {
    fn from(e: mail_store::Error) -> Self { Error::Store(e) }
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// One running jail.
struct Jail {
    child: Child,
    r: BufReader<std::process::ChildStdout>,
    w: BufWriter<std::process::ChildStdin>,
}

impl Drop for Jail {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Parser {
    fn start(&self) -> Result<Jail, Error> {
        let mut child = Command::new(&self.program)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| Error::Jail(format!("start {}: {e}", self.program.display())))?;
        let r = BufReader::new(child.stdout.take().unwrap());
        let w = BufWriter::new(child.stdin.take().unwrap());
        Ok(Jail { child, r, w })
    }

    /// Frames until `Done`, answering lookups (none yet: DKIM keys come from
    /// mail-sync's resolver in a later step, so signed mail stays Pending).
    fn exchange(j: &mut Jail, lens: &dyn Fn(u64) -> Option<u64>) -> Result<Vec<FromJail>, Error> {
        let caps = Caps::default();
        let mut out = Vec::new();
        loop {
            let f: FromJail = wire::read_frame(&mut j.r).map_err(|e| Error::Jail(e.to_string()))?.ok_or_else(|| Error::Jail("closed".into()))?;
            let raw_len = match &f {
                FromJail::Summary { id, .. } | FromJail::Opened { id, .. } | FromJail::Attachment { id, .. } => lens(*id).ok_or_else(|| Error::Jail("answer for a message it was not given".into()))?,
                _ => 0,
            };
            f.check(&caps, raw_len).map_err(|e| Error::Jail(e.to_string()))?;
            match f {
                FromJail::Query { query, .. } => {
                    wire::write_frame(&mut j.w, &ToJail::Answer { query, txt: None }).map_err(|e| Error::Jail(e.to_string()))?;
                }
                FromJail::Done => return Ok(out),
                other => out.push(other),
            }
        }
    }

    /// Summaries for one batch, in order; `None` for a message the jail
    /// refused (too large, not mail). An invalid jail fails the batch.
    pub fn summarize(&self, batch: &[(u64, &[u8])], headers_only: bool) -> Result<Vec<Option<Summary>>, Error> {
        let mut j = self.start()?;
        let msgs: Vec<wire::Raw> = batch.iter().map(|(id, b)| wire::Raw { id: *id, bytes: b.to_vec() }).collect();
        wire::write_frame(&mut j.w, &ToJail::Summarize { authserv_id: None, me: Vec::new(), now: now(), headers_only, batch: msgs })
            .map_err(|e| Error::Jail(e.to_string()))?;
        let lens = |id: u64| batch.iter().find(|(i, _)| *i == id).map(|(_, b)| b.len() as u64);
        let frames = Self::exchange(&mut j, &lens)?;
        let mut out = vec![None; batch.len()];
        let mut seen = 0;
        for f in frames {
            let FromJail::Summary { id, result } = f else { return Err(Error::Jail("unexpected frame".into())) };
            // Answers come in batch order, each once.
            if batch.get(seen).map(|(i, _)| *i) != Some(id) {
                return Err(Error::Jail("answers out of order".into()));
            }
            out[seen] = result.ok();
            seen += 1;
        }
        if seen != batch.len() {
            return Err(Error::Jail("missing answers".into()));
        }
        Ok(out)
    }

    /// One message for reading, alone in its jail.
    /// One attachment's bytes, alone in a jail; `None` when the jail
    /// refused (the part isn't there).
    pub fn attachment(&self, raw: &[u8], which: &wire::Which) -> Result<Option<wire::Attached>, Error> {
        let mut j = self.start()?;
        wire::write_frame(&mut j.w, &ToJail::Attachment { message: wire::Raw { id: 1, bytes: raw.to_vec() }, which: which.clone() })
            .map_err(|e| Error::Jail(e.to_string()))?;
        let len = raw.len() as u64;
        let frames = Self::exchange(&mut j, &|id| (id == 1).then_some(len))?;
        match frames.into_iter().next() {
            Some(FromJail::Attachment { result, .. }) => Ok(result.ok()),
            _ => Err(Error::Jail("no answer".into())),
        }
    }

    pub fn open(&self, raw: &[u8], loaded: Vec<(String, Vec<u8>)>) -> Result<Option<Opened>, Error> {
        let mut j = self.start()?;
        wire::write_frame(&mut j.w, &ToJail::Open { authserv_id: None, me: Vec::new(), now: now(), message: wire::Raw { id: 1, bytes: raw.to_vec() }, loaded })
            .map_err(|e| Error::Jail(e.to_string()))?;
        let len = raw.len() as u64;
        let frames = Self::exchange(&mut j, &|id| (id == 1).then_some(len))?;
        match frames.into_iter().next() {
            Some(FromJail::Opened { result, .. }) => Ok(result.ok()),
            _ => Err(Error::Jail("no answer".into())),
        }
    }
}

pub struct Engine {
    pub store: Store,
    pub parser: Parser,
}

/// What the reading pane shows (§7.2, §7.3).
#[derive(Debug, Clone)]
pub struct Reading {
    pub summary: Summary,
    /// The text, or `None` when only the headers are here yet.
    pub text: Option<String>,
    pub has_html: bool,
    /// The HTML body converted into a document, with the message's own
    /// pictures, for the document viewer.
    pub document: Option<wire::Document>,
    /// The attachments (§8.1), empty until the whole message is here.
    pub attachments: Vec<wire::Attachment>,
}

/// What a parse leaves for the next opening: the converted document and
/// the attachment list (a postcard blob, `Message::document`).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Shown {
    pub document: Option<wire::Document>,
    pub attachments: Vec<wire::Attachment>,
}

impl Engine {
    pub fn new(store: Store, parser: Parser) -> Engine {
        Engine { store, parser }
    }

    pub fn sink(&self, account: AccountId) -> AccountSink<'_> {
        AccountSink { e: self, account, refused: 0 }
    }

    /// A message for reading. Its whole bytes when here, parsed alone; else
    /// what the index has (the caller then asks sync for the text). The
    /// converted document is kept, so a second opening parses nothing again
    /// unless it needs the text.
    pub fn open(&self, msg: u64) -> Result<Reading, Error> {
        let m = self.store.message(msg).map_err(|_| Error::NotFound)?;
        let Some(full) = m.full else {
            return Ok(Reading { summary: m.summary, text: None, has_html: false, document: None, attachments: Vec::new() });
        };
        if let Some(sh) = m.document.and_then(|h| self.store.blob(&h).ok()).and_then(|b| postcard::from_bytes::<Shown>(&b).ok()) {
            let has_html = sh.document.is_some();
            let text = m.summary.text.clone().or(Some(String::new()));
            return Ok(Reading { summary: m.summary, text, has_html, document: sh.document, attachments: sh.attachments });
        }
        let raw = self.store.blob(&full)?;
        // Remote pictures already loaded: their first bytes, for their sizes.
        let loaded = m
            .pictures
            .iter()
            .take(wire::LOADED_MAX)
            .filter_map(|(id, h)| self.store.blob(h).ok().map(|b| (id.clone(), b[..b.len().min(wire::LOADED_HEAD)].to_vec())))
            .collect();
        match self.parser.open(&raw, loaded)? {
            Some(o) => {
                let sh = Shown { document: o.document, attachments: o.attachments };
                let h = self.store.put_blob(&postcard::to_stdvec(&sh).map_err(|e| Error::Jail(e.to_string()))?)?;
                self.store.write(|w| w.set_document(msg, Some(h)))?;
                let has_html = sh.document.is_some();
                let text = o.summary.text.clone().or_else(|| sh.document.as_ref().map(|_| String::new()));
                Ok(Reading { summary: o.summary, text, has_html, document: sh.document, attachments: sh.attachments })
            }
            None => Ok(Reading { summary: m.summary, text: Some("This message is too large to show; you can save it.".into()), has_html: false, document: None, attachments: Vec::new() }),
        }
    }
}

/// mail-sync's view of the engine for one account.
pub struct AccountSink<'a> {
    e: &'a Engine,
    account: AccountId,
    /// Messages the parse jail refused (kept out of the list).
    pub refused: usize,
}

fn role(r: mail_sync::Role) -> Role {
    use mail_sync::Role as S;
    match r {
        S::Inbox => Role::Inbox,
        S::Sent => Role::Sent,
        S::Drafts => Role::Drafts,
        S::Archive => Role::Archive,
        S::Junk => Role::Junk,
        S::Trash => Role::Trash,
        S::All => Role::All,
        S::Flagged => Role::Flagged,
        S::Other => Role::Other,
    }
}

impl AccountSink<'_> {
    fn fid(&self, name: &str) -> Result<FolderId, String> {
        let r = if name.eq_ignore_ascii_case("INBOX") { Role::Inbox } else { Role::Other };
        self.e.store.folder_id(self.account, name, r).map_err(|e| e.to_string())
    }
}

impl Sink for AccountSink<'_> {
    fn folders(&mut self, list: &[mail_sync::RemoteFolder]) -> Result<(), String> {
        let known: Vec<(FolderId, Folder)> = self.e.store.folders(self.account).map_err(|e| e.to_string())?;
        for f in list {
            let id = self.e.store.folder_id(self.account, &f.name, role(f.role)).map_err(|e| e.to_string())?;
            let mut cur = self.e.store.folder(id).map_err(|e| e.to_string())?;
            let (r, hidden) = (role(f.role), !f.selectable);
            if cur.role != r || cur.hidden != hidden {
                cur.role = r;
                cur.hidden = hidden;
                self.e.store.write(|w| w.set_folder(id, &cur)).map_err(|e| e.to_string())?;
            }
        }
        // Folders gone from the server are hidden, not deleted (their mail
        // may still be reachable from All Mail or another label).
        for (id, mut f) in known {
            if !list.iter().any(|r| r.name == f.name) && !f.hidden {
                f.hidden = true;
                self.e.store.write(|w| w.set_folder(id, &f)).map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }

    fn state(&mut self, folder: &str) -> FolderState {
        let Ok(id) = self.fid(folder) else { return FolderState::default() };
        let Ok(f) = self.e.store.folder(id) else { return FolderState::default() };
        FolderState { uidvalidity: f.uidvalidity, synced_from: f.synced_from, uidnext: f.uidnext, highestmodseq: f.highestmodseq, exists: f.exists }
    }

    fn reset(&mut self, folder: &str) -> Result<(), String> {
        let id = self.fid(folder)?;
        let mut f = self.e.store.folder(id).map_err(|e| e.to_string())?;
        f.uidvalidity = 0;
        f.synced_from = 0;
        f.uidnext = 0;
        f.highestmodseq = 0;
        self.e.store.write(|w| { w.clear_folder(id)?; w.set_folder(id, &f) }).map_err(|e| e.to_string())
    }

    fn known_uids(&mut self, folder: &str) -> Vec<u32> {
        self.fid(folder).ok().and_then(|id| self.e.store.uids(id).ok()).unwrap_or_default()
    }

    fn messages(&mut self, folder: &str, page: Vec<mail_sync::Fetched>) -> Result<(), String> {
        let id = self.fid(folder)?;
        let mut stored = Vec::with_capacity(page.len());
        // Blobs first (named by content), then summaries from jails in
        // batches, then one index transaction per batch.
        let mut i = 0;
        while i < page.len() {
            let mut end = i;
            let mut bytes = 0;
            while end < page.len() && end - i < wire::MAX_BATCH && bytes + page[end].bytes.len() <= wire::MAX_BATCH_BYTES {
                bytes += page[end].bytes.len();
                end += 1;
            }
            if end == i {
                end = i + 1; // one message over the batch size: alone
            }
            let chunk = &page[i..end];
            let full = chunk[0].full;
            let input: Vec<(u64, &[u8])> = chunk.iter().enumerate().map(|(k, f)| (k as u64, f.bytes.as_slice())).collect();
            let sums = self.e.parser.summarize(&input, !full).map_err(|e| e.to_string())?;
            for (f, s) in chunk.iter().zip(sums) {
                let Some(summary) = s else {
                    self.refused += 1;
                    continue;
                };
                let blob = self.e.store.put_blob(&f.bytes).map_err(|e| e.to_string())?;
                stored.push((f.uid, Incoming {
                    server_id: f.server_id.clone(),
                    size: f.size.max(f.bytes.len() as u64),
                    internal_date: f.internal_date,
                    flags: flag::from_imap(&f.flags),
                    blob,
                    full: f.full,
                    summary,
                }));
            }
            i = end;
        }
        let account = self.account;
        self.e.store.write(|w| {
            for (uid, m) in stored {
                w.put(id, account, uid, m)?;
            }
            Ok(())
        }).map_err(|e| e.to_string())
    }

    fn flags(&mut self, folder: &str, changes: &[(u32, Vec<String>)]) -> Result<(), String> {
        let id = self.fid(folder)?;
        self.e.store.write(|w| {
            for (uid, f) in changes {
                w.set_flags(id, *uid, flag::from_imap(f))?;
            }
            Ok(())
        }).map_err(|e| e.to_string())
    }

    fn vanished(&mut self, folder: &str, uids: &[u32]) -> Result<(), String> {
        let id = self.fid(folder)?;
        self.e.store.write(|w| {
            for u in uids {
                w.remove(id, *u)?;
            }
            Ok(())
        }).map_err(|e| e.to_string())
    }

    fn checkpoint(&mut self, folder: &str, st: &FolderState) -> Result<(), String> {
        let id = self.fid(folder)?;
        let mut f = self.e.store.folder(id).map_err(|e| e.to_string())?;
        f.uidvalidity = st.uidvalidity;
        f.synced_from = st.synced_from;
        f.uidnext = st.uidnext;
        f.highestmodseq = st.highestmodseq;
        f.exists = st.exists;
        self.e.store.write(|w| w.set_folder(id, &f)).map_err(|e| e.to_string())
    }

    fn text_wanted(&mut self, folder: &str, limit: usize) -> Vec<(u32, u64)> {
        let Ok(id) = self.fid(folder) else { return Vec::new() };
        let cutoff = now() - TEXT_WINDOW;
        let mut out = Vec::new();
        let mut offset = 0;
        'pages: loop {
            let Ok(rows) = self.e.store.list(id, offset, 500) else { break };
            if rows.is_empty() {
                break;
            }
            offset += rows.len();
            for r in rows {
                if r.date < cutoff {
                    break 'pages;
                }
                if !r.has_text {
                    let size = self.e.store.message(r.msg).map(|m| m.size).unwrap_or(u64::MAX);
                    out.push((r.uid, size));
                    if out.len() >= limit {
                        break 'pages;
                    }
                }
            }
        }
        out
    }

    fn changes(&mut self) -> Vec<mail_sync::Change> {
        let Ok(due) = self.e.store.ops_due(self.account, now()) else { return Vec::new() };
        let name = |f: FolderId| self.e.store.folder(f).ok().map(|f| f.name);
        due.into_iter()
            .filter_map(|(id, op)| {
                let what = match op.kind {
                    OpKind::Flags { add, remove } => mail_sync::What::Flags { add: imap_flags(add), remove: imap_flags(remove) },
                    OpKind::Move { to } => mail_sync::What::Move { to: name(to)? },
                    OpKind::Expunge => mail_sync::What::Expunge,
                };
                Some(mail_sync::Change { id, folder: name(op.folder)?, uid: op.uid, what })
            })
            .collect()
    }

    fn change_done(&mut self, id: u64, ok: bool) -> Result<(), String> {
        self.e.store.write(|w| w.op_done(id, ok)).map_err(|e| e.to_string())
    }
}

/// Flag bits as IMAP's system flags.
fn imap_flags(bits: u32) -> Vec<String> {
    [(flag::SEEN, "\\Seen"), (flag::ANSWERED, "\\Answered"), (flag::FLAGGED, "\\Flagged"), (flag::DRAFT, "\\Draft")]
        .iter()
        .filter(|(b, _)| bits & b != 0)
        .map(|(_, n)| n.to_string())
        .collect()
}

/// What the person does to messages from the list or the reader (§15.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// To the account's Archive (Gmail: All Mail).
    Archive,
    /// To Trash; from Trash (or with none), for good.
    Delete,
    Read(bool),
    Flag(bool),
}

/// How long a list action can be undone before sync makes it (§15.5).
pub const UNDO_SECS: i64 = 10;

impl Engine {
    /// Do `action` to the messages at `uids` in `folder`: in the index at
    /// once, on the server after the undo window (`due`, seconds; reads go
    /// at once). The op ids, for [`Engine::undo`].
    pub fn act(&self, folder: FolderId, uids: &[u32], action: Action) -> Result<Vec<u64>, Error> {
        let f = self.store.folder(folder)?;
        let role_folder = |roles: &[Role]| -> Result<Option<FolderId>, Error> {
            for r in roles {
                if let Some((id, _)) = self.store.folders(f.account)?.into_iter().find(|(_, x)| x.role == *r && !x.hidden) {
                    return Ok(Some(id));
                }
            }
            Ok(None)
        };
        let (kind, due) = match action {
            Action::Read(on) => (OpKind::Flags { add: if on { flag::SEEN } else { 0 }, remove: if on { 0 } else { flag::SEEN } }, now()),
            Action::Flag(on) => (OpKind::Flags { add: if on { flag::FLAGGED } else { 0 }, remove: if on { 0 } else { flag::FLAGGED } }, now() + UNDO_SECS),
            Action::Archive => match role_folder(&[Role::Archive, Role::All])? {
                Some(to) if to != folder => (OpKind::Move { to }, now() + UNDO_SECS),
                _ => return Err(Error::Refused("This account has no Archive folder")),
            },
            Action::Delete => match role_folder(&[Role::Trash])? {
                Some(to) if to != folder => (OpKind::Move { to }, now() + UNDO_SECS),
                _ => (OpKind::Expunge, now() + UNDO_SECS),
            },
        };
        let mut ids = Vec::new();
        self.store.write(|w| {
            for &u in uids {
                match w.queue(folder, u, kind.clone(), due) {
                    Ok(id) => ids.push(id),
                    // Already leaving, or gone: nothing to do for it.
                    Err(mail_store::Error::NotFound) => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        })?;
        Ok(ids)
    }

    /// Undo ops not yet made on the server; how many were.
    pub fn undo(&self, ops: &[u64]) -> Result<usize, Error> {
        Ok(self.store.write(|w| {
            let mut n = 0;
            for &id in ops {
                n += w.undo(id)? as usize;
            }
            Ok(n)
        })?)
    }
}
