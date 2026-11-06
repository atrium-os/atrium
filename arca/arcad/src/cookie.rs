//! Cookies (arca.md §8.1): Set-Cookie validation and matching, by Arca.
//!
//! RFC 6265bis, with Arca's stricter choices:
//! - every courier is https, so `Secure` is always acceptable and a cookie
//!   is only ever sent over https;
//! - a `Domain` must domain-match the courier's host and must not be a
//!   public suffix (the list is built in, `psl`); a `Domain` equal to a
//!   public-suffix host makes a host-only cookie, as browsers do;
//! - `__Secure-` and `__Host-` are enforced; `SameSite=None` needs `Secure`;
//! - lifetimes are capped at 400 days, sizes at 4 KiB, counts per group.
//!
//! A cookie is a store item: recipient = (https, its domain, the courier's
//! port, partition), account = name, value = value, scope = [`Attrs`]. So a
//! `Domain=example.com` cookie lives in example.com's group and a host-only
//! one in its host's; matching walks the request host's parent domains up
//! to, not including, the public suffix.

use arca_store::secret::LockedBuf;

/// The longest a cookie may live (RFC 6265bis §5.5).
pub const MAX_AGE_CAP: u64 = 400 * 86_400;
/// name + value.
pub const MAX_SIZE: usize = 4096;
/// Cookies per (domain, partition) group; the oldest goes first.
pub const PER_GROUP: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SameSite {
    /// No attribute: treated as Lax.
    Default = 0,
    Strict = 1,
    Lax = 2,
    None = 3,
}

/// What a cookie item's scope holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attrs {
    pub host_only: bool,
    pub secure: bool,
    pub http_only: bool,
    pub same_site: SameSite,
    /// Unix seconds; `None` for a session cookie.
    pub expires: Option<u64>,
    /// Unix seconds by Arca's clock; a replacement keeps the original's
    /// (RFC 6265 §5.3 step 11). Orders the header and the eviction.
    pub created: u64,
    pub path: String,
}

/// A validated Set-Cookie.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetCookie {
    pub name: String,
    pub value: Vec<u8>,
    /// The group's host: the request host, or the `Domain`.
    pub domain: String,
    pub attrs: Attrs,
}

impl SetCookie {
    /// An expiry at or before now: the cookie is to be deleted, not kept.
    pub fn expired(&self, now: u64) -> bool {
        self.attrs.expires.is_some_and(|e| e <= now)
    }
}

/// Why a Set-Cookie was ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    Name,
    Value,
    TooLarge,
    /// `Domain` doesn't domain-match the host, or is a public suffix.
    Domain,
    /// `__Secure-` / `__Host-` rules.
    Prefix,
    /// `SameSite=None` without `Secure`.
    SameSiteNone,
}

const FLAG_HOST_ONLY: u8 = 1;
const FLAG_SECURE: u8 = 2;
const FLAG_HTTP_ONLY: u8 = 4;

impl Attrs {
    /// `u8 version (1) | u8 flags | u8 samesite | u64 expires (0 = session) | u64 created | u16 len | path`
    pub fn encode(&self) -> Vec<u8> {
        let mut o = vec![1u8];
        let mut f = 0;
        if self.host_only {
            f |= FLAG_HOST_ONLY;
        }
        if self.secure {
            f |= FLAG_SECURE;
        }
        if self.http_only {
            f |= FLAG_HTTP_ONLY;
        }
        o.push(f);
        o.push(self.same_site as u8);
        o.extend_from_slice(&self.expires.unwrap_or(0).to_le_bytes());
        o.extend_from_slice(&self.created.to_le_bytes());
        o.extend_from_slice(&(self.path.len() as u16).to_le_bytes());
        o.extend_from_slice(self.path.as_bytes());
        o
    }

    pub fn decode(b: &[u8]) -> Option<Attrs> {
        if b.len() < 21 || b[0] != 1 || b[1] & !(FLAG_HOST_ONLY | FLAG_SECURE | FLAG_HTTP_ONLY) != 0 {
            return None;
        }
        let same_site = match b[2] {
            0 => SameSite::Default,
            1 => SameSite::Strict,
            2 => SameSite::Lax,
            3 => SameSite::None,
            _ => return None,
        };
        let e = u64::from_le_bytes(b[3..11].try_into().ok()?);
        let created = u64::from_le_bytes(b[11..19].try_into().ok()?);
        let n = u16::from_le_bytes(b[19..21].try_into().ok()?) as usize;
        let path = std::str::from_utf8(b.get(21..21 + n)?).ok()?;
        if b.len() != 21 + n || !path.starts_with('/') {
            return None;
        }
        Some(Attrs {
            host_only: b[1] & FLAG_HOST_ONLY != 0,
            secure: b[1] & FLAG_SECURE != 0,
            http_only: b[1] & FLAG_HTTP_ONLY != 0,
            same_site,
            expires: (e != 0).then_some(e),
            created,
            path: path.into(),
        })
    }
}

