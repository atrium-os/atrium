//! Mail's store (docs/spec/mail.md §6.2).
//!
//! - **Raw messages are the source of truth**: files named by the SHA-256 of
//!   their bytes, byte-exact as the server gave them. A message in two
//!   folders, or fetched twice, is one file. During the staged first sync
//!   (§6.4) a message may at first have only its header block; its full
//!   bytes arrive later, as a second blob.
//! - **The index** (redb, one file): accounts, folders with their sync
//!   state, where each message is (folder, UID) and with which flags, and per
//!   message the parse jail's bounded summary. The list a folder shows is a
//!   key range, newest first, so opening a folder of 200,000 reads only the
//!   rows on screen. The index can always be rebuilt from the blobs.
//!
//! The store never parses a message: summaries come from a parse jail and are
//! checked before they get here (mail-schema).

use std::io::Write as _;
use std::ops::Bound::{Excluded, Unbounded};
use std::path::{Path, PathBuf};

use mail_schema::{SenderCheck, Summary};
use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

mod ops;
pub use ops::{Op, OpKind, LEAVING};
mod drafts;
pub use drafts::LocalDraft;
mod outbox;
pub use outbox::{retry_after, Outgoing, SendState};
pub mod search;
pub mod senders;
pub mod certs;
pub mod dns;
pub mod pgp;
pub mod config;
pub use config::{AccountConfig, Protocol, Reached, Server};
pub mod later;
pub mod people;
pub mod imports;
pub mod away;
pub mod sieve;
mod thread;
pub use thread::{base_subject, Conversation, InThread, ThreadId, SUBJECT_WINDOW};

/// The index's format and the invariants expected by this Mail build.
const SCHEMA: u64 = 8;

pub type AccountId = u32;
pub type FolderId = u32;
pub type MsgId = u64;
pub type Hash = [u8; 32];

// Tables. Values are postcard.
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
pub(crate) const ACCOUNTS: TableDefinition<u32, &[u8]> = TableDefinition::new("accounts");
pub(crate) const FOLDERS: TableDefinition<u32, &[u8]> = TableDefinition::new("folders");
/// (account, folder name) → folder id.
pub(crate) const FOLDER_NAMES: TableDefinition<(u32, &str), u32> = TableDefinition::new("folder_names");
pub(crate) const MESSAGES: TableDefinition<u64, &[u8]> = TableDefinition::new("messages");
/// (folder, uid) → (msg, flags).
pub(crate) const LOCATIONS: TableDefinition<(u32, u32), (u64, u32)> = TableDefinition::new("locations");
/// (folder, newest-first date key, msg) → uid: the list.
pub(crate) const LIST: TableDefinition<(u32, u64, u64), u32> = TableDefinition::new("list");
/// (account, server's stable id) → msg: each message once (§6.4).
const SERVER_IDS: TableDefinition<(u32, &str), u64> = TableDefinition::new("server_ids");
/// (account, lowercase sender address) → 1: load this sender's remote
/// pictures without asking (§7.4; only offered for verified senders).
const SENDER_PICTURES: TableDefinition<(u32, &str), u8> = TableDefinition::new("sender_pictures");
/// account → its identities (postcard `Vec<Identity>`), the default first.
pub(crate) const IDENTITIES: TableDefinition<u32, &[u8]> = TableDefinition::new("identities");

/// Flags as bits; of keywords only `$Snoozed`, `$Junk` and `$NotJunk` are kept.
pub mod flag {
    pub const SEEN: u32 = 1;
    pub const ANSWERED: u32 = 2;
    pub const FLAGGED: u32 = 4;
    pub const DELETED: u32 = 8;
    pub const DRAFT: u32 = 16;
    /// The `$Snoozed` keyword: set aside with *Later* (mail.md §11.4), so
    /// other clients that know it show the conversation as snoozed.
    pub const SNOOZED: u32 = 32;
    /// `$Junk` and `$NotJunk` (mail.md §7.7): the person said a message is
    /// spam, or isn't; servers' filters learn from them.
    pub const JUNK: u32 = 64;
    pub const NOT_JUNK: u32 = 128;
    /// `$Phishing` (mail.md §7.7): reported as phishing, with `$Junk`.
    pub const PHISHING: u32 = 256;

