//! What Mail keeps about senders and lists (mail.md §11.3): lists the
//! person unsubscribed from or blocked, the addresses they have written
//! to or let through the screener, and per-account switches.

use redb::{ReadableTable, TableDefinition};

use crate::{date_key, AccountId, Error, FolderId, MsgId, Result, Store, Writer, LIST};

/// (account, list key) → [`list`] state.
pub(crate) const LISTS: TableDefinition<(u32, &str), u8> = TableDefinition::new("lists");
/// (account, lowercase address) → [`contact`] bits.
pub(crate) const CONTACTS: TableDefinition<(u32, &str), u8> = TableDefinition::new("contacts");
/// (account, name) → value: per-account switches (the screener).
pub(crate) const SETTINGS: TableDefinition<(u32, &str), u64> = TableDefinition::new("account_settings");
/// Text kept per account by sync (JMAP's Email state).
pub(crate) const SYNC_STATE: TableDefinition<(u32, &str), &str> = TableDefinition::new("account_sync_state");

/// (account, newest-first date key, message) → (folder, uid, [`why`]):
/// mail held out of its folder's list, by the screener (*New senders*) or
/// set aside with *Later*.
pub(crate) const HELD: TableDefinition<(u32, u64, u64), (u32, u32, u8)> = TableDefinition::new("held");

/// Why a message is held.
pub mod why {
    pub const SCREENER: u8 = 1;
    pub const LATER: u8 = 2;
}

/// (account, lane, newest-first date key, message) → (): the lanes and
/// views that are not folders (the newsletter lane, §11.3).
pub(crate) const LANES: TableDefinition<(u32, u8, u64, u64), ()> = TableDefinition::new("lanes");

/// Lanes.
pub mod lane {
    pub const NEWSLETTERS: u8 = 1;
}

/// A list's state.
pub mod list {
    pub const NONE: u8 = 0;
    /// Unsubscribe was done (or asked for).
    pub const UNSUBSCRIBED: u8 = 1;
    /// Later mail from it goes to Trash.
    pub const BLOCKED: u8 = 2;
}

/// What is known of an address.
pub mod contact {
    /// The person has written to it.
    pub const WROTE: u8 = 1;
    /// Accepted in the screener.
    pub const ACCEPTED: u8 = 2;
    /// Refused in the screener: its mail goes to Screened out.
    pub const REFUSED: u8 = 4;
    /// Mail came from it before the screener was turned on.
    pub const KNOWN: u8 = 8;
}

impl Store {
    pub fn list_state(&self, account: AccountId, key: &str) -> Result<u8> {
        let tx = self.db.begin_read()?;
        let v = tx.open_table(LISTS)?.get((account, key))?.map_or(list::NONE, |v| v.value());
        Ok(v)
    }

    pub fn contact(&self, account: AccountId, addr: &str) -> Result<u8> {
        let tx = self.db.begin_read()?;
        let a = addr.to_ascii_lowercase();
        let v = tx.open_table(CONTACTS)?.get((account, a.as_str()))?.map_or(0, |v| v.value());
        Ok(v)
    }

    /// Addresses with any of `bits`, in order.
    pub fn contacts_with(&self, account: AccountId, bits: u8) -> Result<Vec<String>> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(CONTACTS)?;
        let mut out = Vec::new();
        for e in t.range((account, "")..)? {
            let (k, v) = e?;
            let (a, addr) = k.value();
            if a != account {
                break;
            }
            if v.value() & bits != 0 {
                out.push(addr.to_string());
            }
        }
        Ok(out)
    }

    /// Mail held for `account`, newest first: (message, folder, uid,
    /// [`why`]).
    pub fn held(&self, account: AccountId) -> Result<Vec<(MsgId, FolderId, u32, u8)>> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(HELD)?;
        let mut out = Vec::new();
        for e in t.range((account, 0, 0)..=(account, u64::MAX, u64::MAX))? {
            let (k, v) = e?;
            let (f, u, w) = v.value();
            out.push((k.value().2, f, u, w));
        }
        Ok(out)
    }

    /// Messages in `lane` for `account`, newest first (a message gone
    /// since is the caller's to skip).
    pub fn lane(&self, account: AccountId, lane: u8, offset: usize, limit: usize) -> Result<Vec<MsgId>> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(LANES)?;
        let mut out = Vec::new();
        for e in t.range((account, lane, 0, 0)..=(account, lane, u64::MAX, u64::MAX))?.skip(offset).take(limit) {
            out.push(e?.0.value().3);
        }
        Ok(out)
    }

    pub fn setting(&self, account: AccountId, name: &str) -> Result<Option<u64>> {
        let tx = self.db.begin_read()?;
        let v = tx.open_table(SETTINGS)?.get((account, name))?.map(|v| v.value());
        Ok(v)
    }

    pub fn sync_state(&self, account: AccountId, name: &str) -> Result<Option<String>> {
        let tx = self.db.begin_read()?;
        let v = tx.open_table(SYNC_STATE)?.get((account, name))?.map(|v| v.value().to_string());
        Ok(v)
    }
}

