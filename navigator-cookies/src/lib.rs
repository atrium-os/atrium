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

use arca_courier::schema::{Encoding, Header, Method, Place, Request, Response, Segment, Slot};
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
    /// A form the reader sent (only in a sign-in window's jar).
    Form = 3,
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
    /// A form's fields, `application/x-www-form-urlencoded` (a POST).
    pub body: Vec<u8>,
    /// A password the person typed into Arca's field, which goes into
    /// `body` at this byte offset (form-encoded by the courier), never
    /// through this process: Arca's handle for it (arca-autofill.md §3).
    pub secret: Option<(usize, [u8; 16])>,
}

impl Fetch {
    pub fn navigation(url: &str, initiator: Option<&str>, gesture: bool) -> Fetch {
        Fetch { url: url.into(), method: Method::Get, reason: Reason::Navigation, initiator: initiator.map(Into::into), gesture, body: vec![], secret: None }
    }
    pub fn subresource(url: &str, page_host: &str) -> Fetch {
        Fetch { url: url.into(), method: Method::Get, reason: Reason::Subresource, initiator: Some(page_host.into()), gesture: false, body: vec![], secret: None }
    }
    /// A form the reader pressed to send, as a POST of `body` from a page
    /// on `page_host`. Only a sign-in window's view sends one.
    pub fn form(url: &str, page_host: &str, body: Vec<u8>) -> Fetch {
        Fetch { url: url.into(), method: Method::Post, reason: Reason::Form, initiator: Some(page_host.into()), gesture: true, body, secret: None }
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
    /// "Sign out here" (arcad `W_SIGN_OUT`): one partition's session
    /// cookies end. Returns how many.
    fn sign_out(&mut self, _partition: &str) -> Result<u32, String> {
        Err("this Arca cannot sign out".into())
    }
    /// The accounts Arca holds for `site` (arcad `W_ACCOUNTS`): (kind,
    /// name), kind `ACCOUNT_PASSKEY` or `ACCOUNT_PASSWORD`.
    /// A cookied view's `document.cookie` (arcad `W_SCRIPT_COOKIES`):
    /// what a request to `host:port` `path` would carry from `partition`,
    /// without its `HttpOnly` cookies, as `n=v; ...`.
    fn script_cookies(&mut self, _host: &str, _port: u16, _partition: &str, _path: &str) -> Result<String, String> {
        Err("not supported".into())
    }
    /// Keep what a page's script wrote (`document.cookie = line` at
    /// `path`), for the reader's press of a consent control (arcad
    /// `W_SCRIPT_SET_COOKIE`).
    fn script_set_cookie(&mut self, _host: &str, _port: u16, _partition: &str, _path: &str, _line: &str) -> Result<(), String> {
        Err("not supported".into())
    }
    fn accounts(&mut self, _site: &str) -> Result<Vec<(u8, String)>, String> {
        Err("this Arca cannot list accounts".into())
    }
    /// A typed password's handle, handed on to the form's endpoint and
    /// jar (arcad `W_HANDLE_FOR`): the handle the courier fills from.
    fn handle_for(&mut self, _handle: &[u8; 16], _host: &str, _port: u16, _partition: &str) -> Result<[u8; 16], String> {
        Err("this Arca cannot hand on a password".into())
    }
}

/// The couriers: one per (host, port, partition), from Arca's launcher.
pub trait Couriers {
    fn send(&mut self, host: &str, port: u16, partition: &str, req: &Request) -> Result<Response, String>;
}

#[derive(Debug, Clone)]
struct View { partition: String, cookied: Option<Why>, sign_in: Option<String>, microsoft: bool }

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

pub use arcad::proto::broker_op::{ACCOUNT_PASSKEY, ACCOUNT_PASSWORD};

/// The partition for `top_url` in VPN profile `profile`'s sign-in jar:
/// `vpn-<profile>.` then the site, as Arca reads it (arcad's
/// `jar::vpn_profile`).
pub fn vpn_partition(profile: &str, top_url: &str) -> Option<String> {
    Some(format!("{}{profile}.{}", arcad::jar::VPN_JAR, partition_of(top_url)?))
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
            _ => { self.views.insert(view, View { partition: p, cookied: None, sign_in: None, microsoft: false }); }
        }
        Ok(())
    }

    /// ★ A VPN sign-in window's view (vpn.md §9.3): every partition in the
    /// profile's own jar ([`vpn_partition`]), cookied from the start (the
    /// identity provider's session is kept for the next sign-in, apart from
    /// the person's browsing), and its forms are sent, on a press. Arca
    /// keeps the gateway's sign-on token out of the jar.
    pub fn show_sign_in(&mut self, view: ViewId, top_url: &str, profile: &str) -> Result<(), Refused> {
        // One jar for the whole sign-in: the gateway's site names it, and
        // the identity provider's pages it passes through stay in it.
        if self.views.get(&view).is_some_and(|v| v.sign_in.as_deref() == Some(profile)) {
            return Ok(());
        }
        let p = vpn_partition(profile, top_url).ok_or_else(|| Refused::Malformed(format!("no host in {top_url:?}")))?;
        self.views.insert(view, View { partition: p, cookied: Some(Why::Reader), sign_in: Some(profile.into()), microsoft: false });
        Ok(())
    }

    /// Begin the first-screen GET in a session-specific Arca cookie partition.
    /// The trusted sheet gets the URL from arcad by claiming Mail's handle.
    pub fn mail_first_screen(&mut self, view: ViewId, handle: &[u8; 16], url: &str) -> Result<Fetched, Refused> {
        if url.split_once('?').map(|(base, _)| base) != Some(arcad::oauth::MICROSOFT.authorize) {
            return Err(Refused::Malformed("unexpected Microsoft authorization endpoint".into()));
        }
        let partition = partition_in(Some(u64::from_le_bytes(handle[..8].try_into().unwrap())), url)
            .ok_or_else(|| Refused::Malformed("invalid authorization URL".into()))?;
        self.views.insert(view, View { partition,
            cookied: Some(Why::Reader), sign_in: None, microsoft: true });
        self.fetch(view, &Fetch::navigation(url, None, true))
    }

    pub fn close_mail_sheet(&mut self, view: ViewId) {
        if let Some(v) = self.views.get(&view).filter(|v| v.microsoft) {
            let _ = self.arca.forget(&v.partition, false);
            self.close(view);
        }
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

    /// Whether a view is a sign-in window's (it may send forms).
    pub fn sign_in(&self, view: ViewId) -> bool { self.views.get(&view).is_some_and(|v| v.sign_in.is_some()) }

    /// The view closed.
    pub fn close(&mut self, view: ViewId) { self.views.remove(&view); }

    /// Whether (and why) a view is cookied.
    pub fn cookied(&self, view: ViewId) -> Option<Why> { self.views.get(&view).and_then(|v| v.cookied) }

    pub fn partition(&self, view: ViewId) -> Option<&str> { self.views.get(&view).map(|v| v.partition.as_str()) }

    /// ★ What a cookied view's page scripts start with as
    /// `document.cookie` (legacy-web §5.4.1e step 6), for the document at
    /// `url`: its non-`HttpOnly` cookies from the view's partition. A view
    /// that is not cookied has none, and Arca is not asked.
    pub fn script_cookies(&mut self, view: ViewId, url: &str) -> Result<Option<String>, Refused> {
        let Some(View { partition, cookied: Some(_), .. }) = self.views.get(&view).cloned() else { return Ok(None) };
        let url = url::Url::parse(url).map_err(|e| Refused::Malformed(e.to_string()))?;
        if url.scheme() != "https" { return Ok(None) }
        let host = url.host_str().ok_or_else(|| Refused::Malformed("no host".into()))?.to_ascii_lowercase();
        let port = url.port_or_known_default().unwrap_or(443);
        let path = if url.path().is_empty() { "/" } else { url.path() };
        self.arca.script_cookies(&host, port, &partition, path).map(Some).map_err(Refused::Ticket)
    }

    /// ★ One request of a view. A view that is not cookied gets
    /// [`Refused::NotCookied`] before anything is asked of Arca: the caller
    /// sends it the plain way, and it carries no cookie.
    pub fn fetch(&mut self, view: ViewId, f: &Fetch) -> Result<Fetched, Refused> {
        self.fetch_with_redirects(view, f, MAX_REDIRECTS)
    }

    /// Fetch exactly the URL granted for an embedded resource.
    pub fn fetch_once(&mut self, view: ViewId, f: &Fetch) -> Result<Fetched, Refused> {
        self.fetch_with_redirects(view, f, 0)
    }

    fn fetch_with_redirects(&mut self, view: ViewId, f: &Fetch, limit: usize) -> Result<Fetched, Refused> {
        let Some(View { partition, cookied: Some(_), sign_in, microsoft }) = self.views.get(&view).cloned() else { return Err(Refused::NotCookied) };
        // A form only from a sign-in window, and only as a form.
        let form = sign_in.is_some() && f.method == Method::Post && f.reason == Reason::Form;
        if !matches!(f.method, Method::Get | Method::Head) && !form { return Err(Refused::Method) }
        if f.reason == Reason::Form && !form { return Err(Refused::Method) }
        if f.body.len() > arca_courier::schema::BODY_MAX { return Err(Refused::Malformed("the form is too big".into())) }
        let (mut method, mut reason, mut form_body) = (f.method, f.reason, f.body.clone());
        if f.secret.is_some_and(|(at, _)| at > f.body.len() || !form) { return Err(Refused::Malformed("a password goes only in a sign-in window's form".into())) }
        let mut secret = f.secret;
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
            if microsoft {
                let login = host == "login.microsoftonline.com";
                let asset = matches!(host.as_str(), "aadcdn.msauth.net" | "aadcdn.msftauth.net");
                if url.scheme() != "https" || port != 443 || !url.username().is_empty() || url.password().is_some()
                    || !(login || asset && reason == Reason::Subresource)
                    || login && url.path().eq_ignore_ascii_case("/common/oauth2/nativeclient") {
                    return Err(Refused::Redirect("outside the Microsoft first-screen grant".into()));
                }
            }
            let path = if url.path().is_empty() { "/".to_string() } else { url.path().to_string() };
            let query = url.query().unwrap_or("").to_string();
            let site = arcad::cookie::site(&host);
            // Arca decides same-site from the initiator it is given: the
            // original one while the chain stays on this site, else the
            // first host on the way that is not.
            let initiator = std::iter::once(&origin).filter(|o| !o.is_empty()).chain(chain.iter())
                .find(|h| arcad::cookie::site(h) != site).cloned().unwrap_or_else(|| origin.clone());
            let ask = TicketAsk { host: host.clone(), port, partition: partition.clone(), method, path: path.clone(),
                                  query: query.clone(), reason, initiator, gesture: f.gesture };
            let mut headers = vec![(Header::Accept, b"*/*".to_vec())];
            if method == Method::Post {
                headers.push((Header::ContentType, b"application/x-www-form-urlencoded".to_vec()));
            }
            let segs = |at: Option<usize>| match at {
                // The password's place: Arca's courier puts it there.
                Some(at) => [Segment::Literal(form_body[..at].to_vec()), Segment::Slot { slot: 0, encoding: Encoding::Form },
                             Segment::Literal(form_body[at..].to_vec())].into_iter().filter(|s| *s != Segment::Literal(vec![])).collect(),
                None if form_body.is_empty() => vec![],
                None => vec![Segment::Literal(form_body.clone())],
            };
            let req = |ticket, slot: Option<(usize, [u8; 16])>| Request { method, path: path.clone().into_bytes(), query: query.clone().into_bytes(),
                                headers: headers.clone(),
                                slots: slot.map(|(_, h)| vec![Slot { source: arcad::proto::courier_op::SOURCE_HANDLE, reference: h, place: Place::Body }]).unwrap_or_default(),
                                body: segs(slot.map(|(at, _)| at)), ticket: Some(ticket) };
            // A ticket is spent once: a courier that failed mid-exchange
            // may have spent it, so the one retry asks for a new one. A
            // password's handle is spent too: no retry with one.
            let mut sent = Err(String::new());
            let slot = match secret.take() {
                Some((at, h)) => Some((at, self.arca.handle_for(&h, &host, port, &partition).map_err(Refused::Ticket)?)),
                None => None,
            };
            for _ in 0..if slot.is_some() { 1 } else { 2 } {
                self.tickets += 1;
                let ticket = self.arca.ticket(&ask).map_err(Refused::Ticket)?;
                sent = self.couriers.send(&host, port, &partition, &req(ticket, slot));
                if sent.is_ok() { break }
            }
            let resp = sent.map_err(Refused::Courier)?;
            let (status, headers, body) = match resp {
                Response::Http { status, headers, body } => (status, headers, body),
                Response::Refused(r) => return Err(Refused::CourierRefused(format!("{r:?}"))),
                // Navigator never asks a courier for an event stream.
                other => return Err(Refused::CourierRefused(format!("{other:?}"))),
            };
            let location = headers.iter().find(|(n, _)| n.eq_ignore_ascii_case("location")).map(|(_, v)| v.clone());
            match (status, location) {
                (301 | 302 | 303 | 307 | 308, Some(loc)) => {
                    redirects += 1;
                    if redirects > limit { return Err(Refused::Redirect(format!("more than {limit} redirects"))) }
                    let next = url.join(&loc).map_err(|e| Refused::Redirect(e.to_string()))?;
                    if next.scheme() != "https" { return Err(Refused::Redirect(format!("down to {}", next.scheme()))) }
                    chain.push(host);
                    url = next;
                    // After a form, the answer's page is fetched (a 307 or
                    // 308 would send the form on, to wherever it says: no).
                    if method == Method::Post {
                        if matches!(status, 307 | 308) { return Err(Refused::Redirect("a form sent on by 307 or 308".into())) }
                        (method, reason, form_body) = (Method::Get, Reason::Navigation, vec![]);
                    }
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
        let (st, body) = arcad::proto::read_app_reply(&mut s, arcad::proto::AppReply::Ticket).map_err(|e| format!("arcad: {e}"))?;
        if st != arcad::proto::status::OK { return Err(format!("arcad refused the ticket (status {st})")) }
        body.as_slice().try_into().map_err(|_| "arcad: a ticket is 16 bytes".to_string())
    }

    fn forget(&mut self, partition: &str, prefix: bool) -> Result<u32, String> {
        let mut s = UnixStream::connect(&self.path).map_err(|e| format!("arcad: {e}"))?;
        s.set_read_timeout(Some(Duration::from_secs(10))).ok();
        let mut w = Writer::new();
        w.str(partition);
        w.u8(prefix as u8);
        arcad::proto::write_request(&mut s, arcad::proto::broker_op::W_FORGET, &w.buf).map_err(|e| format!("arcad: {e}"))?;
        let (st, body) = arcad::proto::read_app_reply(&mut s, arcad::proto::AppReply::Count).map_err(|e| format!("arcad: {e}"))?;
        if st != arcad::proto::status::OK { return Err(format!("arcad refused to forget (status {st})")) }
        let b: [u8; 4] = body.as_slice().try_into().map_err(|_| "arcad: a count is 4 bytes".to_string())?;
        Ok(u32::from_le_bytes(b))
    }

    fn handle_for(&mut self, handle: &[u8; 16], host: &str, port: u16, partition: &str) -> Result<[u8; 16], String> {
        let mut w = Writer::new();
        w.raw(handle);
        arcad::proto::write_endpoint(&mut w, host, port);
        w.str(partition);
        let body = self.ask(arcad::proto::broker_op::W_HANDLE_FOR, &w.buf, "hand on the password")?;
        body.as_slice().try_into().map_err(|_| "arcad: a handle is 16 bytes".to_string())
    }

    fn sign_out(&mut self, partition: &str) -> Result<u32, String> {
        let mut w = Writer::new();
        w.str(partition);
        let body = self.ask(arcad::proto::broker_op::W_SIGN_OUT, &w.buf, "sign out")?;
        let b: [u8; 4] = body.as_slice().try_into().map_err(|_| "arcad: a count is 4 bytes".to_string())?;
        Ok(u32::from_le_bytes(b))
    }

    fn script_cookies(&mut self, host: &str, port: u16, partition: &str, path: &str) -> Result<String, String> {
        let mut w = Writer::new();
        arcad::proto::write_endpoint(&mut w, host, port);
        w.str(partition);
        w.str(path);
        let body = self.ask(arcad::proto::broker_op::W_SCRIPT_COOKIES, &w.buf, "read a page's cookies")?;
        String::from_utf8(body).map_err(|_| "arcad: cookies that are not UTF-8".to_string())
    }

    fn script_set_cookie(&mut self, host: &str, port: u16, partition: &str, path: &str, line: &str) -> Result<(), String> {
        let mut w = Writer::new();
        arcad::proto::write_endpoint(&mut w, host, port);
        w.str(partition);
        w.str(path);
        w.str(line);
        self.ask(arcad::proto::broker_op::W_SCRIPT_SET_COOKIE, &w.buf, "keep a page's cookie").map(|_| ())
    }

    fn accounts(&mut self, site: &str) -> Result<Vec<(u8, String)>, String> {
        let mut w = Writer::new();
        w.str(site);
        let body = self.ask(arcad::proto::broker_op::W_ACCOUNTS, &w.buf, "list accounts")?;
        let mut r = arca_store::codec::Reader::new(&body);
        let bad = |_| "arcad: a malformed account list".to_string();
        let n = r.u32().map_err(bad)? as usize;
        if n > arcad::proto::broker_op::ACCOUNTS_MAX {
            return Err("arcad: too many accounts".into());
        }
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let k = r.u8().map_err(bad)?;
            let a = r.str(arca_store::model::caps::ACCOUNT).map_err(bad)?;
            out.push((k, a.to_string()));
        }
        r.end().map_err(bad)?;
        Ok(out)
    }
}

impl ArcadSocket {
    pub fn claim_mail_sign_in(&self, handle: &[u8; 16]) -> Result<String, String> {
        let bytes = self.ask(arcad::proto::broker_op::W_MAIL_SIGN_IN, handle, "open the Mail sign-in sheet")?;
        String::from_utf8(bytes).map_err(|_| "Arca returned an invalid authorization URL".into())
    }

    /// One request to arcad, its OK body.
    fn ask(&self, op: u16, body: &[u8], what: &str) -> Result<Vec<u8>, String> {
        let mut s = UnixStream::connect(&self.path).map_err(|e| format!("arcad: {e}"))?;
        s.set_read_timeout(Some(Duration::from_secs(10))).ok();
        arcad::proto::write_request(&mut s, op, body).map_err(|e| format!("arcad: {e}"))?;
        let (st, b) = if op == arcad::proto::broker_op::W_MAIL_SIGN_IN {
            arcad::proto::read_app_reply(&mut s, arcad::proto::AppReply::AuthorizationUrl)
        } else {
            arcad::proto::read_reply(&mut s).map(|(st, b)| (st, b.as_ref().to_vec()))
        }.map_err(|e| format!("arcad: {e}"))?;
        if st != arcad::proto::status::OK {
            return Err(format!("arcad refused to {what} (status {st})"));
        }
        Ok(b)
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
