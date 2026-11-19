//! Messages imported from an mbox file (mail.md §6.5), waiting to be
//! uploaded to their folder on the account's server (IMAP `APPEND`, JMAP
//! `Email/import`). Sync then brings them in like any other mail, so the
//! server and every other client have them too.

use redb::{ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::{dec, enc, FolderId, Hash, Result, Store, Writer};

/// id → postcard [`Import`].
pub(crate) const IMPORTS: TableDefinition<u64, &[u8]> = TableDefinition::new("imports");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Import {
    pub folder: FolderId,
    /// The message as it was in the file.
    pub blob: Hash,
    /// Failed uploads, and why the last one failed.
    pub tries: u32,
    pub why: Option<String>,
}

/// After this many failed uploads a message waits for the person.
pub const TRIES_MAX: u32 = 5;

impl Writer<'_> {
    /// Queue `blob` for upload to `folder`; its id.
    pub fn queue_import(&mut self, folder: FolderId, blob: Hash) -> Result<u64> {
        let id = Store::next_id(self.tx, "import")?;
        self.tx.open_table(IMPORTS)?.insert(id, enc(&Import { folder, blob, tries: 0, why: None }).as_slice())?;
        Ok(id)
    }

    /// Uploaded: done.
    pub fn import_done(&mut self, id: u64) -> Result<()> {
        self.tx.open_table(IMPORTS)?.remove(id)?;
        Ok(())
    }

    /// The upload failed; tried again on later passes up to [`TRIES_MAX`].
    pub fn import_failed(&mut self, id: u64, why: &str) -> Result<()> {
        let mut t = self.tx.open_table(IMPORTS)?;
        let Some(mut i) = t.get(id)?.map(|v| dec::<Import>(v.value())).transpose()? else { return Ok(()) };
        i.tries += 1;
        i.why = Some(why.chars().take(200).collect());
        t.insert(id, enc(&i).as_slice())?;
        Ok(())
    }

    /// Forget every waiting upload to `folder` (the person cancelled).
    pub fn cancel_imports(&mut self, folder: FolderId) -> Result<usize> {
        let mut t = self.tx.open_table(IMPORTS)?;
        let ids: Vec<u64> = t.iter()?.filter_map(|e| e.ok()).filter_map(|(k, v)| (dec::<Import>(v.value()).ok()?.folder == folder).then(|| k.value())).collect();
        for id in &ids {
            t.remove(*id)?;
        }
        Ok(ids.len())
    }
}

impl Store {
    /// Every upload waiting, oldest first.
    pub fn imports(&self) -> Result<Vec<(u64, Import)>> {
        let tx = self.db.begin_read()?;
        let mut out = Vec::new();
        for e in tx.open_table(IMPORTS)?.iter()? {
            let (k, v) = e?;
            out.push((k.value(), dec(v.value())?));
        }
        Ok(out)
    }
}
