//! The link bar (mail.md §7.6): a press on a link never opens it; it shows
//! where the link goes, peeled of known redirect wrappers, and *Open* (or
//! Enter) is Mail's press for Monstrator. What opens is what the bar shows:
//! the peeled destination, without tracking parameters.

use url::Url;

/// What the bar shows for a link, and what *Open* opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bar {
    /// What opens: peeled, without tracking parameters, checked as a link
    /// Monstrator takes.
    pub open: String,
    /// `https://`, `mailto:`.
    pub lead: String,
    /// The host before its registrable domain (`dash.`), drawn light.
    pub sub: String,
    /// The registrable domain (`acme.example`), or a mailto's address,
    /// drawn bold.
    pub domain: String,
    /// The path and the rest, drawn light.
    pub rest: String,
    /// The redirector the link went through, when it was peeled (or, for
    /// Mimecast, which keeps the destination on its own servers, when it
    /// couldn't be).
    pub via: Option<&'static str>,
    /// The link's visible text, when it names a different site (§7.6
    /// "Mismatch"): the bar shows both, the destination first.
    pub says: Option<String>,
    /// Company link scanning kept (§7.6): the scanner the link opens
    /// through, as it came; the bar still shows where it goes.
    pub scanned: Option<&'static str>,
}

/// The company link scanners (§7.6): with scanning kept, a link through
/// one of them opens through it.
pub const SCANNERS: [&str; 3] = ["Microsoft Safe Links", "Proofpoint URL Defense", "Mimecast"];

/// The company scanner `link` goes through first, if any.
pub fn scanned_by(link: &str) -> Option<&'static str> {
    let u = web(link)?;
    if mimecast(&u) {
        return Some("Mimecast");
    }
    unwrap_once(&u).map(|(_, who)| who).filter(|w| SCANNERS.contains(w))
}

/// [`bar`], with company scanning kept when `keep` (a named persona's
/// default): a scanned link opens as it came, through the scanner.
pub fn bar_kept(link: &str, text: Option<&str>, keep: bool) -> Result<Bar, String> {
    let mut b = bar(link, text)?;
    if keep {
        if let Some(s) = scanned_by(link) {
            b.open = atrium_links::check(link.trim())?.url;
            b.scanned = Some(s);
        }
    }
    Ok(b)
}

/// Wrappers inside wrappers: peeled at most this many times.
const PEEL_MAX: usize = 4;

/// Query parameters that only track (removed from the link that opens).
const TRACKING: &[&str] = &[
    "fbclid", "gclid", "dclid", "gbraid", "wbraid", "msclkid", "yclid", "twclid", "ttclid", "igshid",
    "mc_eid", "mc_cid", "_hsenc", "_hsmi", "mkt_tok", "oly_anon_id", "oly_enc_id", "vero_id", "vero_conv",
    "wickedid", "__s", "_kx", "ss_source", "ss_campaign_id", "ml_subscriber", "ml_subscriber_hash",
];

fn web(s: &str) -> Option<Url> {
    let u = Url::parse(s.trim()).ok()?;
    matches!(u.scheme(), "http" | "https").then_some(u)
}

