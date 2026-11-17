//! Finding an account's servers from its address (mail.md §5.1).
//!
//! Order: a provider Mail already knows by the address's domain (no lookup
//! at all), then the domain's JMAP session resource, Mozilla-style
//! autoconfig on the domain itself, SRV records (RFC 6186, RFC 8314), and
//! the MX host's provider. The Thunderbird ISPDB is never asked (a third
//! party learning every address added). Servers outside the address's own
//! domain are reported in [`Found::handled_by`] for the person to accept.
//!
//! The lookups are the caller's ([`Lookup`]: Nuntius and the system
//! resolver); everything read back is bounded and checked here.

/// The network, as discovery sees it.
pub trait Lookup {
    /// GET `url` (https only); the body of a 2xx answer, at most 256 KiB.
    fn get(&mut self, url: &str) -> Option<Vec<u8>>;
    /// SRV records for `name`: (priority, weight, port, target).
    fn srv(&mut self, name: &str) -> Vec<(u16, u16, u16, String)>;
    /// MX hosts of `domain`, most preferred first.
    fn mx(&mut self, domain: &str) -> Vec<String>;
}

/// A server, by Arca's scheme name (`imaps`, `imap`, `submissions`,
/// `submission`, `https`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    pub scheme: &'static str,
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settings {
    Imap { imap: Server, smtp: Server },
    Jmap { session: String },
}

/// Where the settings came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    Provider(&'static str),
    Jmap,
    Autoconfig,
    Srv,
    /// `imap.<domain>` and `smtp.<domain>`, unconfirmed: shown as a guess.
    Guess,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub settings: Settings,
    pub via: Via,
    /// The sign-in is OAuth with this provider (arca-courier `oauth`).
    pub oauth: Option<&'static str>,
    /// The servers are someone else's: "Mail for you@example.org is handled
    /// by …" needs a press.
    pub handled_by: Option<String>,
    /// What the person should know before signing in.
    pub note: Option<&'static str>,
}

struct Provider {
    name: &'static str,
    domains: &'static [&'static str],
    /// MX host suffixes that mean this provider hosts the domain.
    mx: &'static [&'static str],
    imap: (&'static str, u16),
    smtp: (&'static str, &'static str, u16),
    oauth: Option<&'static str>,
    note: Option<&'static str>,
}

const PROVIDERS: &[Provider] = &[
    Provider {
        name: "Google",
        domains: &["gmail.com", "googlemail.com"],
        mx: &[".google.com", ".googlemail.com"],
        imap: ("imap.gmail.com", 993),
        smtp: ("submissions", "smtp.gmail.com", 465),
        oauth: None,
        note: Some("Google lets Mail sign in with an app password for now: make one under Security in your Google account (2-Step Verification must be on)."),
    },
    Provider {
        name: "Microsoft",
        domains: &["outlook.com", "hotmail.com", "live.com", "msn.com", "hotmail.co.uk", "outlook.in", "live.in"],
        mx: &[".mail.protection.outlook.com", ".olc.protection.outlook.com"],
        imap: ("outlook.office365.com", 993),
        smtp: ("submission", "smtp.office365.com", 587),
        oauth: Some("microsoft"),
        note: None,
    },
    Provider {
        name: "iCloud",
        domains: &["icloud.com", "me.com", "mac.com"],
        mx: &[".mail.icloud.com"],
        imap: ("imap.mail.me.com", 993),
        smtp: ("submission", "smtp.mail.me.com", 587),
        oauth: None,
        note: Some("iCloud needs an app-specific password: make one at account.apple.com under Sign-In and Security."),
    },
    Provider {
        name: "Fastmail",
        domains: &["fastmail.com", "fastmail.fm"],
        mx: &[".messagingengine.com"],
        imap: ("imap.fastmail.com", 993),
        smtp: ("submissions", "smtp.fastmail.com", 465),
        oauth: None,
        note: Some("Fastmail needs an app password: make one under Settings, Privacy & Security."),
    },
    Provider {
        name: "Yahoo",
        domains: &["yahoo.com", "ymail.com", "rocketmail.com", "yahoo.co.uk", "yahoo.co.in"],
        mx: &[".yahoodns.net"],
        imap: ("imap.mail.yahoo.com", 993),
        smtp: ("submissions", "smtp.mail.yahoo.com", 465),
        oauth: None,
        note: Some("Yahoo needs an app password: make one under Account security."),
    },
];

