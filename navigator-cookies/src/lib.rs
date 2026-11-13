//! ★ THE NAVIGATOR BROKER'S SIDE OF ARCA'S WEB COOKIES (arca.md §8.2,
//! legacy-web §5.4.1e).
//!
//! Cookies are off by default. A view turns *cookied* only when the reader
//! says so, or when the site needs it (`Why`); until then the broker never
//! asks Arca for anything, and that view's requests take the plain fetcher
//! (Nuntius), which sends no cookie and keeps none. A cookied view's
//! requests go instead through Arca's courier for their (partition,
//! origin), each with a TICKET the broker asked Arca for, naming exactly:
//!
//! - the partition: the site of the view's top-level document (one jar per
//!   partition: a site embedded in two pages has two jars);
//! - the request: endpoint, method, path and query;
//! - the reason (a navigation or a subresource) and the initiating host
//!   ("" for the reader's own navigation), which is what SameSite goes by;
//! - whether a reader's gesture started it.
//!
//! Arca releases cookies only for that ticket, once, to the courier whose
//! label matches, over TLS to the host the ticket names, and admits only
//! that response's `Set-Cookie`s. The converter never holds a ticket: it
//! asks the broker for a URL and gets bytes back, so a compromised
//! converter cannot claim a same-site context or a gesture.
//!
//! What a cookied view may send: `GET` and `HEAD` over `https` only (no
//! logins: a form POST stays refused, §5.4.1e); a request over `http` goes
//! the plain way, with no cookie. A courier never follows a redirect; the
//! broker does, at most [`MAX_REDIRECTS`] times, never down to `http`, with
//! a new ticket per hop — and a hop that crossed to another site makes
//! every later hop cross-site for SameSite (RFC 6265bis §5.2).

use std::collections::HashMap;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use arca_courier::schema::{Header, Method, Request, Response};
use arca_store::codec::Writer;

/// As navigator-fetch's.
pub const MAX_REDIRECTS: usize = 5;

/// A view: one reader's tab, showing one top-level document at a time.
pub type ViewId = u64;

/// Why a request is made (Arca's ticket reasons).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// The view's top-level document.
    Navigation = 1,
    /// Anything the document needs: script, style, image, a page's fetch.
    Subresource = 2,
}

/// Why a view turned cookied (legacy-web §5.4.1e).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// The reader asked (a consent or continue control; the trusted UI's
    /// "allow cookies for this site").
    Reader,
    /// The site needs it: its chain without cookies ended in an error, a
    /// loop, or a URL naming a cookie error.
    SiteNeedsIt,
}

/// One request a view makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetch {
    pub url: String,
    pub method: Method,
    pub reason: Reason,
    /// The host of the page that asked; `None` for the reader's own
    /// navigation (typed, a bookmark, the back button).
    pub initiator: Option<String>,
    /// A reader's gesture started it (a click, a key).
    pub gesture: bool,
}

impl Fetch {
    pub fn navigation(url: &str, initiator: Option<&str>, gesture: bool) -> Fetch {
        Fetch { url: url.into(), method: Method::Get, reason: Reason::Navigation, initiator: initiator.map(Into::into), gesture }
    }
    pub fn subresource(url: &str, page_host: &str) -> Fetch {
        Fetch { url: url.into(), method: Method::Get, reason: Reason::Subresource, initiator: Some(page_host.into()), gesture: false }
    }
}

/// A response that came through couriers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// The URL that answered, after redirects.
    pub url: String,
    pub redirects: usize,
}

/// Why the broker did not send a request through a courier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// The view is not cookied (or unknown): the request goes the plain
    /// way, cookieless. No ticket was asked for.
    NotCookied,
    /// Not `https`: the plain way, cookieless.
    NotHttps,
    /// A cookied view sends only `GET` and `HEAD` (no logins yet).
    Method,
    Malformed(String),
    /// A redirect to `http`, or past [`MAX_REDIRECTS`].
    Redirect(String),
    /// Arca would not issue the ticket.
    Ticket(String),
    /// The launcher would not give a courier, or it failed.
    Courier(String),
    /// The courier refused the request (nothing was sent).
    CourierRefused(String),
}

