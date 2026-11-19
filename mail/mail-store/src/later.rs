//! *Later* (mail.md §11.4): a conversation set aside with a time it comes
//! back, what it is for, and a note. The store also tracks the Inbox
//! location of messages moved to the server's snooze mailbox.

use redb::{ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::{dec, enc, AccountId, MsgId, Result, Store, ThreadId, Writer};

/// id → [`Later`].
pub(crate) const LATER: TableDefinition<u64, &[u8]> = TableDefinition::new("later");
/// thread → its open item.
pub(crate) const LATER_BY_THREAD: TableDefinition<u64, u64> = TableDefinition::new("later_by_thread");

/// id → ([`Later`], when it was dropped): items dropped by hand or after
/// 30 days overdue (§11.4), kept so a drop can be undone.
pub(crate) const LATER_DROPPED: TableDefinition<u64, &[u8]> = TableDefinition::new("later_dropped");

/// Message-ID (without `<>`) → (account, when): a follow-up asked for on
/// send, waiting for the sent copy to arrive in Sent (§9, §11.4).
pub(crate) const FOLLOW_UPS: TableDefinition<&str, (u32, i64)> = TableDefinition::new("follow_ups");

/// Physical inbox messages entrusted to the server's snooze mailbox.
pub(crate) const PARKED: TableDefinition<(AccountId, MsgId), &[u8]> = TableDefinition::new("snoozed_server");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Parked {
    pub inbox: crate::FolderId,
    /// Identifies copies on IMAP servers without a stable message ID.
    pub header: crate::Hash,
    pub size: u64,
    /// Inbox arrivals can be matched only after a return has been planned.
    pub returning: bool,
}

/// The fingerprint of a header-only fetch or the header of a whole message.
pub fn header_hash(bytes: &[u8]) -> crate::Hash {
    use sha2::{Digest, Sha256};
    let end = bytes.windows(4).position(|w| w == b"\r\n\r\n")
        .or_else(|| bytes.windows(2).position(|w| w == b"\n\n")).unwrap_or(bytes.len());
    let header = bytes[..end].trim_ascii_end();
    Sha256::digest(header).into()
}

impl Store {
    pub fn parked(&self, account: AccountId) -> Result<Vec<(MsgId, Parked)>> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(PARKED)?;
        t.range((account, 0)..=(account, MsgId::MAX))?.map(|entry| {
            let (k, v) = entry?;
            Ok((k.value().1, dec(v.value())?))
        }).collect()
    }
}

impl Writer<'_> {
    pub fn park(&mut self, account: AccountId, msg: MsgId, park: &Parked) -> Result<()> {
        self.tx.open_table(PARKED)?.insert((account, msg), enc(park).as_slice())?;
        Ok(())
    }

    pub fn forget_parked(&mut self, account: AccountId, msg: MsgId) -> Result<()> {
        self.tx.open_table(PARKED)?.remove((account, msg))?;
        if let Some(m) = self.get_msg(msg)?.filter(|m| m.locations.is_empty() && m.leaving.is_empty() && m.held.is_empty()) {
            self.remove_unplaced(msg, &m)?;
            self.thread_refresh(m.thread)?;
        }
        Ok(())
    }
}

/// What an item is for.
pub mod kind {
    pub const REPLY: u8 = 1;
    pub const READ: u8 = 2;
    pub const DO: u8 = 3;
    /// Waiting for someone else (the follow-up of §9 is one).
    pub const WAITING: u8 = 4;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Later {
    pub account: AccountId,
    pub thread: ThreadId,
    /// The message it was set on (the newest then).
    pub msg: MsgId,
    pub kind: u8,
    /// When it comes back.
    pub due: i64,
    pub note: String,
    /// When it was set aside (first).
    pub set_at: i64,
    /// Times it was moved to another time.
    pub pushed: u32,
    /// Back in the inbox (its time came, or new mail brought it).
    pub back: bool,
}

impl Store {
    /// Every open item of `account`, soonest first.
    pub fn later_items(&self, account: AccountId) -> Result<Vec<(u64, Later)>> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(LATER)?;
        let mut out = Vec::new();
        for e in t.iter()? {
            let (k, v) = e?;
            let l: Later = dec(v.value())?;
            if l.account == account {
                out.push((k.value(), l));
            }
        }
        out.sort_by_key(|(id, l)| (l.due, *id));
        Ok(out)
    }

    /// The open item on a conversation.
    pub fn later_of(&self, thread: ThreadId) -> Result<Option<(u64, Later)>> {
        let tx = self.db.begin_read()?;
        let Some(id) = tx.open_table(LATER_BY_THREAD)?.get(thread)?.map(|v| v.value()) else { return Ok(None) };
        let v = tx.open_table(LATER)?.get(id)?.map(|v| dec::<Later>(v.value())).transpose()?;
        Ok(v.map(|l| (id, l)))
    }
}

