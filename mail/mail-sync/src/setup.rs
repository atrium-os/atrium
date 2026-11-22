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

use std::fmt::Write as _;

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
    h.len() <= 253
        && h.contains('.')
        && h.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
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

fn encode_query_value(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            write!(encoded, "%{byte:02X}").unwrap();
        }
    }
    encoded
}

/// Find the servers for `address`.
pub fn discover(address: &str, l: &mut dyn Lookup) -> Result<Found, &'static str> {
    let address = address.trim();
    let domain = domain_of(address)?;
    if let Some(p) = PROVIDERS.iter().find(|p| p.domains.contains(&domain.as_str())) {
        return Ok(from_provider(p, &domain, false));
    }
    // JMAP: the session resource must parse and offer mail.
    let url = format!("https://{domain}/.well-known/jmap");
    if l.get(&url).filter(|b| b.len() <= MAX_ANSWER).is_some_and(|b| mail_proto::jmap::session(&b).is_ok()) {
        return Ok(Found { settings: Settings::Jmap { session: url }, via: Via::Jmap, oauth: None, handled_by: None, note: None });
    }
    let query_address = encode_query_value(address);
    for url in [format!("https://autoconfig.{domain}/mail/config-v1.1.xml?emailaddress={query_address}"), format!("https://{domain}/.well-known/autoconfig/mail/config-v1.1.xml")] {
        if let Some(b) = l.get(&url).filter(|b| b.len() <= MAX_ANSWER) {
            if let Some((imap, smtp, oauth)) = autoconfig(&b, address, &domain) {
                let outside = [&imap.host, &smtp.host].into_iter().find(|h| !under(h, &domain)).cloned();
                return Ok(Found { settings: Settings::Imap { imap, smtp }, via: Via::Autoconfig, oauth, handled_by: outside, note: None });
            }
        }
    }
    let best = |l: &mut dyn Lookup, name: &str| -> Option<(String, u16)> {
        l.srv(name)
            .into_iter()
            .take(32)
            .filter_map(|(priority, weight, port, mut host)| {
                if host.ends_with('.') {
                    host.pop();
                }
                (port != 0 && host_ok(&host)).then_some((priority, weight, port, host))
            })
            .min_by_key(|&(priority, weight, _, _)| (priority, u16::MAX - weight))
            .map(|(_, _, port, host)| (host.to_ascii_lowercase(), port))
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

fn xml_text(field: roxmltree::Node<'_, '_>) -> Option<String> {
    let mut text = String::new();
    for child in field.children() {
        if child.is_element() {
            return None;
        }
        if child.is_text() {
            text.push_str(child.text()?);
        }
    }
    Some(text)
}

/// The first usable IMAP and SMTP servers of an autoconfig document:
/// TLS only (no plain), with `%EMAILADDRESS%`-style placeholders filled.
fn autoconfig(b: &[u8], address: &str, domain: &str) -> Option<(Server, Server, Option<&'static str>)> {
    // The caller bounds the response size. DTDs are disabled by default.
    let doc = roxmltree::Document::parse(std::str::from_utf8(b).ok()?).ok()?;
    let root = doc.root_element();
    if !root.has_tag_name("clientConfig") {
        return None;
    }
    std::iter::once(root)
        .chain(root.children().filter(|child| child.has_tag_name("emailProvider")))
        .find_map(|provider| autoconfig_provider(provider, address, domain))
}

fn autoconfig_provider(
    settings: roxmltree::Node<'_, '_>,
    address: &str,
    domain: &str,
) -> Option<(Server, Server, Option<&'static str>)> {
    let local = address.rsplit_once('@').map(|(l, _)| l).unwrap_or("");
    let fill = |h: &str| h.replace("%EMAILDOMAIN%", domain).replace("%EMAILLOCALPART%", local).replace("%EMAILADDRESS%", address).to_ascii_lowercase();
    let pick = |kind: &str, tls: &'static str, starttls: &'static str| -> Option<(Server, bool)> {
        let tag = if kind == "imap" { "incomingServer" } else { "outgoingServer" };
        for server in settings.children().filter(|child| child.has_tag_name(tag)).take(16) {
            if server.attribute("type") != Some(kind) {
                continue;
            }
            let field = |name| server.children().find(|child| child.has_tag_name(name)).and_then(xml_text);
            let (Some(hostname), Some(port_text)) = (field("hostname"), field("port")) else {
                continue;
            };
            let host = fill(hostname.trim());
            let Ok(port) = port_text.trim().parse::<u16>() else {
                continue;
            };
            let scheme = match field("socketType").as_deref().map(str::trim) {
                Some("SSL") => tls,
                Some("STARTTLS") => starttls,
                _ => continue,
            };
            if !host_ok(&host) || port == 0 {
                continue;
            }
            let microsoft_oauth = under(&host, "office365.com")
                && server.children().any(|child| {
                    child.has_tag_name("authentication")
                        && xml_text(child).is_some_and(|value| value.trim() == "OAuth2")
                });
            return Some((Server { scheme, host, port }, microsoft_oauth));
        }
        None
    };
    let (imap, imap_oauth) = pick("imap", "imaps", "imap")?;
    let (smtp, smtp_oauth) = pick("smtp", "submissions", "submission")?;
    let oauth = if imap_oauth || smtp_oauth {
        // Arca creates both sign-in items for the preset endpoints.
        let provider = PROVIDERS.iter().find(|p| p.oauth == Some("microsoft"))?;
        if (imap.scheme, imap.host.as_str(), imap.port) != ("imaps", provider.imap.0, provider.imap.1)
            || (smtp.scheme, smtp.host.as_str(), smtp.port) != provider.smtp
        {
            return None;
        }
        provider.oauth
    } else {
        None
    };
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
    fn autoconfig_passes_the_complete_address_as_one_query_value() {
        let mut n = Net::default();
        let url = "https://autoconfig.example.test/mail/config-v1.1.xml?emailaddress=Ada%2Bops%26tag%23x%40Example.Test";
        n.pages.insert(url.into(), br#"<clientConfig>
<incomingServer type="imap"><hostname>imap.%EMAILDOMAIN%</hostname><port>993</port><socketType>SSL</socketType></incomingServer>
<outgoingServer type="smtp"><hostname>smtp.%EMAILDOMAIN%</hostname><port>465</port><socketType>SSL</socketType></outgoingServer>
</clientConfig>"#.to_vec());
        let f = discover(" Ada+ops&tag#x@Example.Test ", &mut n).unwrap();
        assert_eq!(f.via, Via::Autoconfig);
        assert_eq!(n.asked, ["https://example.test/.well-known/jmap", url]);
        assert!(matches!(f.settings, Settings::Imap { imap, .. } if imap.host == "imap.example.test"));
    }

    #[test]
    fn autoconfig_skips_unusable_servers_before_valid_tls_endpoints() {
        let mut n = Net::default();
        let url = "https://autoconfig.example.test/mail/config-v1.1.xml?emailaddress=ada%40example.test";
        n.pages.insert(url.into(), br#"<clientConfig>
<incomingServer type="imap"><port>993</port><socketType>SSL</socketType></incomingServer>
<incomingServer type="imap"><hostname>imap.bad.example.test</hostname><port>bad</port><socketType>SSL</socketType></incomingServer>
<incomingServer type="imap"><hostname>imap.clear.example.test</hostname><port>143</port><socketType>plain</socketType></incomingServer>
<incomingServer type="imap"><hostname>imap.good.example.test</hostname><port>993</port><socketType>SSL</socketType></incomingServer>
<outgoingServer type="smtp"><hostname>smtp.bad.example.test</hostname><port>465</port></outgoingServer>
<outgoingServer type="smtp"><hostname>smtp.zero.example.test</hostname><port>0</port><socketType>SSL</socketType></outgoingServer>
<outgoingServer type="smtp"><hostname>smtp.good.example.test</hostname><port>587</port><socketType>STARTTLS</socketType></outgoingServer>
</clientConfig>"#.to_vec());

        let found = discover("ada@example.test", &mut n).unwrap();

        assert_eq!(found.via, Via::Autoconfig);
        assert_eq!(found.settings, Settings::Imap {
            imap: Server { scheme: "imaps", host: "imap.good.example.test".into(), port: 993 },
            smtp: Server { scheme: "submission", host: "smtp.good.example.test".into(), port: 587 },
        });
        assert_eq!(n.asked, ["https://example.test/.well-known/jmap", url]);
    }

    #[test]
    fn srv_discovery_ranks_only_usable_mail_endpoints() {
        let mut n = Net::default();
        n.srv.insert("_imaps._tcp.example.test".into(), vec![
            (0, 10, 0, "disabled.example.test.".into()),
            (0, 9, 993, "imap-.example.test.".into()),
            (0, 8, 993, "imap.example.test..".into()),
            (10, 0, 993, "IMAP.EXAMPLE.TEST.".into()),
        ]);
        n.srv.insert("_submissions._tcp.example.test".into(), vec![
            (0, 0, 0, "broken.example.test.".into()),
        ]);
        n.srv.insert("_submission._tcp.example.test".into(), vec![
            (0, 0, 587, "smtp.example.test.".into()),
        ]);

        let found = discover("ada@example.test", &mut n).unwrap();

        assert_eq!(found.via, Via::Srv);
        assert_eq!(found.settings, Settings::Imap {
            imap: Server { scheme: "imaps", host: "imap.example.test".into(), port: 993 },
            smtp: Server { scheme: "submission", host: "smtp.example.test".into(), port: 587 },
        });
        assert!(n.asked.iter().any(|query| query == "_submission._tcp.example.test"));
        assert!(!n.asked.iter().any(|query| query.starts_with("MX ")));
    }

    #[test]
    fn microsoft_autoconfig_requires_the_supported_endpoint_pair() {
        let discover = |imap: &str, imap_port: u16, smtp: &str, smtp_port: u16| {
            let mut net = Net::default();
            net.pages.insert(
                "https://autoconfig.example.test/mail/config-v1.1.xml?emailaddress=ada%40example.test".into(),
                format!(r#"<clientConfig>
<incomingServer type="imap"><hostname>{imap}</hostname><port>{imap_port}</port><socketType>SSL</socketType><authentication>OAuth2</authentication></incomingServer>
<outgoingServer type="smtp"><hostname>{smtp}</hostname><port>{smtp_port}</port><socketType>STARTTLS</socketType></outgoingServer>
</clientConfig>"#).into_bytes(),
            );
            net.mx.insert("example.test".into(), vec!["tenant.mail.protection.outlook.com.".into()]);
            super::discover("ada@example.test", &mut net).unwrap()
        };

        let found = discover("outlook.office365.com", 993, "smtp.office365.com", 587);
        assert_eq!(found.via, Via::Autoconfig);
        assert_eq!(found.oauth, Some("microsoft"));

        for found in [
            discover("outlook.office365.com", 1993, "smtp.office365.com", 587),
            discover("outlook.office365.com", 993, "smtp.example.test", 587),
        ] {
            assert_eq!(found.via, Via::Provider("Microsoft"));
            assert_eq!(found.oauth, Some("microsoft"));
            assert!(matches!(found.settings, Settings::Imap { imap, smtp }
                if imap.port == 993 && smtp.host == "smtp.office365.com"));
        }
    }

    #[test]
    fn autoconfig_reads_exact_type_attributes_with_either_quote_style() {
        let mut net = Net::default();
        let url = "https://autoconfig.example.test/mail/config-v1.1.xml?emailaddress=ada%40example.test";
        net.pages.insert(url.into(), br#"<clientConfig>
<incomingServer subtype="imap"><hostname>unused.example.test</hostname><port>993</port><socketType>SSL</socketType></incomingServer>
<incomingServer type = 'imap' label="A"><hostname>imap.example.test</hostname><port>993</port><socketType>SSL</socketType></incomingServer>
<outgoingServer label='type="smtp"' type="other"><hostname>unused.example.test</hostname><port>465</port><socketType>SSL</socketType></outgoingServer>
<outgoingServer label='A' type = "smtp"><hostname>smtp.example.test</hostname><port>587</port><socketType>STARTTLS</socketType></outgoingServer>
</clientConfig>"#.to_vec());

        let found = discover("ada@example.test", &mut net).unwrap();

        assert_eq!(found.via, Via::Autoconfig);
        assert_eq!(found.settings, Settings::Imap {
            imap: Server { scheme: "imaps", host: "imap.example.test".into(), port: 993 },
            smtp: Server { scheme: "submission", host: "smtp.example.test".into(), port: 587 },
        });
        assert_eq!(net.asked, ["https://example.test/.well-known/jmap", url]);
    }

    #[test]
    fn autoconfig_allows_a_greater_than_sign_inside_quoted_attributes() {
        let mut net = Net::default();
        let url = "https://autoconfig.example.test/mail/config-v1.1.xml?emailaddress=ada%40example.test";
        net.pages.insert(url.into(), br#"<clientConfig>
<incomingServer label="Team's > shared" type="imap"><hostname>imap.west.example.test</hostname><port>993</port><socketType>SSL</socketType></incomingServer>
<outgoingServer label='Team "west" > shared' type="smtp"><hostname>smtp.west.example.test</hostname><port>587</port><socketType>STARTTLS</socketType></outgoingServer>
</clientConfig>"#.to_vec());

        let found = discover("ada@example.test", &mut net).unwrap();

        assert_eq!(found.via, Via::Autoconfig);
        assert_eq!(found.settings, Settings::Imap {
            imap: Server { scheme: "imaps", host: "imap.west.example.test".into(), port: 993 },
            smtp: Server { scheme: "submission", host: "smtp.west.example.test".into(), port: 587 },
        });
        assert_eq!(net.asked, ["https://example.test/.well-known/jmap", url]);
    }

    #[test]
    fn autoconfig_ignores_comments_and_decodes_character_references() {
        let xml = br#"<clientConfig><emailProvider id="example.test">
<!-- <incomingServer type="imap"><hostname>unused.example.test</hostname><port>993</port><socketType>SSL</socketType></incomingServer> -->
<incomingServer type="&#105;map"><hostname>imap&#46;example<!-- comment -->.test</hostname><port>993</port><socketType>SSL</socketType></incomingServer>
<outgoingServer type="smtp"><hostname>smtp.example.test</hostname><port>587</port><socketType>STARTTLS</socketType></outgoingServer>
</emailProvider></clientConfig>"#;
        let (imap, smtp, oauth) = autoconfig(xml, "ada@example.test", "example.test").unwrap();
        assert_eq!(imap.host, "imap.example.test");
        assert_eq!(smtp.host, "smtp.example.test");
        assert_eq!(oauth, None);
    }

    #[test]
    fn autoconfig_does_not_mix_provider_blocks() {
        let xml = br#"<clientConfig>
<emailProvider id="incomplete"><incomingServer type="imap"><hostname>unused.example.test</hostname><port>993</port><socketType>SSL</socketType></incomingServer></emailProvider>
<emailProvider id="complete">
<incomingServer type="imap"><hostname>imap.example.test</hostname><port>993</port><socketType>SSL</socketType></incomingServer>
<outgoingServer type="smtp"><hostname>smtp.example.test</hostname><port>465</port><socketType>SSL</socketType></outgoingServer>
</emailProvider></clientConfig>"#;
        let (imap, smtp, _) = autoconfig(xml, "ada@example.test", "example.test").unwrap();
        assert_eq!(imap.host, "imap.example.test");
        assert_eq!(smtp.host, "smtp.example.test");
    }

    #[test]
    fn autoconfig_rejects_malformed_documents_and_dtds() {
        let valid = "<clientConfig><incomingServer type='imap'><hostname>imap.example.test</hostname><port>993</port><socketType>SSL</socketType></incomingServer><outgoingServer type='smtp'><hostname>smtp.example.test</hostname><port>465</port><socketType>SSL</socketType></outgoingServer></clientConfig>";
        for invalid in [
            valid.replace("</clientConfig>", ""),
            format!("<!DOCTYPE clientConfig [<!ELEMENT clientConfig ANY>]>{valid}"),
        ] {
            let mut net = Net::default();
            let url = "https://autoconfig.example.test/mail/config-v1.1.xml?emailaddress=ada%40example.test";
            net.pages.insert(url.into(), invalid.into_bytes());
            let found = discover("ada@example.test", &mut net).unwrap();
            assert_eq!(found.via, Via::Guess);
        }
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
