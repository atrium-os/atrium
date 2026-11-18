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
pub mod search;
pub mod drafts;
pub mod pictures;
pub mod send;
pub mod senders;
pub mod later;
pub mod notify;
pub mod smime;
pub mod accounts;
pub mod prefs;
pub mod lanes;
pub mod private;
pub mod pgp;
pub mod dns;

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

pub(crate) fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// One running jail.
struct Jail {
    proc: Proc,
    r: BufReader<Box<dyn std::io::Read + Send>>,
    w: BufWriter<Box<dyn std::io::Write + Send>>,
}

/// How it runs: the app's `parse` worker (dev-tools-plan.md M5), or a child.
enum Proc {
    Worker(atrium_worker::Stdio),
    Child(Child),
}

impl Drop for Jail {
    fn drop(&mut self) {
        match &mut self.proc {
            Proc::Worker(w) => w.kill(),
            Proc::Child(c) => { let _ = c.kill(); let _ = c.wait(); }
        }
    }
}

/// The worker's name in Mail's `[[workers]]`.
pub const PARSE_WORKER: &str = "parse";

impl Parser {
    /// Installed, Mail declares the parser as its `parse` worker: a jail of
    /// its own, as a uid of its own, with no capability (app-composition.md
    /// §4). Elsewhere (development, tests) `program` runs as a child.
    fn start(&self) -> Result<Jail, Error> {
        let named = std::env::var_os("MAIL_PARSE_JAIL").is_some();
        if let Some(w) = atrium_worker::Workers::get().ok().filter(|w| !named && w.has(PARSE_WORKER)) {
            let mut p = w.spawn_stdio(PARSE_WORKER).map_err(|e| Error::Jail(format!("start the parse worker: {e}")))?;
            let r: Box<dyn std::io::Read + Send> = Box::new(p.stdout.take().expect("piped"));
            let w: Box<dyn std::io::Write + Send> = Box::new(p.stdin.take().expect("piped"));
            // Its stderr is dropped, as the child's is.
            drop(p.stderr.take());
            return Ok(Jail { proc: Proc::Worker(p), r: BufReader::new(r), w: BufWriter::new(w) });
        }
        let mut child = Command::new(&self.program)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| Error::Jail(format!("start {}: {e}", self.program.display())))?;
        let r: Box<dyn std::io::Read + Send> = Box::new(child.stdout.take().unwrap());
        let w: Box<dyn std::io::Write + Send> = Box::new(child.stdin.take().unwrap());
        Ok(Jail { proc: Proc::Child(child), r: BufReader::new(r), w: BufWriter::new(w) })
    }

    /// Frames until `Done`, answering lookups (none yet: DKIM keys come from
    /// mail-sync's resolver in a later step, so signed mail stays Pending).
    fn exchange(j: &mut Jail, lens: &dyn Fn(u64) -> Option<u64>, dns: &dyn Fn(&str) -> Option<String>) -> Result<Vec<FromJail>, Error> {
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
                FromJail::Query { query, name } => {
                    // mail-schema's check held the name to DKIM and DMARC records.
                    let txt = dns(&name).filter(|t| t.len() <= 4096);
                    wire::write_frame(&mut j.w, &ToJail::Answer { query, txt }).map_err(|e| Error::Jail(e.to_string()))?;
                }
                FromJail::Done => return Ok(out),
                other => out.push(other),
            }
        }
    }

    /// Summaries for one batch, in order; `None` for a message the jail
    /// refused (too large, not mail). An invalid jail fails the batch.
    pub fn summarize(&self, batch: &[(u64, &[u8])], headers_only: bool, roots: &[Vec<u8>]) -> Result<Vec<Option<Summary>>, Error> {
        self.summarize_keyed(batch, headers_only, roots).map(|(s, _)| s)
    }

    /// As [`Parser::summarize`], with the OpenPGP keys the messages carried
    /// in `Autocrypt:` for their From addresses: `(index in batch, addr, key)`.
    pub fn summarize_keyed(&self, batch: &[(u64, &[u8])], headers_only: bool, roots: &[Vec<u8>]) -> Result<(Vec<Option<Summary>>, Vec<(usize, String, Vec<u8>)>), Error> {
        self.summarize_with(batch, headers_only, roots, &|_| None)
    }

    /// As [`Parser::summarize_keyed`], answering DKIM/DMARC lookups with `dns`.
    pub fn summarize_with(&self, batch: &[(u64, &[u8])], headers_only: bool, roots: &[Vec<u8>], dns: &dyn Fn(&str) -> Option<String>) -> Result<(Vec<Option<Summary>>, Vec<(usize, String, Vec<u8>)>), Error> {
        let mut j = self.start()?;
        let msgs: Vec<wire::Raw> = batch.iter().map(|(id, b)| wire::Raw { id: *id, bytes: b.to_vec() }).collect();
        wire::write_frame(&mut j.w, &ToJail::Summarize { authserv_id: None, me: Vec::new(), now: now(), headers_only, roots: if headers_only { Vec::new() } else { roots.to_vec() }, batch: msgs })
            .map_err(|e| Error::Jail(e.to_string()))?;
        let lens = |id: u64| batch.iter().find(|(i, _)| *i == id).map(|(_, b)| b.len() as u64);
        let frames = Self::exchange(&mut j, &lens, dns)?;
        let mut out = vec![None; batch.len()];
        let mut keys = Vec::new();
        let mut seen = 0;
        for f in frames {
            match f {
                // A key comes just before its message's summary.
                FromJail::Autocrypt { id, addr, key } => {
                    if batch.get(seen).map(|(i, _)| *i) != Some(id) {
                        return Err(Error::Jail("a key for a message out of order".into()));
                    }
                    if keys.iter().any(|(k, _, _)| *k == seen) {
                        return Err(Error::Jail("two keys for one message".into()));
                    }
                    keys.push((seen, addr, key));
                }
                FromJail::Summary { id, result } => {
                    // Answers come in batch order, each once.
                    if batch.get(seen).map(|(i, _)| *i) != Some(id) {
                        return Err(Error::Jail("answers out of order".into()));
                    }
                    out[seen] = result.ok();
                    seen += 1;
                }
                _ => return Err(Error::Jail("unexpected frame".into())),
            }
        }
        if seen != batch.len() {
            return Err(Error::Jail("missing answers".into()));
        }
        // A key counts only for a message summarized, and for its From.
        keys.retain(|(i, addr, _)| out[*i].as_ref().and_then(|s| s.from.first()).is_some_and(|f| f.addr.eq_ignore_ascii_case(addr)));
        Ok((out, keys))
    }

    /// One message for reading, alone in its jail.
    /// One attachment's bytes, alone in a jail; `None` when the jail
    /// refused (the part isn't there).
    pub fn attachment(&self, raw: &[u8], which: &wire::Which, key: Option<Vec<u8>>) -> Result<Option<wire::Attached>, Error> {
        let mut j = self.start()?;
        wire::write_frame(&mut j.w, &ToJail::Attachment { message: wire::Raw { id: 1, bytes: raw.to_vec() }, which: which.clone(), key })
            .map_err(|e| Error::Jail(e.to_string()))?;
        let len = raw.len() as u64;
        let frames = Self::exchange(&mut j, &|id| (id == 1).then_some(len), &|_| None)?;
        match frames.into_iter().next() {
            Some(FromJail::Attachment { result, .. }) => Ok(result.ok()),
            _ => Err(Error::Jail("no answer".into())),
        }
    }

    /// Which of `certs` an encrypted message is for, and the content key
    /// wrapped for it (S/MIME, §10.1).
    pub fn key_for(&self, raw: &[u8], certs: &[Vec<u8>]) -> Result<Option<(usize, Vec<u8>)>, Error> {
        let certs: Vec<Vec<u8>> = certs.iter().take(wire::MY_CERTS_MAX).cloned().collect();
        let mut j = self.start()?;
        wire::write_frame(&mut j.w, &ToJail::KeyFor { message: wire::Raw { id: 1, bytes: raw.to_vec() }, certs: certs.clone() }).map_err(|e| Error::Jail(e.to_string()))?;
        let frames = Self::exchange(&mut j, &|_| Some(raw.len() as u64), &|_| None)?;
        match frames.into_iter().next() {
            Some(FromJail::KeyFor { which, .. }) => match which {
                Some((i, w)) if (i as usize) < certs.len() => Ok(Some((i as usize, w))),
                Some(_) => Err(Error::Jail("key for a certificate it was not given".into())),
                None => Ok(None),
            },
            _ => Err(Error::Jail("no answer".into())),
        }
    }

    pub fn open(&self, raw: &[u8], loaded: Vec<(String, Vec<u8>)>, roots: &[Vec<u8>], key: Option<Vec<u8>>, pgp_keys: Vec<Vec<u8>>) -> Result<Option<Opened>, Error> {
        self.open_with(raw, loaded, roots, key, pgp_keys, &|_| None)
    }

    /// As [`Parser::open`], answering DKIM/DMARC lookups with `dns`.
    pub fn open_with(&self, raw: &[u8], loaded: Vec<(String, Vec<u8>)>, roots: &[Vec<u8>], key: Option<Vec<u8>>, pgp_keys: Vec<Vec<u8>>, dns: &dyn Fn(&str) -> Option<String>) -> Result<Option<Opened>, Error> {
        let mut j = self.start()?;
        let pgp_keys = pgp_keys.into_iter().filter(|k| k.len() <= wire::PGP_KEY_MAX).take(wire::PGP_KEYS_MAX).collect();
        wire::write_frame(&mut j.w, &ToJail::Open { authserv_id: None, me: Vec::new(), now: now(), roots: roots.to_vec(), key, message: wire::Raw { id: 1, bytes: raw.to_vec() }, loaded, pgp_keys })
            .map_err(|e| Error::Jail(e.to_string()))?;
        let len = raw.len() as u64;
        let frames = Self::exchange(&mut j, &|id| (id == 1).then_some(len), dns)?;
        match frames.into_iter().next() {
            Some(FromJail::Opened { result, .. }) => Ok(result.ok()),
            _ => Err(Error::Jail("no answer".into())),
        }
    }
}

