//! Regression witness used only by the signed, disposable legacy sheet gate.
//! A legacy preview never opens Mail's normal reader or queues a mailbox action.

use mail_store::{flag, FolderId, Incoming, Role, Store};

const ACCOUNT: &str = "legacy-preview@fixture.invalid";
const SUBJECT: &str = "Synthetic legacy S/MIME message";

pub(super) struct PreviewFixture {
    store: Store,
    inbox: FolderId,
}

impl PreviewFixture {
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
        };
        if store.count(inbox)? != 0 {
            return Ok(fixture);
        }
        let raw = mail_compose::smime::enveloped(
            b"From: test@fixture.invalid\r\nTo: legacy-preview@fixture.invalid\r\nSubject: Synthetic legacy S/MIME message\r\nMIME-Version: 1.0\r\n",
            include_bytes!("../../../../mail-smime/tests/data/env.der"),
        );
        let blob = store.put_blob(&raw)?;
        store.write(|w| {
            w.put(
                inbox,
                account,
                1,
                Incoming {
                    server_id: None,
                    size: raw.len() as u64,
                    internal_date: None,
                    flags: 0,
                    blob,
                    full: true,
                    summary: mail_schema::Summary {
                        subject: SUBJECT.into(),
                        encrypted: true,
                        ..Default::default()
                    },
                },
            )
        })?;
        Ok(fixture)
    }

    pub(super) fn still_unread(&self) -> Result<bool, mail_store::Error> {
        let rows = self.store.list(self.inbox, 0, 2)?;
        Ok(
            matches!(rows.as_slice(), [row] if row.subject == SUBJECT && row.flags & flag::SEEN == 0)
                && self.store.ops_pending()? == 0,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