/// Whether `d` is a public suffix (com, co.uk, github.io, ...). Unknown
/// TLDs count as suffixes (the list's implicit `*` rule).
pub fn is_public_suffix(d: &str) -> bool {
    psl::suffix_str(d) == Some(d)
}

/// A host's site: its registrable domain (example.co.uk for
/// a.b.example.co.uk), or the host itself when it has none.
pub fn site(host: &str) -> String {
    let h = host.to_ascii_lowercase();
    psl::domain_str(&h).map(str::to_string).unwrap_or(h)
}

/// SameSite against the broker's ticket (RFC 6265bis §5.8.3): Strict
/// needs a same-site request; Lax (and no attribute) also goes on a
/// cross-site top-level navigation with a safe method; None always goes.
pub fn same_site_allows(a: &Attrs, t: &crate::tickets::Ticket) -> bool {
    match a.same_site {
        SameSite::None => true,
        SameSite::Strict => t.same_site(),
        SameSite::Lax | SameSite::Default => t.same_site() || (t.top_level() && t.safe_method()),
    }
}

/// RFC 9110 token characters.
pub fn is_token_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&c)
}

/// The default path of a request path (RFC 6265 §5.1.4).
pub fn default_path(request_path: &str) -> String {
    if !request_path.starts_with('/') {
        return "/".into();
    }
    match request_path.rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => request_path[..i].into(),
    }
}

/// Validate a Set-Cookie the courier for `host` received for a request to
/// `request_path`. `attrs` is the header's text after the first `;`.
pub fn parse(name: &str, value: &[u8], attrs: &[u8], host: &str, request_path: &str, now: u64) -> Result<SetCookie, Refused> {
    if name.is_empty() || !name.bytes().all(is_token_char) {
        return Err(Refused::Name);
    }
    // cookie-octets, leniently: no controls, no ';', no DEL.
    if value.iter().any(|&c| c < 0x20 || c == b';' || c == 0x7f) {
        return Err(Refused::Value);
    }
    if name.len() + value.len() > MAX_SIZE {
        return Err(Refused::TooLarge);
    }
    let host = host.to_ascii_lowercase();
    let mut domain: Option<String> = None;
    let mut path: Option<String> = None;
    let (mut max_age, mut expires): (Option<i64>, Option<u64>) = (None, None);
    let (mut secure, mut http_only, mut same_site) = (false, false, SameSite::Default);
    for av in attrs.split(|&c| c == b';') {
        let Ok(av) = std::str::from_utf8(av) else { continue };
        let (k, v) = match av.split_once('=') {
            Some((k, v)) => (k.trim(), v.trim()),
            None => (av.trim(), ""),
        };
        // The last of each attribute wins (RFC 6265 §5.3 step 6).
        match k.to_ascii_lowercase().as_str() {
            "expires" => expires = parse_date(v).or(expires),
            "max-age" => {
                let ok = !v.is_empty() && v.bytes().enumerate().all(|(i, c)| c.is_ascii_digit() || (i == 0 && c == b'-'));
                if ok {
                    max_age = Some(v.parse::<i64>().unwrap_or(if v.starts_with('-') { i64::MIN } else { i64::MAX }));
                }
            }
            "domain" if !v.is_empty() => domain = Some(v.trim_start_matches('.').to_ascii_lowercase()),
            "path" => path = v.starts_with('/').then(|| v.to_string()),
            "secure" => secure = true,
            "httponly" => http_only = true,
            "samesite" => {
                same_site = match v.to_ascii_lowercase().as_str() {
                    "strict" => SameSite::Strict,
                    "lax" => SameSite::Lax,
                    "none" => SameSite::None,
                    _ => SameSite::Default,
                }
            }
            _ => {}
        }
    }
    let expires = match (max_age, expires) {
        (Some(a), _) if a <= 0 => Some(0),
        (Some(a), _) => Some(now.saturating_add((a as u64).min(MAX_AGE_CAP))),
        (None, Some(e)) => Some(e.min(now.saturating_add(MAX_AGE_CAP))),
        (None, None) => None,
    };
    let had_domain = domain.is_some();
    let (domain, host_only) = match domain {
        None => (host.clone(), true),
        // A Domain equal to a public-suffix host is host-only (§5.6 step 5).
        Some(d) if d == host => (host.clone(), is_public_suffix(&d)),
        Some(d) => {
            if !host.ends_with(&format!(".{d}")) || is_public_suffix(&d) {
                return Err(Refused::Domain);
            }
            (d, false)
        }
    };
    let path = path.unwrap_or_else(|| default_path(request_path));
    if name.starts_with("__Secure-") && !secure {
        return Err(Refused::Prefix);
    }
    if name.starts_with("__Host-") && (!secure || had_domain || path != "/") {
        return Err(Refused::Prefix);
    }
    if same_site == SameSite::None && !secure {
        return Err(Refused::SameSiteNone);
    }
    Ok(SetCookie {
        name: name.into(),
        value: value.to_vec(),
        domain,
        attrs: Attrs { host_only, secure, http_only, same_site, expires: expires.map(|e| e.max(1)), created: now, path },
    })
}