pub struct Engine {
    pub store: Store,
    pub parser: Parser,
    /// Trusted roots for S/MIME (DER), sent to each jail ([`smime`]).
    roots: Vec<Vec<u8>>,
    /// Content keys of encrypted messages unlocked this session, in memory
    /// only; their text is never kept (§10.1).
    keys: std::cell::RefCell<std::collections::HashMap<mail_store::MsgId, Vec<u8>>>,
    /// DKIM/DMARC lookups ([`dns`]), and what they answered.
    dns: Option<Box<dyn dns::Dns>>,
    dns_cache: std::cell::RefCell<dns::Cache>,
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
    /// The sender's card (§14.3), only when the sender is verified.
    pub card: Option<wire::Card>,
    /// An OpenPGP signature as the jail checked it (§10.2).
    pub pgp: Option<wire::Pgp>,
    /// PGP/MIME encrypted: reading it needs the person's key, in Arca.
    pub pgp_encrypted: bool,
}

impl Reading {
    fn bare(summary: Summary, text: Option<String>) -> Reading {
        Reading { summary, text, has_html: false, document: None, attachments: Vec::new(), card: None, pgp: None, pgp_encrypted: false }
    }
}

/// What a parse leaves for the next opening: the converted document and
/// the attachment list (a postcard blob, `Message::document`).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Shown {
    pub document: Option<wire::Document>,
    pub attachments: Vec<wire::Attachment>,
    pub card: Option<wire::Card>,
    pub pgp: Option<wire::Pgp>,
    pub pgp_encrypted: bool,
}

