//! The one parser for "your own server" addresses (background-apps.md
//! §4.4). The address bar, navigatord and fetchd all use it, so there is
//! one answer to "is this a typed local address".
//!
//! It accepts exactly `http://localhost:PORT`, `http://127.0.0.1:PORT` and
//! `http://[::1]:PORT`, each with an optional path and query, and nothing
//! else: not the rest of 127/8, not `0.0.0.0`, not `::ffff:127.0.0.1`, not
//! `localhost.` or `*.localhost`, not https, not credentials, not a port
//! spelled with a leading zero. `localhost` is a spelling here, never a
//! name looked up anywhere.

/// Which loopback spelling the address used. The three are one origin
/// family but distinct origins, as in a browser.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Host {
    Localhost,
    V4,
    V6,
}

impl Host {
    pub fn as_str(self) -> &'static str {
        match self {
            Host::Localhost => "localhost",
            Host::V4 => "127.0.0.1",
            Host::V6 => "[::1]",
        }
    }
}

/// A local origin: scheme `http`, one of the three hosts, a port.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Origin {
    pub host: Host,
    pub port: u16,
}

impl Origin {
    /// `http://localhost:8080`, no trailing slash.
    pub fn spell(&self) -> String {
        format!("http://{}:{}", self.host.as_str(), self.port)
    }

    /// The `Host` header a request to it carries.
    pub fn host_header(&self) -> String {
        format!("{}:{}", self.host.as_str(), self.port)
    }
}

/// A parsed local address: its origin and its path with query (always
/// starting with `/`; the fragment dropped).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Local {
    pub origin: Origin,
    pub path: String,
}

impl Local {
    /// The address in its one spelling.
    pub fn url(&self) -> String {
        format!("{}{}", self.origin.spell(), self.path)
    }
}

/// A path or query byte: printable ASCII, no space, and none of the bytes
/// that would end or reshape a request line.
fn path_byte(b: u8) -> bool {
    (0x21..=0x7e).contains(&b) && !matches!(b, b'"' | b'<' | b'>' | b'\\' | b'^' | b'`' | b'{' | b'|' | b'}')
}

/// `url` as a local address, or `None` when it is not exactly one.
pub fn parse(url: &str) -> Option<Local> {
    if url.len() > crate::URL_MAX {
        return None;
    }
    let scheme = url.get(..7)?;
    if !scheme.eq_ignore_ascii_case("http://") {
        return None;
    }
    let rest = &url[7..];
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let (host, port) = if let Some(h) = authority.strip_prefix('[') {
        let (inside, after) = h.split_once(']')?;
        if inside != "::1" {
            return None;
        }
        (Host::V6, after.strip_prefix(':')?)
    } else {
        let (h, p) = authority.split_once(':')?;
        let host = if h.eq_ignore_ascii_case("localhost") {
            Host::Localhost
        } else if h == "127.0.0.1" {
            Host::V4
        } else {
            return None;
        };
        (host, p)
    };
    if port.is_empty() || port.len() > 5 || !port.bytes().all(|b| b.is_ascii_digit()) || port.starts_with('0') {
        return None;
    }
    let port: u16 = port.parse().ok()?;
    // The fragment never leaves the page.
    let tail = tail.split_once('#').map_or(tail, |(t, _)| t);
    if !tail.bytes().all(path_byte) {
        return None;
    }
    let path = if tail.is_empty() {
        "/".to_string()
    } else if tail.starts_with('?') {
        format!("/{tail}")
    } else {
        tail.to_string()
    };
    Some(Local { origin: Origin { host, port }, path })
}

/// `href` resolved against the local page `base`, when the result is a
/// local address of the same origin; anything else is `None` (it is not
/// this server's, so it takes the normal web path or is refused there).
pub fn join_same_origin(base: &Local, href: &str) -> Option<Local> {
    let href = href.trim();
    if href.len() > crate::URL_MAX {
        return None;
    }
    let joined = if href.starts_with("//") {
        format!("http:{href}")
    } else if href.starts_with('/') {
        format!("{}{href}", base.origin.spell())
    } else if href.starts_with('?') {
        let path = base.path.split_once('?').map_or(base.path.as_str(), |(p, _)| p);
        format!("{}{path}{href}", base.origin.spell())
    } else if href[..href.find(['/', '?', '#']).unwrap_or(href.len())].contains(':') {
        // An absolute address (a scheme before any path): judged as typed.
        href.to_string()
    } else {
        let path = base.path.split_once('?').map_or(base.path.as_str(), |(p, _)| p);
        let dir = path.rsplit_once('/').map_or("", |(d, _)| d);
        format!("{}{dir}/{href}", base.origin.spell())
    };
    let l = parse(&joined)?;
    if l.origin != base.origin || l.path.split(['?', '/']).any(|seg| seg == "..") {
        return None;
    }
    Some(l)
}