    /// IMAP system flags → bits (others ignored).
    pub fn from_imap<S: AsRef<str>>(flags: &[S]) -> u32 {
        flags.iter().fold(0, |b, f| {
            b | match f.as_ref().to_ascii_lowercase().as_str() {
                "\\seen" => SEEN,
                "\\answered" => ANSWERED,
                "\\flagged" => FLAGGED,
                "\\deleted" => DELETED,
                "\\draft" => DRAFT,
                "$snoozed" => SNOOZED,
                "$junk" => JUNK,
                "$notjunk" => NOT_JUNK,
                "$phishing" => PHISHING,
                _ => 0,
            }
        })
    }
}

#[derive(Debug)]
pub enum Error {
    Db(String),
    Io(std::io::Error),
    Corrupt(&'static str),
    NotFound,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Db(e) => write!(f, "index: {e}"),
            Error::Io(e) => write!(f, "store: {e}"),
            Error::Corrupt(w) => write!(f, "index record: {w}"),
            Error::NotFound => f.write_str("not found"),
        }
    }
}

impl std::error::Error for Error {}

macro_rules! db_err {
    ($($t:ty),*) => { $(impl From<$t> for Error { fn from(e: $t) -> Self { Error::Db(e.to_string()) } })* };
}
db_err!(redb::DatabaseError, redb::TransactionError, redb::TableError, redb::StorageError, redb::CommitError);

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self { Error::Io(e) }
}

pub type Result<T> = std::result::Result<T, Error>;

fn enc<T: Serialize>(v: &T) -> Vec<u8> {
    postcard::to_stdvec(v).expect("encodes")
}

fn dec<'a, T: Deserialize<'a>>(b: &'a [u8]) -> Result<T> {
    postcard::from_bytes(b).map_err(|_| Error::Corrupt("postcard"))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub name: String,
}

/// A From address of an account (mail.md §9.2): the account's own, or an
/// alias the server lets it send as.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Identity {
    pub name: String,
    pub addr: String,
    /// Added under new messages and replies ("-- " above it).
    pub signature: String,
}

/// A folder's role (SPECIAL-USE, RFC 6154), for order and icons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role { Inbox, Sent, Drafts, Archive, Junk, Trash, All, Flagged, Other }

/// A folder and where its sync stands. Resumable: every field is a
/// checkpoint (§6.4 "Resumable").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folder {
    pub account: AccountId,
    pub name: String,
    pub role: Role,
    /// Not selectable (`\Noselect`), or gone from the server.
    pub hidden: bool,
    pub uidvalidity: u32,
    /// Every UID ≥ this one the server had at the last sync is listed here.
    pub synced_from: u32,
    /// The next UID the server will assign, as of the last sync.
    pub uidnext: u32,
    /// CONDSTORE/QRESYNC state, 0 when the server has none.
    pub highestmodseq: u64,
    pub exists: u32,
}

/// What a sync hands the store for one message in one folder.
#[derive(Debug, Clone)]
pub struct Incoming {
    pub server_id: Option<String>,
    pub size: u64,
    pub internal_date: Option<i64>,
    pub flags: u32,
    /// The header block, or the whole message when `full`.
    pub blob: Hash,
    pub full: bool,
    pub summary: Summary,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub account: AccountId,
    pub server_id: Option<String>,
    pub size: u64,
    pub date: i64,
    /// The header block (always) and the whole message (once fetched).
    pub headers: Hash,
    pub full: Option<Hash>,
    pub summary: Summary,
    pub locations: Vec<(FolderId, u32)>,
    /// Its conversation (§11.1).
    pub thread: ThreadId,
    /// The HTML body as converted by a parse jail (a postcard
    /// `mail_schema::Document` blob), kept so opening it again does no work
    /// and fires nothing (§7.3).
    pub document: Option<Hash>,
    /// Remote pictures fetched on the person's request (§7.4), by the
    /// document's resource id, kept so a second view fetches nothing.
    pub pictures: Vec<(String, Hash)>,
    /// Locations the person moved or deleted it from, until the server
    /// has done it too (they are no longer in `locations`).
    pub leaving: Vec<(FolderId, u32)>,
    /// The attachments' names and text, as a parse jail took it out (§8,
    /// §11.2), for search: one blob of UTF-8 text.
    pub files_text: Option<Hash>,
    /// Locations held back from their folder's list by the screener
    /// (§11.3) until the sender is accepted; local only.
    pub held: Vec<(FolderId, u32)>,
}