impl Writer<'_> {
    pub fn set_list_state(&mut self, account: AccountId, key: &str, state: u8) -> Result<()> {
        let mut t = self.tx.open_table(LISTS)?;
        if state == list::NONE {
            t.remove((account, key))?;
        } else {
            t.insert((account, key), state)?;
        }
        Ok(())
    }

    /// Add `add` and take away `remove` from what is known of `addr`.
    pub fn mark_contact(&mut self, account: AccountId, addr: &str, add: u8, remove: u8) -> Result<()> {
        let a = addr.to_ascii_lowercase();
        let mut t = self.tx.open_table(CONTACTS)?;
        let cur = t.get((account, a.as_str()))?.map_or(0, |v| v.value());
        let new = (cur | add) & !remove;
        if new == 0 {
            t.remove((account, a.as_str()))?;
        } else if new != cur {
            t.insert((account, a.as_str()), new)?;
        }
        Ok(())
    }

    pub fn set_lane(&mut self, account: AccountId, lane: u8, msg: MsgId, date: i64, on: bool) -> Result<()> {
        let mut t = self.tx.open_table(LANES)?;
        if on {
            t.insert((account, lane, date_key(date), msg), ())?;
        } else {
            t.remove((account, lane, date_key(date), msg))?;
        }
        Ok(())
    }

    /// Hold (folder, uid) back from its folder's list ([`why`]).
    pub fn hold(&mut self, folder: FolderId, uid: u32, why: u8) -> Result<bool> {
        let Some((msg, _)) = self.tx.open_table(crate::LOCATIONS)?.get((folder, uid))?.map(|v| v.value()) else { return Ok(false) };
        let mut m = self.get_msg(msg)?.ok_or(Error::NotFound)?;
        if !m.locations.contains(&(folder, uid)) {
            return Ok(false);
        }
        self.tx.open_table(LIST)?.remove((folder, date_key(m.date), msg))?;
        self.tx.open_table(HELD)?.insert((m.account, date_key(m.date), msg), (folder, uid, why))?;
        m.locations.retain(|l| *l != (folder, uid));
        m.held.push((folder, uid));
        self.put_msg(msg, &m)?;
        self.thread_refresh(m.thread)?;
        Ok(true)
    }

    /// Put a held message back in its folder's list.
    pub fn release(&mut self, msg: MsgId) -> Result<()> {
        let Some(mut m) = self.get_msg(msg)? else { return Ok(()) };
        let previous_locations = m.locations.len();
        for (folder, uid) in std::mem::take(&mut m.held) {
            self.tx.open_table(HELD)?.remove((m.account, date_key(m.date), msg))?;
            if self.tx.open_table(crate::LOCATIONS)?.get((folder, uid))?.is_some() {
                self.tx.open_table(LIST)?.insert((folder, date_key(m.date), msg), uid)?;
                m.locations.push((folder, uid));
            }
        }
        if m.locations.len() > previous_locations {
            self.queue_calendar(msg, &m)?;
        }
        self.put_msg(msg, &m)?;
        self.thread_refresh(m.thread)
    }

    pub fn set_sync_state(&mut self, account: AccountId, name: &str, value: Option<&str>) -> Result<()> {
        let mut t = self.tx.open_table(SYNC_STATE)?;
        match value {
            Some(v) => t.insert((account, name), v)?,
            None => t.remove((account, name))?,
        };
        Ok(())
    }

    pub fn set_setting(&mut self, account: AccountId, name: &str, value: Option<u64>) -> Result<()> {
        let mut t = self.tx.open_table(SETTINGS)?;
        match value {
            Some(v) => t.insert((account, name), v)?,
            None => t.remove((account, name))?,
        };
        Ok(())
    }
}
