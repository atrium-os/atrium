//! OpenPGP keys per address (mail.md §10.2): learned from the `Autocrypt:`
//! header of mail from that address, or given by the person. What
//! encrypting to them uses, and what their signatures are checked against.
//! The store keeps the bytes; the parse jail reads them.

use redb::{ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::{dec, enc, Result, Store, Writer};

/// Address (lower case) → postcard `Vec<PgpKey>`, newest first.
pub(crate) const PGP_KEYS: TableDefinition<&str, &[u8]> = TableDefinition::new("pgp_keys");

/// Keys kept per address (a new key adds one; the oldest goes).
pub const PGP_KEYS_PER_ADDRESS: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PgpKey {
    /// The transferable public key, binary.
    pub key: Vec<u8>,
    pub first_seen: i64,
    pub last_seen: i64,
    /// Given by the person, not learned from a header.
    pub given: bool,
}

impl Store {
    /// Every address with keys, by address, each's keys newest first.
    pub fn all_pgp_keys(&self) -> Result<Vec<(String, Vec<PgpKey>)>> {
        let tx = self.db.begin_read()?;
        let mut out = Vec::new();
        for e in tx.open_table(PGP_KEYS)?.iter()? {
            let (k, v) = e?;
            out.push((k.value().to_string(), dec(v.value())?));
        }
        Ok(out)
    }

    /// The keys for `addr`, newest first.
    pub fn pgp_keys(&self, addr: &str) -> Result<Vec<PgpKey>> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(PGP_KEYS)?;
        let v = t.get(addr.to_ascii_lowercase().as_str())?;
        match v {
            Some(v) => dec(v.value()),
            None => Ok(Vec::new()),
        }
    }
}

impl Writer<'_> {
    /// `key` for `addr`, seen (or given) at `key.last_seen`. A key given
    /// by the person stays first until they give another.
    pub fn saw_pgp_key(&mut self, addr: &str, key: PgpKey) -> Result<()> {
        let addr = addr.to_ascii_lowercase();
        let mut t = self.tx.open_table(PGP_KEYS)?;
        let mut list: Vec<PgpKey> = match t.get(addr.as_str())? {
            Some(v) => dec(v.value())?,
            None => Vec::new(),
        };
        match list.iter_mut().find(|k| k.key == key.key) {
            Some(k) => {
                k.last_seen = k.last_seen.max(key.last_seen);
                k.first_seen = k.first_seen.min(key.first_seen);
                k.given |= key.given;
            }
            None => list.push(key),
        }
        list.sort_by(|a, b| b.given.cmp(&a.given).then(b.last_seen.cmp(&a.last_seen)));
        list.truncate(PGP_KEYS_PER_ADDRESS);
        t.insert(addr.as_str(), enc(&list).as_slice())?;
        Ok(())
    }

    /// Forget the keys for `addr`.
    pub fn forget_pgp_keys(&mut self, addr: &str) -> Result<()> {
        self.tx.open_table(PGP_KEYS)?.remove(addr.to_ascii_lowercase().as_str())?;
        Ok(())
    }
}
