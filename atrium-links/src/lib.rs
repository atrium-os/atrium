//! Which links may be opened in apps (docs/spec/opening-links.md).
//!
//! - [`declarable`]: a scheme an app may list in `[[links]]` (§4.1).
//! - [`never_taken`]: schemes no app takes and Monstrator refuses (§4.2).
//! - [`check`]: what Monstrator does to a link before anything else (§3.3):
//!   bounded, no control or direction characters, a scheme that may be
//!   handed on, and what its sheet shows of it.
//!
//! Portcullisd checks the scheme and matches the serialized authority when
//! a persona VPN crossing requires a broker verdict ([`may_leave_vpn`]).

/// The longest link handed on.
pub const MAX_URL: usize = 8192;
/// Most schemes one app may declare.
pub const MAX_SCHEMES: usize = 16;
/// The longest scheme name.
pub const MAX_SCHEME: usize = 32;
/// The Navigator's schemes: declared only with `navigator-broker` (D5).
pub const WEB: &[&str] = &["http", "https"];
/// Opens an installed app by its id (§4.3).
pub const APP: &str = "atrium-app";

/// Schemes that are commands, local files or the browser's own (§4.2).
const NEVER: &[&str] = &[
    "file", "javascript", "data", "vbscript", "about", "blob", "filesystem", "jar",
    "view-source", "shell", "chrome", "resource", "moz-extension", "chrome-extension",
    "livescript", "jscript", "wyciwyg", "res", "ftp", "smb", "nfs", "afp", "sftp",
    "ssh", "telnet", "atrium-consent", "atrium-doc",
];

/// `[a-z][a-z0-9+.-]*`, at most [`MAX_SCHEME`] bytes.
pub fn valid_scheme(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= MAX_SCHEME
        && b[0].is_ascii_lowercase()
        && b.iter().all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'+' | b'.' | b'-'))
}

/// A scheme no app ever takes: commands, local or shared files, the
/// browser's own, Windows' (`ms-*`), and Atrium's internal ones.
pub fn never_taken(s: &str) -> bool {
    let s = s.to_ascii_lowercase();
    NEVER.contains(&s.as_str()) || s.starts_with("ms-") || s.starts_with("x-atrium")
}

/// Whether an app may declare `s` in `[[links]]`; `Err` says why not.
/// [`WEB`] schemes are allowed here; the manifest check limits them to the
/// Navigator, and `atrium-app` to nobody (it names apps, it isn't taken).
pub fn declarable(s: &str) -> Result<(), String> {
    if !valid_scheme(s) {
        return Err(format!("{s:?}: a scheme is a lowercase letter, then letters, digits, + . or -, at most {MAX_SCHEME}"));
    }
    if never_taken(s) {
        return Err(format!("{s:?}: links of this kind are never opened in an app"));
    }
    if s == APP {
        return Err(format!("{APP}: opens apps; no app takes it"));
    }
    Ok(())
}

/// Whether `url` begins with `scheme:` (ASCII case ignored): the only look
/// portcullisd takes at a link.
pub fn starts_with_scheme(url: &str, scheme: &str) -> bool {
    let (b, s) = (url.as_bytes(), scheme.as_bytes());
    b.len() > s.len() && b[s.len()] == b':' && b[..s.len()].eq_ignore_ascii_case(s)
}

/// Characters a link may never carry: controls, and the ones that turn
/// text around or hide (bidi controls, zero-width, BOM).
fn hidden(c: char) -> bool {
    c.is_control()
        || c.is_whitespace()
        || matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}' | '\u{061C}')
}

/// A link that passed [`check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// As it will be handed on (the parser's form: lowercase scheme,
    /// ASCII host).
    pub url: String,
    pub scheme: String,
    /// For `atrium-app:` links, the app's id.
    pub app: Option<String>,
    /// What a sheet shows: the scheme and the host, or for a link without
    /// one (`mailto:`) the scheme and the start of the rest.
    pub shown: String,
}