impl Refused {
    /// Whether the request may still go the plain, cookieless way.
    pub fn plain_instead(&self) -> bool { matches!(self, Refused::NotCookied | Refused::NotHttps) }
}

/// What the broker asks Arca for (`W_TICKET`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TicketAsk {
    pub host: String,
    pub port: u16,
    pub partition: String,
    pub method: Method,
    pub path: String,
    pub query: String,
    pub reason: Reason,
    /// "" for the reader's own navigation.
    pub initiator: String,
    pub gesture: bool,
}

impl TicketAsk {
    /// arcad's `W_TICKET` body.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        arcad::proto::write_endpoint(&mut w, &self.host, self.port);
        w.str(&self.partition);
        w.u8(self.method as u8);
        w.str(&self.path);
        w.u8(self.reason as u8);
        w.str(&self.initiator);
        w.u8(self.gesture as u8);
        w.str(&self.query);
        w.buf
    }
}

/// Arca, as the broker sees it.
pub trait Arca {
    fn ticket(&mut self, ask: &TicketAsk) -> Result<[u8; 16], String>;
    /// Forget jars (arcad `W_FORGET`): one partition, or every partition
    /// beginning with `partition` when `prefix` (an Unrecorded jar's, or
    /// all of them). Returns how many cookie groups went.
    fn forget(&mut self, _partition: &str, _prefix: bool) -> Result<u32, String> {
        Err("this Arca cannot forget".into())
    }
}

/// The couriers: one per (host, port, partition), from Arca's launcher.
pub trait Couriers {
    fn send(&mut self, host: &str, port: u16, partition: &str, req: &Request) -> Result<Response, String>;
}

#[derive(Debug, Clone)]
struct View { partition: String, cookied: Option<Why> }

/// ★ THE BROKER'S COOKIE STATE: which views are cookied, and the only
/// path by which a request of theirs carries a cookie.
pub struct Broker<A: Arca, C: Couriers> {
    pub arca: A,
    pub couriers: C,
    views: HashMap<ViewId, View>,
    /// Tickets asked for, in total (for checks: an uncookied view never
    /// causes one).
    pub tickets: u64,
}

/// The partition a top-level document's view lives in: its site.
pub fn partition_of(top_url: &str) -> Option<String> {
    let u = url::Url::parse(top_url).ok()?;
    Some(arcad::cookie::site(u.host_str()?))
}

/// An Unrecorded trail's jar prefix (navigator-ui.md §2.4): every partition
/// of that trail begins with it, so closing the trail forgets them all.
pub fn jar_prefix(jar: u64) -> String {
    format!("{}{jar:016x}.", arcad::proto::broker_op::UNRECORDED)
}

/// The partition for `top_url` in a persona's jar: the site itself for
/// Personal (`jar` `None`), else the site under the Unrecorded trail's
/// prefix, so two Unrecorded trails, or one and Personal, share nothing.
pub fn partition_in(jar: Option<u64>, top_url: &str) -> Option<String> {
    let site = partition_of(top_url)?;
    Some(match jar {
        None => site,
        Some(j) => format!("{}{site}", jar_prefix(j)),
    })
}

impl<A: Arca, C: Couriers> Broker<A, C> {
    pub fn new(arca: A, couriers: C) -> Self { Broker { arca, couriers, views: HashMap::new(), tickets: 0 } }

    /// A view shows `top_url` (its first document, or a navigation to
    /// another site): its partition is that document's site. A view that
    /// moves to another SITE starts uncookied again — consent given to one
    /// site is not consent given to the next.
    pub fn show(&mut self, view: ViewId, top_url: &str) -> Result<(), Refused> {
        self.show_in(view, top_url, None)
    }

    /// [`Self::show`] in a persona's jar ([`partition_in`]).
    pub fn show_in(&mut self, view: ViewId, top_url: &str, jar: Option<u64>) -> Result<(), Refused> {
        let p = partition_in(jar, top_url).ok_or_else(|| Refused::Malformed(format!("no host in {top_url:?}")))?;
        match self.views.get_mut(&view) {
            Some(v) if v.partition == p => {}
            _ => { self.views.insert(view, View { partition: p, cookied: None }); }
        }
        Ok(())
    }

