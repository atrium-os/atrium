//! Expiring DKIM and DMARC TXT answers in the instance's own Mail index.

use std::time::Duration;

use redb::{ReadableTable, ReadableTableMetadata, TableDefinition};

use crate::{Error, Result, Store, Writer};

pub(crate) const DNS_TXT: TableDefinition<&str, (i64, &str)> = TableDefinition::new("dns_txt");

pub const KEEP_FOUND: Duration = Duration::from_secs(6 * 3600);
pub const KEEP_NONE: Duration = Duration::from_secs(3600);
pub const CACHE_MAX: usize = 4096;
const TXT_MAX: usize = 4096;

fn expires(observed: i64, answer: &str) -> i64 {
    let keep = if answer.is_empty() {
        KEEP_NONE
    } else {
        KEEP_FOUND
    };
    observed.saturating_add(keep.as_secs() as i64)
}

impl Store {
    /// Read a fresh cached answer. An empty string is an authoritative
    /// absence, while None also covers a clock that has moved backwards.
    pub fn cached_dns(&self, name: &str, now: i64) -> Result<Option<String>> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(DNS_TXT)?;
        let entry = table.get(name.to_ascii_lowercase().as_str())?;
        Ok(entry.and_then(|entry| {
            let (observed, answer) = entry.value();
            (observed <= now && now < expires(observed, answer)).then(|| answer.to_owned())
        }))
    }
}

impl Writer<'_> {
    /// Keep a successful lookup, including an empty answer. Resolver errors
    /// never reach this table. observed is the lookup's start in Unix seconds.
    pub fn cache_dns(&mut self, name: &str, answer: &str, observed: i64, now: i64) -> Result<()> {
        if !mail_schema::query_allowed(name) || answer.len() > TXT_MAX || observed < 0 {
            return Err(Error::Corrupt("invalid DNS cache entry"));
        }
        if observed > now || expires(observed, answer) <= now {
            return Ok(());
        }
        let name = name.to_ascii_lowercase();
        let mut table = self.tx.open_table(DNS_TXT)?;
        // Replacing an existing key does not consume another slot. If full,
        // evict an unusable entry first, then the one closest to expiry.
        let previous = table.get(name.as_str())?.map(|entry| entry.value().0);
        // The reader and sync engines share the table. A lookup that began
        // earlier should not overwrite a newer observation when it returns.
        if previous.is_some_and(|seen| observed < seen && seen <= now) {
            return Ok(());
        }
        if previous.is_none() && table.len()? >= CACHE_MAX as u64 {
            let mut victim = None;
            for entry in table.iter()? {
                let (key, value) = entry?;
                let (seen, txt) = value.value();
                let rank = if seen > now {
                    i64::MIN
                } else {
                    expires(seen, txt)
                };
                if victim.as_ref().is_none_or(|(best, _)| rank < *best) {
                    victim = Some((rank, key.value().to_owned()));
                }
            }
            if let Some((_, victim)) = victim {
                table.remove(victim.as_str())?;
            }
        }
        table.insert(name.as_str(), (observed, answer))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_lookup_cannot_replace_a_newer_observation() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let name = "_dmarc.example.com";
        store
            .write(|w| {
                w.cache_dns(name, "v=DMARC1; p=reject", 110, 113)?;
                w.cache_dns(name, "v=DMARC1; p=none", 100, 114)
            })
            .unwrap();
        assert_eq!(
            store.cached_dns(name, 114).unwrap().as_deref(),
            Some("v=DMARC1; p=reject")
        );
        // After the clock moves backwards, a live answer can replace the
        // future-dated row rather than leaving the cache permanently cold.
        store
            .write(|w| w.cache_dns(name, "v=DMARC1; p=none", 80, 81))
            .unwrap();
        assert_eq!(
            store.cached_dns(name, 81).unwrap().as_deref(),
            Some("v=DMARC1; p=none")
        );
    }

    #[test]
    fn an_existing_index_opens_with_the_new_cache_table() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let account = store.add_account("reader@example.com").unwrap();
        // Simulate the same index schema before the additive cache table.
        let tx = store.db.begin_write().unwrap();
        assert!(tx.delete_table(DNS_TXT).unwrap());
        tx.commit().unwrap();
        drop(store);
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.accounts().unwrap()[0].0, account);
        assert_eq!(store.cached_dns("_dmarc.example.com", 0).unwrap(), None);
    }
}
