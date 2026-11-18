//! DKIM and DMARC lookups (mail.md §7.5): the parse jail asks for a TXT
//! record (`<selector>._domainkey.<domain>` or `_dmarc.<domain>`, held to
//! those by mail-schema) and the engine answers through the caller's
//! [`Dns`] (Mail's Nuntius, which asks the system's resolver).
//!
//! Answers are kept in memory for hours, so a sender's DNS can't count
//! messages: it sees one lookup per key per few hours, from the resolver.
//! A name that exists with no record is answered with an empty record,
//! which the jail reads as no key (not verified); a lookup that failed is
//! answered with nothing, so the check stays pending and is tried again.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::Engine;

/// TXT lookups, by whoever has the network.
pub trait Dns {
    /// The TXT records at `name`; `Err` when the lookup itself failed.
    fn txt(&self, name: &str) -> Result<Vec<String>, String>;
}

/// How long an answer is kept: one with records, and one without.
pub const KEEP_FOUND: Duration = Duration::from_secs(6 * 3600);
pub const KEEP_NONE: Duration = Duration::from_secs(3600);
/// Most names kept.
pub const CACHE_MAX: usize = 4096;

#[derive(Default)]
pub(crate) struct Cache {
    map: HashMap<String, (Option<String>, Instant)>,
    /// Lookups made (not answered from the cache), for tests.
    pub(crate) asked: usize,
}

/// The one record the jail gets from `records` at `name`: a DMARC record,
/// or the DKIM key record (the first with a key); `""` for none.
pub fn pick(name: &str, records: &[String]) -> String {
    let dmarc = name.starts_with("_dmarc.");
    records
        .iter()
        .find(|r| if dmarc { r.trim_start().starts_with("v=DMARC1") } else { r.contains("p=") })
        .cloned()
        .unwrap_or_default()
}

impl Engine {
    /// Answer the jail's lookups through `dns` from now on.
    pub fn set_dns(&mut self, dns: Box<dyn Dns>) {
        self.dns = Some(dns);
    }

    /// Lookups made so far (not from the cache).
    pub fn dns_lookups(&self) -> usize {
        self.dns_cache.borrow().asked
    }

    /// The jail's answer for `name`.
    pub(crate) fn answer(&self, name: &str) -> Option<String> {
        let dns = self.dns.as_ref()?;
        let name = name.to_ascii_lowercase();
        let now = Instant::now();
        if let Some((a, until)) = self.dns_cache.borrow().map.get(&name) {
            if *until > now {
                return a.clone();
            }
        }
        let got = dns.txt(&name);
        let mut c = self.dns_cache.borrow_mut();
        c.asked += 1;
        let a = match got {
            Ok(r) => {
                let a = pick(&name, &r);
                let keep = if a.is_empty() { KEEP_NONE } else { KEEP_FOUND };
                if c.map.len() >= CACHE_MAX {
                    c.map.retain(|_, (_, until)| *until > now);
                    if c.map.len() >= CACHE_MAX {
                        c.map.clear();
                    }
                }
                c.map.insert(name, (Some(a.clone()), now + keep));
                Some(a)
            }
            Err(_) => None,
        };
        a
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks() {
        let r = vec!["google-site-verification=x".to_string(), "v=DMARC1; p=reject".to_string()];
        assert_eq!(pick("_dmarc.example.com", &r), "v=DMARC1; p=reject");
        assert_eq!(pick("_dmarc.example.com", &[]), "");
        assert_eq!(pick("s._domainkey.example.com", &["v=DKIM1; k=ed25519; p=abc".into()]), "v=DKIM1; k=ed25519; p=abc");
        assert_eq!(pick("s._domainkey.example.com", &["junk".into()]), "");
    }
}