fn param(u: &Url, names: &[&str]) -> Option<String> {
    u.query_pairs().find(|(k, _)| names.contains(&k.as_ref())).map(|(_, v)| v.into_owned())
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One known wrapper taken off: the destination it carries, and its name.
fn unwrap_once(u: &Url) -> Option<(Url, &'static str)> {
    let host = u.host_str()?.to_ascii_lowercase();
    let path = u.path();
    let found = |names: &[&str], who: &'static str| param(u, names).and_then(|t| web(&t)).map(|t| (t, who));
    if host.ends_with(".safelinks.protection.outlook.com") {
        return found(&["url"], "Microsoft Safe Links");
    }
    if host == "urldefense.proofpoint.com" && path.starts_with("/v2/url") {
        // v2: `-` stands for `%` and `_` for `/`.
        let t = param(u, &["u"])?.replace('_', "/").replace('-', "%");
        return web(&percent_decode(&t)).map(|t| (t, "Proofpoint URL Defense"));
    }
    if host == "urldefense.com" {
        // v3: `https://urldefense.com/v3/__<link>__;<bytes>!!<sig>`; a `*`
        // in the link stands for a byte after the `;`, so those are left.
        let s = u.as_str();
        let at = s.find("/v3/__")? + "/v3/__".len();
        let rest = &s[at..];
        let end = rest.find("__;").or_else(|| rest.rfind("__"))?;
        let t = &rest[..end];
        if t.contains('*') {
            return None;
        }
        return web(t).map(|t| (t, "Proofpoint URL Defense"));
    }
    let google = host == "google.com" || host.starts_with("www.google.") || host.starts_with("google.");
    if google && path == "/url" {
        return found(&["q", "url"], "Google");
    }
    match (host.as_str(), path) {
        ("l.facebook.com" | "lm.facebook.com", "/l.php") => found(&["u"], "Facebook"),
        ("l.instagram.com", _) => found(&["u"], "Instagram"),
        ("www.youtube.com" | "youtube.com", "/redirect") => found(&["q"], "YouTube"),
        ("slack-redir.net", "/link") => found(&["url"], "Slack"),
        ("out.reddit.com", _) => found(&["url"], "Reddit"),
        ("t.umblr.com", "/redirect") => found(&["z"], "Tumblr"),
        ("steamcommunity.com", "/linkfilter/") => found(&["url", "u"], "Steam"),
        _ => None,
    }
}

/// Mimecast keeps the destination on its own servers: the link can't be
/// peeled, and the bar says it goes through Mimecast.
fn mimecast(u: &Url) -> bool {
    u.host_str().is_some_and(|h| {
        let h = h.to_ascii_lowercase();
        (h.starts_with("protect-") || h.starts_with("protect.")) && h.ends_with(".mimecast.com")
    })
}

/// The destination under known redirect wrappers, and the last wrapper's name.
pub fn peel(link: &str) -> (String, Option<&'static str>) {
    let Some(mut u) = web(link) else { return (link.to_string(), None) };
    let mut via = None;
    for _ in 0..PEEL_MAX {
        match unwrap_once(&u) {
            Some((t, who)) => {
                u = t;
                via = Some(who);
            }
            None => break,
        }
    }
    if via.is_none() && mimecast(&u) {
        via = Some("Mimecast");
    }
    (u.into(), via)
}

/// `link` without tracking parameters (`utm_*` and [`TRACKING`]).
pub fn strip_tracking(link: &str) -> String {
    let Some(mut u) = web(link) else { return link.to_string() };
    if u.query().is_none() {
        return u.into();
    }
    let all: Vec<(String, String)> = u.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    let keep: Vec<&(String, String)> = all.iter().filter(|(k, _)| !(k.to_ascii_lowercase().starts_with("utm_") || TRACKING.contains(&k.to_ascii_lowercase().as_str()))).collect();
    if keep.len() == all.len() {
        return u.into();
    }
    if keep.is_empty() {
        u.set_query(None);
    } else {
        u.query_pairs_mut().clear().extend_pairs(keep.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    }
    u.into()
}

/// The registrable domain of a host (`dash.acme.example` → `acme.example`),
/// or the host itself when it has none (an address, an unknown suffix).
fn registrable(host: &str) -> String {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    if let Some(d) = known_domain(&h) {
        return d;
    }
    // An unknown suffix (or an address): the last two labels, as most are.
    if h.parse::<std::net::IpAddr>().is_ok() || h.starts_with('[') {
        return h;
    }
    let labels: Vec<&str> = h.split('.').collect();
    labels[labels.len().saturating_sub(2)..].join(".")
}

/// The registrable domain under a suffix the list knows (the list's
/// default rule would make `report.pdf` a domain).
fn known_domain(h: &str) -> Option<String> {
    let d = psl::domain(h.as_bytes())?;
    if !d.suffix().is_known() {
        return None;
    }
    std::str::from_utf8(d.as_bytes()).ok().map(str::to_string)
}

/// The site a link's visible text names, if it names one: a URL, or a bare
/// domain with a known suffix ("acme.example/login", "www.bank.com").
fn site_named(text: &str) -> Option<String> {
    let t = text.trim().trim_end_matches(['.', ',', ';', ':', ')', '>']).trim_start_matches(['(', '<']);
    if t.is_empty() || t.contains(char::is_whitespace) {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Url::parse(t).ok()?.host_str().map(registrable);
    }
    let u = {
        // A bare domain: letters, digits, dots and dashes before any path.
        let host = t.split(['/', '?', '#']).next()?;
        if !host.contains('.') || !host.chars().all(|c| c.is_alphanumeric() || c == '.' || c == '-') {
            return None;
        }
        Url::parse(&format!("https://{t}")).ok()?
    };
    let h = u.host_str()?.to_ascii_lowercase();
    known_domain(&h)
}

/// The bar for a pressed link and its visible text. `Err`: a link
/// Monstrator would refuse, said why.
pub fn bar(link: &str, text: Option<&str>) -> Result<Bar, String> {
    let (peeled, via) = peel(link);
    let open = strip_tracking(&peeled);
    let checked = atrium_links::check(&open)?;
    let open = checked.url;
    let u = Url::parse(&open).map_err(|e| e.to_string())?;
    let (lead, sub, domain, rest) = match u.host_str() {
        Some(h) if !h.is_empty() => {
            let shown = atrium_links::shown_host(h);
            let reg = registrable(h);
            // Split what is shown at the registrable domain's labels.
            let n = reg.split('.').count();
            let labels: Vec<&str> = shown.split('.').collect();
            let cut = labels.len().saturating_sub(n);
            let sub = if cut > 0 { format!("{}.", labels[..cut].join(".")) } else { String::new() };
            let domain = labels[cut..].join(".");
            let mut rest = String::new();
            if let Some(p) = u.port() {
                rest.push_str(&format!(":{p}"));
            }
            if u.path() != "/" || u.query().is_some() || u.fragment().is_some() {
                rest.push_str(&open[u[..url::Position::BeforePath].len()..]);
            }
            (format!("{}://", u.scheme()), sub, domain, rest)
        }
        _ => {
            let body = &open[u.scheme().len() + 1..];
            let (head, tail) = match body.find('?') {
                Some(i) => (&body[..i], &body[i..]),
                None => (body, ""),
            };
            (format!("{}:", u.scheme()), String::new(), percent_decode(head), tail.to_string())
        }
    };
    let says = text.and_then(|t| {
        let named = site_named(t)?;
        let dest = u.host_str().map(registrable);
        (dest.as_deref() != Some(named.as_str())).then(|| t.trim().to_string())
    });
    Ok(Bar { open, lead, sub, domain, rest, via, says, scanned: None })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn company_scanning_kept() {
        let safe = "https://eur01.safelinks.protection.outlook.com/?url=https%3A%2F%2Fdash.acme.example%2Fq4&data=05";
        assert_eq!(scanned_by(safe), Some("Microsoft Safe Links"));
        let b = bar_kept(safe, None, true).unwrap();
        assert_eq!((b.open.as_str(), b.domain.as_str(), b.scanned), (safe, "acme.example", Some("Microsoft Safe Links")));
        let b = bar_kept(safe, None, false).unwrap();
        assert_eq!((b.open.as_str(), b.scanned), ("https://dash.acme.example/q4", None));
        // Not a company scanner: peeled either way.
        let g = "https://www.google.com/url?q=https%3A%2F%2Facme.example%2F";
        assert_eq!(scanned_by(g), None);
        assert_eq!(bar_kept(g, None, true).unwrap().open, "https://acme.example/");
        let pp = "https://urldefense.proofpoint.com/v2/url?u=https-3A__acme.example_a&d=x";
        assert_eq!(scanned_by(pp), Some("Proofpoint URL Defense"));
        assert_eq!(bar_kept(pp, None, true).unwrap().open, pp);
        let mc = "https://protect-eu.mimecast.com/s/abc?domain=acme.example";
        assert_eq!(bar_kept(mc, None, true).unwrap().scanned, Some("Mimecast"));
    }

    #[test]
    fn wrappers_are_peeled() {
        let safe = "https://eur01.safelinks.protection.outlook.com/?url=https%3A%2F%2Fdash.acme.example%2Fq4%3Fx%3D1&data=05%7C01&reserved=0";
        assert_eq!(peel(safe), ("https://dash.acme.example/q4?x=1".into(), Some("Microsoft Safe Links")));
        let v2 = "https://urldefense.proofpoint.com/v2/url?u=https-3A__acme.example_login&d=DwMF&c=x";
        assert_eq!(peel(v2).0, "https://acme.example/login");
        let v3 = "https://urldefense.com/v3/__https://acme.example/report__;!!abc$";
        assert_eq!(peel(v3).0, "https://acme.example/report");
        let g = "https://www.google.com/url?q=https://acme.example/a&sa=D";
        assert_eq!(peel(g), ("https://acme.example/a".into(), Some("Google")));
        // Inside one another.
        let both = format!("https://www.google.com/url?q={}", url::form_urlencoded::byte_serialize(safe.as_bytes()).collect::<String>());
        assert_eq!(peel(&both).0, "https://dash.acme.example/q4?x=1");
        // Not a wrapper: left alone, even with a url= in it.
        let share = "https://social.example/share?url=https://acme.example";
        assert_eq!(peel(share), (share.to_string(), None));
        // A redirector carrying something that isn't a web link is left.
        assert_eq!(peel("https://www.google.com/url?q=javascript:alert(1)").1, None);
        assert_eq!(peel("https://protect-eu.mimecast.com/s/AbCd?domain=acme.example").1, Some("Mimecast"));
    }

    #[test]
    fn tracking_is_removed() {
        assert_eq!(strip_tracking("https://acme.example/p?id=7&utm_source=news&utm_medium=email&fbclid=x"), "https://acme.example/p?id=7");
        assert_eq!(strip_tracking("https://acme.example/p?utm_source=a&gclid=b"), "https://acme.example/p");
        assert_eq!(strip_tracking("https://acme.example/p?q=1"), "https://acme.example/p?q=1");
        assert_eq!(strip_tracking("mailto:a@b.example?subject=Hi"), "mailto:a@b.example?subject=Hi");
    }

    #[test]
    fn the_bar() {
        let b = bar("https://eur01.safelinks.protection.outlook.com/?url=https%3A%2F%2Fdash.acme.example%2Fq4%3Futm_source%3Dx", Some("dash.acme.example")).unwrap();
        assert_eq!((b.lead.as_str(), b.sub.as_str(), b.domain.as_str(), b.rest.as_str()), ("https://", "dash.", "acme.example", "/q4"));
        assert_eq!(b.open, "https://dash.acme.example/q4");
        assert_eq!(b.via, Some("Microsoft Safe Links"));
        assert_eq!(b.says, None, "the text names the same site");
        // A mismatch: the text names the bank, the link goes elsewhere.
        let b = bar("https://login.evil.example/x", Some("https://www.bank.com/login")).unwrap();
        assert_eq!(b.domain, "evil.example");
        assert_eq!(b.says.as_deref(), Some("https://www.bank.com/login"));
        assert_eq!(bar("https://login.evil.example/x", Some("www.bank.com")).unwrap().says.as_deref(), Some("www.bank.com"));
        // Words aren't a site.
        assert_eq!(bar("https://acme.example/", Some("see the sale")).unwrap().says, None);
        assert_eq!(bar("https://acme.example/", Some("report.pdf")).unwrap().says, None);
        // A bare root shows no path.
        assert_eq!(bar("https://acme.example/", None).unwrap().rest, "");
        let m = bar("mailto:priya@example.com?subject=Q4", None).unwrap();
        assert_eq!((m.lead.as_str(), m.domain.as_str(), m.rest.as_str()), ("mailto:", "priya@example.com", "?subject=Q4"));
        // Never opened in an app.
        assert!(bar("javascript:alert(1)", None).is_err());
        assert!(bar("file:///etc/passwd", None).is_err());
    }
}
