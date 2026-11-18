//! Changes the person makes (mail.md §6.3, §15.5): applied to the index at
//! once, kept as an op until sync has made them on the server. Each op
//! waits until its `due` time first, so Undo (10 s) never reaches the
//! server. A message moved or deleted leaves its folder's list at once but
//! stays known to sync (its location is marked [`LEAVING`]), so a resync in
//! the undo window doesn't fetch it again.

use redb::{ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::{date_key, dec, enc, AccountId, Error, FolderId, MsgId, Result, Store, Writer, LIST, LOCATIONS};

pub(crate) const OPS: TableDefinition<u64, &[u8]> = TableDefinition::new("ops");

/// A location's flag bit: the message is leaving this folder (a move or a
/// delete waits for the server); not one of IMAP's.
pub const LEAVING: u32 = 1 << 31;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OpKind {
    /// Add and remove flag bits (`flag::*`).
    Flags { add: u32, remove: u32 },
    /// Move to another folder of the same account.
    Move { to: FolderId },
    /// Delete for good (a message already in Trash).
    Expunge,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Op {
    pub account: AccountId,
    pub folder: FolderId,
    pub uid: u32,
    pub msg: MsgId,
    pub kind: OpKind,
    /// Not sent before this time (seconds), so Undo stays local.
    pub due: i64,
    /// The location's flags before, for Undo.
    pub before: u32,
}

impl Writer<'_> {
    /// Make a change in the index now and keep it for sync; its op id.
    pub fn queue(&mut self, folder: FolderId, uid: u32, kind: OpKind, due: i64) -> Result<u64> {
        let (msg, flags) = self.tx.open_table(LOCATIONS)?.get((folder, uid))?.map(|v| v.value()).ok_or(Error::NotFound)?;
        if flags & LEAVING != 0 {
            return Err(Error::NotFound);
        }
        let m = self.get_msg(msg)?.ok_or(Error::NotFound)?;
        if let OpKind::Move { to } = kind {
            let f = self.store_folder(to)?;
            if f.account != m.account || to == folder {
                return Err(Error::NotFound);
            }
        }
        match kind {
            OpKind::Flags { add, remove } => {
                self.tx.open_table(LOCATIONS)?.insert((folder, uid), (msg, (flags | add) & !remove & !LEAVING))?;
                self.thread_refresh(m.thread)?;
            }
            OpKind::Move { .. } | OpKind::Expunge => self.hide(folder, uid, msg, flags)?,
        }
        let id = Store::next_id(self.tx, "op")?;
        let op = Op { account: m.account, folder, uid, msg, kind, due, before: flags };
        self.tx.open_table(OPS)?.insert(id, enc(&op).as_slice())?;
        Ok(id)
    }

    /// Undo an op not yet sent: the index as it was. False when it has gone
    /// (sent, or never was).
    pub fn undo(&mut self, id: u64) -> Result<bool> {
        let Some(op) = self.tx.open_table(OPS)?.remove(id)?.map(|v| dec::<Op>(v.value())).transpose()? else { return Ok(false) };
        self.revert(&op)?;
        Ok(true)
    }

    /// Sync made op `id` on the server (`ok`), or the server refused it: the
    /// index goes back as it was, and the next resync tells the truth.
    pub fn op_done(&mut self, id: u64, ok: bool) -> Result<()> {
        let Some(op) = self.tx.open_table(OPS)?.remove(id)?.map(|v| dec::<Op>(v.value())).transpose()? else { return Ok(()) };
        if !ok {
            self.revert(&op)?;
        }
        Ok(())
    }

    fn revert(&mut self, op: &Op) -> Result<()> {
        let cur = self.tx.open_table(LOCATIONS)?.get((op.folder, op.uid))?.map(|v| v.value());
        let Some((msg, flags)) = cur else { return Ok(()) };
        match op.kind {
            OpKind::Flags { .. } => {
                self.tx.open_table(LOCATIONS)?.insert((op.folder, op.uid), (msg, (op.before & !LEAVING) | (flags & LEAVING)))?;
                if let Some(m) = self.get_msg(msg)? {
                    self.thread_refresh(m.thread)?;
                }
            }
            OpKind::Move { .. } | OpKind::Expunge => self.unhide(op.folder, op.uid, msg, flags)?,
        }
        Ok(())
    }

    fn hide(&mut self, folder: FolderId, uid: u32, msg: MsgId, flags: u32) -> Result<()> {
        let mut m = self.get_msg(msg)?.ok_or(Error::NotFound)?;
        self.tx.open_table(LOCATIONS)?.insert((folder, uid), (msg, flags | LEAVING))?;
        self.tx.open_table(LIST)?.remove((folder, date_key(m.date), msg))?;
        m.locations.retain(|l| *l != (folder, uid));
        m.leaving.push((folder, uid));
        self.put_msg(msg, &m)?;
        self.thread_refresh(m.thread)
    }

    fn unhide(&mut self, folder: FolderId, uid: u32, msg: MsgId, flags: u32) -> Result<()> {
        let Some(mut m) = self.get_msg(msg)? else { return Ok(()) };
        self.tx.open_table(LOCATIONS)?.insert((folder, uid), (msg, flags & !LEAVING))?;
        self.tx.open_table(LIST)?.insert((folder, date_key(m.date), msg), uid)?;
        m.leaving.retain(|l| *l != (folder, uid));
        if !m.locations.contains(&(folder, uid)) {
            m.locations.push((folder, uid));
        }
        self.put_msg(msg, &m)?;
        self.thread_refresh(m.thread)
    }

    pub(crate) fn store_folder(&self, id: FolderId) -> Result<crate::Folder> {
        let t = self.tx.open_table(crate::FOLDERS)?;
        let v = t.get(id)?.ok_or(Error::NotFound)?;
        dec(v.value())
    }
}

impl Store {
    /// An account's ops due by `now`, oldest first.
    pub fn ops_due(&self, account: AccountId, now: i64) -> Result<Vec<(u64, Op)>> {
        let tx = self.db.begin_read()?;
        let mut out = Vec::new();
        for e in tx.open_table(OPS)?.iter()? {
            let (k, v) = e?;
            let op: Op = dec(v.value())?;
            if op.account == account && op.due <= now {
                out.push((k.value(), op));
            }
        }
        Ok(out)
    }

    /// Whether an op not yet sent acts on (folder, uid): the server's flags
    /// for it are not the last word until it is.
    pub fn op_pending_at(&self, folder: FolderId, uid: u32) -> Result<bool> {
        let tx = self.db.begin_read()?;
        for e in tx.open_table(OPS)?.iter()? {
            let (_, v) = e?;
            let op: Op = dec(v.value())?;
            if op.folder == folder && op.uid == uid {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Ops not yet sent, for every account.
    pub fn ops_pending(&self) -> Result<usize> {
        let tx = self.db.begin_read()?;
        Ok(tx.open_table(OPS)?.iter()?.count())
    }
}
