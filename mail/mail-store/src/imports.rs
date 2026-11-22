//! Messages imported from an mbox file (mail.md §6.5), waiting to be
//! uploaded to their folder on the account's server (IMAP `APPEND`, JMAP
//! `Email/import`). Sync then brings them in like any other mail, so the
//! server and every other client have them too.

use redb::{MultimapTableDefinition, ReadableMultimapTable, ReadableTable, TableDefinition, WriteTransaction};
use serde::{Deserialize, Serialize};

use crate::{dec, enc, AccountId, Folder, FolderId, Hash, Result, Store, Writer, FOLDERS, FOLDER_NAMES, META};

/// id → postcard [`Import`].
pub(crate) const IMPORTS: TableDefinition<u64, &[u8]> = TableDefinition::new("imports");

const UNINDEXED_IMPORTS: TableDefinition<u64, ()> = TableDefinition::new("unindexed_imports");
const IMPORT_IDENTITIES: TableDefinition<u64, (AccountId, &str)> = TableDefinition::new("import_identities");
const IMPORTS_BY_MESSAGE_ID: TableDefinition<(AccountId, &str, u64), ()> =
    TableDefinition::new("imports_by_message_id");

// An accepted upload protects its Message-ID until mailbox sync indexes it.
const ACCEPTED_IMPORTS: TableDefinition<(AccountId, &str), ()> = TableDefinition::new("accepted_imports");

// The original header is an account-scoped alias of the accepted JMAP object.
const JMAP_IMPORTS_BY_ORIGINAL: MultimapTableDefinition<(AccountId, &str), &str> =
    MultimapTableDefinition::new("jmap_imports_by_original");
const JMAP_IMPORTS_BY_OBJECT: MultimapTableDefinition<(AccountId, &str), &str> =
    MultimapTableDefinition::new("jmap_imports_by_object");

type ImapLocation = (FolderId, u32, u32);
const IMAP_IMPORTS_BY_ORIGINAL: MultimapTableDefinition<(AccountId, &str), ImapLocation> =
    MultimapTableDefinition::new("imap_imports_by_original");
const IMAP_IMPORTS_PENDING: MultimapTableDefinition<ImapLocation, &str> =
    MultimapTableDefinition::new("imap_imports_pending");

// Once an exact APPENDUID readback supplies a stable object ID, the
// alias is independent of the mailbox and UID it was first seen in.
const IMAP_IMPORT_OBJECTS: MultimapTableDefinition<(AccountId, &str), &str> =
    MultimapTableDefinition::new("imap_import_objects");

const IMAP_IMPORTS_BY_OBJECT: MultimapTableDefinition<(AccountId, &str), &str> =
    MultimapTableDefinition::new("imap_imports_by_object");
const IMAP_IMPORT_CURSOR: &str = "imap-import-presence";

