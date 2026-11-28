//! Regression witness used only by the signed, disposable legacy sheet gate.
//! A legacy preview never opens Mail's normal reader or queues a mailbox action.

use mail_smime::legacy_wire::{self, Selection};
use mail_store::{flag, FolderId, Incoming, Role, Store};

const ACCOUNT: &str = legacy_wire::FIXTURE_ACCOUNT;
const SUBJECT: &str = "Synthetic legacy S/MIME message";

pub(super) struct PreviewFixture {
    store: Store,
    inbox: FolderId,
    pub(super) account: mail_store::AccountId,
}

impl PreviewFixture {
    pub(super) fn reopen(store: &Store) -> Result<Option<Self>, mail_store::Error> {
        if let Some(previous) = store.setting(0, atrium_mail::accounts::FIXTURE_ACCOUNT_ID)? {
            if !store.accounts()?.iter().any(|(id, _)| u64::from(*id) == previous) {
                return Ok(None);
            }
        }
        Self::seed(store).map(Some)
    }

    pub(super) fn seed(store: &Store) -> Result<Self, mail_store::Error> {
        let account = match store
            .accounts()?
            .into_iter()
            .find(|(_, account)| account.name == ACCOUNT)
        {
            Some((id, _)) => id,
            None => store.add_account(ACCOUNT)?,
        };
        let inbox = store.folder_id(account, "INBOX", Role::Inbox)?;
        let fixture = Self {
            store: store.clone(),
            inbox,
            account,
        };
        let present = store.list(inbox, 0, 3)?;
        for (uid, raw) in [
            (1, legacy_wire::fixture_message()),
            (2, legacy_wire::unsupported_fixture_message()),
        ] {
            if present.iter().any(|row| row.uid == uid) {
                continue;
            }
            let blob = store.put_blob(&raw)?;
            store.write(|w| {
                w.put(
                    inbox,
                    account,
                    uid,
                    Incoming {
                        server_id: None,
                        size: raw.len() as u64,
                        internal_date: None,
                        flags: 0,
                        blob,
                        full: true,
                        summary: mail_schema::Summary {
                            subject: SUBJECT.into(),
                            encrypted: uid == 1,
                            ..Default::default()
                        },
                    },
                )
            })?;
        }
        Ok(fixture)
    }

    pub(super) fn snapshot(&self, unsupported: bool) -> Option<Vec<u8>> {
        use mail_store::removal::Lifecycle;
        match self.store.account_lifecycle(self.account).ok()? {
            Some(Lifecycle::Enrolled(_)) => {}
            None if self.store.setting(0, atrium_mail::accounts::FIXTURE_ACCOUNT_ID).ok()?.is_none() => {}
            _ => return None,
        }
        let uid = if unsupported { 2 } else { 1 };
        let row = self
            .store
            .list(self.inbox, 0, 3)
            .ok()?
            .into_iter()
            .find(|row| row.uid == uid)?;
        let message = self.store.message(row.msg).ok()?;
        if !self
            .store
            .accounts()
            .ok()?
            .into_iter()
            .any(|(id, account)| id == message.account && account.name == ACCOUNT)
        {
            return None;
        }
        let raw = self.store.blob(&message.full?).ok()?;
        let mut snapshot = Selection::for_message(message.account, row.msg, &raw)
            .encode()
            .to_vec();
        snapshot.extend_from_slice(&raw);
        legacy_wire::received_selection(&snapshot)?;
        Some(snapshot)
    }

    pub(super) fn still_unread(&self) -> Result<bool, mail_store::Error> {
        let rows = self.store.list(self.inbox, 0, 3)?;
        Ok(rows.len() == 2
            && rows
                .iter()
                .all(|row| row.subject == SUBJECT && row.flags & flag::SEEN == 0)
            && self.store.ops_pending()? == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_selection_uses_the_stored_account_and_message() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let fixture = PreviewFixture::seed(&store).unwrap();
        for (unsupported, expected) in [
            (false, legacy_wire::fixture_message()),
            (true, legacy_wire::unsupported_fixture_message()),
        ] {
            let snapshot = fixture.snapshot(unsupported).unwrap();
            let (selected, bytes) = legacy_wire::received_selection(&snapshot).unwrap();
            assert_eq!(selected.account, 1);
            assert_eq!(selected.message, if unsupported { 2 } else { 1 });
            assert_eq!(bytes, expected);
        }
        store.write(|w| w.remove(fixture.inbox, 1)).unwrap();
        assert!(fixture.snapshot(false).is_none());
        let replacement = PreviewFixture::seed(&store).unwrap();
        assert!(replacement.still_unread().unwrap());
        assert!(replacement.snapshot(false).is_none());
    }

    #[test]
    fn preview_fixture_detects_a_seen_change() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let fixture = PreviewFixture::seed(&store).unwrap();
        assert!(fixture.still_unread().unwrap());
        store
            .write(|w| w.set_flags(fixture.inbox, 1, flag::SEEN))
            .unwrap();
        assert!(!fixture.still_unread().unwrap());
        let reopened = PreviewFixture::seed(&store).unwrap();
        assert_eq!(store.accounts().unwrap().len(), 1);
        assert!(!reopened.still_unread().unwrap());
    }
}
