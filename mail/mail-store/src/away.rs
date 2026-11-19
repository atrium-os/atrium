//! The automatic reply while away (mail.md §11.6), per account: what the
//! person asked for until the server has it, and what the server said
//! last. The server keeps the reply; Mail only sets and shows it.

use redb::TableDefinition;
use serde::{Deserialize, Serialize};

use crate::{dec, enc, AccountId, Result, Store, Writer};

/// account → postcard [`AwayState`].
pub(crate) const AWAY: TableDefinition<u32, &[u8]> = TableDefinition::new("away");

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Away {
    pub on: bool,
    pub from: Option<i64>,
    pub until: Option<i64>,
    pub subject: String,
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AwayState {
    /// Asked for here, not yet on the server.
    pub wanted: Option<Away>,
    /// The server's, as last read; `None` before the first read.
    pub server: Option<Away>,
    /// The server has no automatic reply Mail can set (`Some(false)`).
    pub supported: Option<bool>,
    /// Why the last attempt failed.
    pub why: Option<String>,
}

impl Writer<'_> {
    pub fn set_away(&mut self, account: AccountId, s: &AwayState) -> Result<()> {
        self.tx.open_table(AWAY)?.insert(account, enc(s).as_slice())?;
        Ok(())
    }
}

impl Store {
    pub fn away(&self, account: AccountId) -> Result<AwayState> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(AWAY)?;
        let v = t.get(account)?;
        Ok(match v {
            Some(v) => dec(v.value())?,
            None => AwayState::default(),
        })
    }
}
