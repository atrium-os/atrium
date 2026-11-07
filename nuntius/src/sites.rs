//! ★ Per-app site limits (docs/spec/nuntius.md §4, "Origins"): an app's
//! installed manifest may narrow its fetcher to listed sites with
//! `net-fetch-sites`. portcullisd reads the list from the INSTALLED manifest
//! (never from the caller) and hands it to the fetcher at spawn in
//! [`SITES_ENV`]; the fetcher checks every hop, redirects included, before
//! any lookup or connect.
//!
//! An entry is a host name, matched exactly (`example.com`), or `*.` and a
//! name, matching every name below it but not the name itself
//! (`*.example.com`). Names are compared in ASCII lower case; an
//! internationalised name is written in its `xn--` form. A literal address,
//! a port, a scheme or a path is not an entry: the address rule and the
//! scheme and port rules apply as for every client, and a site list only
//! takes away.

/// The environment variable portcullisd passes the list in: entries
/// separated by commas. Present and empty means "no sites at all".
pub const SITES_ENV: &str = "ATRIUM_NUNTIUS_SITES";
/// At most this many entries.
pub const MAX_SITES: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Entry { Exact(String), Below(String) }

/// A parsed site list. Unrestricted fetchers have none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sites(Vec<Entry>);

impl Sites {
    /// Parse a list. Any bad entry refuses the whole list: a fetcher whose
    /// limits cannot be read does not start rather than start unlimited.
    pub fn parse<S: AsRef<str>>(entries: &[S]) -> Result<Sites, String> {
        if entries.len() > MAX_SITES { return Err(format!("more than {MAX_SITES} sites")) }
        let mut out = Vec::with_capacity(entries.len());
        for e in entries {
            let e = e.as_ref().trim();
            let (below, name) = match e.strip_prefix("*.") { Some(n) => (true, n), None => (false, e) };
            let name = name.to_ascii_lowercase();
            valid_name(&name).map_err(|why| format!("site {e:?}: {why}"))?;
            out.push(if below { Entry::Below(name) } else { Entry::Exact(name) });
        }
        Ok(Sites(out))
    }

    /// Parse [`SITES_ENV`]'s form: entries separated by commas.
    pub fn from_env_value(v: &str) -> Result<Sites, String> {
        let list: Vec<&str> = v.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
        Sites::parse(&list)
    }

    /// The form [`from_env_value`](Self::from_env_value) reads.
    pub fn to_env_value(&self) -> String {
        self.0.iter().map(|e| match e { Entry::Exact(n) => n.clone(), Entry::Below(n) => format!("*.{n}") })
            .collect::<Vec<_>>().join(",")
    }

    /// Whether `host` (a URL's host, as `url` writes it) is one of the
    /// sites. A literal address never is.
    pub fn admits(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if host.starts_with('[') || host.parse::<std::net::IpAddr>().is_ok() { return false }
        self.0.iter().any(|e| match e {
            Entry::Exact(n) => host == *n,
            Entry::Below(n) => host.len() > n.len() + 1 && host.ends_with(n.as_str())
                && host.as_bytes()[host.len() - n.len() - 1] == b'.',
        })
    }

    pub fn is_empty(&self) -> bool { self.0.is_empty() }
}

fn valid_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() || name.len() > 253 { return Err("not a host name") }
    if name.parse::<std::net::IpAddr>().is_ok() { return Err("an address is not a site") }
    for label in name.split('.') {
        if label.is_empty() || label.len() > 63 { return Err("empty or overlong label") }
        if !label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err("only letters, digits and '-' (write an IDN in its xn-- form)")
        }
        if label.starts_with('-') || label.ends_with('-') { return Err("a label starts or ends with '-'") }
    }
    if !name.contains('.') { return Err("a single label is not a site") }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_and_below() {
        let s = Sites::parse(&["Example.com", "*.cdn.example.net"]).unwrap();
        assert!(s.admits("example.com"));
        assert!(s.admits("EXAMPLE.com."));
        assert!(!s.admits("www.example.com"));
        assert!(!s.admits("badexample.com"));
        assert!(s.admits("a.cdn.example.net"));
        assert!(s.admits("x.y.cdn.example.net"));
        assert!(!s.admits("cdn.example.net"));
        assert!(!s.admits("evilcdn.example.net"));
        assert!(!s.admits("cdn.example.net.evil.com"));
    }

    #[test]
    fn addresses_and_junk_are_refused() {
        for bad in ["1.2.3.4", "*.", "", "com", "a..b", "exa mple.com", "*.*.x.com", "x.com:443",
                    "https://x.com", "x.com/a", "-x.com", "ünï.com"] {
            assert!(Sites::parse(&[bad]).is_err(), "{bad:?} accepted");
        }
        let s = Sites::parse(&["example.com"]).unwrap();
        assert!(!s.admits("[::1]"));
        assert!(!s.admits("93.184.216.34"));
    }

    #[test]
    fn env_round_trip_and_empty() {
        let s = Sites::parse(&["a.com", "*.b.org"]).unwrap();
        assert_eq!(Sites::from_env_value(&s.to_env_value()).unwrap(), s);
        let none = Sites::from_env_value("").unwrap();
        assert!(none.is_empty() && !none.admits("a.com"));
        assert!(Sites::parse(&vec!["a.com"; MAX_SITES + 1]).is_err());
    }
}
