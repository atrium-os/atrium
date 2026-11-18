//! Who a sender is to the person (mail.md §7.5): a first message from an
//! address, or a domain that looks like one the person writes to. Shown in
//! Mail's chrome on reading, and again when replying (§9.1).
//!
//! - **First contact**: the persona has never written to the address (the
//!   address book, §14.2) and no other message here is from it.
//! - **Look-alike**: the sender's domain isn't one the person corresponds
//!   with (people written to, the accounts' own addresses), but has the
//!   same UTS #39 skeleton as one ("examp1e.com", "аpple.com" in Cyrillic)
//!   or is one edit away from one ("exmaple.com", "example.co").
//!
//! Only what is here counts: a sender last heard from before the offline
//! window began is a first contact again.

use std::collections::BTreeSet;

use mail_store::search::field;
use mail_store::MsgId;

use crate::{Engine, Error};

/// Domains shorter than this aren't compared by edits ("a.io", "b.io").
pub const EDIT_MIN: usize = 6;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SenderNote {
    /// "First message from this address".
    FirstContact,
    /// "Looks like `like`, but is `is`" (both as ASCII, `xn--` kept; the
    /// window shows them as Monstrator would).
    LookAlike { like: String, is: String },
}

impl SenderNote {
    /// The line Mail shows, domains as Monstrator shows them (another
    /// script's letters as `xn--`); whether it warns.
    pub fn line(&self) -> (String, bool) {
        match self {
            SenderNote::FirstContact => ("First message from this address".into(), false),
            SenderNote::LookAlike { like, is } => (format!("Looks like {}, but is {}", atrium_links::shown_host(like), atrium_links::shown_host(is)), true),
        }
    }
}

fn domain_of(addr: &str) -> Option<String> {
    let (_, d) = addr.trim().rsplit_once('@')?;
    let d = d.trim_end_matches('.').to_lowercase();
    (!d.is_empty()).then_some(d)
}

/// A domain's UTS #39 skeleton, from its own letters (an `xn--` label is
/// read as what it stands for).
pub fn skeleton(domain: &str) -> String {
    let (uni, res) = idna::domain_to_unicode(domain);
    let d = if res.is_ok() { uni } else { domain.to_string() };
    unicode_security::skeleton(&d).collect::<String>().to_lowercase()
}

/// At most one edit (insert, delete, change, or two letters swapped).
pub fn one_edit(a: &str, b: &str) -> bool {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a == b {
        return false;
    }
    let (s, l) = if a.len() <= b.len() { (&a, &b) } else { (&b, &a) };
    match l.len() - s.len() {
        0 => {
            let diff: Vec<usize> = (0..s.len()).filter(|i| s[*i] != l[*i]).collect();
            diff.len() == 1 || (diff.len() == 2 && diff[1] == diff[0] + 1 && s[diff[0]] == l[diff[1]] && s[diff[1]] == l[diff[0]])
        }
        1 => {
            let i = (0..s.len()).find(|i| s[*i] != l[*i]).unwrap_or(s.len());
            s[i..] == l[i + 1..]
        }
        _ => false,
    }
}

/// The known domain `d` looks like, if any.
pub fn looks_like<'a>(d: &str, known: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let sk = skeleton(d);
    let ascii = |x: &str| idna::domain_to_ascii(x).unwrap_or_else(|_| x.to_string());
    let da = ascii(d);
    for k in known {
        let ka = ascii(k);
        if ka == da {
            continue;
        }
        if skeleton(k) == sk || (da.len().min(ka.len()) >= EDIT_MIN && one_edit(&da, &ka)) {
            return Some(ka);
        }
    }
    None
}

impl Engine {
    /// Domains the person corresponds with: those written to, and the
    /// accounts' own.
    pub fn known_domains(&self) -> Result<BTreeSet<String>, Error> {
        let mut out: BTreeSet<String> = self.store.people(0)?.iter().filter_map(|p| domain_of(&p.addr)).collect();
        for (a, _) in self.store.accounts()? {
            out.extend(self.identities(a)?.iter().filter_map(|i| domain_of(&i.addr)));
        }
        Ok(out)
    }

    /// What the reader says about `msg`'s sender, if anything. A look-alike
    /// wins over a first contact.
    pub fn sender_note(&self, msg: MsgId) -> Result<Option<SenderNote>, Error> {
        let m = self.store.message(msg)?;
        let Some(from) = m.summary.from.first() else { return Ok(None) };
        let addr = from.addr.trim().to_lowercase();
        let Some(d) = domain_of(&addr) else { return Ok(None) };
        let mine = self.identities(m.account)?.iter().any(|i| i.addr.trim().eq_ignore_ascii_case(&addr));
        if mine {
            return Ok(None);
        }
        let known = self.known_domains()?;
        if !known.contains(&d) {
            if let Some(like) = looks_like(&d, known.iter().map(String::as_str)) {
                let is = idna::domain_to_ascii(&d).unwrap_or(d);
                return Ok(Some(SenderNote::LookAlike { like, is }));
            }
        }
        if self.store.person(&addr)?.is_none() && !self.store.term_elsewhere(&addr, field::FROM, msg)? {
            return Ok(Some(SenderNote::FirstContact));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_and_skeletons() {
        assert!(one_edit("exmaple.com", "example.com"));
        assert!(one_edit("example.co", "example.com"));
        assert!(one_edit("exampel.com", "example.com"));
        assert!(one_edit("examp1e.com", "example.com"));
        assert!(!one_edit("example.org", "example.com"));
        assert!(!one_edit("example.com", "example.com"));
        assert_eq!(skeleton("examp1e.com"), skeleton("example.com"));
        assert_eq!(skeleton("rnicrosoft.com"), skeleton("microsoft.com"));
        assert_eq!(skeleton("\u{0430}pple.com"), skeleton("apple.com"));
        assert_eq!(skeleton("xn--pple-43d.com"), skeleton("apple.com"));
        let known = ["example.com", "a.io", "partner.example"];
        assert_eq!(looks_like("examp1e.com", known), Some("example.com".into()));
        assert_eq!(looks_like("exmaple.com", known), Some("example.com".into()));
        assert_eq!(looks_like("b.io", known), None, "too short to compare by edits");
        assert_eq!(looks_like("example.com", known), None);
        assert_eq!(looks_like("elsewhere.org", known), None);
        assert_eq!(looks_like("partner.examp1e", known), Some("partner.example".into()));
        let n = SenderNote::LookAlike { like: "apple.com".into(), is: idna::domain_to_ascii("\u{0430}pple.com").unwrap() };
        assert_eq!(n.line(), ("Looks like apple.com, but is xn--pple-43d.com".into(), true));
    }
}
