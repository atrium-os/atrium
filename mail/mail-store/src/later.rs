//! *Later* (mail.md §11.4): a conversation set aside with a time it comes
//! back, what it is for, and a note. Local; the server only sees
//! `\Flagged` on its newest message.

use redb::{ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::{dec, enc, AccountId, MsgId, Result, Store, ThreadId, Writer};

/// id → [`Later`].
pub(crate) const LATER: TableDefinition<u64, &[u8]> = TableDefinition::new("later");
/// thread → its open item.
pub(crate) const LATER_BY_THREAD: TableDefinition<u64, u64> = TableDefinition::new("later_by_thread");

/// Message-ID (without `<>`) → (account, when): a follow-up asked for on
/// send, waiting for the sent copy to arrive in Sent (§9, §11.4).
pub(crate) const FOLLOW_UPS: TableDefinition<&str, (u32, i64)> = TableDefinition::new("follow_ups");

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