/// The hosts whose groups may hold cookies for `host`: itself, then each
/// parent that isn't a public suffix.
pub fn candidate_domains(host: &str) -> Vec<String> {
    let host = host.to_ascii_lowercase();
    let mut out = vec![host.clone()];
    let mut d = host.as_str();
    while let Some((_, parent)) = d.split_once('.') {
        if is_public_suffix(parent) {
            break;
        }
        out.push(parent.to_string());
        d = parent;
    }
    out
}

/// Path-match (RFC 6265 §5.1.4).
pub fn path_matches(request_path: &str, cookie_path: &str) -> bool {
    request_path == cookie_path
        || (request_path.starts_with(cookie_path)
            && (cookie_path.ends_with('/') || request_path.as_bytes().get(cookie_path.len()) == Some(&b'/')))
}

/// Whether a cookie stored in `domain`'s group goes on a request to
/// `host` + `path` at `now`. SameSite is the ticket's (A2b); a courier with
/// no ticket is an app's, where every request is first-party.
pub fn applies(domain: &str, a: &Attrs, host: &str, path: &str, now: u64) -> bool {
    let host = host.to_ascii_lowercase();
    let domain_ok = if a.host_only { host == domain } else { host == domain || host.ends_with(&format!(".{domain}")) };
    domain_ok && path_matches(path, &a.path) && a.expires.is_none_or(|e| e > now)
}

/// The `Cookie` header, in locked memory, from matched (name, value, path
/// length, created) in RFC 6265 §5.4 order: longer paths first, then
/// earlier created.
pub fn header(mut cs: Vec<(String, LockedBuf, usize, u64)>) -> std::io::Result<LockedBuf> {
    cs.sort_by(|a, b| b.2.cmp(&a.2).then(a.3.cmp(&b.3)));
    let total: usize = cs.iter().map(|c| c.0.len() + 1 + c.1.len() + 2).sum::<usize>().saturating_sub(2);
    let mut out = LockedBuf::zeroed(total)?;
    let mut at = 0;
    for (i, (n, v, _, _)) in cs.iter().enumerate() {
        let mut put = |b: &[u8]| {
            out.as_mut()[at..at + b.len()].copy_from_slice(b);
            at += b.len();
        };
        if i > 0 {
            put(b"; ");
        }
        put(n.as_bytes());
        put(b"=");
        put(v.as_ref());
    }
    Ok(out)
}

/// RFC 6265 §5.1.1's cookie-date, to Unix seconds.
pub fn parse_date(s: &str) -> Option<u64> {
    let (mut time, mut day, mut month, mut year): (Option<(u64, u64, u64)>, Option<u64>, Option<u64>, Option<u64>) =
        (None, None, None, None);
    let delim = |c: char| {
        let b = c as u32;
        b == 0x09 || (0x20..=0x2f).contains(&b) || (0x3b..=0x40).contains(&b) || (0x5b..=0x60).contains(&b) || (0x7b..=0x7e).contains(&b)
    };
    for tok in s.split(delim).filter(|t| !t.is_empty()) {
        if time.is_none() {
            let p: Vec<&str> = tok.split(':').collect();
            if p.len() == 3 && p.iter().all(|x| (1..=2).contains(&x.len()) && x.bytes().all(|c| c.is_ascii_digit())) {
                time = Some((p[0].parse().ok()?, p[1].parse().ok()?, p[2].parse().ok()?));
                continue;
            }
        }
        let digits: String = tok.chars().take_while(|c| c.is_ascii_digit()).collect();
        if day.is_none() && (1..=2).contains(&digits.len()) && tok.len() == digits.len() {
            day = digits.parse().ok();
            continue;
        }
        if month.is_none() && tok.len() >= 3 {
            let m = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"]
                .iter()
                .position(|m| tok.as_bytes()[..3].eq_ignore_ascii_case(m.as_bytes()));
            if let Some(m) = m {
                month = Some(m as u64 + 1);
                continue;
            }
        }
        if year.is_none() && (2..=4).contains(&digits.len()) && tok.len() == digits.len() {
            year = digits.parse().ok();
            continue;
        }
    }
    let (mut y, d, mo, (h, mi, se)) = (year?, day?, month?, time?);
    if (70..=99).contains(&y) {
        y += 1900;
    } else if y <= 69 {
        y += 2000;
    }
    if !(1..=31).contains(&d) || y < 1601 || h > 23 || mi > 59 || se > 59 {
        return None;
    }
    // Days from 1970-01-01 (civil-from-days inverse, Howard Hinnant).
    let (yy, m) = if mo <= 2 { (y as i64 - 1, mo as i64 + 9) } else { (y as i64, mo as i64 - 3) };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let doy = (153 * m + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + (h * 3600 + mi * 60 + se) as i64;
    // Before the epoch: already expired.
    Some(secs.max(1) as u64)
}
