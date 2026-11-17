//! Drafts the person is writing (mail.md §9.1): kept here every few seconds
//! so a crash loses nothing, and copied to the server's Drafts folder now
//! and then. Each copy replaces the last; copies that are no longer the
//! draft (`stale`) are deleted on the server once sync has seen them.

use redb::{ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::{dec, enc, AccountId, Error, Hash, Result, Store, Writer};

pub(crate) const DRAFTS: TableDefinition<u64, &[u8]> = TableDefinition::new("drafts");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalDraft {
    pub account: AccountId,
    /// The draft (the compose window's fields, as the engine encodes them).
    pub draft: Hash,
    pub updated: i64,
    /// The draft on the server: its blob when last copied there, and its
    /// UID in Drafts (when the server said).
    pub on_server: Option<Hash>,
    pub server_uid: Option<u32>,
    /// Older server copies (UIDs in Drafts) to delete.
    pub stale: Vec<u32>,
    /// Sent or thrown away: kept only until the stale copies are gone.
    pub gone: bool,
}

impl LocalDraft {
    /// The server's copy is behind.
    pub fn dirty(&self) -> bool {
        !self.gone && self.on_server != Some(self.draft)
    }
}

impl Writer<'_> {
    /// Keep `draft` (a blob) as draft `id`, or as a new draft; its id.
    /// Saving the same bytes again changes nothing.
    pub fn save_draft(&mut self, id: Option<u64>, account: AccountId, draft: Hash, now: i64) -> Result<u64> {
        let mut t = self.tx.open_table(DRAFTS)?;
        if let Some(id) = id {
            let cur = t.get(id)?.map(|v| dec::<LocalDraft>(v.value())).transpose()?;
            if let Some(mut d) = cur {
                if d.gone || d.account != account {
                    return Err(Error::NotFound);
                }
                if d.draft != draft {
                    d.draft = draft;
                    d.updated = now;
                    t.insert(id, enc(&d).as_slice())?;
                }
                return Ok(id);
            }
        }
        drop(t);
        let id = Store::next_id(self.tx, "draft")?;
        let d = LocalDraft { account, draft, updated: now, on_server: None, server_uid: None, stale: Vec::new(), gone: false };
        self.tx.open_table(DRAFTS)?.insert(id, enc(&d).as_slice())?;
        Ok(id)
    }

    /// The draft was sent or thrown away: its server copies go too.
    pub fn drop_draft(&mut self, id: u64) -> Result<()> {
        let mut t = self.tx.open_table(DRAFTS)?;
        let Some(mut d) = t.get(id)?.map(|v| dec::<LocalDraft>(v.value())).transpose()? else { return Ok(()) };
        d.stale.extend(d.server_uid.take());
        d.gone = true;
        if d.stale.is_empty() && d.on_server.is_none() {
            t.remove(id)?;
        } else {
            t.insert(id, enc(&d).as_slice())?;
        }
        Ok(())
    }

    /// The server now has `copied` (the draft's blob then) as `uid` in
    /// Drafts (`None`: it didn't say); the previous copy becomes stale.
    pub fn draft_copied(&mut self, id: u64, copied: Hash, uid: Option<u32>) -> Result<()> {
        let mut t = self.tx.open_table(DRAFTS)?;
        let Some(mut d) = t.get(id)?.map(|v| dec::<LocalDraft>(v.value())).transpose()? else { return Err(Error::NotFound) };
        d.stale.extend(d.server_uid.take());
        d.server_uid = uid;
        d.on_server = Some(copied);
        if d.gone {
            // Thrown away while copying: that copy goes too.
            d.stale.extend(d.server_uid.take());
        }
        t.insert(id, enc(&d).as_slice())?;
        Ok(())
    }

    /// These stale copies have been taken care of (deleted, or their
    /// deletion queued); a gone draft with none left is forgotten.
    pub fn stale_done(&mut self, id: u64, uids: &[u32]) -> Result<()> {
        let mut t = self.tx.open_table(DRAFTS)?;
        let Some(mut d) = t.get(id)?.map(|v| dec::<LocalDraft>(v.value())).transpose()? else { return Ok(()) };
        d.stale.retain(|u| !uids.contains(u));
        if d.gone && d.stale.is_empty() {
            t.remove(id)?;
        } else {
            t.insert(id, enc(&d).as_slice())?;
        }
        Ok(())
    }
}

impl Store {
    /// Every draft, gone ones included (their stale copies still to
    /// delete), oldest first.
    pub fn all_drafts(&self) -> Result<Vec<(u64, LocalDraft)>> {
        let tx = self.db.begin_read()?;
        let mut out = Vec::new();
        for e in tx.open_table(DRAFTS)?.iter()? {
            let (k, v) = e?;
            out.push((k.value(), dec(v.value())?));
        }
        Ok(out)
    }

    /// The drafts being written in `account` (to open again after a crash).
    pub fn drafts(&self, account: AccountId) -> Result<Vec<(u64, LocalDraft)>> {
        Ok(self.all_drafts()?.into_iter().filter(|(_, d)| d.account == account && !d.gone).collect())
    }

    pub fn draft(&self, id: u64) -> Result<LocalDraft> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(DRAFTS)?;
        let v = t.get(id)?.ok_or(Error::NotFound)?;
        dec(v.value())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drafts() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let id = s.write(|w| w.save_draft(None, 1, [1; 32], 100)).unwrap();
        assert_eq!(s.write(|w| w.save_draft(Some(id), 1, [1; 32], 200)).unwrap(), id);
        assert_eq!(s.draft(id).unwrap().updated, 100, "same bytes: unchanged");
        assert!(s.draft(id).unwrap().dirty());
        s.write(|w| w.draft_copied(id, [1; 32], Some(7))).unwrap();
        assert!(!s.draft(id).unwrap().dirty());
        s.write(|w| w.save_draft(Some(id), 1, [2; 32], 300).map(|_| ())).unwrap();
        assert!(s.draft(id).unwrap().dirty());
        s.write(|w| w.draft_copied(id, [2; 32], Some(8))).unwrap();
        assert_eq!(s.draft(id).unwrap().stale, [7]);
        // Sent: every copy goes, then the draft is forgotten.
        s.write(|w| w.drop_draft(id)).unwrap();
        assert!(s.drafts(1).unwrap().is_empty());
        assert_eq!(s.draft(id).unwrap().stale, [7, 8]);
        assert!(s.write(|w| w.save_draft(Some(id), 1, [3; 32], 400)).is_err());
        s.write(|w| w.stale_done(id, &[7, 8])).unwrap();
        assert!(s.all_drafts().unwrap().is_empty());
        // Never copied: forgotten at once.
        let id = s.write(|w| w.save_draft(None, 1, [4; 32], 500)).unwrap();
        s.write(|w| w.drop_draft(id)).unwrap();
        assert!(s.all_drafts().unwrap().is_empty());
    }
}