/// Longest answer read from a lookup.
const MAX_ANSWER: usize = 256 << 10;

fn host_ok(h: &str) -> bool {
    !h.is_empty() && h.len() <= 253 && h.contains('.') && h.split('.').all(|l| !l.is_empty() && l.len() <= 63 && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') && !l.starts_with('-'))
}

fn under(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

fn from_provider(p: &Provider, domain: &str, mx_matched: bool) -> Found {
    Found {
        settings: Settings::Imap { imap: Server { scheme: "imaps", host: p.imap.0.into(), port: p.imap.1 }, smtp: Server { scheme: p.smtp.0, host: p.smtp.1.into(), port: p.smtp.2 } },
        via: Via::Provider(p.name),
        oauth: p.oauth,
        // A known provider's own domain needs no press; a custom domain it
        // hosts is said ("handled by Microsoft").
        handled_by: (mx_matched && !p.domains.contains(&domain)).then(|| p.name.to_string()),
        note: p.note,
    }
}

/// The domain of `address`, lower case, or an error to show.
pub fn domain_of(address: &str) -> Result<String, &'static str> {
    let (local, domain) = address.trim().rsplit_once('@').ok_or("That doesn't look like an email address.")?;
    let domain = domain.to_ascii_lowercase();
    if local.is_empty() || local.len() > 64 || !host_ok(&domain) {
        return Err("That doesn't look like an email address.");
    }
    Ok(domain)
}

/// Find the servers for `address`.
pub fn discover(address: &str, l: &mut dyn Lookup) -> Result<Found, &'static str> {
    let domain = domain_of(address)?;
    if let Some(p) = PROVIDERS.iter().find(|p| p.domains.contains(&domain.as_str())) {
        return Ok(from_provider(p, &domain, false));
    }
    // JMAP: the session resource must parse and offer mail.
    let url = format!("https://{domain}/.well-known/jmap");
    if l.get(&url).filter(|b| b.len() <= MAX_ANSWER).is_some_and(|b| mail_proto::jmap::session(&b).is_ok()) {
        return Ok(Found { settings: Settings::Jmap { session: url }, via: Via::Jmap, oauth: None, handled_by: None, note: None });
    }
    for url in [format!("https://autoconfig.{domain}/mail/config-v1.1.xml?emailaddress={}", address.trim()), format!("https://{domain}/.well-known/autoconfig/mail/config-v1.1.xml")] {
        if let Some(b) = l.get(&url).filter(|b| b.len() <= MAX_ANSWER) {
            if let Some((imap, smtp, oauth)) = autoconfig(&b, address.trim(), &domain) {
                let outside = [&imap.host, &smtp.host].into_iter().find(|h| !under(h, &domain)).cloned();
                return Ok(Found { settings: Settings::Imap { imap, smtp }, via: Via::Autoconfig, oauth, handled_by: outside, note: None });
            }
        }
    }
    let best = |l: &mut dyn Lookup, name: &str| -> Option<(String, u16)> {
        let mut v: Vec<(u16, u16, u16, String)> = l.srv(name).into_iter().take(32).filter(|r| r.3 != "." && host_ok(r.3.trim_end_matches('.'))).collect();
        v.sort_by_key(|r| (r.0, u16::MAX - r.1));
        v.first().map(|r| (r.3.trim_end_matches('.').to_ascii_lowercase(), r.2))
    };
    if let Some((ih, ip)) = best(l, &format!("_imaps._tcp.{domain}")) {
        let smtp = best(l, &format!("_submissions._tcp.{domain}")).map(|(h, p)| Server { scheme: "submissions", host: h, port: p }).or_else(|| best(l, &format!("_submission._tcp.{domain}")).map(|(h, p)| Server { scheme: "submission", host: h, port: p }));
        if let Some(smtp) = smtp {
            let imap = Server { scheme: "imaps", host: ih, port: ip };
            let outside = [&imap.host, &smtp.host].into_iter().find(|h| !under(h, &domain)).cloned();
            return Ok(Found { settings: Settings::Imap { imap, smtp }, via: Via::Srv, oauth: None, handled_by: outside, note: None });
        }
    }
    let mx: Vec<String> = l.mx(&domain).into_iter().take(16).map(|h| h.trim_end_matches('.').to_ascii_lowercase()).collect();
    for h in &mx {
        if let Some(p) = PROVIDERS.iter().find(|p| p.mx.iter().any(|s| h.ends_with(s))) {
            return Ok(from_provider(p, &domain, true));
        }
    }
    Ok(Found {
        settings: Settings::Imap { imap: Server { scheme: "imaps", host: format!("imap.{domain}"), port: 993 }, smtp: Server { scheme: "submissions", host: format!("smtp.{domain}"), port: 465 } },
        via: Via::Guess,
        oauth: None,
        handled_by: None,
        note: Some("Mail couldn't find this account's settings, so these are a guess. Check them with your mail provider."),
    })
}

