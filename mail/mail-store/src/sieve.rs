//! An IMAP account's ManageSieve server (mail.md §11.6): where it is, the
//! Arca item that signs in to it (a reference: Mail never holds the
//! password), and what Mail last put there. Kept beside the account's
//! settings, which predate it.

use redb::TableDefinition;
use serde::{Deserialize, Serialize};

use crate::{dec, enc, AccountId, Result, Server, Store, Writer};

/// account → postcard [`SieveAccount`].
pub(crate) const SIEVE: TableDefinition<u32, &[u8]> = TableDefinition::new("sieve");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SieveAccount {
    pub server: Server,
    pub item: [u8; 16],
    /// What was last put on the server (the engine's own words for it),
    /// so an unchanged script isn't sent again.
    pub put: Option<String>,
    /// Another script is active there (its name): Mail leaves it alone.
    pub others: Option<String>,
    /// Why the last attempt failed, and when it was.
    pub why: Option<String>,
    pub tried: i64,
    /// Rules the server's Sieve can't do.
    pub skipped: u32,
}

impl Writer<'_> {
    pub fn set_sieve(&mut self, account: AccountId, s: Option<&SieveAccount>) -> Result<()> {
        let mut t = self.tx.open_table(SIEVE)?;
        match s {
            Some(s) => t.insert(account, enc(s).as_slice())?,
            None => t.remove(account)?,
        };
        Ok(())
    }
}

impl Store {
    pub fn sieve(&self, account: AccountId) -> Result<Option<SieveAccount>> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(SIEVE)?;
        let v = t.get(account)?;
        v.map(|v| dec(v.value())).transpose()
    }
}
