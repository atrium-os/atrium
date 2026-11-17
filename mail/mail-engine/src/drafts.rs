//! Drafts (mail.md §9.1): the compose window saves here every few seconds
//! (a crash loses nothing) and the engine copies them to the server's
//! Drafts folder now and then, each copy replacing the last.

use std::io::{Read, Write};

use mail_compose::{Draft, Stamp};
use mail_store::{AccountId, OpKind, Role};

use crate::send::When;
use crate::{now, Engine, Error};

/// How often the compose window saves (seconds).
pub const SAVE_SECS: i64 = 3;
/// A draft is copied to the server once it has rested this long.
pub const SERVER_SECS: i64 = 30;

impl Engine {
    /// Keep `draft` as draft `id` (or a new one); its id.
    pub fn save_draft(&self, id: Option<u64>, account: AccountId, draft: &Draft) -> Result<u64, Error> {
        let b = postcard::to_stdvec(draft).map_err(|e| Error::Compose(e.to_string()))?;
        let h = self.store.put_blob(&b)?;
        Ok(self.store.write(|w| w.save_draft(id, account, h, now()))?)
    }

    /// The drafts being written in `account`, to open again (after a crash,
    /// or from the Drafts list).
    pub fn drafts(&self, account: AccountId) -> Result<Vec<(u64, Draft)>, Error> {
        let mut out = Vec::new();
        for (id, d) in self.store.drafts(account)? {
            let b = self.store.blob(&d.draft)?;
            let draft = postcard::from_bytes(&b).map_err(|_| Error::Store(mail_store::Error::Corrupt("draft")))?;
            out.push((id, draft));
        }
        Ok(out)
    }

    /// Throw draft `id` away (its server copies go too).
    pub fn discard_draft(&self, id: u64) -> Result<(), Error> {
        Ok(self.store.write(|w| w.drop_draft(id))?)
    }

    /// Send draft `id` as `draft` (what the window holds now): it goes to
    /// the outbox and the draft is done. The send's id, for Undo, which
    /// gives the draft back to save as a new one.
    pub fn send_draft(&self, id: Option<u64>, account: AccountId, draft: &Draft, when: When) -> Result<u64, Error> {
        self.send_draft_protected(id, account, draft, when, crate::smime::Protect::default())
    }

    /// [`Engine::send_draft`], signed and/or encrypted (§10.1).
    pub fn send_draft_protected(&self, id: Option<u64>, account: AccountId, draft: &Draft, when: When, protect: crate::smime::Protect) -> Result<u64, Error> {
        let send = self.send_protected(account, draft, when, protect)?;
        if let Some(id) = id {
            self.discard_draft(id)?;
        }
        Ok(send)
    }

    /// Bring the server's Drafts up to date for `account` on `imap`: copy
    /// each draft that rested `rest` seconds and changed since its last
    /// copy, and queue the deletion of copies it replaced (sync makes them
    /// once it has seen them). How many were copied.
    pub fn copy_drafts<S: Read + Write>(&self, account: AccountId, imap: &mut mail_sync::Session<S>, rest: i64) -> Result<usize, Error> {
        let Some((folder, f)) = self.store.folders(account)?.into_iter().find(|(_, f)| f.role == Role::Drafts && !f.hidden) else {
            return Ok(0);
        };
        let t = now();
        let mut copied = 0;
        for (id, d) in self.store.all_drafts()?.into_iter().filter(|(_, d)| d.account == account) {
            if d.dirty() && d.updated + rest <= t {
                let b = self.store.blob(&d.draft)?;
                let draft: Draft = postcard::from_bytes(&b).map_err(|_| Error::Store(mail_store::Error::Corrupt("draft")))?;
                let w = mail_compose::write_draft(&draft, &Stamp { date: d.updated, random: crate::send::random16() })
                    .map_err(|e| Error::Compose(e.to_string()))?;
                let uid = imap.append(&f.name, &["\\Draft", "\\Seen"], &w.bytes).map_err(|e| Error::Sync(e.to_string()))?;
                self.store.write(|wr| wr.draft_copied(id, d.draft, uid))?;
                copied += 1;
            }
        }
        // Replaced copies: delete those sync has seen; forget those the
        // server no longer has.
        let f = self.store.folder(folder)?;
        for (id, d) in self.store.all_drafts()?.into_iter().filter(|(_, d)| d.account == account && !d.stale.is_empty()) {
            let mut done = Vec::new();
            for &uid in &d.stale {
                if self.store.at(folder, uid)?.is_some() {
                    match self.store.write(|w| w.queue(folder, uid, OpKind::Expunge, t)) {
                        Ok(_) | Err(mail_store::Error::NotFound) => done.push(uid),
                        Err(e) => return Err(e.into()),
                    }
                } else if uid >= f.synced_from && uid < f.uidnext {
                    // The last sync covered it and it isn't there: gone.
                    done.push(uid);
                }
            }
            if !done.is_empty() {
                self.store.write(|w| w.stale_done(id, &done))?;
            }
        }
        Ok(copied)
    }
}
