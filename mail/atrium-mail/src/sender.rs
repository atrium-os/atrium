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

/// How an S/MIME line is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Good,
    Warn,
    Bad,
    Plain,
}

/// The S/MIME line under the sender (mail.md §10.1): who signed, from the
/// checked certificate; or why the signature isn't good; or that the
/// message is encrypted.
pub fn smime_line(s: &Summary) -> Option<(String, Tone)> {
    use mail_schema::SmimeState as S;
    if s.encrypted {
        return Some(("Encrypted with S/MIME".into(), Tone::Plain));
    }
    let m = s.smime.as_ref()?;
    let who = m.signer.as_ref().map(|c| {
        let addr = c.emails.first().cloned().unwrap_or_default();
        if c.name.is_empty() || c.name == addr { addr } else { format!("{} <{addr}>", c.name) }
    });
    let issuer = m.signer.as_ref().map(|c| c.issuer.clone()).filter(|i| !i.is_empty());
    Some(match &m.state {
        S::Valid => (
            match issuer {
                Some(i) => format!("Signed by {} · certificate from {i}", who.unwrap_or_default()),
                None => format!("Signed by {}", who.unwrap_or_default()),
            },
            Tone::Good,
        ),
        S::NotTrusted(why) => (format!("Signed by {}, not trusted: {why}", who.unwrap_or_else(|| "someone".into())), Tone::Warn),
        S::Invalid(why) => (format!("The signature isn't good: {why}"), Tone::Bad),
        S::WrongAddress(l) => (format!("Signed with a certificate for {}, not the sender's address", l.first().map(String::as_str).unwrap_or("another address")), Tone::Bad),
    })
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

    #[test]
    fn smime_lines() {
        use mail_schema::{Smime, SmimeCert, SmimeState};
        let mut s = Summary::default();
        assert_eq!(smime_line(&s), None);
        let cert = SmimeCert { name: "Grace Hopper".into(), emails: vec!["grace@example.net".into()], issuer: "Fixture".into(), not_before: 0, not_after: 1, sha256: [0; 32], der: vec![] };
        s.smime = Some(Smime { state: SmimeState::Valid, signer: Some(cert), issuers: vec![], signed_at: None });
        assert_eq!(smime_line(&s), Some(("Signed by Grace Hopper <grace@example.net> · certificate from Fixture".into(), Tone::Good)));
        s.smime.as_mut().unwrap().state = SmimeState::NotTrusted("the certificate is self-signed".into());
        assert_eq!(smime_line(&s).unwrap(), ("Signed by Grace Hopper <grace@example.net>, not trusted: the certificate is self-signed".into(), Tone::Warn));
        s.smime.as_mut().unwrap().state = SmimeState::WrongAddress(vec!["grace@example.net".into()]);
        assert_eq!(smime_line(&s).unwrap().1, Tone::Bad);
        s.encrypted = true;
        assert_eq!(smime_line(&s).unwrap().0, "Encrypted with S/MIME");
    }
}