/// One row of a folder's list.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub msg: MsgId,
    pub uid: u32,
    pub flags: u32,
    pub date: i64,
    pub from: String,
    pub subject: String,
    pub preview: String,
    /// The message text is here (stage 3), not only its headers.
    pub has_text: bool,
}

/// Cheap to clone: the window and sync share one open index (redb
/// allows one opener per file) from their own threads.
#[derive(Clone)]
pub struct Store {
    db: std::sync::Arc<Database>,
    blobs: PathBuf,
}

/// Newest first: the key sorts ascending, so it is the date reversed.
fn date_key(date: i64) -> u64 {
    (i64::MAX - date.max(0)) as u64
}

impl Store {
    /// Open (or create) the store in `dir`: `index.redb` and `blobs/`.
    pub fn open(dir: &Path) -> Result<Store> {
        std::fs::create_dir_all(dir.join("blobs"))?;
        let db = Database::create(dir.join("index.redb"))?;
        let tx = db.begin_write()?;
        {
            tx.open_table(META)?;
            tx.open_table(ACCOUNTS)?;
            tx.open_table(FOLDERS)?;
            tx.open_table(FOLDER_NAMES)?;
            tx.open_table(MESSAGES)?;
            tx.open_table(LOCATIONS)?;
            tx.open_table(LIST)?;
            tx.open_table(SERVER_IDS)?;
            tx.open_table(SENDER_PICTURES)?;
            tx.open_table(IDENTITIES)?;
            tx.open_table(ops::OPS)?;
            tx.open_table(outbox::OUTBOX)?;
            tx.open_table(drafts::DRAFTS)?;
            tx.open_table(thread::THREADS)?;
            tx.open_table(thread::MSGID_THREADS)?;
            tx.open_table(thread::SUBJECT_THREADS)?;
            tx.open_table(thread::CONVERSATIONS)?;
            tx.open_table(thread::VIEWS)?;
            tx.open_table(search::TERMS)?;
            tx.open_table(search::MSG_TERMS)?;
            tx.open_table(search::SAVED)?;
            tx.open_table(senders::LISTS)?;
            tx.open_table(senders::CONTACTS)?;
            tx.open_table(senders::SETTINGS)?;
            tx.open_table(senders::SYNC_STATE)?;
            tx.open_table(config::CONFIG)?;
            tx.open_table(senders::HELD)?;
            tx.open_table(senders::LANES)?;
            tx.open_table(certs::CERTS)?;
            tx.open_table(dns::DNS_TXT)?;
            tx.open_table(pgp::PGP_KEYS)?;
            tx.open_table(later::LATER)?;
            tx.open_table(later::LATER_BY_THREAD)?;
            tx.open_table(later::FOLLOW_UPS)?;
            tx.open_table(later::LATER_DROPPED)?;
            tx.open_table(later::PARKED)?;
            tx.open_table(people::PEOPLE)?;
            tx.open_table(people::PEOPLE_COUNTED)?;
            tx.open_table(imports::IMPORTS)?;
            tx.open_table(away::AWAY)?;
            tx.open_table(sieve::SIEVE)?;
            let mut meta = tx.open_table(META)?;
            let schema = meta.get("schema")?.map(|v| v.value());
            let empty = meta.get("message")?.is_none() && meta.get("account")?.is_none();
            match schema {
                Some(SCHEMA) => {}
                Some(7) => {
                    reset_sender_verdicts(&tx)?;
                    meta.insert("schema", SCHEMA)?;
                }
                None if empty => {
                    meta.insert("schema", SCHEMA)?;
                }
                _ => return Err(Error::Corrupt("index of an older layout")),
            }
        }
        tx.commit()?;
        Ok(Store { db: std::sync::Arc::new(db), blobs: dir.join("blobs") })
    }

