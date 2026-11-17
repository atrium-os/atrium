//! Who sent it, as Mail's chrome shows it (mail.md §7.5): from what was
//! checked, never from what the message says about itself.

use mail_schema::{Address, SenderCheck, Summary};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    /// DKIM aligned with From (or the provider said so).
    Verified,
    /// Checked and not verified.
    NotVerified,
    /// Not checked yet (the text, or a key, is still to come).
    NotYet,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Sender {
    /// The display name, or the address when the name is empty or looks
    /// like an address of its own.
    pub name: String,
    /// The verified domain, else the whole address.
    pub detail: String,
    pub check: Check,
    /// "Replies go to …" when Reply-To leaves From's domain.
    pub replies_to: Option<String>,
}

fn domain(addr: &str) -> String {
    addr.rsplit_once('@').map(|(_, d)| d.trim_end_matches('.').to_ascii_lowercase()).unwrap_or_default()
}

/// The name to show for an address: the real address when the display
/// name is empty, or carries an address or a domain (§7.5).
pub fn name_of(a: &Address) -> String {
    let n = a.name.trim();
    let looks_like_domain = n.contains('.') && !n.contains(' ') && n.split('.').all(|p| !p.is_empty());
    if n.is_empty() || a.name_looks_like_address || n.contains('@') || looks_like_domain { a.addr.clone() } else { n.to_string() }
}

pub fn sender(s: &Summary) -> Sender {
    let Some(from) = s.from.first() else {
        return Sender { name: "(no sender)".into(), detail: String::new(), check: Check::NotVerified, replies_to: None };
    };
    let name = name_of(from);
    let (detail, check) = match &s.sender_check {
        // The domain is From's own (mail-schema's check refuses any other).
        SenderCheck::Verified { domain, .. } => (domain.clone(), Check::Verified),
        SenderCheck::NotVerified => (from.addr.clone(), Check::NotVerified),
        SenderCheck::Pending => (from.addr.clone(), Check::NotYet),
    };
    let fd = domain(&from.addr);
    let replies_to = s.reply_to.iter().find(|r| domain(&r.addr) != fd).map(|r| r.addr.clone());
    // The name alone when it is the address shown beside it.
    let detail = if detail == name { String::new() } else { detail };
    Sender { name, detail, check, replies_to }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(name: &str, addr: &str) -> Address {
        Address { name: name.into(), addr: addr.into(), name_looks_like_address: false }
    }

    #[test]
    fn lines() {
        let mut s = Summary { from: vec![a("Priya Nair", "priya@example.com")], ..Default::default() };
        s.sender_check = SenderCheck::Verified { domain: "example.com".into(), by_provider: false };
        let x = sender(&s);
        assert_eq!((x.name.as_str(), x.detail.as_str(), x.check), ("Priya Nair", "example.com", Check::Verified));
        s.sender_check = SenderCheck::NotVerified;
        assert_eq!(sender(&s).detail, "priya@example.com");
        // A name that is a domain or an address is replaced by the address.
        s.from = vec![a("paypal.com", "x@evil.example")];
        assert_eq!(sender(&s).name, "x@evil.example");
        assert_eq!(sender(&s).detail, "");
        s.from = vec![a("support@bank.example", "x@evil.example")];
        assert_eq!(sender(&s).name, "x@evil.example");
        s.reply_to = vec![a("", "billing@other.example")];
        assert_eq!(sender(&s).replies_to.as_deref(), Some("billing@other.example"));
        s.reply_to = vec![a("", "y@EVIL.example")];
        assert_eq!(sender(&s).replies_to, None);
    }
}
