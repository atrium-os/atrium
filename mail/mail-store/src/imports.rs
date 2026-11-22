//! Messages imported from an mbox file (mail.md §6.5), waiting to be
//! uploaded to their folder on the account's server (IMAP `APPEND`, JMAP
//! `Email/import`). Sync then brings them in like any other mail, so the
//! server and every other client have them too.

use redb::{ReadableTable, TableDefinition, WriteTransaction};
use serde::{Deserialize, Serialize};

use crate::{dec, enc, AccountId, Folder, FolderId, Hash, Result, Store, Writer, FOLDERS, META};

/// id → postcard [`Import`].
pub(crate) const IMPORTS: TableDefinition<u64, &[u8]> = TableDefinition::new("imports");

const UNINDEXED_IMPORTS: TableDefinition<u64, ()> = TableDefinition::new("unindexed_imports");
const IMPORT_IDENTITIES: TableDefinition<u64, (AccountId, &str)> = TableDefinition::new("import_identities");
const IMPORTS_BY_MESSAGE_ID: TableDefinition<(AccountId, &str, u64), ()> =
    TableDefinition::new("imports_by_message_id");

// An accepted upload protects its Message-ID until mailbox sync indexes it.
const ACCEPTED_IMPORTS: TableDefinition<(AccountId, &str), ()> = TableDefinition::new("accepted_imports");

