//! The persona's address book (mail.md §14.2): the people the person has
//! written to, with the name they were written to by, how often and when
//! last. Recipient fields complete from it.

use redb::{ReadableTable, ReadableTableMetadata, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::{dec, enc, Result, Store, Writer};

/// Address (lower case) → postcard [`Person`].
pub(crate) const PEOPLE: TableDefinition<&str, &[u8]> = TableDefinition::new("people");
/// Message-ID → (): messages already counted, so a sent message and its
/// copy in Sent count once.
pub(crate) const PEOPLE_COUNTED: TableDefinition<&str, ()> = TableDefinition::new("people_counted");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Person {
    /// As last written (its case kept).
    pub addr: String,
    /// The name last written with it, if any was.
    pub name: String,
    /// Messages written to it.
    pub count: u32,
    /// When last written to (seconds).
    pub last: i64,
}

impl Person {
    /// How likely this is the one meant: often and lately written to
    /// count most; a month's quiet halves it.
    pub fn weight(&self, now: i64) -> f64 {
        let months = ((now - self.last).max(0) as f64) / (30.0 * 86_400.0);
        self.count as f64 * 0.5f64.powf(months)
    }

    /// Whether `typed` (lower case) starts the address, its domain, or a
    /// word of the name.
    pub fn matches(&self, typed: &str) -> bool {
        let addr = self.addr.to_lowercase();
        addr.starts_with(typed)
            || addr.split_once('@').is_some_and(|(local, domain)| domain.starts_with(typed) || local.split(['.', '_', '-', '+']).any(|p| p.starts_with(typed)))
            || self.name.to_lowercase().split(|c: char| c.is_whitespace() || c == ',' || c == '"').any(|w| w.starts_with(typed))
            || self.name.to_lowercase().starts_with(typed)
    }
}

fn message_key(id: &str) -> String {
    id.trim().trim_start_matches('<').trim_end_matches('>').to_ascii_lowercase()
}

impl Store {
    /// Everyone in the address book, most likely first.
    pub fn people(&self, now: i64) -> Result<Vec<Person>> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(PEOPLE)?;
        let mut out = Vec::new();
        for e in t.iter()? {
            out.push(dec::<Person>(e?.1.value())?);
        }
        out.sort_by(|a, b| b.weight(now).total_cmp(&a.weight(now)).then_with(|| a.addr.cmp(&b.addr)));
        Ok(out)
    }

    /// At most `n` people `typed` could mean, most likely first. Nothing
    /// for nothing typed.
    pub fn complete(&self, typed: &str, now: i64, n: usize) -> Result<Vec<Person>> {
        let typed = typed.trim().to_lowercase();
        if typed.is_empty() {
            return Ok(Vec::new());
        }
        let mut all = self.people(now)?;
        all.retain(|p| p.matches(&typed));
        all.truncate(n);
        Ok(all)
    }

    pub fn people_count(&self) -> Result<u64> {
        let tx = self.db.begin_read()?;
        Ok(tx.open_table(PEOPLE)?.len()?)
    }
}

impl Writer<'_> {
    /// The person wrote message `message_id` to `to` (name, address) at
    /// `at`. A message counted before (sent here, then seen in Sent) adds
    /// only a name that was missing.
    pub fn wrote_to(&mut self, message_id: Option<&str>, to: &[(&str, &str)], at: i64) -> Result<()> {
        let counted = match message_id.map(message_key).filter(|k| !k.is_empty()) {
            Some(k) => {
                let mut c = self.tx.open_table(PEOPLE_COUNTED)?;
                let seen = c.get(k.as_str())?.is_some();
                if !seen {
                    c.insert(k.as_str(), ())?;
                }
                seen
            }
            None => false,
        };
        let mut t = self.tx.open_table(PEOPLE)?;
        let mut done: Vec<String> = Vec::new();
        for (name, addr) in to {
            let addr = addr.trim();
            let key = addr.to_ascii_lowercase();
            if !addr.contains('@') || done.contains(&key) {
                continue;
            }
            done.push(key.clone());
            let old = t.get(key.as_str())?.map(|v| dec::<Person>(v.value())).transpose()?;
            let name = name.trim();
            let p = match old {
                Some(mut p) if counted => {
                    if p.name.is_empty() && !name.is_empty() {
                        p.name = name.to_string();
                    } else {
                        continue;
                    }
                    p
                }
                Some(mut p) => {
                    p.count = p.count.saturating_add(1);
                    // The newest message's spelling wins; an older one only
                    // fills a missing name.
                    if at >= p.last {
                        p.last = at;
                        p.addr = addr.to_string();
                        if !name.is_empty() {
                            p.name = name.to_string();
                        }
                    } else if p.name.is_empty() {
                        p.name = name.to_string();
                    }
                    p
                }
                None => Person { addr: addr.to_string(), name: name.to_string(), count: 1, last: at },
            };
            t.insert(key.as_str(), enc(&p).as_slice())?;
        }
        Ok(())
    }

    /// Take `addr` out of the address book.
    pub fn forget_person(&mut self, addr: &str) -> Result<()> {
        self.tx.open_table(PEOPLE)?.remove(addr.to_ascii_lowercase().as_str())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_book() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let day = 86_400;
        let now = 1_800_000_000;
        s.write(|w| {
            w.wrote_to(Some("<a1@x>"), &[("Grace Hopper", "grace@navy.example"), ("", "ada@x.example")], now - 3 * day)?;
            w.wrote_to(Some("<a2@x>"), &[("", "Grace@Navy.example"), ("", "ada@x.example")], now - 2 * day)?;
            // The same message again (its copy in Sent): counted once.
            w.wrote_to(Some("a2@X"), &[("Ada Lovelace", "ada@x.example")], now - 2 * day)?;
            w.wrote_to(None, &[("Old Friend", "gregor@old.example")], now - 400 * day)?;
            w.wrote_to(None, &[("", "not an address")], now)
        })
        .unwrap();
        let p = s.people(now).unwrap();
        assert_eq!(p.len(), 3);
        let grace = p.iter().find(|p| p.addr.eq_ignore_ascii_case("grace@navy.example")).unwrap();
        assert_eq!((grace.count, grace.name.as_str(), grace.addr.as_str()), (2, "Grace Hopper", "Grace@Navy.example"));
        let ada = p.iter().find(|p| p.addr == "ada@x.example").unwrap();
        assert_eq!((ada.count, ada.name.as_str()), (2, "Ada Lovelace"));
        let addrs = |t: &str| s.complete(t, now, 5).unwrap().into_iter().map(|p| p.addr.to_lowercase()).collect::<Vec<_>>();
        // By a word of the name, the address, or its domain.
        assert_eq!(addrs("hop"), ["grace@navy.example"]);
        assert_eq!(addrs("NAVY"), ["grace@navy.example"]);
        assert_eq!(addrs("g"), ["grace@navy.example", "gregor@old.example"], "the recent one first");
        assert_eq!(addrs("lovelace"), ["ada@x.example"]);
        assert!(addrs("").is_empty() && addrs("zz").is_empty());
        s.write(|w| w.forget_person("ADA@x.example")).unwrap();
        assert!(addrs("ada").is_empty());
        assert_eq!(s.people_count().unwrap(), 2);
    }
}
