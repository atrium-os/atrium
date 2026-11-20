//! DKIM and DMARC lookups (mail.md §7.5): the parse jail asks for a TXT
//! record (`<selector>._domainkey.<domain>` or `_dmarc.<domain>`, held to
//! those by mail-schema) and the engine answers through the caller's
//! [`Dns`] (Mail's Nuntius, which asks the system's resolver).
//!
//! Answers are kept in the instance’s index up to the resolver’s TTL and
//! Mail’s own caps so repeated messages can reuse a fresh answer.
//! A name that exists with no record is answered with an empty record,
//! which the jail reads as no key (not verified); a lookup that failed is
//! answered with nothing, so the check stays pending and is tried again.

pub use mail_store::dns::{CACHE_MAX, KEEP_FOUND, KEEP_NONE};

use crate::Engine;

/// TXT lookups, by whoever has the network.
pub trait Dns {
    /// The TXT records at `name` and their remaining TTL in seconds.
    /// A zero TTL permits use for the current lookup only.
    fn txt(&self, name: &str) -> Result<(Vec<String>, u32), String>;
}

/// The one record the jail gets from `records` at `name`: a DMARC record,
/// or the DKIM key record (the first with a key); `""` for none.
pub fn pick(name: &str, records: &[String]) -> String {
    let dmarc = name.starts_with("_dmarc.");
    records
        .iter()
        .find(|r| {
            if dmarc {
                r.trim_start().starts_with("v=DMARC1")
            } else {
                r.contains("p=")
            }
        })
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
        self.dns_lookups.get()
    }

    /// The jail's answer for `name`.
    pub(crate) fn answer(&self, name: &str) -> Option<String> {
        let dns = self.dns.as_ref()?;
        let name = name.to_ascii_lowercase();
        let now = crate::now();
        if let Ok(Some(answer)) = self.store.cached_dns(&name, now) {
            return Some(answer);
        }
        self.dns_lookups.set(self.dns_lookups.get() + 1);
        let (records, ttl) = dns.txt(&name).ok()?;
        let answer = pick(&name, &records);
        // The live result can still be used if the optional cache cannot be
        // written. Do not cache resolver errors or use expired records.
        if ttl != 0 {
            let _ = self.store.write(|w| w.cache_dns(&name, &answer, now, crate::now(), ttl));
        }
        Some(answer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks() {
        let r = vec![
            "google-site-verification=x".to_string(),
            "v=DMARC1; p=reject".to_string(),
        ];
        assert_eq!(pick("_dmarc.example.com", &r), "v=DMARC1; p=reject");
        assert_eq!(pick("_dmarc.example.com", &[]), "");
        assert_eq!(
            pick(
                "s._domainkey.example.com",
                &["v=DKIM1; k=ed25519; p=abc".into()]
            ),
            "v=DKIM1; k=ed25519; p=abc"
        );
        assert_eq!(pick("s._domainkey.example.com", &["junk".into()]), "");
    }
}

#[cfg(test)]
mod persistent {
    use super::*;
    use crate::Parser;
    use mail_store::Store;

    struct Answer(&'static str);

    impl Dns for Answer {
        fn txt(&self, _: &str) -> Result<(Vec<String>, u32), String> {
            if self.0 == "error" {
                return Err("resolver offline".into());
            }
            Ok((vec![self.0.to_string()], 120))
        }
    }

    const KEY: &str = "sel._domainkey.example.com";
    const POLICY: &str = "_dmarc.example.com";

    fn engine(path: &std::path::Path, answer: &'static str) -> Engine {
        let store = Store::open(path).unwrap();
        let mut engine = Engine::new(
            store,
            Parser {
                program: "unused".into(),
            },
        );
        engine.set_dns(Box::new(Answer(answer)));
        engine
    }

    #[test]
    fn restart_reuses_positive_and_negative_answers() {
        let dir = tempfile::tempdir().unwrap();
        let first = engine(dir.path(), "v=DKIM1; p=key");
        assert_eq!(first.answer(KEY).as_deref(), Some("v=DKIM1; p=key"));
        assert_eq!(first.answer(POLICY).as_deref(), Some(""));
        drop(first);

        let second = engine(dir.path(), "error");
        assert_eq!(second.answer(KEY).as_deref(), Some("v=DKIM1; p=key"));
        assert_eq!(second.answer(POLICY).as_deref(), Some(""));
        assert_eq!(second.dns_lookups(), 0);
    }

    #[test]
    fn expired_rows_refresh_and_resolver_failures_are_not_saved() {
        let dir = tempfile::tempdir().unwrap();
        let now = crate::now();
        let old = now - 120;
        let missing = now - 60;
        let store = Store::open(dir.path()).unwrap();
        store
            .write(|w| {
                w.cache_dns(KEY, "v=DKIM1; p=old", old, old, 120)?;
                w.cache_dns(POLICY, "", missing, missing, 60)
            })
            .unwrap();
        assert_eq!(
            store.cached_dns(KEY, now - 1).unwrap().as_deref(),
            Some("v=DKIM1; p=old")
        );
        assert_eq!(
            store.cached_dns(POLICY, now - 1).unwrap().as_deref(),
            Some("")
        );
        assert_eq!(store.cached_dns(KEY, old - 1).unwrap(), None);
        drop(store);

        let first = engine(dir.path(), "error");
        assert_eq!(first.answer(KEY), None);
        assert_eq!(first.answer(POLICY), None);
        assert_eq!(first.dns_lookups(), 2);
        drop(first);

        let second = engine(dir.path(), "v=DKIM1; p=fresh");
        assert_eq!(second.answer(KEY).as_deref(), Some("v=DKIM1; p=fresh"));
        assert_eq!(second.answer(POLICY).as_deref(), Some(""));
        assert_eq!(second.dns_lookups(), 2);
    }
}