impl Store {
    /// Dropped items of `account`, latest dropped first, with when.
    pub fn later_dropped(&self, account: AccountId) -> Result<Vec<(u64, Later, i64)>> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(LATER_DROPPED)?;
        let mut out = Vec::new();
        for e in t.iter()? {
            let (k, v) = e?;
            let (l, at): (Later, i64) = dec(v.value())?;
            if l.account == account {
                out.push((k.value(), l, at));
            }
        }
        out.sort_by_key(|(id, _, at)| (std::cmp::Reverse(*at), std::cmp::Reverse(*id)));
        Ok(out)
    }

    /// The dropped item on a conversation, the latest when several.
    pub fn later_dropped_of(&self, thread: ThreadId) -> Result<Option<(u64, Later, i64)>> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(LATER_DROPPED)?;
        let mut best: Option<(u64, Later, i64)> = None;
        for e in t.iter()? {
            let (k, v) = e?;
            let (l, at): (Later, i64) = dec(v.value())?;
            if l.thread == thread && best.as_ref().is_none_or(|b| (at, k.value()) > (b.2, b.0)) {
                best = Some((k.value(), l, at));
            }
        }
        Ok(best)
    }
}

impl Writer<'_> {
    /// Keep `item` (new when `id` is None); its id.
    pub fn put_later(&mut self, id: Option<u64>, item: &Later) -> Result<u64> {
        let id = match id {
            Some(i) => i,
            None => Store::next_id(self.tx, "later")?,
        };
        self.tx.open_table(LATER)?.insert(id, enc(item).as_slice())?;
        self.tx.open_table(LATER_BY_THREAD)?.insert(item.thread, id)?;
        Ok(id)
    }

    /// Remind if nobody replies to the message `message_id` by `due`.
    pub fn set_follow_up(&mut self, message_id: &str, account: AccountId, due: i64) -> Result<()> {
        self.tx.open_table(FOLLOW_UPS)?.insert(message_id.trim_matches(['<', '>']), (account, due))?;
        Ok(())
    }

    /// The follow-up asked for on `message_id`, taken.
    pub fn take_follow_up(&mut self, message_id: &str) -> Result<Option<(AccountId, i64)>> {
        Ok(self.tx.open_table(FOLLOW_UPS)?.remove(message_id.trim_matches(['<', '>']))?.map(|v| v.value()))
    }

    /// Drop an open item at `at`: it closes, kept aside for an undo.
    pub fn drop_later(&mut self, id: u64, at: i64) -> Result<Option<Later>> {
        let Some(l) = self.close_later(id)? else { return Ok(None) };
        self.tx.open_table(LATER_DROPPED)?.insert(id, enc(&(&l, at)).as_slice())?;
        Ok(Some(l))
    }

    /// Undo a drop: the item is open again, unless its conversation has
    /// another open item by now (then it stays dropped: None).
    pub fn undrop_later(&mut self, id: u64) -> Result<Option<Later>> {
        let Some((l, _)) = self.tx.open_table(LATER_DROPPED)?.get(id)?.map(|v| dec::<(Later, i64)>(v.value())).transpose()? else { return Ok(None) };
        if self.tx.open_table(LATER_BY_THREAD)?.get(l.thread)?.is_some() {
            return Ok(None);
        }
        self.tx.open_table(LATER_DROPPED)?.remove(id)?;
        self.put_later(Some(id), &l)?;
        Ok(Some(l))
    }

    /// Forget dropped items dropped before `before`; how many.
    pub fn forget_dropped(&mut self, before: i64) -> Result<usize> {
        let mut t = self.tx.open_table(LATER_DROPPED)?;
        let mut old = Vec::new();
        for e in t.iter()? {
            let (k, v) = e?;
            let (_, at): (Later, i64) = dec(v.value())?;
            if at < before {
                old.push(k.value());
            }
        }
        for k in &old {
            t.remove(*k)?;
        }
        Ok(old.len())
    }

    /// Close an item (done, dropped, or closed by a reply).
    pub fn close_later(&mut self, id: u64) -> Result<Option<Later>> {
        let Some(l) = self.tx.open_table(LATER)?.remove(id)?.map(|v| dec::<Later>(v.value())).transpose()? else { return Ok(None) };
        let mut t = self.tx.open_table(LATER_BY_THREAD)?;
        if t.get(l.thread)?.is_some_and(|v| v.value() == id) {
            t.remove(l.thread)?;
        }
        Ok(Some(l))
    }
}
