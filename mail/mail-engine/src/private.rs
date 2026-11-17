//! Private reply addresses (mail.md §13.3, E12): with a domain of the
//! person's own that takes mail for any address (a catch-all), Mail makes
//! an address per site or sender (`shop-k3qx@me.example`), remembers who
//! it was for, replies from it, and can turn one off when it starts
//! bringing mail from someone else. Hosted alias relays are Loculus's.

use mail_schema::Summary;
use mail_store::{AccountId, MsgId};

use crate::{Engine, Error};

/// The account's own domain (settings, per account).
const DOMAIN: &str = "private_domain";
/// `addr \t for \t made \t off`, one per line.
const LIST: &str = "private_addresses";
/// Most addresses kept per account.
pub const PRIVATE_MAX: usize = 2000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Private {
    pub addr: String,
    /// Who it was made for: a site or a sender's domain.
    pub made_for: String,
    pub made: i64,
    /// Turned off: mail to it goes to Trash.
    pub off: bool,
}

/// What the reader says about mail that came to a private address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameTo {
    pub addr: String,
    pub made_for: String,
    /// It came from somewhere other than whom it was made for.
    pub leaked: bool,
    pub off: bool,
}

fn domain_of(addr: &str) -> Option<String> {
    addr.rsplit_once('@').map(|(_, d)| d.trim().trim_end_matches('>').to_ascii_lowercase()).filter(|d| !d.is_empty())
}

/// Whether `domain` is `of` or under it.
fn under(domain: &str, of: &str) -> bool {
    domain == of || domain.ends_with(&format!(".{of}"))
}

/// A site's name as the start of an address: lower case letters, digits
/// and dashes, at most 20.
fn slug(s: &str) -> String {
    let s = s.trim().to_ascii_lowercase();
    // "shop.example.com" → "shop"; "www." dropped.
    let s = s.strip_prefix("www.").unwrap_or(&s);
    let head = s.split(['.', '@', ' ']).find(|p| !p.is_empty()).unwrap_or("mail");
    let mut out: String = head.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    out = out.trim_matches('-').chars().take(20).collect();
    if out.is_empty() { "mail".into() } else { out }
}

fn suffix() -> String {
    const A: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
    let mut b = [0u8; 4];
    getrandom::getrandom(&mut b).expect("random bytes");
    b.iter().map(|x| A[(*x as usize) % A.len()] as char).collect()
}

fn parse(text: &str) -> Vec<Private> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            Some(Private { addr: f.next()?.to_string(), made_for: f.next()?.to_string(), made: f.next()?.parse().ok()?, off: f.next() == Some("off") })
        })
        .collect()
}

fn format(list: &[Private]) -> String {
    list.iter().map(|p| format!("{}\t{}\t{}\t{}", p.addr, p.made_for.replace(['\t', '\n', '\r'], " "), p.made, if p.off { "off" } else { "on" })).collect::<Vec<_>>().join("\n")
}

impl Engine {
    /// The domain private addresses are made at, if the person set one.
    pub fn private_domain(&self, account: AccountId) -> Option<String> {
        self.store.sync_state(account, DOMAIN).ok().flatten()
    }

