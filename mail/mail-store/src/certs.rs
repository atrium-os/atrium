//! S/MIME certificates seen on signed mail (mail.md §10.1), per address:
//! what encrypting to that person uses. Only certificates from signatures
//! the parse jail found valid (trusted, for that address) are kept.

use redb::{ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::{dec, enc, Result, Store, Writer};

/// Address (lower case) → postcard `Vec<SeenCert>`, newest first.
pub(crate) const CERTS: TableDefinition<&str, &[u8]> = TableDefinition::new("certs");

/// Certificates kept per address (a renewal adds one; the oldest goes).
pub const CERTS_PER_ADDRESS: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeenCert {
    pub sha256: [u8; 32],
    pub der: Vec<u8>,
    pub name: String,
    pub issuer: String,
    pub not_after: i64,
    pub first_seen: i64,
    pub last_seen: i64,
}

impl Store {
    /// Every address with certificates, each's newest first.
    pub fn all_certs(&self) -> Result<Vec<(String, Vec<SeenCert>)>> {
        let tx = self.db.begin_read()?;
        let mut out = Vec::new();
        for e in tx.open_table(CERTS)?.iter()? {
            let (k, v) = e?;
            out.push((k.value().to_string(), dec(v.value())?));
        }
        Ok(out)
    }

    /// The certificates seen for `addr`, newest first.
    pub fn certs(&self, addr: &str) -> Result<Vec<SeenCert>> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(CERTS)?;
        let v = t.get(addr.to_ascii_lowercase().as_str())?;
        match v {
            Some(v) => dec(v.value()),
            None => Ok(Vec::new()),
        }
    }
}

impl Writer<'_> {
    /// `cert` signed mail from `addr` at `when`.
    pub fn saw_cert(&mut self, addr: &str, cert: SeenCert) -> Result<()> {
        let addr = addr.to_ascii_lowercase();
        let mut t = self.tx.open_table(CERTS)?;
        let mut list: Vec<SeenCert> = match t.get(addr.as_str())? {
            Some(v) => dec(v.value())?,
            None => Vec::new(),
        };
        match list.iter_mut().find(|c| c.sha256 == cert.sha256) {
            Some(c) => {
                c.last_seen = c.last_seen.max(cert.last_seen);
                c.first_seen = c.first_seen.min(cert.first_seen);
            }
            None => list.push(cert),
        }
        // Newest certificate first: the one that expires last.
        list.sort_by(|a, b| b.not_after.cmp(&a.not_after).then(b.last_seen.cmp(&a.last_seen)));
        list.truncate(CERTS_PER_ADDRESS);
        t.insert(addr.as_str(), enc(&list).as_slice())?;
        Ok(())
    }

    /// Forget the certificates for `addr` (the person removed them).
    pub fn forget_certs(&mut self, addr: &str) -> Result<()> {
        self.tx.open_table(CERTS)?.remove(addr.to_ascii_lowercase().as_str())?;
        Ok(())
    }
}