    /// The reader's action, or the site's need, turns a view cookied: from
    /// now on its requests carry cookies from its partition's jar (and its
    /// conversions are the reader's alone, never shared).
    pub fn set_cookied(&mut self, view: ViewId, why: Why) -> Result<(), Refused> {
        let v = self.views.get_mut(&view).ok_or(Refused::NotCookied)?;
        v.cookied = Some(why);
        Ok(())
    }

    /// Back to anonymous (the reader turned cookies off for this view).
    pub fn set_anonymous(&mut self, view: ViewId) {
        if let Some(v) = self.views.get_mut(&view) { v.cookied = None }
    }

    /// The view closed.
    pub fn close(&mut self, view: ViewId) { self.views.remove(&view); }

    /// Whether (and why) a view is cookied.
    pub fn cookied(&self, view: ViewId) -> Option<Why> { self.views.get(&view).and_then(|v| v.cookied) }

    pub fn partition(&self, view: ViewId) -> Option<&str> { self.views.get(&view).map(|v| v.partition.as_str()) }

    /// ★ One request of a view. A view that is not cookied gets
    /// [`Refused::NotCookied`] before anything is asked of Arca: the caller
    /// sends it the plain way, and it carries no cookie.
    pub fn fetch(&mut self, view: ViewId, f: &Fetch) -> Result<Fetched, Refused> {
        let Some(View { partition, cookied: Some(_) }) = self.views.get(&view).cloned() else { return Err(Refused::NotCookied) };
        if !matches!(f.method, Method::Get | Method::Head) { return Err(Refused::Method) }
        let mut url = url::Url::parse(&f.url).map_err(|e| Refused::Malformed(e.to_string()))?;
        if url.scheme() != "https" { return Err(Refused::NotHttps) }
        let origin = f.initiator.clone().unwrap_or_default().to_ascii_lowercase();
        // The hosts the chain has passed through (RFC 6265bis §5.2: a
        // request is same-site only if its initiator AND every hop before
        // it share the target's site).
        let mut chain: Vec<String> = vec![];
        let mut redirects = 0;
        loop {
            let host = url.host_str().ok_or_else(|| Refused::Malformed("no host".into()))?.to_ascii_lowercase();
            let port = url.port_or_known_default().unwrap_or(443);
            let path = if url.path().is_empty() { "/".to_string() } else { url.path().to_string() };
            let query = url.query().unwrap_or("").to_string();
            let site = arcad::cookie::site(&host);
            // Arca decides same-site from the initiator it is given: the
            // original one while the chain stays on this site, else the
            // first host on the way that is not.
            let initiator = std::iter::once(&origin).filter(|o| !o.is_empty()).chain(chain.iter())
                .find(|h| arcad::cookie::site(h) != site).cloned().unwrap_or_else(|| origin.clone());
            let ask = TicketAsk { host: host.clone(), port, partition: partition.clone(), method: f.method, path: path.clone(),
                                  query: query.clone(), reason: f.reason, initiator, gesture: f.gesture };
            let req = |ticket| Request { method: f.method, path: path.clone().into_bytes(), query: query.clone().into_bytes(),
                                headers: vec![(Header::Accept, b"*/*".to_vec())], slots: vec![], body: vec![], ticket: Some(ticket) };
            // A ticket is spent once: a courier that failed mid-exchange
            // may have spent it, so the one retry asks for a new one.
            let mut sent = Err(String::new());
            for _ in 0..2 {
                self.tickets += 1;
                let ticket = self.arca.ticket(&ask).map_err(Refused::Ticket)?;
                sent = self.couriers.send(&host, port, &partition, &req(ticket));
                if sent.is_ok() { break }
            }
            let resp = sent.map_err(Refused::Courier)?;
            let (status, headers, body) = match resp {
                Response::Http { status, headers, body } => (status, headers, body),
                Response::Refused(r) => return Err(Refused::CourierRefused(format!("{r:?}"))),
            };
            let location = headers.iter().find(|(n, _)| n.eq_ignore_ascii_case("location")).map(|(_, v)| v.clone());
            match (status, location) {
                (301 | 302 | 303 | 307 | 308, Some(loc)) => {
                    redirects += 1;
                    if redirects > MAX_REDIRECTS { return Err(Refused::Redirect(format!("more than {MAX_REDIRECTS} redirects"))) }
                    let next = url.join(&loc).map_err(|e| Refused::Redirect(e.to_string()))?;
                    if next.scheme() != "https" { return Err(Refused::Redirect(format!("down to {}", next.scheme()))) }
                    chain.push(host);
                    url = next;
                }
                _ => return Ok(Fetched { status, headers, body, url: url.to_string(), redirects }),
            }
        }
    }
}