    // ── blobs ───────────────────────────────────────────────────────

    fn blob_path(&self, h: &Hash) -> PathBuf {
        let hex: String = h.iter().map(|b| format!("{b:02x}")).collect();
        self.blobs.join(&hex[..2]).join(&hex[2..])
    }

    /// Keep these bytes; their name is their hash. Written to a temporary
    /// file and renamed, so a crash never leaves a half blob under a name.
    pub fn put_blob(&self, bytes: &[u8]) -> Result<Hash> {
        let h: Hash = Sha256::digest(bytes).into();
        let p = self.blob_path(&h);
        if p.exists() {
            return Ok(h);
        }
        std::fs::create_dir_all(p.parent().unwrap())?;
        let tmp = p.with_extension("tmp");
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_data()?;
        std::fs::rename(&tmp, &p)?;
        Ok(h)
    }

    /// A blob's bytes, checked against its name.
    pub fn blob(&self, h: &Hash) -> Result<Vec<u8>> {
        let b = std::fs::read(self.blob_path(h)).map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { Error::NotFound } else { Error::Io(e) })?;
        let got: Hash = Sha256::digest(&b).into();
        if &got != h {
            return Err(Error::Corrupt("blob does not match its name"));
        }
        Ok(b)
    }

    // ── accounts and folders ────────────────────────────────────────

    fn next_id(tx: &redb::WriteTransaction, key: &str) -> Result<u64> {
        let mut t = tx.open_table(META)?;
        let n = t.get(key)?.map(|v| v.value()).unwrap_or(0) + 1;
        t.insert(key, n)?;
        Ok(n)
    }

    pub fn add_account(&self, name: &str) -> Result<AccountId> {
        let tx = self.db.begin_write()?;
        let id = Self::next_id(&tx, "account")? as u32;
        tx.open_table(ACCOUNTS)?.insert(id, enc(&Account { name: name.into() }).as_slice())?;
        tx.commit()?;
        Ok(id)
    }

    /// `account`'s From addresses, the default first.
    pub fn identities(&self, account: AccountId) -> Result<Vec<Identity>> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(IDENTITIES)?;
        let v = t.get(account)?;
        v.map(|v| dec(v.value())).transpose().map(Option::unwrap_or_default)
    }

    pub fn set_identities(&self, account: AccountId, ids: &[Identity]) -> Result<()> {
        let tx = self.db.begin_write()?;
        tx.open_table(IDENTITIES)?.insert(account, enc(&ids.to_vec()).as_slice())?;
        tx.commit()?;
        Ok(())
    }

    pub fn accounts(&self) -> Result<Vec<(AccountId, Account)>> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(ACCOUNTS)?;
        let mut out = Vec::new();
        for e in t.iter()? {
            let (k, v) = e?;
            out.push((k.value(), dec(v.value())?));
        }
        Ok(out)
    }

    /// The folder `name` of `account`, made if new.
    pub fn folder_id(&self, account: AccountId, name: &str, role: Role) -> Result<FolderId> {
        let tx = self.db.begin_write()?;
        let existing = tx.open_table(FOLDER_NAMES)?.get((account, name))?.map(|v| v.value());
        let id = match existing {
            Some(id) => id,
            None => {
                let id = Self::next_id(&tx, "folder")? as u32;
                tx.open_table(FOLDER_NAMES)?.insert((account, name), id)?;
                let f = Folder { account, name: name.into(), role, hidden: false, uidvalidity: 0, synced_from: 0, uidnext: 0, highestmodseq: 0, exists: 0 };
                tx.open_table(FOLDERS)?.insert(id, enc(&f).as_slice())?;
                id
            }
        };
        tx.commit()?;
        Ok(id)
    }

    pub fn folder(&self, id: FolderId) -> Result<Folder> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(FOLDERS)?;
        let v = t.get(id)?.ok_or(Error::NotFound)?;
        dec(v.value())
    }

    pub fn folders(&self, account: AccountId) -> Result<Vec<(FolderId, Folder)>> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(FOLDERS)?;
        let mut out = Vec::new();
        for e in t.iter()? {
            let (k, v) = e?;
            let f: Folder = dec(v.value())?;
            if f.account == account {
                out.push((k.value(), f));
            }
        }
        // Inbox first, then by role, then by name.
        out.sort_by(|a, b| (a.1.role as u8, &a.1.name).cmp(&(b.1.role as u8, &b.1.name)));
        Ok(out)
    }

    /// Run several changes in one transaction (sync writes in small
    /// transactions, §6.4: one per page of messages).
    pub fn write<T>(&self, f: impl FnOnce(&mut Writer<'_>) -> Result<T>) -> Result<T> {
        let tx = self.db.begin_write()?;
        let out = {
            let mut w = Writer { tx: &tx };
            f(&mut w)?
        };
        tx.commit()?;
        Ok(out)
    }

    // ── reading ─────────────────────────────────────────────────────

    pub fn message(&self, id: MsgId) -> Result<Message> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(MESSAGES)?;
        let v = t.get(id)?.ok_or(Error::NotFound)?;
        dec(v.value())
    }

    /// Rows `offset..offset+limit` of a folder's list, newest first.
    pub fn list(&self, folder: FolderId, offset: usize, limit: usize) -> Result<Vec<Row>> {
        let tx = self.db.begin_read()?;
        let list = tx.open_table(LIST)?;
        let locs = tx.open_table(LOCATIONS)?;
        let msgs = tx.open_table(MESSAGES)?;
        let mut out = Vec::with_capacity(limit);
        for e in list.range((folder, 0u64, 0u64)..=(folder, u64::MAX, u64::MAX))?.skip(offset).take(limit) {
            let (k, uid) = e?;
            let (_, _, msg) = k.value();
            let uid = uid.value();
            let flags = locs.get((folder, uid))?.map(|v| v.value().1).unwrap_or(0);
            let Some(m) = msgs.get(msg)? else { continue };
            let m: Message = dec(m.value())?;
            let from = m.summary.from.first().map(|a| if a.name.is_empty() { a.addr.clone() } else { a.name.clone() }).unwrap_or_default();
            out.push(Row { msg, uid, flags, date: m.date, from, subject: m.summary.subject.clone(), preview: m.summary.preview.clone(), has_text: m.full.is_some() });
        }
        Ok(out)
    }

    pub fn count(&self, folder: FolderId) -> Result<usize> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(LIST)?;
        Ok(t.range((folder, 0u64, 0u64)..=(folder, u64::MAX, u64::MAX))?.count())
    }

    /// The UIDs listed in a folder, ascending.
    pub fn uids(&self, folder: FolderId) -> Result<Vec<u32>> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(LOCATIONS)?;
        let mut out = Vec::new();
        for e in t.range((folder, 0u32)..=(folder, u32::MAX))? {
            out.push(e?.0.value().1);
        }
        Ok(out)
    }

    /// The message at (folder, uid), with its flags.
    pub fn at(&self, folder: FolderId, uid: u32) -> Result<Option<(MsgId, u32)>> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(LOCATIONS)?;
        Ok(t.get((folder, uid))?.map(|v| v.value()))
    }

    pub fn by_server_id(&self, account: AccountId, server_id: &str) -> Result<Option<MsgId>> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(SERVER_IDS)?;
        Ok(t.get((account, server_id))?.map(|v| v.value()))
    }

    /// Whether the person chose to load `addr`'s pictures without asking.
    pub fn always_pictures(&self, account: AccountId, addr: &str) -> Result<bool> {
        let tx = self.db.begin_read()?;
        let a = addr.to_ascii_lowercase();
        let v = tx.open_table(SENDER_PICTURES)?.get((account, a.as_str()))?.is_some();
        Ok(v)
    }

    pub fn message_count(&self) -> Result<u64> {
        let tx = self.db.begin_read()?;
        Ok(tx.open_table(MESSAGES)?.len()?)
    }
}