// ── cookies of your own server ─────────────────────────────────────────
// The fetcher passes on the cookies a local server sets in this shape, and
// navigatord keeps them per (jar, app, origin), in memory.

/// The longest cookie name and value passed on.
pub const SET_COOKIE_MAX: usize = 4096;
/// `Set-Cookie`s passed on from one response.
pub const SET_COOKIES_MAX: usize = 32;

/// A cookie the server set, as navigatord's jar keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetCookie {
    pub name: String,
    pub value: String,
    pub path: String,
    /// Seconds it lives; `None` for the session; 0 removes it.
    pub max_age: Option<u64>,
}

impl SetCookie {
    /// Its answer line (without the newline).
    pub fn line(&self) -> String {
        let age = self.max_age.map_or("-".to_string(), |a| a.to_string());
        format!("cookie {} {age} {}={}", self.path, self.name, self.value)
    }

    /// A line navigatord read back: refused unless it is in shape.
    pub fn from_line(line: &str) -> Option<SetCookie> {
        let mut f = line.strip_prefix("cookie ")?.splitn(3, ' ');
        let (path, age, pair) = (f.next()?, f.next()?, f.next()?);
        let (name, value) = pair.split_once('=')?;
        let max_age = if age == "-" { None } else { Some(age.parse().ok()?) };
        (path_ok(path) && token(name) && value.bytes().all(cookie_octet) && name.len() + value.len() <= SET_COOKIE_MAX)
            .then(|| SetCookie { name: name.into(), value: value.into(), path: path.into(), max_age })
    }
}

/// RFC 6265 §4.1.1 `token` (a cookie's name).
pub fn token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// RFC 6265 §4.1.1 `cookie-octet`.
pub fn cookie_octet(b: u8) -> bool {
    matches!(b, 0x21 | 0x23..=0x2b | 0x2d..=0x3a | 0x3c..=0x5b | 0x5d..=0x7e)
}

pub fn path_ok(p: &str) -> bool {
    p.starts_with('/') && p.len() <= 256 && p.bytes().all(|b| (0x21..=0x7e).contains(&b) && b != b';')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_spellings_with_a_port() {
        let l = parse("http://localhost:8080").unwrap();
        assert_eq!((l.origin.host, l.origin.port, l.path.as_str()), (Host::Localhost, 8080, "/"));
        assert_eq!(l.url(), "http://localhost:8080/");
        assert_eq!(parse("HTTP://LocalHost:8080/a?b=1#frag").unwrap().url(), "http://localhost:8080/a?b=1");
        assert_eq!(parse("http://127.0.0.1:9000/x").unwrap().origin, Origin { host: Host::V4, port: 9000 });
        assert_eq!(parse("http://[::1]:9000").unwrap().origin, Origin { host: Host::V6, port: 9000 });
        assert_eq!(parse("http://localhost:8080?q").unwrap().path, "/?q");
    }

    #[test]
    fn everything_else_is_refused() {
        for u in [
            "http://127.0.0.2:8080/", "http://0.0.0.0:8080/", "http://[::ffff:127.0.0.1]:8080/",
            "http://localhost.:8080/", "http://a.localhost:8080/", "http://localhost/", "http://localhost:/",
            "https://localhost:8080/", "http://user@localhost:8080/", "http://localhost:08080/",
            "http://localhost:65536/", "http://localhost:0/", "http://localhost:8080 /", "http://localhost:8080/a b",
            "http://[::1]/", "http://[0:0:0:0:0:0:0:1]:8080/", "http://2130706433:8080/", "http://127.1:8080/",
            "http://localhost:8080/\"", "localhost:8080", "ws://localhost:8080/", "http://localhost:8080\\@evil/",
        ] {
            assert!(parse(u).is_none(), "{u} must be refused");
        }
    }

    #[test]
    fn links_stay_on_the_origin_or_leave_it() {
        let b = parse("http://localhost:8080/app/index.html?x").unwrap();
        assert_eq!(join_same_origin(&b, "style.css").unwrap().url(), "http://localhost:8080/app/style.css");
        assert_eq!(join_same_origin(&b, "/img/a.png").unwrap().url(), "http://localhost:8080/img/a.png");
        assert_eq!(join_same_origin(&b, "?y").unwrap().url(), "http://localhost:8080/app/index.html?y");
        assert_eq!(join_same_origin(&b, "http://localhost:8080/z").unwrap().url(), "http://localhost:8080/z");
        for other in ["http://localhost:9090/", "//localhost:9090/", "http://127.0.0.1:8080/", "https://example.com/",
                      "javascript:alert(1)", "../../etc", "http://localhost:8080/../x"] {
            assert!(join_same_origin(&b, other).is_none(), "{other} is not this origin");
        }
    }
}