/// One hop of a document's redirect chain fetched WITHOUT cookies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hop { pub url: String, pub status: u16, pub set_cookie: bool }

/// ★ DOES THE SITE NEED COOKIES? (legacy-web §5.4.1e, the narrower
/// trigger the measurement asked for.) A cookie check is a chain that sets
/// a cookie and comes back to its own origin, expecting it; but only a
/// check that FAILED without one is a reason to make the view private:
/// the chain ends in an error status, goes round in a loop, or lands on a
/// URL that names a cookie error. On the 146-URL measurement this catches
/// exactly the two that needed it (sciencedirect, nature) and none of the
/// five whose cookie-setting redirects changed nothing.
pub fn site_needs_cookies(chain: &[Hop]) -> bool {
    let origin = |u: &str| url::Url::parse(u).ok().map(|u| u.origin().ascii_serialization());
    let Some(first) = chain.first().and_then(|h| origin(&h.url)) else { return false };
    // A cookie set, then the same origin again after it.
    let set_at = chain.iter().position(|h| h.set_cookie);
    let returns = set_at.is_some_and(|i| chain[i + 1..].iter().any(|h| origin(&h.url).as_deref() == Some(first.as_str())));
    if !returns { return false }
    let last = chain.last().expect("non-empty");
    let failed = last.status >= 400;
    let looped = chain.iter().enumerate().any(|(i, h)| chain[..i].iter().any(|g| g.url == h.url));
    let names_error = url::Url::parse(&last.url).is_ok_and(|u| {
        let s = format!("{}?{}", u.path(), u.query().unwrap_or("")).to_ascii_lowercase();
        s.contains("cookie") && ["error", "not_supported", "notsupported", "disabled", "required", "enable"].iter().any(|w| s.contains(w))
    });
    failed || looped || names_error
}

// ---------------------------------------------------------------------------
// The real Arca and launcher, over their sockets.
// ---------------------------------------------------------------------------

/// arcad's socket, as the web broker (`ARCA_WEB_BROKER` names this
/// process's app and publisher; arcad knows it by its peer uid).
pub struct ArcadSocket { pub path: PathBuf }

impl Arca for ArcadSocket {
    fn ticket(&mut self, ask: &TicketAsk) -> Result<[u8; 16], String> {
        let mut s = UnixStream::connect(&self.path).map_err(|e| format!("arcad: {e}"))?;
        s.set_read_timeout(Some(Duration::from_secs(10))).ok();
        arcad::proto::write_request(&mut s, arcad::proto::broker_op::W_TICKET, &ask.encode()).map_err(|e| format!("arcad: {e}"))?;
        let (st, body) = arcad::proto::read_reply(&mut s).map_err(|e| format!("arcad: {e}"))?;
        if st != arcad::proto::status::OK { return Err(format!("arcad refused the ticket (status {st})")) }
        body.as_ref().try_into().map_err(|_| "arcad: a ticket is 16 bytes".to_string())
    }

