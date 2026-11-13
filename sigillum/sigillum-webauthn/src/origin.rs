//! The origin the broker knows, and which RP IDs it may use (WebAuthn L3
//! §5.1.3 step 7 and §5.1.4 step 6: "is a registrable domain suffix of or
//! is equal to"; HTML's definition, which refuses public suffixes).

use crate::DomError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scheme {
    Https,
    /// Only for `localhost` and `*.localhost`, which are secure contexts.
    Http,
}

/// A tuple origin. `host` is lowercase ASCII (IDNs already in punycode, as
/// the courier connected to them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub scheme: Scheme,
    pub host: String,
    /// `None` for the scheme's default port.
    pub port: Option<u16>,
}

fn is_localhost(h: &str) -> bool {
    h == "localhost" || h.ends_with(".localhost")
}

impl Origin {
    /// Parse a serialized origin (`https://login.example.com`,
    /// `http://localhost:8080`). No path, user info or trailing slash.
    pub fn parse(s: &str) -> Option<Origin> {
        let (scheme, rest) = if let Some(r) = s.strip_prefix("https://") {
            (Scheme::Https, r)
        } else if let Some(r) = s.strip_prefix("http://") {
            (Scheme::Http, r)
        } else {
            return None;
        };
        let (host, port) = match rest.rsplit_once(':') {
            Some((h, p)) => {
                let p: u16 = p.parse().ok().filter(|p| *p != 0)?;
                if !p.to_string().eq(&rest[h.len() + 1..]) {
                    return None;
                }
                (h, Some(p))
            }
            None => (rest, None),
        };
        if !sigillum_proto::rp::valid_domain(host) {
            return None;
        }
        let default = match scheme {
            Scheme::Https => 443,
            Scheme::Http => 80,
        };
        Some(Origin { scheme, host: host.to_string(), port: port.filter(|p| *p != default) })
    }

    /// The ASCII serialization, for `clientDataJSON.origin`.
    pub fn serialize(&self) -> String {
        let s = match self.scheme {
            Scheme::Https => "https",
            Scheme::Http => "http",
        };
        match self.port {
            Some(p) => format!("{s}://{}:{p}", self.host),
            None => format!("{s}://{}", self.host),
        }
    }

    /// A secure context: `https`, or `http` on localhost.
    pub fn is_secure(&self) -> bool {
        self.scheme == Scheme::Https || is_localhost(&self.host)
    }

    /// The RP ID for a call from this origin: the one the page asked for, if
    /// it is this origin's to use, else `SecurityError`; the host itself if
    /// it asked for none.
    pub fn rp_id(&self, asked: Option<&str>) -> Result<String, DomError> {
        if !self.is_secure() {
            return Err(DomError::Security);
        }
        let Some(rp) = asked else { return Ok(self.host.clone()) };
        if !sigillum_proto::rp::valid_domain(rp) {
            return Err(DomError::Security);
        }
        if rp == self.host {
            return Ok(rp.to_string());
        }
        // A registrable suffix: a parent of the host that is not a public
        // suffix (com, co.uk, github.io, and unknown TLDs).
        if self.host.ends_with(&format!(".{rp}")) && !is_public_suffix(rp) {
            return Ok(rp.to_string());
        }
        Err(DomError::Security)
    }
}

/// Whether `d` is a public suffix. Unknown TLDs count as suffixes (the
/// list's implicit `*` rule), so they are never an RP ID for a subdomain.
pub fn is_public_suffix(d: &str) -> bool {
    if is_localhost(d) {
        return d == "localhost";
    }
    psl::suffix_str(d) == Some(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(s: &str) -> Origin {
        Origin::parse(s).unwrap()
    }

    #[test]
    fn parses_and_serializes() {
        assert_eq!(o("https://example.com").serialize(), "https://example.com");
        assert_eq!(o("https://example.com:443").serialize(), "https://example.com", "default port dropped");
        assert_eq!(o("https://example.com:8443").serialize(), "https://example.com:8443");
        assert_eq!(o("http://localhost:8080").serialize(), "http://localhost:8080");
        for bad in ["example.com", "ftp://example.com", "https://example.com/", "https://Example.com", "https://u@example.com", "https://example.com:0", "https://example.com:08443", "https://10.0.0.1", "https://example.com:x"] {
            assert!(Origin::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn rp_id_is_the_host_or_a_registrable_suffix() {
        let login = o("https://login.example.com");
        assert_eq!(login.rp_id(None).unwrap(), "login.example.com");
        assert_eq!(login.rp_id(Some("login.example.com")).unwrap(), "login.example.com");
        assert_eq!(login.rp_id(Some("example.com")).unwrap(), "example.com");
        for bad in ["com", "evil.com", "ample.com", "other.example.com", "a.login.example.com", "Example.com", ""] {
            assert_eq!(login.rp_id(Some(bad)), Err(DomError::Security), "{bad}");
        }
        // Public suffixes are never an RP ID for a subdomain.
        assert_eq!(o("https://shop.example.co.uk").rp_id(Some("co.uk")), Err(DomError::Security));
        assert_eq!(o("https://shop.example.co.uk").rp_id(Some("example.co.uk")).unwrap(), "example.co.uk");
        assert_eq!(o("https://alice.github.io").rp_id(Some("github.io")), Err(DomError::Security));
        assert_eq!(o("https://a.b.unknown-tld-xyz").rp_id(Some("unknown-tld-xyz")), Err(DomError::Security));
    }

    #[test]
    fn only_secure_origins() {
        assert_eq!(o("http://example.com").rp_id(None), Err(DomError::Security));
        assert_eq!(o("http://localhost:3000").rp_id(None).unwrap(), "localhost");
        assert_eq!(o("http://app.localhost").rp_id(Some("app.localhost")).unwrap(), "app.localhost");
        // `localhost` is kept a suffix of its own: no parent RP ID for it.
        assert_eq!(o("http://app.localhost").rp_id(Some("localhost")), Err(DomError::Security));
    }
}