pub(crate) fn open(tx: &WriteTransaction) -> Result<()> {
    let imports = tx.open_table(IMPORTS)?;
    let mut unindexed = tx.open_table(UNINDEXED_IMPORTS)?;
    tx.open_table(IMPORT_IDENTITIES)?;
    tx.open_table(IMPORTS_BY_MESSAGE_ID)?;
    tx.open_table(ACCEPTED_IMPORTS)?;
    let mut meta = tx.open_table(META)?;
    if meta.get("import_identity_index")?.is_none() {
        for row in imports.iter()? {
            let (id, _) = row?;
            unindexed.insert(id.value(), ())?;
        }
        meta.insert("import_identity_index", 1)?;
    }
    Ok(())
}

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
    /// Queue without parsed headers for the engine to index later.
    pub fn queue_import(&mut self, folder: FolderId, blob: Hash) -> Result<u64> {
        let id = self.insert_import(folder, blob)?;
        self.tx.open_table(UNINDEXED_IMPORTS)?.insert(id, ())?;
        Ok(id)
    }

    /// Check the account's index, queue and accepted uploads for the same
    /// Message-ID. Insert a new row in that same transaction.
    pub fn queue_import_if_new(
        &mut self,
        folder: FolderId,
        message_id: Option<&str>,
        save_blob: impl FnOnce() -> Result<Hash>,
    ) -> Result<bool> {
        let account = self.store_folder(folder)?.account;
        if let Some(mid) = message_id {
            let queue = self.tx.open_table(IMPORTS_BY_MESSAGE_ID)?;
            if queue.range((account, mid, 0)..=(account, mid, u64::MAX))?.next().transpose()?.is_some()
                || self.tx.open_table(ACCEPTED_IMPORTS)?.get((account, mid))?.is_some()
                || self.has_message_id(account, mid)?
            {
                return Ok(false);
            }
        }
        let id = self.insert_import(folder, save_blob()?)?;
        if let Some(mid) = message_id {
            self.index_message_id(id, account, mid)?;
        }
        Ok(true)
    }

    fn insert_import(&mut self, folder: FolderId, blob: Hash) -> Result<u64> {
        let id = Store::next_id(self.tx, "import")?;
        self.tx.open_table(IMPORTS)?.insert(id, enc(&Import { folder, blob, tries: 0, why: None }).as_slice())?;
        Ok(id)
    }

    fn index_message_id(&mut self, id: u64, account: AccountId, mid: &str) -> Result<()> {
        self.tx.open_table(IMPORT_IDENTITIES)?.insert(id, (account, mid))?;
        self.tx.open_table(IMPORTS_BY_MESSAGE_ID)?.insert((account, mid, id), ())?;
        Ok(())
    }

    /// Index a previously queued row without modifying its retry state.
    pub fn index_import(&mut self, id: u64, mid: Option<&str>) -> Result<()> {
        if self.tx.open_table(UNINDEXED_IMPORTS)?.remove(id)?.is_none() {
            return Ok(());
        }
        if let Some(mid) = mid {
            let imports = self.tx.open_table(IMPORTS)?;
            let row = imports.get(id)?.ok_or(crate::Error::NotFound)?;
            let import: Import = dec(row.value())?;
            let account = self.store_folder(import.folder)?.account;
            self.index_message_id(id, account, mid)?;
        }
        Ok(())
    }

    /// Drop the upload from the queue and keep its Message-ID protected
    /// until the account indexes it.
    pub fn import_accepted(&mut self, id: u64) -> Result<()> {
        {
            let identities = self.tx.open_table(IMPORT_IDENTITIES)?;
            let identity = identities.get(id)?;
            if let Some(row) = identity {
                let (account, mid) = row.value();
                let account_exists = self.tx.open_table(crate::ACCOUNTS)?.get(account)?.is_some();
                if account_exists && !self.has_message_id(account, mid)? {
                    self.tx.open_table(ACCEPTED_IMPORTS)?.insert((account, mid), ())?;
                }
            }
        }
        self.import_done(id)
    }

    pub(crate) fn confirm_import_readback(&mut self, account: AccountId, mid: &str) -> Result<()> {
        let mut accepted = self.tx.open_table(ACCEPTED_IMPORTS)?;
        // Only retire the acceptance when the regular index can suppress a
        // repeat import on its own.
        if accepted.get((account, mid))?.is_some() && self.has_message_id(account, mid)? {
            accepted.remove((account, mid))?;
        }
        Ok(())
    }

    pub(crate) fn clear_accepted_imports(&mut self, account: AccountId) -> Result<()> {
        let mut accepted = self.tx.open_table(ACCEPTED_IMPORTS)?;
        let mut ids = Vec::new();
        for entry in accepted.range((account, "")..)? {
            let (key, _) = entry?;
            let (owner, mid) = key.value();
            if owner != account {
                break;
            }
            ids.push(mid.to_owned());
        }
        for mid in ids {
            accepted.remove((account, mid.as_str()))?;
        }
        Ok(())
    }

    /// Remove a queued row and release its Message-ID.
    pub fn import_done(&mut self, id: u64) -> Result<()> {
        self.tx.open_table(IMPORTS)?.remove(id)?;
        self.tx.open_table(UNINDEXED_IMPORTS)?.remove(id)?;
        let mut identities = self.tx.open_table(IMPORT_IDENTITIES)?;
        if let Some(row) = identities.remove(id)? {
            let (account, mid) = row.value();
            self.tx.open_table(IMPORTS_BY_MESSAGE_ID)?.remove((account, mid, id))?;
        }
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

    /// Try a given-up upload again from the start.
    pub fn import_retry(&mut self, id: u64) -> Result<()> {
        let mut t = self.tx.open_table(IMPORTS)?;
        let Some(mut i) = t.get(id)?.map(|v| dec::<Import>(v.value())).transpose()? else { return Ok(()) };
        i.tries = 0;
        t.insert(id, enc(&i).as_slice())?;
        Ok(())
    }

    /// Forget every waiting upload to `folder` (the person cancelled).
    pub fn cancel_imports(&mut self, folder: FolderId) -> Result<usize> {
        let mut ids = Vec::new();
        for row in self.tx.open_table(IMPORTS)?.iter()? {
            let (id, row) = row?;
            if dec::<Import>(row.value())?.folder == folder {
                ids.push(id.value());
            }
        }
        for &id in &ids {
            self.import_done(id)?;
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

    /// Queued rows that still need their headers parsed.
    pub fn unindexed_imports(&self, account: AccountId) -> Result<Vec<(u64, Hash)>> {
        let tx = self.db.begin_read()?;
        let imports = tx.open_table(IMPORTS)?;
        let folders = tx.open_table(FOLDERS)?;
        let mut out = Vec::new();
        for row in tx.open_table(UNINDEXED_IMPORTS)?.iter()? {
            let (id, _) = row?;
            let id = id.value();
            let row = imports.get(id)?.ok_or(crate::Error::NotFound)?;
            let import: Import = dec(row.value())?;
            if let Some(folder) = folders.get(import.folder)? {
                if dec::<Folder>(folder.value())?.account == account {
                    out.push((id, import.blob));
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_an_old_queue_marks_existing_rows_for_indexing() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let account = store.add_account("old@example.org").unwrap();
        let folder = store.folder_id(account, "Inbox", crate::Role::Inbox).unwrap();
        let blob = store.put_blob(b"Message-ID: <old@example.org>\r\n\r\n").unwrap();
        // Write the old queue row and clear the marker used by the new index.
        let id = store.write(|w| {
            w.tx.open_table(META)?.remove("import_identity_index")?;
            w.insert_import(folder, blob)
        }).unwrap();
        assert!(store.unindexed_imports(account).unwrap().is_empty());
        drop(store);

        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.unindexed_imports(account).unwrap(), [(id, blob)]);
        assert_eq!(store.imports().unwrap()[0].0, id);
        store.write(|w| w.index_import(id, Some("old@example.org"))).unwrap();
        drop(store);

        let store = Store::open(dir.path()).unwrap();
        assert!(store.unindexed_imports(account).unwrap().is_empty());
        assert!(!store.write(|w| w.queue_import_if_new(folder, Some("old@example.org"), || {
            panic!("a duplicate must not save another blob")
        })).unwrap());
    }
}