    fn forget(&mut self, partition: &str, prefix: bool) -> Result<u32, String> {
        let mut s = UnixStream::connect(&self.path).map_err(|e| format!("arcad: {e}"))?;
        s.set_read_timeout(Some(Duration::from_secs(10))).ok();
        let mut w = Writer::new();
        w.str(partition);
        w.u8(prefix as u8);
        arcad::proto::write_request(&mut s, arcad::proto::broker_op::W_FORGET, &w.buf).map_err(|e| format!("arcad: {e}"))?;
        let (st, body) = arcad::proto::read_reply(&mut s).map_err(|e| format!("arcad: {e}"))?;
        if st != arcad::proto::status::OK { return Err(format!("arcad refused to forget (status {st})")) }
        let b: [u8; 4] = body.as_ref().try_into().map_err(|_| "arcad: a count is 4 bytes".to_string())?;
        Ok(u32::from_le_bytes(b))
    }
}

/// Arca's courier launcher: one courier per (host, port, partition), kept
/// while it answers (each is a jail; the launcher caps the broker at
/// `arca_courierd::PER_BROKER`).
pub struct Launcher { pub socket: String, open: HashMap<(String, u16, String), UnixStream> }

impl Launcher {
    pub fn new(socket: impl Into<String>) -> Self { Launcher { socket: socket.into(), open: HashMap::new() } }

    fn exchange(s: &mut UnixStream, req: &Request) -> io::Result<Response> {
        s.set_read_timeout(Some(Duration::from_secs(60))).ok();
        arca_courier::send::write_frame(s, &req.encode())?;
        let frame = arca_courier::send::read_frame(s)?.ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
        Response::decode(&frame).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "a courier's reply out of shape"))
    }
}

impl Couriers for Launcher {
    fn send(&mut self, host: &str, port: u16, partition: &str, req: &Request) -> Result<Response, String> {
        let key = (host.to_string(), port, partition.to_string());
        // A kept courier first; one that has gone (it exits when idle) is
        // dropped, and the broker retries with a new ticket.
        if let Some(s) = self.open.get_mut(&key) {
            return match Self::exchange(s, req) {
                Ok(r) => Ok(r),
                Err(e) => { self.open.remove(&key); Err(format!("courier {host}: {e}")) }
            };
        }
        let mut s = arca_courierd::open(&self.socket, host, port, Some(partition))
            .map_err(|code| format!("the courier launcher refused {host}:{port} (code {code})"))?;
        let r = Self::exchange(&mut s, req).map_err(|e| format!("courier {host}: {e}"))?;
        self.open.insert(key, s);
        Ok(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn hop(url: &str, status: u16, set_cookie: bool) -> Hop { Hop { url: url.into(), status, set_cookie } }

    #[test]
    fn a_failed_cookie_check_needs_cookies_and_a_passing_one_does_not() {
        // sciencedirect's shape: a cookie, back to itself, 400 without it.
        assert!(site_needs_cookies(&[hop("https://www.sd.example/", 302, true), hop("https://auth.sd.example/check", 302, false), hop("https://www.sd.example/", 400, false)]));
        // nature's: back to itself naming the error.
        assert!(site_needs_cookies(&[hop("https://www.nature.example/a", 303, true), hop("https://www.nature.example/a?error=cookies_not_supported", 200, false)]));
        // A loop.
        assert!(site_needs_cookies(&[hop("https://x.example/", 302, true), hop("https://x.example/go", 302, false), hop("https://x.example/", 302, false)]));
        // A cookie-setting redirect that works anyway: not a reason.
        assert!(!site_needs_cookies(&[hop("https://weather.example/", 302, true), hop("https://weather.example/today", 200, false)]));
        // An error with no cookie in the chain: not a cookie check.
        assert!(!site_needs_cookies(&[hop("https://x.example/", 404, false)]));
        // A cookie set, the error elsewhere (another origin): not this site's check.
        assert!(!site_needs_cookies(&[hop("https://x.example/", 302, true), hop("https://login.other.example/", 400, false)]));
        // A page about cookies that loads fine.
        assert!(!site_needs_cookies(&[hop("https://x.example/", 302, true), hop("https://x.example/cookie-policy", 200, false)]));
        assert!(!site_needs_cookies(&[]));
    }
}