    /// Set (or clear) the account's own catch-all domain.
    pub fn set_private_domain(&self, account: AccountId, domain: Option<&str>) -> Result<(), Error> {
        let d = domain.map(|d| d.trim().trim_start_matches('@').to_ascii_lowercase()).filter(|d| !d.is_empty());
        if let Some(d) = &d {
            let ok = d.contains('.') && d.len() <= 253 && d.split('.').all(|l| !l.is_empty() && l.len() <= 63 && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
            if !ok {
                return Err(Error::Refused("that isn't a domain name"));
            }
        }
        self.store.write(|w| w.set_sync_state(account, DOMAIN, d.as_deref()))?;
        Ok(())
    }

    pub fn private_addresses(&self, account: AccountId) -> Vec<Private> {
        parse(&self.store.sync_state(account, LIST).ok().flatten().unwrap_or_default())
    }

    fn keep_private(&self, account: AccountId, list: &[Private]) -> Result<(), Error> {
        let t = format(list);
        self.store.write(|w| w.set_sync_state(account, LIST, (!t.is_empty()).then_some(t.as_str())))?;
        Ok(())
    }

    /// A new address for `made_for` (a site or sender), remembered. The
    /// same one again if one was made for it.
    pub fn make_private(&self, account: AccountId, made_for: &str, now: i64) -> Result<String, Error> {
        let Some(domain) = self.private_domain(account) else { return Err(Error::Refused("set your own domain first")) };
        let made_for = made_for.trim().to_ascii_lowercase();
        if made_for.is_empty() || made_for.len() > 200 || made_for.contains(['\t', '\n', '\r']) {
            return Err(Error::Refused("say who it is for"));
        }
        let mut list = self.private_addresses(account);
        if let Some(p) = list.iter().find(|p| p.made_for == made_for && !p.off) {
            return Ok(p.addr.clone());
        }
        if list.len() >= PRIVATE_MAX {
            return Err(Error::Refused("too many private addresses"));
        }
        let addr = loop {
            let a = format!("{}-{}@{domain}", slug(&made_for), suffix());
            if !list.iter().any(|p| p.addr == a) {
                break a;
            }
        };
        list.push(Private { addr: addr.clone(), made_for, made: now, off: false });
        self.keep_private(account, &list)?;
        Ok(addr)
    }

    /// Turn an address off (its mail goes to Trash) or on again.
    pub fn set_private_off(&self, account: AccountId, addr: &str, off: bool) -> Result<(), Error> {
        let mut list = self.private_addresses(account);
        let Some(p) = list.iter_mut().find(|p| p.addr.eq_ignore_ascii_case(addr)) else { return Err(Error::NotFound) };
        p.off = off;
        self.keep_private(account, &list)
    }

    /// The private address `s` was sent to, if any: one made here, or any
    /// address at the domain (a catch-all takes them all), remembered on
    /// first sight with the sender's domain.
    pub(crate) fn private_to(&self, account: AccountId, s: &Summary) -> Option<String> {
        let domain = self.private_domain(account)?;
        s.to.iter().chain(&s.cc).map(|a| a.addr.to_ascii_lowercase()).find(|a| domain_of(a).is_some_and(|d| d == domain))
    }

    /// What the reader says about `msg` having come to a private address.
    pub fn came_to(&self, msg: MsgId) -> Option<CameTo> {
        let m = self.store.message(msg).ok()?;
        let addr = self.private_to(m.account, &m.summary)?;
        let list = self.private_addresses(m.account);
        let p = list.iter().find(|p| p.addr == addr)?;
        let from = m.summary.from.first().and_then(|a| domain_of(&a.addr)).unwrap_or_default();
        // Made for a site: mail from it or its subdomains is expected.
        let made = p.made_for.rsplit('@').next().unwrap_or(&p.made_for);
        let leaked = !from.is_empty() && made.contains('.') && !under(&from, made) && !under(made, &from);
        Some(CameTo { addr, made_for: p.made_for.clone(), leaked, off: p.off })
    }

    /// New mail to private addresses: ones not made here are remembered
    /// for whoever first wrote to them; mail to one turned off is to go
    /// to Trash (the `uid`s returned).
    pub(crate) fn private_arrived(&self, account: AccountId, new: &[(u32, Summary, i64)]) -> Result<Vec<u32>, Error> {
        if self.private_domain(account).is_none() {
            return Ok(Vec::new());
        }
        let mut list = self.private_addresses(account);
        let (mut off, mut changed) = (Vec::new(), false);
        for (uid, s, date) in new {
            let Some(addr) = self.private_to(account, s) else { continue };
            match list.iter().find(|p| p.addr == addr) {
                Some(p) if p.off => off.push(*uid),
                Some(_) => {}
                None if list.len() < PRIVATE_MAX => {
                    let made_for = s.from.first().and_then(|a| domain_of(&a.addr)).unwrap_or_else(|| "someone".into());
                    list.push(Private { addr, made_for, made: *date, off: false });
                    changed = true;
                }
                None => {}
            }
        }
        if changed {
            self.keep_private(account, &list)?;
        }
        Ok(off)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_and_lists() {
        assert_eq!(slug("www.Shop.example.com"), "shop");
        assert_eq!(slug("news@weekly.example"), "news");
        assert_eq!(slug("..."), "mail");
        assert_eq!(slug("a very long site name indeed"), "a");
        let l = vec![Private { addr: "shop-ab2c@me.example".into(), made_for: "shop.example".into(), made: 5, off: true }];
        assert_eq!(parse(&format(&l)), l);
        assert!(under("mail.shop.example", "shop.example") && !under("evilshop.example", "shop.example"));
    }
}