/// A card is shown only for a sender whose domain checked out (§14.3):
/// anyone can write JSON-LD saying their mail is your bank's.
fn verified_card(s: &Summary, card: Option<wire::Card>) -> Option<wire::Card> {
    card.filter(|_| matches!(s.sender_check, wire::SenderCheck::Verified { .. }))
}

impl Engine {
    pub fn new(store: Store, parser: Parser) -> Engine {
        Engine { store, parser, roots: smime::system_roots(), keys: Default::default(), dns: None, dns_cache: Default::default() }
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
            return Ok(Reading::bare(m.summary, None));
        };
        if m.summary.encrypted {
            // Read only with its key, each time, and nothing of it kept.
            let Some(key) = self.keys.borrow().get(&msg).cloned() else {
                return Ok(Reading::bare(m.summary, Some(String::new())));
            };
            let raw = self.store.blob(&full)?;
            return match self.parser.open_with(&raw, Vec::new(), &self.roots, Some(key), self.pgp_keys_of(&m.summary), &|n| self.answer(n))? {
                Some(o) => {
                    let text = o.summary.text.clone().or_else(|| o.document.as_ref().map(|_| String::new()));
                    let card = verified_card(&o.summary, o.card);
                    Ok(Reading { has_html: o.document.is_some(), summary: o.summary, text, document: o.document, attachments: o.attachments, card, pgp: o.pgp, pgp_encrypted: o.pgp_encrypted })
                }
                None => Ok(Reading::bare(m.summary, Some("This message couldn't be decrypted.".into()))),
            };
        }
        let pgp_keys = self.pgp_keys_of(&m.summary);
        // Kept from a parse that had no key for the signer, while there is
        // one now: parse again.
        // A sender check that waited for a key: check again now lookups can run.
        let recheck = m.summary.sender_check == wire::SenderCheck::Pending && self.dns.is_some();
        let stale = |sh: &Shown| recheck || matches!(&sh.pgp, Some(p) if p.state == wire::PgpState::NoKey) && !pgp_keys.is_empty();
        if let Some(sh) = m.document.and_then(|h| self.store.blob(&h).ok()).and_then(|b| postcard::from_bytes::<Shown>(&b).ok()).filter(|sh| !stale(sh)) {
            let has_html = sh.document.is_some();
            let text = m.summary.text.clone().or(Some(String::new()));
            let card = verified_card(&m.summary, sh.card);
            return Ok(Reading { summary: m.summary, text, has_html, document: sh.document, attachments: sh.attachments, card, pgp: sh.pgp, pgp_encrypted: sh.pgp_encrypted });
        }
        let raw = self.store.blob(&full)?;
        // Remote pictures already loaded: their first bytes, for their sizes.
        let loaded = m
            .pictures
            .iter()
            .take(wire::LOADED_MAX)
            .filter_map(|(id, h)| self.store.blob(h).ok().map(|b| (id.clone(), b[..b.len().min(wire::LOADED_HEAD)].to_vec())))
            .collect();
        match self.parser.open_with(&raw, loaded, &self.roots, None, pgp_keys, &|n| self.answer(n))? {
            Some(o) => {
                let sh = Shown { document: o.document, attachments: o.attachments, card: o.card, pgp: o.pgp, pgp_encrypted: o.pgp_encrypted };
                let h = self.store.put_blob(&postcard::to_stdvec(&sh).map_err(|e| Error::Jail(e.to_string()))?)?;
                let checked = (recheck && o.summary.sender_check != wire::SenderCheck::Pending).then(|| o.summary.sender_check.clone());
                self.store.write(|w| {
                    w.set_document(msg, Some(h))?;
                    match checked {
                        Some(c) => w.set_sender_check(msg, c),
                        None => Ok(()),
                    }
                })?;
                let has_html = sh.document.is_some();
                let text = o.summary.text.clone().or_else(|| sh.document.as_ref().map(|_| String::new()));
                let card = verified_card(&o.summary, sh.card);
                Ok(Reading { summary: o.summary, text, has_html, document: sh.document, attachments: sh.attachments, card, pgp: sh.pgp, pgp_encrypted: sh.pgp_encrypted })
            }
            None => Ok(Reading::bare(m.summary, Some("This message is too large to show; you can save it.".into()))),
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
        let mut keys = Vec::new();
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
            let (sums, found) = self.e.parser.summarize_with(&input, !full, &self.e.roots, &|n| self.e.answer(n)).map_err(|e| e.to_string())?;
            for (k, addr, key) in found {
                let when = sums[k].as_ref().and_then(|s| s.date).or(chunk[k].internal_date).unwrap_or_else(now).min(now());
                keys.push((addr, key, when, chunk[k].uid));
            }
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
        // New to the index (not a resync of what is here): the rules apply.
        let fresh: Vec<bool> = stored.iter().map(|(uid, _)| self.e.store.at(id, *uid).ok().flatten().is_none()).collect();
        let mut new = Vec::new();
        let mut all = Vec::new();
        self.e.store.write(|w| {
            for ((uid, m), fresh) in stored.into_iter().zip(fresh) {
                let msg = w.put(id, account, uid, m)?;
                all.push(msg);
                if fresh {
                    new.push((uid, msg));
                }
            }
            Ok(())
        }).map_err(|e| e.to_string())?;
        self.e.certs_seen(&all).map_err(|e| e.to_string())?;
        self.e.pgp_keys_seen(account, id, &keys).map_err(|e| e.to_string())?;
        self.e.arrived(account, id, &new).map_err(|e| e.to_string())
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

    fn server_state(&mut self, key: &str) -> Option<String> {
        self.e.store.sync_state(self.account, key).ok().flatten()
    }

    fn set_server_state(&mut self, key: &str, value: &str) -> Result<(), String> {
        let account = self.account;
        self.e.store.write(|w| w.set_sync_state(account, key, Some(value))).map_err(|e| e.to_string())
    }

    fn listed(&mut self, id: &str) -> Vec<(String, u32)> {
        let Ok(Some(msg)) = self.e.store.by_server_id(self.account, id) else { return Vec::new() };
        let Ok(m) = self.e.store.message(msg) else { return Vec::new() };
        // Held and leaving places are still the server's view of it.
        m.locations.iter().chain(&m.held).chain(&m.leaving).filter_map(|(f, uid)| Some((self.e.store.folder(*f).ok()?.name, *uid))).collect()
    }

    fn server_id_at(&mut self, folder: &str, uid: u32) -> Option<String> {
        let id = self.fid(folder).ok()?;
        let (msg, _) = self.e.store.at(id, uid).ok()??;
        self.e.store.message(msg).ok()?.server_id
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