/// Old verified verdicts predate the current From and DMARC policy checks.
fn reset_sender_verdicts(tx: &redb::WriteTransaction) -> Result<()> {
    const PAGE: usize = 256;
    let mut messages = tx.open_table(MESSAGES)?;
    let mut next = Unbounded;
    loop {
        let mut verified = Vec::new();
        let mut last = None;
        for entry in messages.range((next, Unbounded))?.take(PAGE) {
            let (key, bytes) = entry?;
            let id = key.value();
            last = Some(id);
            let message: Message = dec(bytes.value())?;
            if matches!(message.summary.sender_check, SenderCheck::Verified { .. }) {
                verified.push(id);
            }
        }
        let Some(last) = last else { return Ok(()) };
        next = Excluded(last);
        for id in verified {
            let mut message: Message = dec(messages.get(id)?.ok_or(Error::NotFound)?.value())?;
            message.summary.sender_check = SenderCheck::Pending;
            messages.insert(id, enc(&message).as_slice())?;
        }
    }
}

/// Changes inside one transaction ([`Store::write`]).
pub struct Writer<'a> {
    tx: &'a redb::WriteTransaction,
}

impl Writer<'_> {
    pub fn set_folder(&mut self, id: FolderId, f: &Folder) -> Result<()> {
        self.tx.open_table(FOLDERS)?.insert(id, enc(f).as_slice())?;
        Ok(())
    }

    fn get_msg(&self, id: MsgId) -> Result<Option<Message>> {
        let t = self.tx.open_table(MESSAGES)?;
        let v = t.get(id)?;
        v.map(|v| dec(v.value())).transpose()
    }

    fn put_msg(&mut self, id: MsgId, m: &Message) -> Result<()> {
        self.tx.open_table(MESSAGES)?.insert(id, enc(m).as_slice())?;
        Ok(())
    }

    /// A message seen at (folder, uid). Returns its id: the existing one
    /// when the folder already lists that UID or the account already has the
    /// server id (the same message in another folder or label), else new.
    /// A fuller blob or summary replaces a header-only one, never the reverse.
    pub fn put(&mut self, folder: FolderId, account: AccountId, uid: u32, m: Incoming) -> Result<MsgId> {
        self.put_linked(folder, account, uid, m, None)
    }

    /// As `put`, with the identity of a tracked server-side transfer.
    pub fn put_linked(&mut self, folder: FolderId, account: AccountId, uid: u32, m: Incoming, link: Option<MsgId>) -> Result<MsgId> {
        let at = self.tx.open_table(LOCATIONS)?.get((folder, uid))?.map(|v| v.value().0);
        let by_sid = match &m.server_id {
            Some(s) => self.tx.open_table(SERVER_IDS)?.get((account, s.as_str()))?.map(|v| v.value()),
            None => None,
        };
        let date = m.summary.date.or(m.internal_date).unwrap_or(0);
        let id = match at.or(by_sid).or(link) {
            Some(id) => {
                let mut old = self.get_msg(id)?.ok_or(Error::Corrupt("location without message"))?;
                if m.full && old.full.is_none() {
                    old.full = Some(m.blob);
                    old.summary = m.summary;
                    self.index(id, &old, None)?;
                }
                if !old.locations.contains(&(folder, uid)) && !old.leaving.contains(&(folder, uid)) && !old.held.contains(&(folder, uid)) {
                    old.locations.push((folder, uid));
                }
                // The list key carries the date: re-key if it changed.
                if old.date != date && m.full {
                    for (f, u) in old.locations.clone() {
                        self.tx.open_table(LIST)?.remove((f, date_key(old.date), id))?;
                        self.tx.open_table(LIST)?.insert((f, date_key(date), id), u)?;
                    }
                    old.date = date;
                }
                self.put_msg(id, &old)?;
                id
            }
            None => {
                let id = Store::next_id(self.tx, "message")?;
                let msg = Message {
                    account,
                    server_id: m.server_id.clone(),
                    size: m.size,
                    date,
                    headers: m.blob,
                    full: m.full.then_some(m.blob),
                    summary: m.summary,
                    locations: vec![(folder, uid)],
                    thread: 0,
                    document: None,
                    pictures: Vec::new(),
                    files_text: None,
                    leaving: Vec::new(),
                    held: Vec::new(),
                };
                self.put_msg(id, &msg)?;
                self.index(id, &msg, None)?;
                if let Some(s) = &m.server_id {
                    self.tx.open_table(SERVER_IDS)?.insert((account, s.as_str()), id)?;
                }
                id
            }
        };
        let cur = self.get_msg(id)?.ok_or(Error::NotFound)?;
        // Leaving this folder (a move waits for the server): stays out of its list.
        if cur.leaving.contains(&(folder, uid)) {
            self.tx.open_table(LOCATIONS)?.insert((folder, uid), (id, m.flags | LEAVING))?;
            return Ok(id);
        }
        // Held by the screener: known, but not in the folder's list.
        if cur.held.contains(&(folder, uid)) {
            self.tx.open_table(LOCATIONS)?.insert((folder, uid), (id, m.flags))?;
            return Ok(id);
        }
        self.tx.open_table(LOCATIONS)?.insert((folder, uid), (id, m.flags))?;
        self.tx.open_table(LIST)?.insert((folder, date_key(cur.date), id), uid)?;
        if cur.thread == 0 {
            self.thread_new(id)?;
        } else {
            self.thread_refresh(cur.thread)?;
        }
        Ok(id)
    }

    /// A sender check done again (a key that couldn't be fetched before).
    pub fn set_sender_check(&mut self, id: MsgId, check: mail_schema::SenderCheck) -> Result<()> {
        let mut m = self.get_msg(id)?.ok_or(Error::NotFound)?;
        m.summary.sender_check = check;
        self.put_msg(id, &m)
    }

    /// Keep (or forget) a message's converted HTML document.
    pub fn set_document(&mut self, id: MsgId, document: Option<Hash>) -> Result<()> {
        let mut m = self.get_msg(id)?.ok_or(Error::NotFound)?;
        m.document = document;
        self.put_msg(id, &m)
    }

    /// Keep a message's attachment text for search, and index it
    /// (`text`: the blob's contents).
    pub fn set_files_text_indexed(&mut self, id: MsgId, hash: Option<Hash>, text: Option<&str>) -> Result<()> {
        self.set_files_text(id, hash)?;
        let m = self.get_msg(id)?.ok_or(Error::NotFound)?;
        self.index(id, &m, text)
    }

    /// Keep a message's attachment text for search.
    pub fn set_files_text(&mut self, id: MsgId, text: Option<Hash>) -> Result<()> {
        let mut m = self.get_msg(id)?.ok_or(Error::NotFound)?;
        m.files_text = text;
        self.put_msg(id, &m)
    }

    /// Keep remote pictures with a message (a resource id already kept is
    /// not added again).
    pub fn add_pictures(&mut self, id: MsgId, pictures: Vec<(String, Hash)>) -> Result<()> {
        if pictures.is_empty() {
            return Ok(());
        }
        let mut m = self.get_msg(id)?.ok_or(Error::NotFound)?;
        for (k, h) in pictures {
            if !m.pictures.iter().any(|(x, _)| *x == k) {
                m.pictures.push((k, h));
            }
        }
        self.put_msg(id, &m)
    }

    /// Load (or stop loading) remote pictures from `addr` without asking.
    pub fn set_always_pictures(&mut self, account: AccountId, addr: &str, on: bool) -> Result<()> {
        let a = addr.to_ascii_lowercase();
        let mut t = self.tx.open_table(SENDER_PICTURES)?;
        if on {
            t.insert((account, a.as_str()), 1)?;
        } else {
            t.remove((account, a.as_str()))?;
        }
        Ok(())
    }

    pub fn set_flags(&mut self, folder: FolderId, uid: u32, flags: u32) -> Result<bool> {
        let mut t = self.tx.open_table(LOCATIONS)?;
        let Some(cur) = t.get((folder, uid))?.map(|v| v.value()) else { return Ok(false) };
        // A message leaving the folder stays so until its op is done.
        t.insert((folder, uid), (cur.0, (flags & !LEAVING) | (cur.1 & LEAVING)))?;
        Ok(true)
    }

    /// The server no longer has (folder, uid). A message left in no folder
    /// leaves the index; its blobs stay until a sweep.
    pub fn remove(&mut self, folder: FolderId, uid: u32) -> Result<()> {
        let Some((id, _)) = self.tx.open_table(LOCATIONS)?.remove((folder, uid))?.map(|v| v.value()) else { return Ok(()) };
        let Some(mut m) = self.get_msg(id)? else { return Ok(()) };
        self.tx.open_table(LIST)?.remove((folder, date_key(m.date), id))?;
        m.locations.retain(|l| *l != (folder, uid));
        m.leaving.retain(|l| *l != (folder, uid));
        if m.held.contains(&(folder, uid)) {
            m.held.retain(|l| *l != (folder, uid));
            self.tx.open_table(senders::HELD)?.remove((m.account, date_key(m.date), id))?;
        }
        let parked = self.tx.open_table(later::PARKED)?.get((m.account, id))?.is_some();
        if !parked && m.locations.is_empty() && m.leaving.is_empty() && m.held.is_empty() {
            self.remove_unplaced(id, &m)?;
        } else {
            self.put_msg(id, &m)?;
        }
        self.thread_refresh(m.thread)
    }

    fn remove_unplaced(&mut self, id: MsgId, m: &Message) -> Result<()> {
        self.tx.open_table(MESSAGES)?.remove(id)?;
        self.unindex(id)?;
        if let Some(s) = &m.server_id {
            self.tx.open_table(SERVER_IDS)?.remove((m.account, s.as_str()))?;
        }
        Ok(())
    }

    /// UIDVALIDITY changed: everything the folder listed is void.
    pub fn clear_folder(&mut self, folder: FolderId) -> Result<()> {
        let uids: Vec<u32> = {
            let t = self.tx.open_table(LOCATIONS)?;
            let mut v = Vec::new();
            for e in t.range((folder, 0u32)..=(folder, u32::MAX))? {
                v.push(e?.0.value().1);
            }
            v
        };
        for u in uids {
            self.remove(folder, u)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod migration_tests {
    use super::*;

    #[test]
    fn version_seven_sender_verdicts_are_reset_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let account = store.add_account("reader@example.org").unwrap();
        let inbox = store.folder_id(account, "INBOX", Role::Inbox).unwrap();
        let raw = b"Subject: Example\r\n\r\nBody\r\n";
        let blob = store.put_blob(raw).unwrap();
        let document = store.put_blob(b"cached document").unwrap();
        let verified = SenderCheck::Verified { domain: "example.org".into(), by_provider: false };
        let end = 257;
        let ids = store.write(|w| {
            let mut ids = Vec::new();
            for uid in 1..=end {
                let sender_check = if uid == 1 || uid == end {
                    verified.clone()
                } else {
                    SenderCheck::NotVerified
                };
                let id = w.put(inbox, account, uid, Incoming {
                    server_id: None,
                    size: raw.len() as u64,
                    internal_date: None,
                    flags: flag::FLAGGED,
                    blob,
                    full: true,
                    summary: Summary { sender_check, ..Default::default() },
                })?;
                ids.push(id);
            }
            w.set_document(ids[0], Some(document))?;
            w.tx.open_table(META)?.insert("schema", 7)?;
            Ok(ids)
        }).unwrap();
        let before: Vec<_> = ids.iter().map(|&id| store.message(id).unwrap()).collect();
        drop(store);

        let store = Store::open(dir.path()).unwrap();
        for (i, (&id, mut expected)) in ids.iter().zip(before).enumerate() {
            if matches!(expected.summary.sender_check, SenderCheck::Verified { .. }) {
                expected.summary.sender_check = SenderCheck::Pending;
            }
            assert_eq!(store.message(id).unwrap(), expected);
            assert_eq!(store.at(inbox, i as u32 + 1).unwrap(), Some((id, flag::FLAGGED)));
        }
        assert_eq!(store.blob(&blob).unwrap(), raw);
        assert_eq!(store.blob(&document).unwrap(), b"cached document");

        store.write(|w| w.set_sender_check(ids[0], verified.clone())).unwrap();
        drop(store);
        assert_eq!(Store::open(dir.path()).unwrap().message(ids[0]).unwrap().summary.sender_check, verified);
    }
}