// Validate the worker's scope in both the preflight read and the transaction
// which retires the alias. FOLDER_NAMES includes folders hidden by later LISTs.
fn imap_scope_complete(
    names: &impl ReadableTable<(AccountId, &'static str), FolderId>,
    account: AccountId,
    mailboxes: &[String],
) -> Result<bool> {
    if mailboxes.len() > 192 || mailboxes.iter().any(|name| {
        name.is_empty() || name.len() > 1024 || (name.eq_ignore_ascii_case("INBOX") && name != "INBOX")
    }) {
        return Ok(false);
    }
    let scope: std::collections::BTreeSet<_> = mailboxes.iter().map(String::as_str).collect();
    if !scope.contains("INBOX") || scope.len() != mailboxes.len() {
        return Ok(false);
    }
    for row in names.range((account, "")..)? {
        let (key, _) = row?;
        let (owner, name) = key.value();
        if owner != account {
            break;
        }
        let canonical = if name.eq_ignore_ascii_case("INBOX") { "INBOX" } else { name };
        if !scope.contains(canonical) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// A protocol-issued identity for a successful upload.
pub enum ImportIdentity {
    Jmap(String),
    Imap { validity: u32, uid: u32 },
}

pub(crate) fn open(tx: &WriteTransaction) -> Result<()> {
    let imports = tx.open_table(IMPORTS)?;
    let mut unindexed = tx.open_table(UNINDEXED_IMPORTS)?;
    tx.open_table(IMPORT_IDENTITIES)?;
    tx.open_table(IMPORTS_BY_MESSAGE_ID)?;
    tx.open_table(ACCEPTED_IMPORTS)?;
    tx.open_multimap_table(JMAP_IMPORTS_BY_ORIGINAL)?;
    tx.open_multimap_table(JMAP_IMPORTS_BY_OBJECT)?;
    tx.open_multimap_table(IMAP_IMPORTS_BY_ORIGINAL)?;
    let mut pending_imap = tx.open_multimap_table(IMAP_IMPORTS_PENDING)?;
    let imap_objects = tx.open_multimap_table(IMAP_IMPORT_OBJECTS)?;
    let mut by_object = tx.open_multimap_table(IMAP_IMPORTS_BY_OBJECT)?;
    let mut meta = tx.open_table(META)?;
    if meta.get("import_identity_index")?.is_none() {
        for row in imports.iter()? {
            let (id, _) = row?;
            unindexed.insert(id.value(), ())?;
        }
        meta.insert("import_identity_index", 1)?;
    }
    if meta.get("imap_import_object_index")?.is_none() {
        // Earlier builds kept the location alias after rewritten readback
        // but discarded its pending entry. Allow a later exact fetch to bind it.
        let originals = tx.open_multimap_table(IMAP_IMPORTS_BY_ORIGINAL)?;
        for row in originals.iter()? {
            let (key, locations) = row?;
            let (_, mid) = key.value();
            for location in locations {
                pending_imap.insert(location?.value(), mid)?;
            }
        }
        meta.insert("imap_import_object_index", 1)?;
    }
    if meta.get("imap_import_reverse_index")?.is_none() {
        for row in imap_objects.iter()? {
            let (key, objects) = row?;
            let (account, mid) = key.value();
            for object in objects {
                by_object.insert((account, object?.value()), mid)?;
            }
        }
        meta.insert("imap_import_reverse_index", 1)?;
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
                || self.tx.open_multimap_table(JMAP_IMPORTS_BY_ORIGINAL)?.get((account, mid))?.next().transpose()?.is_some()
                || self.tx.open_multimap_table(IMAP_IMPORTS_BY_ORIGINAL)?.get((account, mid))?.next().transpose()?.is_some()
                || self.tx.open_multimap_table(IMAP_IMPORT_OBJECTS)?.get((account, mid))?.next().transpose()?.is_some()
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
    /// until the account indexes it, or until JMAP reports its object deleted
    /// when the upload returned an Email ID.
    pub fn import_accepted(&mut self, id: u64, identity: Option<ImportIdentity>) -> Result<()> {
        {
            let identities = self.tx.open_table(IMPORT_IDENTITIES)?;
            let pending = identities.get(id)?;
            if let Some(row) = pending {
                let (account, mid) = row.value();
                let account_exists = self.tx.open_table(crate::ACCOUNTS)?.get(account)?.is_some();
                if account_exists {
                    match identity {
                        Some(ImportIdentity::Jmap(object)) => {
                            self.tx.open_multimap_table(JMAP_IMPORTS_BY_ORIGINAL)?.insert((account, mid), object.as_str())?;
                            self.tx.open_multimap_table(JMAP_IMPORTS_BY_OBJECT)?.insert((account, object.as_str()), mid)?;
                        }
                        Some(ImportIdentity::Imap { validity, uid }) if validity != 0 && uid != 0 => {
                            let imports = self.tx.open_table(IMPORTS)?;
                            let upload: Import = dec(imports.get(id)?.ok_or(crate::Error::NotFound)?.value())?;
                            let location = (upload.folder, validity, uid);
                            self.tx.open_multimap_table(IMAP_IMPORTS_BY_ORIGINAL)?.insert((account, mid), location)?;
                            self.tx.open_multimap_table(IMAP_IMPORTS_PENDING)?.insert(location, mid)?;
                        }
                        _ if !self.has_message_id(account, mid)? => {
                            self.tx.open_table(ACCEPTED_IMPORTS)?.insert((account, mid), ())?;
                        }
                        _ => {}
                    }
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

    /// Bind a rewritten import to the object returned for its exact APPENDUID.
    pub fn confirm_imap_import(
        &mut self, account: AccountId, location: ImapLocation, observed: Option<&str>, object: Option<&str>,
    ) -> Result<()> {
        let mut pending = self.tx.open_multimap_table(IMAP_IMPORTS_PENDING)?;
        let rows = pending.get(location)?.map(|mid| Ok(mid?.value().to_owned())).collect::<Result<Vec<_>>>()?;
        if rows.is_empty() {
            return Ok(());
        }
        let mut originals = self.tx.open_multimap_table(IMAP_IMPORTS_BY_ORIGINAL)?;
        let mut objects = self.tx.open_multimap_table(IMAP_IMPORT_OBJECTS)?;
        for original in rows {
            let mid = original.as_str();
            if observed != Some(mid) || !self.has_message_id(account, mid)? {
                let Some(object) = object else { continue };
                objects.insert((account, mid), object)?;
                self.tx.open_multimap_table(IMAP_IMPORTS_BY_OBJECT)?.insert((account, object), mid)?;
            }
            originals.remove((account, mid), location)?;
            pending.remove(location, mid)?;
        }
        Ok(())
    }

    /// Retire an alias only if the proof covers the scope at commit time.
    pub fn finish_imap_import_probe(&mut self, account: AccountId, object: &str, proof: Option<&[String]>) -> Result<()> {
        if self.tx.open_table(crate::ACCOUNTS)?.get(account)?.is_none() {
            return Ok(());
        }
        let complete = match proof {
            Some(mailboxes) => imap_scope_complete(&self.tx.open_table(FOLDER_NAMES)?, account, mailboxes)?,
            None => false,
        };
        if complete {
            let mut originals = self.tx.open_multimap_table(IMAP_IMPORT_OBJECTS)?;
            let mut by_object = self.tx.open_multimap_table(IMAP_IMPORTS_BY_OBJECT)?;
            for original in by_object.remove_all((account, object))? {
                originals.remove((account, original?.value()), object)?;
            }
        }
        self.set_sync_state(account, IMAP_IMPORT_CURSOR, Some(object))
    }

    /// Release aliases only for objects the JMAP server reported destroyed.
    pub fn forget_jmap_imports(&mut self, account: AccountId, objects: &[String]) -> Result<()> {
        let mut originals = self.tx.open_multimap_table(JMAP_IMPORTS_BY_ORIGINAL)?;
        let mut by_object = self.tx.open_multimap_table(JMAP_IMPORTS_BY_OBJECT)?;
        for object in objects {
            for mid in by_object.remove_all((account, object.as_str()))? {
                originals.remove((account, mid?.value()), object.as_str())?;
            }
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
        let mut objects = Vec::new();
        for entry in self.tx.open_multimap_table(JMAP_IMPORTS_BY_OBJECT)?.range((account, "")..)? {
            let (key, _) = entry?;
            let (owner, object) = key.value();
            if owner != account {
                break;
            }
            objects.push(object.to_owned());
        }
        self.forget_jmap_imports(account, &objects)?;
        let mut originals = self.tx.open_multimap_table(IMAP_IMPORTS_BY_ORIGINAL)?;
        let mut pending = self.tx.open_multimap_table(IMAP_IMPORTS_PENDING)?;
        let mut mids = Vec::new();
        for entry in originals.range((account, "")..)? {
            let (key, _) = entry?;
            let (owner, mid) = key.value();
            if owner != account {
                break;
            }
            mids.push(mid.to_owned());
        }
        for mid in mids {
            for location in originals.remove_all((account, mid.as_str()))? {
                pending.remove(location?.value(), mid.as_str())?;
            }
        }
        let mut objects = self.tx.open_multimap_table(IMAP_IMPORT_OBJECTS)?;
        let mut mids = Vec::new();
        for row in objects.range((account, "")..)? {
            let (key, _) = row?;
            let (owner, mid) = key.value();
            if owner != account {
                break;
            }
            mids.push(mid.to_owned());
        }
        let mut by_object = self.tx.open_multimap_table(IMAP_IMPORTS_BY_OBJECT)?;
        for mid in mids {
            for object in objects.remove_all((account, mid.as_str()))? {
                by_object.remove((account, object?.value()), mid.as_str())?;
            }
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

    /// Whether the probe's visible scope covers the account's known folders.
    pub fn imap_import_scope_complete(&self, account: AccountId, mailboxes: &[String]) -> Result<bool> {
        let tx = self.db.begin_read()?;
        imap_scope_complete(&tx.open_table(FOLDER_NAMES)?, account, mailboxes)
    }

    /// Rotate through stable IMAP objects using a persisted, account-scoped cursor.
    pub fn imap_import_probe(&self, account: AccountId) -> Result<Option<String>> {
        use std::ops::Bound::{Excluded, Unbounded};

        let cursor = self.sync_state(account, IMAP_IMPORT_CURSOR)?.unwrap_or_default();
        let tx = self.db.begin_read()?;
        let table = tx.open_multimap_table(IMAP_IMPORTS_BY_OBJECT)?;
        for after in [cursor.as_str(), ""] {
            if let Some(row) = table.range((Excluded((account, after)), Unbounded))?.next() {
                let (key, _) = row?;
                let (owner, object) = key.value();
                if owner == account {
                    return Ok(Some(object.to_owned()));
                }
            }
        }
        Ok(None)
    }

    /// A bounded page of tracked JMAP objects for one account, in ID order.
    pub fn jmap_import_ids(&self, account: AccountId, after: &str, limit: usize) -> Result<Vec<String>> {
        use std::ops::Bound::{Excluded, Unbounded};

        let tx = self.db.begin_read()?;
        let table = tx.open_multimap_table(JMAP_IMPORTS_BY_OBJECT)?;
        let mut ids = Vec::new();
        for entry in table.range((Excluded((account, after)), Unbounded))?.take(limit) {
            let (key, _) = entry?;
            let (owner, id) = key.value();
            if owner != account {
                break;
            }
            ids.push(id.to_owned());
        }
        Ok(ids)
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

    #[test]
    fn imap_legacy_location_promotes_only_from_exact_readback() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let account = store.add_account("old@example.org").unwrap();
        let folder = store.folder_id(account, "INBOX", crate::Role::Inbox).unwrap();
        let location = (folder, 7, 9);
        let mid = "original@example.org";
        let object = "emailid:Mstable";
        store.write(|w| {
            w.tx.open_table(META)?.remove("imap_import_object_index")?;
            w.tx.open_multimap_table(IMAP_IMPORTS_BY_ORIGINAL)?.insert((account, mid), location)?;
            Ok(())
        }).unwrap();
        drop(store);

        let store = Store::open(dir.path()).unwrap();
        store.write(|w| {
            w.confirm_imap_import(account, (folder, 8, 9), Some("replacement@example.org"), Some("emailid:Mwrong"))?;
            w.confirm_imap_import(account, location, Some("replacement@example.org"), None)
        }).unwrap();
        {
            let tx = store.db.begin_read().unwrap();
            assert!(tx.open_multimap_table(IMAP_IMPORT_OBJECTS).unwrap().get((account, mid)).unwrap().next().is_none());
            assert!(tx.open_multimap_table(IMAP_IMPORTS_PENDING).unwrap().get(location).unwrap().next().is_some());
        }
        store.write(|w| {
            w.confirm_imap_import(account, location, Some("replacement@example.org"), Some(object))?;
            // Simulate the first stable-object schema, without its reverse index.
            w.tx.open_multimap_table(IMAP_IMPORTS_BY_OBJECT)?.remove_all((account, object))?;
            w.tx.open_table(META)?.remove("imap_import_reverse_index")?;
            Ok(())
        }).unwrap();
        drop(store);

        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.imap_import_probe(account).unwrap().as_deref(), Some(object));
        {
            let tx = store.db.begin_read().unwrap();
            let aliases = tx.open_multimap_table(IMAP_IMPORT_OBJECTS).unwrap();
            let actual: Vec<_> = aliases.get((account, mid)).unwrap().map(|value| value.unwrap().value().to_owned()).collect();
            assert_eq!(actual, [object]);
            assert!(tx.open_multimap_table(IMAP_IMPORTS_PENDING).unwrap().get(location).unwrap().next().is_none());
            assert!(tx.open_multimap_table(IMAP_IMPORTS_BY_ORIGINAL).unwrap().get((account, mid)).unwrap().next().is_none());
        }
        assert!(!store.write(|w| w.queue_import_if_new(folder, Some(mid), || panic!("object alias must guard the import"))).unwrap());
        store.write(|w| w.remove_account(account)).unwrap();
        let tx = store.db.begin_read().unwrap();
        assert!(tx.open_multimap_table(IMAP_IMPORT_OBJECTS).unwrap().get((account, mid)).unwrap().next().is_none());
    }

    #[test]
    fn imap_absence_requires_a_complete_account_scope_at_commit() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let account = store.add_account("ada@example.org").unwrap();
        let other = store.add_account("bea@example.org").unwrap();
        let inbox = store.folder_id(account, "inbox", crate::Role::Inbox).unwrap();
        store.folder_id(other, "OtherAccount", crate::Role::Other).unwrap();
        let mid = "original@example.org";
        let object = "emailid:A";
        store.write(|w| {
            w.tx.open_multimap_table(IMAP_IMPORT_OBJECTS)?.insert((account, mid), object)?;
            w.tx.open_multimap_table(IMAP_IMPORTS_BY_OBJECT)?.insert((account, object), mid)?;
            Ok(())
        }).unwrap();

        let proof = vec!["INBOX".to_owned()];
        assert!(store.imap_import_scope_complete(account, &proof).unwrap());
        store.folder_id(account, "NewMailbox", crate::Role::Other).unwrap();
        let complete = vec!["INBOX".to_owned(), "NewMailbox".to_owned()];
        let duplicate = vec!["INBOX".to_owned(), "NewMailbox".to_owned(), "NewMailbox".to_owned()];
        for incomplete in [&proof, &duplicate] {
            store.write(|w| w.finish_imap_import_probe(account, object, Some(incomplete))).unwrap();
            assert_eq!(store.imap_import_probe(account).unwrap().as_deref(), Some(object));
        }
        store.write(|w| w.finish_imap_import_probe(account, object, Some(&complete))).unwrap();
        assert!(store.imap_import_probe(account).unwrap().is_none());
        let blob = store.put_blob(b"Subject: reimport").unwrap();
        assert!(store.write(|w| w.queue_import_if_new(inbox, Some(mid), || Ok(blob))).unwrap());
        store.write(|w| w.remove_account(account)).unwrap();
        store.write(|w| w.finish_imap_import_probe(account, object, Some(&complete))).unwrap();
        assert!(store.sync_state(account, IMAP_IMPORT_CURSOR).unwrap().is_none());
    }
}
