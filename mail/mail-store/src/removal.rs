//! A local account keeps the Arca instance it was enrolled with. Removing
//! that instance and cleaning up its endpoint items can span Mail restarts.

use redb::{ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::{dec, enc, AccountConfig, AccountId, Error, Result, Store, Writer};

pub(crate) const REMOVALS: TableDefinition<u32, &[u8]> =
    TableDefinition::new("legacy_account_lifecycle");

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Lifecycle {
    Enrolled([u8; 16]),
    Removing([u8; 16]),
    Credentials {
        config: AccountConfig,
        sieve: Option<(crate::Server, [u8; 16])>,
    },
}

impl Store {
    pub fn account_lifecycle(&self, id: AccountId) -> Result<Option<Lifecycle>> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(REMOVALS)?;
        let record = table.get(id)?;
        record.map(|r| dec(r.value())).transpose()
    }

    pub fn account_lifecycles(&self) -> Result<Vec<(AccountId, Lifecycle)>> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(REMOVALS)?;
        table
            .iter()?
            .map(|entry| {
                let (key, value) = entry?;
                Ok((key.value(), dec(value.value())?))
            })
            .collect()
    }
}

impl Writer<'_> {
    pub fn bind_legacy_account(&mut self, id: AccountId, instance: [u8; 16]) -> Result<()> {
        if self.tx.open_table(crate::ACCOUNTS)?.get(id)?.is_none() {
            return Err(Error::NotFound);
        }
        let mut table = self.tx.open_table(REMOVALS)?;
        let old: Option<Lifecycle> = table.get(id)?.map(|v| dec(v.value())).transpose()?;
        let new = Lifecycle::Enrolled(instance);
        if old.is_some_and(|value| value != new) {
            return Err(Error::Corrupt("account instance changed"));
        }
        table.insert(id, enc(&new).as_slice())?;
        Ok(())
    }

    pub fn begin_legacy_removal(&mut self, id: AccountId) -> Result<Option<[u8; 16]>> {
        let mut table = self.tx.open_table(REMOVALS)?;
        let old: Option<Lifecycle> = table.get(id)?.map(|v| dec(v.value())).transpose()?;
        match old {
            Some(Lifecycle::Enrolled(instance) | Lifecycle::Removing(instance)) => {
                table.insert(id, enc(&Lifecycle::Removing(instance)).as_slice())?;
                Ok(Some(instance))
            }
            Some(Lifecycle::Credentials { .. }) => Err(Error::NotFound),
            None => Ok(None),
        }
    }

    /// The acknowledgment and the intent must name the same generation.
    /// Snapshot cleanup references atomically with deleting the account.
    pub fn finish_legacy_removal(&mut self, id: AccountId, acknowledged: [u8; 16]) -> Result<()> {
        let old = self
            .tx
            .open_table(REMOVALS)?
            .get(id)?
            .map(|v| dec(v.value()))
            .transpose()?;
        if matches!(old, None | Some(Lifecycle::Credentials { .. }))
            && self.tx.open_table(crate::ACCOUNTS)?.get(id)?.is_none()
        {
            // Another retry already consumed this intent. Local IDs are
            // allocated monotonically and a re-add gets its own ID.
            return Ok(());
        }
        if old != Some(Lifecycle::Removing(acknowledged)) {
            return Err(Error::Corrupt("account removal acknowledgment"));
        }
        let config = self
            .tx
            .open_table(crate::config::CONFIG)?
            .get(id)?
            .map(|v| dec(v.value()))
            .transpose()?;
        let sieve = self
            .tx
            .open_table(crate::sieve::SIEVE)?
            .get(id)?
            .map(|v| dec::<crate::sieve::SieveAccount>(v.value()))
            .transpose()?;
        self.tx.open_table(REMOVALS)?.remove(id)?;
        self.remove_account(id)?;
        if let Some(config) = config {
            let cleanup = Lifecycle::Credentials {
                config,
                sieve: sieve.map(|s| (s.server, s.item)),
            };
            self.tx
                .open_table(REMOVALS)?
                .insert(id, enc(&cleanup).as_slice())?;
        }
        Ok(())
    }

    pub fn finish_credential_cleanup(&mut self, id: AccountId) -> Result<()> {
        let mut table = self.tx.open_table(REMOVALS)?;
        let old: Option<Lifecycle> = table.get(id)?.map(|v| dec(v.value())).transpose()?;
        match old {
            Some(Lifecycle::Credentials { .. }) => {
                table.remove(id)?;
                Ok(())
            }
            None => Ok(()),
            _ => Err(Error::Corrupt("unexpected account cleanup")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Protocol, Reached};

    #[test]
    fn removal_resumes_on_both_sides_of_the_arca_acknowledgment() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = AccountConfig {
            name: "Fixture".into(),
            address: "login@fixture.invalid".into(),
            protocol: Protocol::Jmap {
                session: "https://jmap.fixture.invalid/session".into(),
            },
            incoming_item: Some([3; 16]),
            outgoing_item: None,
            oauth: None,
            reached: Reached::Never,
        };
        let instance = [7; 16];
        let store = Store::open(dir.path()).unwrap();
        let id = store.add_configured(&cfg).unwrap();
        store
            .write(|w| w.bind_legacy_account(id, instance))
            .unwrap();
        assert!(store.write(|w| w.remove_account(id)).is_err());
        assert_eq!(
            store.write(|w| w.begin_legacy_removal(id)).unwrap(),
            Some(instance)
        );
        drop(store);

        let store = Store::open(dir.path()).unwrap();
        assert_eq!(
            store.account_lifecycles().unwrap(),
            [(id, Lifecycle::Removing(instance))]
        );
        assert!(store
            .write(|w| w.finish_legacy_removal(id, [9; 16]))
            .is_err());
        assert!(store.write(|w| w.bind_legacy_account(id, [9; 16])).is_err());
        assert_eq!(store.account_config(id).unwrap(), Some(cfg.clone()));
        store
            .write(|w| w.finish_legacy_removal(id, instance))
            .unwrap();
        drop(store);

        let store = Store::open(dir.path()).unwrap();
        assert!(store.accounts().unwrap().is_empty());
        assert_eq!(
            store.account_lifecycle(id).unwrap(),
            Some(Lifecycle::Credentials {
                config: cfg.clone(),
                sieve: None
            })
        );
        let replacement = store.add_configured(&cfg).unwrap();
        assert_ne!(replacement, id);
        store
            .write(|w| w.bind_legacy_account(replacement, [9; 16]))
            .unwrap();
        store.write(|w| w.finish_credential_cleanup(id)).unwrap();
        assert_eq!(
            store.account_lifecycles().unwrap(),
            [(replacement, Lifecycle::Enrolled([9; 16]))]
        );
    }
}