/// The longest part of a host-less link a sheet shows.
const SHOWN_REST: usize = 64;

/// Check `text` as a link to hand on (§3.3, §4.2).
pub fn check(text: &str) -> Result<Link, String> {
    if text.len() > MAX_URL {
        return Err(format!("a link over {} KiB", MAX_URL / 1024));
    }
    if text.chars().any(hidden) {
        return Err("a link with control, space or hidden characters".into());
    }
    let u = url::Url::parse(text).map_err(|e| format!("not a link: {e}"))?;
    let scheme = u.scheme().to_string();
    if !valid_scheme(&scheme) || never_taken(&scheme) {
        return Err(format!("{scheme}: links of this kind are never opened in an app"));
    }
    if u.username() != "" || u.password().is_some() {
        return Err("a link with a name or password in it".into());
    }
    let app = if scheme == APP {
        let id = u.host_str().unwrap_or_default().to_string();
        let ok_id = !id.is_empty() && id.len() <= 128 && id.contains('.')
            && id.split('.').all(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-'));
        if !ok_id || !matches!(u.path(), "" | "/") || u.query().is_some() || u.fragment().is_some() {
            return Err("atrium-app: links name an app (atrium-app://org.example.app) and nothing else".into());
        }
        Some(id)
    } else {
        None
    };
    let shown = match u.host_str() {
        Some(h) if !h.is_empty() => format!("{scheme}://{}", shown_host(h)),
        _ => {
            let rest = u.path().split('?').next().unwrap_or_default();
            let mut r: String = rest.chars().take(SHOWN_REST).collect();
            if rest.chars().count() > SHOWN_REST {
                r.push('…');
            }
            format!("{scheme}:{r}")
        }
    };
    if u.as_str().len() > MAX_URL {
        return Err(format!("a link over {} KiB", MAX_URL / 1024));
    }
    Ok(Link { url: u.into(), scheme, app, shown })
}

/// A host as shown: its own letters when every label is Latin script (or
/// plain ASCII), else the ASCII (`xn--`) form, so a look-alike made of
/// another script's letters never passes for a Latin name (§3.3).
pub fn shown_host(ascii: &str) -> String {
    let (uni, res) = idna::domain_to_unicode(ascii);
    if res.is_err() {
        return ascii.to_string();
    }
    let latin = |c: char| c.is_ascii() || matches!(c as u32, 0xC0..=0x24F if c != '×' && c != '÷');
    if uni.split('.').all(|l| l.chars().all(latin)) {
        uni
    } else {
        ascii.to_string()
    }
}

/// Match the authority of Monstrator's serialized URL. Accept only the
/// unambiguous wire form here; URL normalization belongs in its jail.
pub fn may_leave_vpn(url: &str, scheme: &str, clear: impl FnOnce(&str) -> bool) -> bool {
    if scheme == APP { return true } // The app starts without a URL.
    let Some(rest) = url.strip_prefix(scheme).and_then(|s| s.strip_prefix(':')) else { return false };
    let Some(tail) = rest.strip_prefix("//") else {
        return !WEB.contains(&scheme) && !rest.contains('\\');
    };
    let authority = tail.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains(['@', '\\', '%']) { return false }
    let (host, suffix) = if let Some(v6) = authority.strip_prefix('[') {
        let Some((host, port)) = v6.split_once(']') else { return false };
        if host.parse::<std::net::Ipv6Addr>().is_err() { return false }
        (host, port)
    } else {
        let pos = authority.find(':').unwrap_or(authority.len());
        let (name, port) = authority.split_at(pos);
        let host = name.strip_suffix('.').unwrap_or(name);
        if host.is_empty() || host.len() > 253 || !host.split('.').all(|label|
            !label.is_empty() && label.len() <= 63 && !label.starts_with('-') && !label.ends_with('-')
            && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')) { return false }
        // Special-scheme URL parsers interpret a numeric final label as
        // legacy IPv4 syntax. Only canonical dotted IPv4 may take that path.
        let last = host.rsplit('.').next().unwrap_or("");
        let hex = last.to_ascii_lowercase();
        let numeric_hex = hex.strip_prefix("0x").is_some_and(|digits| digits.bytes().all(|b| b.is_ascii_hexdigit()));
        if (last.bytes().all(|b| b.is_ascii_digit()) || numeric_hex)
            && host.parse::<std::net::Ipv4Addr>().is_err() { return false }
        (host, port)
    };
    if !suffix.is_empty() && !suffix.strip_prefix(':').is_some_and(|p|
        p.bytes().all(|b| b.is_ascii_digit()) && p.parse::<u16>().is_ok()) { return false }
    clear(&host.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vpn_verdict_covers_the_delivered_authority() {
        assert!(may_leave_vpn("https://WWW.example.:443/a?x=other", "https", |h| h == "www.example"));
        assert!(!may_leave_vpn("https://outside.example/a", "https", |_| false));
        for u in ["https:host", "https://safe.test@work.test/", "https://host%2fother/", "https://127.1/", "https://safe.test:99999/"] {
            assert!(!may_leave_vpn(u, "https", |_| true), "{u}");
        }
        assert!(may_leave_vpn("https://[::ffff:10.0.0.1]/", "https", |h| h == "::ffff:10.0.0.1"));
        assert!(may_leave_vpn("mailto:ana@example.com", "mailto", |_| panic!("no network host")));
    }

    #[test]
    fn schemes() {
        assert!(declarable("mailto").is_ok());
        assert!(declarable("org.example.notes").is_ok());
        assert!(declarable("https").is_ok(), "the manifest check limits it");
        for s in ["file", "javascript", "JavaScript", "data", "ms-settings", "about", "atrium-app", "x-atrium-y", "", "1abc", "a b", "Mailto", &"a".repeat(33)] {
            assert!(declarable(s).is_err(), "{s:?}");
        }
        assert!(starts_with_scheme("MailTo:a@b", "mailto"));
        assert!(!starts_with_scheme("mailto", "mailto"));
        assert!(!starts_with_scheme("mailtox:a", "mailto"));
    }

    #[test]
    fn links_checked() {
        let l = check("mailto:ana@example.org?subject=Hi").unwrap();
        assert_eq!((l.scheme.as_str(), l.shown.as_str(), l.app), ("mailto", "mailto:ana@example.org", None));
        let l = check("HTTPS://Example.ORG/a?b#c").unwrap();
        assert_eq!((l.scheme.as_str(), l.shown.as_str()), ("https", "https://example.org"));
        let l = check("atrium-app://org.example.notes").unwrap();
        assert_eq!(l.app.as_deref(), Some("org.example.notes"));
        for bad in [
            "javascript:alert(1)", "file:///etc/passwd", "data:text/html,x", "ms-settings:x",
            "mailto:a@b\nbcc:c@d", "mailto:a@b\u{202E}gro", "https://user:pw@example.org/",
            "atrium-app://org.example.notes/../x", "atrium-app://notes", "not a link", "https://a b/",
        ] {
            assert!(check(bad).is_err(), "{bad:?}");
        }
        assert!(check(&format!("mailto:{}", "a".repeat(MAX_URL))).is_err());
        let long = check(&format!("mailto:{}@example.org", "a".repeat(100))).unwrap();
        assert!(long.shown.ends_with('…') && long.shown.chars().count() == "mailto:".len() + SHOWN_REST + 1);
    }

    #[test]
    fn look_alike_hosts_are_shown_in_ascii() {
        // Latin with accents: its own letters.
        assert_eq!(check("https://bücher.example/").unwrap().shown, "https://bücher.example");
        // Cyrillic letters that look Latin ("аpple"): the xn-- form.
        let s = check("https://\u{0430}pple.com/").unwrap().shown;
        assert!(s.starts_with("https://xn--"), "{s}");
        // A whole label in another script: also the ASCII form.
        assert!(check("https://пример.рф/").unwrap().shown.contains("xn--"));
    }
}