// ── autoconfig (Mozilla clientConfig v1.1) ───────────────────────────

/// The text of each `<tag …>…</tag>` inside `within` (no nesting of the
/// same tag), with the opening tag's attributes.
fn elements<'a>(within: &'a str, tag: &str) -> Vec<(&'a str, &'a str)> {
    let mut out = Vec::new();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut rest = within;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        if !after.starts_with(['>', ' ', '\t', '\r', '\n']) {
            rest = after;
            continue;
        }
        let Some(gt) = after.find('>') else { break };
        let attrs = &after[..gt];
        let body = &after[gt + 1..];
        let Some(end) = body.find(&close) else { break };
        out.push((attrs, &body[..end]));
        rest = &body[end + close.len()..];
        if out.len() >= 16 {
            break;
        }
    }
    out
}

fn text<'a>(within: &'a str, tag: &str) -> Option<&'a str> {
    elements(within, tag).first().map(|(_, t)| t.trim())
}

fn attr<'a>(attrs: &'a str, name: &str) -> Option<&'a str> {
    let i = attrs.find(&format!("{name}=\""))? + name.len() + 2;
    let end = attrs[i..].find('"')?;
    Some(&attrs[i..i + end])
}

/// The first usable IMAP and SMTP servers of an autoconfig document:
/// TLS only (no plain), with `%EMAILADDRESS%`-style placeholders filled.
fn autoconfig(b: &[u8], address: &str, domain: &str) -> Option<(Server, Server, Option<&'static str>)> {
    let s = std::str::from_utf8(b).ok()?;
    if !s.contains("<clientConfig") {
        return None;
    }
    let local = address.rsplit_once('@').map(|(l, _)| l).unwrap_or("");
    let fill = |h: &str| h.replace("%EMAILDOMAIN%", domain).replace("%EMAILLOCALPART%", local).replace("%EMAILADDRESS%", address).to_ascii_lowercase();
    let mut oauth = None;
    let mut pick = |kind: &str, tls: &'static str, starttls: &'static str| -> Option<Server> {
        let tag = if kind == "imap" { "incomingServer" } else { "outgoingServer" };
        for (attrs, body) in elements(s, tag) {
            if attr(attrs, "type") != Some(kind) {
                continue;
            }
            let host = fill(text(body, "hostname")?);
            let port: u16 = text(body, "port")?.parse().ok()?;
            let scheme = match text(body, "socketType")? {
                "SSL" => tls,
                "STARTTLS" => starttls,
                _ => continue,
            };
            if !host_ok(&host) || port == 0 {
                continue;
            }
            if elements(body, "authentication").iter().any(|(_, t)| t.trim() == "OAuth2") && host.ends_with("office365.com") {
                oauth = Some("microsoft");
            }
            return Some(Server { scheme, host, port });
        }
        None
    };
    let imap = pick("imap", "imaps", "imap")?;
    let smtp = pick("smtp", "submissions", "submission")?;
    Some((imap, smtp, oauth))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[derive(Default)]
    struct Net {
        pages: HashMap<String, Vec<u8>>,
        srv: HashMap<String, Vec<(u16, u16, u16, String)>>,
        mx: HashMap<String, Vec<String>>,
        asked: Vec<String>,
    }

    impl Lookup for Net {
        fn get(&mut self, url: &str) -> Option<Vec<u8>> {
            self.asked.push(url.to_string());
            self.pages.get(url).cloned()
        }
        fn srv(&mut self, name: &str) -> Vec<(u16, u16, u16, String)> {
            self.asked.push(name.to_string());
            self.srv.get(name).cloned().unwrap_or_default()
        }
        fn mx(&mut self, domain: &str) -> Vec<String> {
            self.asked.push(format!("MX {domain}"));
            self.mx.get(domain).cloned().unwrap_or_default()
        }
    }

    #[test]
    fn known_providers_need_no_lookup() {
        let mut n = Net::default();
        let f = discover("Ada@Outlook.com", &mut n).unwrap();
        assert_eq!(f.via, Via::Provider("Microsoft"));
        assert_eq!(f.oauth, Some("microsoft"));
        assert_eq!(f.handled_by, None);
        assert!(n.asked.is_empty());
        let f = discover("ada@gmail.com", &mut n).unwrap();
        assert!(matches!(f.settings, Settings::Imap { ref smtp, .. } if smtp.scheme == "submissions" && smtp.port == 465));
        assert!(f.note.unwrap().contains("app password"));
        assert!(discover("not an address", &mut n).is_err());
        assert!(discover("a@b", &mut n).is_err());
    }

    #[test]
    fn jmap_autoconfig_srv_mx_then_a_guess() {
        let mut n = Net::default();
        n.pages.insert("https://jmap.example/.well-known/jmap".into(), br#"{"capabilities":{"urn:ietf:params:jmap:core":{},"urn:ietf:params:jmap:mail":{}},"accounts":{},"primaryAccounts":{"urn:ietf:params:jmap:mail":"a1"},"apiUrl":"https://jmap.example/api/","downloadUrl":"https://jmap.example/d/{blobId}","uploadUrl":"https://jmap.example/u/","state":"1"}"#.to_vec());
        assert_eq!(discover("a@jmap.example", &mut n).unwrap().settings, Settings::Jmap { session: "https://jmap.example/.well-known/jmap".into() });

        n.pages.insert("https://auto.example/.well-known/autoconfig/mail/config-v1.1.xml".into(), br#"<?xml version="1.0"?>
<clientConfig version="1.1"><emailProvider id="auto.example">
  <incomingServer type="pop3"><hostname>pop.auto.example</hostname><port>995</port><socketType>SSL</socketType></incomingServer>
  <incomingServer type="imap"><hostname>imap.%EMAILDOMAIN%</hostname><port>143</port><socketType>plain</socketType></incomingServer>
  <incomingServer type="imap"><hostname>mail.hoster.example</hostname><port>993</port><socketType>SSL</socketType><authentication>password-cleartext</authentication></incomingServer>
  <outgoingServer type="smtp"><hostname>smtp.%EMAILDOMAIN%</hostname><port>587</port><socketType>STARTTLS</socketType></outgoingServer>
</emailProvider></clientConfig>"#.to_vec());
        let f = discover("a@auto.example", &mut n).unwrap();
        assert_eq!(f.via, Via::Autoconfig);
        assert_eq!(f.settings, Settings::Imap { imap: Server { scheme: "imaps", host: "mail.hoster.example".into(), port: 993 }, smtp: Server { scheme: "submission", host: "smtp.auto.example".into(), port: 587 } });
        assert_eq!(f.handled_by.as_deref(), Some("mail.hoster.example"));
        assert!(!n.asked.iter().any(|u| u.contains("thunderbird") || u.contains("mozilla")));

        n.srv.insert("_imaps._tcp.srv.example".into(), vec![(10, 0, 993, "backup.srv.example.".into()), (0, 5, 993, "imap.srv.example.".into())]);
        n.srv.insert("_submissions._tcp.srv.example".into(), vec![(0, 0, 465, "smtp.srv.example.".into())]);
        let f = discover("a@srv.example", &mut n).unwrap();
        assert_eq!(f.via, Via::Srv);
        assert_eq!(f.settings, Settings::Imap { imap: Server { scheme: "imaps", host: "imap.srv.example".into(), port: 993 }, smtp: Server { scheme: "submissions", host: "smtp.srv.example".into(), port: 465 } });
        assert_eq!(f.handled_by, None);

        n.mx.insert("contoso.example".into(), vec!["contoso-example.mail.protection.outlook.com.".into()]);
        let f = discover("a@contoso.example", &mut n).unwrap();
        assert_eq!(f.via, Via::Provider("Microsoft"));
        assert_eq!(f.handled_by.as_deref(), Some("Microsoft"));

        let f = discover("a@nothing.example", &mut n).unwrap();
        assert_eq!(f.via, Via::Guess);
    }
}
