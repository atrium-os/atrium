//! A whole page live on the person's press (navigator-islands.md I6.1).
//!
//! The trail's page is shown in the document lane; the tier chip's popover
//! offers **Run this page live**, and only that press starts an engine:
//! Limen's `web-island` role, placed over the whole page area of the UI's
//! window. The engine never names the page. Its first request is for
//! [`limen::web_island::LIVE_PAGE`], which navigatord answers with the page
//! the person pressed on, its address in the answer's final URL. Every
//! later request is a resource of that page, served only inside the page's
//! scope ([`scope`], D12) through the plain fetcher, and refused otherwise
//! with the host and site logged. Stop, leaving the page, or the engine's
//! end retires the island and the trail shows the document lane again.

use std::os::unix::net::UnixStream;
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use limen::{client::Parent, proto::ParentEvent, web_island::LIVE_PAGE};
use nuntius::{Deliver, ErrorCode, Event, Method, Request};
use crate::live_frames::{FrameAsk, Mailbox};

/// The most requests one live page makes before its engine is ended: a
/// map or an editor makes hundreds; thousands is a page that loops.
pub const MAX_REQUESTS: usize = 4096;
/// The most requests a second for navigatord's own addresses (the
/// reporter's, storage, frames' messages); more are refused, not counted.
pub const OWN_PER_SECOND: u32 = 20;
/// The most bytes one live page is served in all.
pub const MAX_BYTES: u64 = 512 << 20;
/// How long the engine has to ask for its page after it is placed.
const FIRST_REQUEST: Duration = Duration::from_secs(20);
/// How long a running page may go without asking for anything before the
/// channel's read times out and is waited on again (not an end).
const IDLE_POLL: Duration = Duration::from_secs(1);

/// Whether `url` is in the scope of the live page at `page` (D12): a plain
/// HTTPS address in its normal spelling, on the page's port, with no
/// credentials or fragment, whose registrable site is the page's. Sibling
/// hosts (`api.app.example` for `app.example`) are in; another site, a
/// downgrade to http, or an address spelled two ways is not.
pub fn scope(page: &str, url: &str) -> Result<(), &'static str> {
    if url.len() > navigator_control::URL_MAX || !navigator_control::clean(url, navigator_control::URL_MAX) {
        return Err("the address is out of shape");
    }
    let page = url::Url::parse(page).ok().filter(|u| u.scheme() == "https").ok_or("the page is not an HTTPS page")?;
    let target = url::Url::parse(url).map_err(|_| "not an address")?;
    if target.scheme() != "https" || !target.username().is_empty() || target.password().is_some() || target.fragment().is_some() {
        return Err("only a plain HTTPS address");
    }
    if target.as_str() != url {
        return Err("the address is not in its normal spelling");
    }
    if target.port_or_known_default() != page.port_or_known_default() {
        return Err("another port is another site's");
    }
    let (Some(a), Some(b)) = (crate::site_of(page.as_str()), crate::site_of(url)) else { return Err("no site") };
    if a != b {
        return Err("another site");
    }
    Ok(())
}

/// A resource's answer for the engine: its status, content type token
/// (`-` for none), body, and the server's CORS answer when the request
/// carried the page's origin (I6.5).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Got {
    pub status: u16,
    pub kind: String,
    pub body: Vec<u8>,
    /// `Access-Control-Allow-Origin` (in [`nuntius::cors_origin`]'s
    /// shape) and whether credentials are allowed.
    pub cors: Option<(String, bool)>,
}

impl Got {
    pub fn new(status: u16, kind: impl Into<String>, body: Vec<u8>) -> Got {
        Got { status, kind: kind.into(), body, cors: None }
    }
}

/// The server's CORS answer, if it gave one in the shape passed on: one
/// `Access-Control-Allow-Origin` header, and `Access-Control-Allow-
/// Credentials: true` read case-sensitively, as browsers do.
fn cors(headers: &[(String, String)]) -> Option<(String, bool)> {
    let named = |n: &'static str| headers.iter().filter(move |(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, v)| v.trim());
    let mut origins = named("access-control-allow-origin");
    let origin = origins.next().filter(|_| origins.next().is_none()).filter(|v| nuntius::cors_origin(v))?;
    Some((origin.to_string(), named("access-control-allow-credentials").next() == Some("true")))
}

/// Where a live page's resources come from: the page's jar through Arca
/// ([`LiveJar`], D9), the plain fetcher (`L`) where there is no Arca, or a
/// test's stand-in.
pub trait Resources {
    /// One GET (`first`: the page itself).
    fn get(&mut self, url: &str, first: bool) -> Result<Got, String>;
    /// A script's POST (I6.3) of `body` as `content_type`.
    fn post(&mut self, _url: &str, _content_type: &str, _body: &[u8]) -> Result<Got, String> {
        Err("a POST needs the page's jar".into())
    }
}

impl Resources for crate::Upstream {
    fn get(&mut self, url: &str, _first: bool) -> Result<Got, String> {
        match self {
            crate::Upstream::Fetchd(f, _) => f.live_resource(url).map(|(status, kind, body)| Got::new(status, kind, body)),
            _ => Err("a live page needs the plain fetcher".into()),
        }
    }
}

/// The content type token a live page's answer carries: `type/subtype`,
/// lower case, or `-` (as fetchd's `L` spells it).
fn type_token(headers: &[(String, String)]) -> String {
    let value = headers.iter().find(|(n, _)| n.eq_ignore_ascii_case("content-type")).map(|(_, v)| v.as_str());
    let Some(mime) = value.and_then(|v| v.split(';').next()).map(str::trim) else { return "-".into() };
    let token = |t: &str| !t.is_empty() && t.len() <= 64 && t.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$&-^_.+".contains(&b));
    match mime.split_once('/') {
        Some((a, b)) if token(a) && token(b) => mime.to_ascii_lowercase(),
        _ => "-".into(),
    }
}

/// A live page's jar (D9): its site's partition in the trail's jar, kept
/// by Arca, the broker's view for this run. Every request goes through
/// Arca's couriers with a ticket; the engine never holds a cookie.
#[derive(Clone)]
pub struct LiveJar {
    pub cookies: crate::Cookies,
    pub view: u64,
    /// The page's host, the initiator of its requests.
    pub host: String,
    /// The page's origin, sent with its scripts' requests to its sibling
    /// hosts and with its POSTs (I6.5).
    origin: String,
    /// The trail's jar (`None`: the persona's own).
    pub jar: Option<u64>,
    /// The page itself, already fetched when this run's navigation was
    /// looked at ([`LiveJar::peek`]), so it is not asked for twice.
    first: Option<Got>,
}

/// What a live page's navigation or form led to (I6.4).
#[derive(Debug, PartialEq)]
pub enum Answer {
    /// A page at this address: the next trail entry, run live again.
    Page(String),
    /// A file to save: held, and the person asked.
    File(Offered),
}

/// A file a live page offered, for navigatord to hold (I6.4).
#[derive(Debug, PartialEq)]
pub struct Offered {
    pub url: String,
    pub name: String,
    pub kind: String,
    pub bytes: Vec<u8>,
}

/// A page a run looked at, kept briefly for the run that shows it.
struct Stashed { jar: Option<u64>, url: String, at: Instant, answer: Got }
static STASH: Mutex<Option<Stashed>> = Mutex::new(None);
/// How long a looked-at page waits for its run.
const STASH_LIFE: Duration = Duration::from_secs(30);

fn stash(jar: Option<u64>, url: &str, answer: Got) {
    if answer.body.len() <= navigator_prerender::fetch::LIVE_RESOURCE_MAX {
        *STASH.lock().unwrap() = Some(Stashed { jar, url: url.to_string(), at: Instant::now(), answer });
    }
}

fn unstash(jar: Option<u64>, url: &str) -> Option<Got> {
    let mut slot = STASH.lock().unwrap();
    let fresh = slot.as_ref().is_some_and(|s| s.jar == jar && s.url == url && s.at.elapsed() < STASH_LIFE);
    if fresh { slot.take().map(|s| s.answer) } else { None }
}

/// A page or a file, from what the jar answered.
fn answered(r: navigator_cookies::Fetched, jar: Option<u64>) -> Result<Answer, String> {
    if r.body.len() as u64 > navigator_control::FILE_MAX {
        return Err("the answer is over the largest file Navigator keeps".into());
    }
    match crate::files::live_file(&r.url, &r.headers, &r.body) {
        Some((name, kind)) => Ok(Answer::File(Offered { url: r.url, name, kind, bytes: r.body })),
        None => {
            stash(jar, &r.url, Got::new(r.status, type_token(&r.headers), r.body));
            Ok(Answer::Page(r.url))
        }
    }
}

impl LiveJar {
    /// A new view for the page at `url` in the jar `jar` (`None`: the
    /// persona's own).
    pub fn open(cookies: crate::Cookies, url: &str, jar: Option<u64>) -> Result<Self, String> {
        let parsed = url::Url::parse(url).map_err(|_| "no address")?;
        let host = parsed.host_str().map(str::to_ascii_lowercase).ok_or("no host")?;
        let origin = parsed.origin().ascii_serialization();
        let view = crate::VIEWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        cookies.lock().unwrap().show_live(view, url, jar).map_err(|e| format!("the page's jar: {e:?}"))?;
        Ok(LiveJar { cookies, view, host, origin, jar, first: unstash(jar, url) })
    }

    fn answer(got: Result<navigator_cookies::Fetched, navigator_cookies::Refused>) -> Result<Got, String> {
        let r = got.map_err(|e| format!("through Arca: {e:?}"))?;
        if r.body.len() > navigator_prerender::fetch::LIVE_RESOURCE_MAX { return Err("the answer is too big".into()); }
        Ok(Got { status: r.status, kind: type_token(&r.headers), cors: cors(&r.headers), body: r.body })
    }

    /// A form the page submitted (I6.3): sent through the jar, its answer's
    /// redirects followed. The page that answered is kept for the next
    /// run, so an answer that is not a redirect is not lost (I6.4).
    pub fn submit(&self, url: &str, body: &[u8]) -> Result<Answer, String> {
        let f = navigator_cookies::Fetch::live_form(url, &self.host, body.to_vec()).from_origin(&self.origin);
        let r = self.cookies.lock().unwrap().fetch(self.view, &f).map_err(|e| format!("through Arca: {e:?}"))?;
        answered(r, self.jar)
    }

    /// A navigation the page proposed within its origin (I6.4): fetched
    /// through the jar to see whether it is a page or a file. A page is
    /// kept for the run that shows it.
    pub fn peek(&self, url: &str) -> Result<Answer, String> {
        let f = navigator_cookies::Fetch::navigation(url, None, true);
        let r = self.cookies.lock().unwrap().fetch(self.view, &f).map_err(|e| format!("through Arca: {e:?}"))?;
        answered(r, self.jar)
    }
}

impl Resources for LiveJar {
    fn get(&mut self, url: &str, first: bool) -> Result<Got, String> {
        if first { if let Some(a) = self.first.take() { return Ok(a); } }
        let f = if first { navigator_cookies::Fetch::navigation(url, None, true) } else { navigator_cookies::Fetch::subresource(url, &self.host) };
        // Another host on the page's site: the page's origin goes with it,
        // so the server's CORS answer is its own (I6.5).
        let other_host = url::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_ascii_lowercase)).is_none_or(|h| h != self.host);
        let f = if !first && other_host { f.from_origin(&self.origin) } else { f };
        Self::answer(self.cookies.lock().unwrap().fetch_once(self.view, &f))
    }

    fn post(&mut self, url: &str, content_type: &str, body: &[u8]) -> Result<Got, String> {
        let f = navigator_cookies::Fetch::script_post(url, &self.host, content_type, body.to_vec()).from_origin(&self.origin);
        Self::answer(self.cookies.lock().unwrap().fetch_once(self.view, &f))
    }
}

/// Where a live page's `localStorage` is kept (I6.3): navigatord's
/// [`crate::live_storage::Stores`], or a test's stand-in. `page` is the
/// delivered page's address, whose site names the store.
pub trait LocalStorage {
    fn load(&mut self, page: &str) -> Result<Vec<u8>, String>;
    fn apply(&mut self, page: &str, batch: &[u8]) -> Result<(), String>;
    /// Its IndexedDB (I6.7): the site's databases, and one part of a commit.
    /// `cache`: the page's Cache API store rather than its IndexedDB
    /// (I6.7); both are `live_idb` stores.
    fn load_idb(&mut self, _page: &str, _cache: bool) -> Result<Vec<u8>, String> { Err("no IndexedDB here".into()) }
    fn apply_idb(&mut self, _page: &str, _part: &[u8], _cache: bool) -> Result<(), String> { Err("no IndexedDB here".into()) }
    /// What the page's site keeps here, and what it may: `localStorage`
    /// and the IndexedDB and Cache API stores together (§15.1 of
    /// navigator-ui.md, the Live code popover).
    fn used(&mut self, _page: &str) -> Option<(u64, u64)> { None }
}

/// What a site may keep in all its stores together.
pub const STORED_MAX: u64 = (crate::live_storage::QUOTA + crate::live_idb::QUOTA) as u64;

/// The stores of one jar.
pub struct JarStores {
    pub stores: Arc<Mutex<crate::live_storage::Stores>>,
    /// Its IndexedDB and Cache API stores (I6.7), one quota between them.
    pub idb: Arc<Mutex<crate::live_idb::Stores>>,
    pub jar: Option<u64>,
}

impl LocalStorage for JarStores {
    fn load(&mut self, page: &str) -> Result<Vec<u8>, String> {
        let key = crate::live_storage::Key::of(self.jar, page).ok_or("the page's site has no store")?;
        Ok(self.stores.lock().unwrap().load(&key))
    }
    fn apply(&mut self, page: &str, batch: &[u8]) -> Result<(), String> {
        let key = crate::live_storage::Key::of(self.jar, page).ok_or("the page's site has no store")?;
        self.stores.lock().unwrap().apply(&key, batch)
    }
    fn load_idb(&mut self, page: &str, cache: bool) -> Result<Vec<u8>, String> {
        let key = crate::live_idb::Key::of(self.jar, page).ok_or("the page's site has no store")?;
        Ok(self.idb.lock().unwrap().load_kind(&key, if cache { crate::live_idb::Kind::Cache } else { crate::live_idb::Kind::Idb }))
    }
    fn apply_idb(&mut self, page: &str, part: &[u8], cache: bool) -> Result<(), String> {
        let key = crate::live_idb::Key::of(self.jar, page).ok_or("the page's site has no store")?;
        self.idb.lock().unwrap().apply_kind(&key, if cache { crate::live_idb::Kind::Cache } else { crate::live_idb::Kind::Idb }, part)
    }
    fn used(&mut self, page: &str) -> Option<(u64, u64)> {
        let ls = crate::live_storage::Key::of(self.jar, page)?;
        let idb = crate::live_idb::Key::of(self.jar, page)?;
        let kept = self.stores.lock().unwrap().used(&ls);
        let indexed = self.idb.lock().unwrap().used(&idb);
        Some(((kept + indexed) as u64, STORED_MAX))
    }
}

/// What the serve loop saw, for the log and the UI.
#[derive(Debug, Default, PartialEq)]
pub struct Served {
    pub requests: usize,
    /// Of `requests`, those for the network (not navigatord's own addresses).
    pub network: usize,
    pub bytes: u64,
    /// Refused requests' hosts and sites, as `refused_target` spells them.
    pub refused: Vec<String>,
}

/// What the serve loop tells its run, and what it keeps for it.
pub trait Hooks {
    /// The page itself was delivered, with its address.
    fn started(&mut self, _page: &str) {}
    /// The page's `localStorage` (I6.3), if it has one here.
    fn storage(&mut self) -> Option<&mut dyn LocalStorage> { None }
    /// The page's stores were loaded or changed.
    fn stored(&mut self, _page: &str) {}
    /// The page's script made a file to save (I6.4), named `name`, of the
    /// content type `kind`.
    fn made(&mut self, _page: &str, _name: &str, _kind: &str, _bytes: Vec<u8>) -> Result<(), String> {
        Err("no downloads here".into())
    }
    /// A request was refused: its host and site, once for each.
    fn refused(&mut self, _target: &str) {}
    /// The reporter says the page asked for an API I6 refuses.
    fn refused_api(&mut self, _api: limen::web_island::Api) {}
    /// The file the person picked for the page (I6.4), taken once.
    fn picked(&mut self) -> Option<Vec<u8>> { None }
    /// The page asked to be printed (I6.4).
    fn print(&mut self) {}
    /// This engine runs a page's frame (I6.5): its reporter works as the
    /// frame's side.
    fn is_frame(&self) -> bool { false }
    /// The page's script reported one of its cross-origin frames (I6.5).
    fn frame(&mut self, _ask: crate::live_frames::FrameAsk) {}
    /// The script posted a message (I6.5): to frame `to` from the page, or
    /// to the page from a frame (`None`).
    fn post_message(&mut self, _to: Option<u32>, _target: &str, _data: String) -> Result<(), &'static str> {
        Err("no frames here")
    }
    /// The messages waiting for this engine, as JSON.
    fn messages(&mut self) -> Vec<u8> { b"[]".to_vec() }
}

/// Serve one live page's engine on its fetch channel until it closes it.
/// `page` is the address the person pressed on.
pub fn serve(socket: UnixStream, page: &str, upstream: &mut dyn Resources, hooks: &mut dyn Hooks) -> Result<Served, String> {
    let mut delivered_url = String::new();
    socket.set_write_timeout(Some(Duration::from_secs(30))).map_err(|e| e.to_string())?;
    socket.set_read_timeout(Some(IDLE_POLL)).map_err(|e| e.to_string())?;
    let (mut input, mut output) = nuntius::server::channel(socket).map_err(|e| e.to_string())?;
    let mut served = Served::default();
    let first = Instant::now();
    let mut delivered = false;
    let (mut own_window, mut own_count) = (Instant::now(), 0u32);
    loop {
        let request = match input.recv() {
            Ok(Some(r)) => r,
            Ok(None) => return Ok(served),
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                if !delivered && first.elapsed() > FIRST_REQUEST {
                    return Err("the engine never asked for its page".into());
                }
                continue;
            }
            Err(e) => return Err(e.to_string()),
        };
        let open = match request {
            Request::Open(open) => open,
            Request::Cancel { .. } | Request::Budget { .. } => continue,
            _ => return Err("a live page's engine asked for something other than a resource".into()),
        };
        // Navigatord's own addresses are counted apart: a page with frames
        // asks for its messages every quarter second.
        let own = delivered && open.url.starts_with("https://live-page.atrium.invalid/");
        // An IndexedDB commit is many parts in a row (I6.7: 32 KiB each, so
        // a 30 MiB transaction is about 960); it is bounded by the run's and
        // the site's quota instead of the rate.
        if own && open.url != limen::web_island::LIVE_IDB && open.url != limen::web_island::LIVE_CACHE {
            // A rate, not a lifetime count: a page with frames reads its
            // messages for as long as it runs.
            if own_window.elapsed() >= Duration::from_secs(1) { (own_window, own_count) = (Instant::now(), 0); }
            own_count += 1;
            if own_count > OWN_PER_SECOND {
                // Loud once a window: a silent refusal reads as a broken page.
                if own_count == OWN_PER_SECOND + 1 {
                    eprintln!("navigatord: a live page asked navigatord more than {OWN_PER_SECOND} times in a second; refused until the second is over");
                }
                output.send(Event::Error { id: open.id, code: ErrorCode::Policy }).map_err(|e| e.to_string())?;
                continue;
            }
        }
        served.requests += 1;
        if !own { served.network += 1; }
        if served.network > MAX_REQUESTS {
            return Err("the page made too many requests".into());
        }
        let id = open.id;
        let mut refuse = |why: &str, code: ErrorCode, served: &mut Served, hooks: &mut dyn Hooks| -> Result<(), String> {
            let target = crate::refused_target(&open.url);
            eprintln!("navigatord: a live page's request refused: {why} ({target})");
            if served.refused.len() < 64 && !served.refused.contains(&target) { hooks.refused(&target); }
            if served.refused.len() < 64 { served.refused.push(target); }
            output.send(Event::Error { id, code }).map_err(|e| e.to_string())
        };
        // A whole GET, or a script's POST with its body (I6.3); never a
        // range or a conditional request.
        let post = match (&open.method, &open.body) {
            (Method::Get, None) => None,
            (Method::Post, Some(b)) if delivered && b.bytes.len() <= navigator_cookies::BODY_MAX
                && !b.content_type.is_empty() && b.content_type.len() <= navigator_cookies::CONTENT_TYPE_MAX
                && b.content_type.bytes().all(|c| (0x20..0x7f).contains(&c)) => Some(b),
            _ => {
                refuse("only a whole GET, or a POST with its body", ErrorCode::Policy, &mut served, hooks)?;
                continue;
            }
        };
        if open.range.is_some() || open.if_range.is_some() || open.deliver != Deliver::Stream {
            refuse("only a whole resource", ErrorCode::Policy, &mut served, hooks)?;
            continue;
        }
        // The page's storage (I6.3): loaded once, changed in batches, kept
        // by navigatord for the page's site.
        let cache = open.url == limen::web_island::LIVE_CACHE;
        let idb = open.url == limen::web_island::LIVE_IDB || cache;
        if delivered && (open.url == limen::web_island::LIVE_STORAGE || idb) {
            let answer = match (hooks.storage(), post) {
                (None, _) => Err("no storage here".to_string()),
                (Some(l), None) if idb => l.load_idb(&delivered_url, cache).map(|b| (200, b)),
                (Some(l), Some(b)) if idb => l.apply_idb(&delivered_url, &b.bytes, cache).map(|_| (204, vec![])),
                (Some(l), None) => l.load(&delivered_url).map(|b| (200, b)),
                (Some(l), Some(b)) => l.apply(&delivered_url, &b.bytes).map(|_| (204, vec![])),
            };
            // Counted on a load, a localStorage batch, and a commit's last
            // part: a part only buffered changes nothing.
            let committed = match post {
                Some(b) if idb => b.bytes.get(crate::live_idb::HEADER - 1) == Some(&1),
                _ => true,
            };
            if answer.is_ok() && committed {
                hooks.stored(&delivered_url);
            }
            match answer {
                Ok((status, body)) if body.len() as u64 <= open.limits.max_bytes => {
                    let length = body.len() as u64;
                    output.send(Event::Head { id, status, final_url: open.url.clone(), redirects: 0,
                        content_type: Some("application/octet-stream".into()), length: Some(length), accept_ranges: false, validator: None })
                        .map_err(|e| e.to_string())?;
                    for (i, bytes) in body.chunks(nuntius::DATA_MAX).enumerate() {
                        output.send(Event::Data { id, offset: (i * nuntius::DATA_MAX) as u64, bytes: bytes.to_vec() }).map_err(|e| e.to_string())?;
                    }
                    output.send(Event::Done { id, bytes: length }).map_err(|e| e.to_string())?;
                }
                Ok(_) => refuse("the store is over the engine's bound", ErrorCode::TooBig, &mut served, hooks)?,
                Err(why) => {
                    eprintln!("navigatord: a live page's storage: {why}");
                    output.send(Event::Error { id, code: ErrorCode::Policy }).map_err(|e| e.to_string())?;
                }
            }
            continue;
        }
        // The reporter's addresses (I6.4), on navigatord's own name: its
        // script, a refused API, a file the page's script made. Only after
        // the page; none goes to the network.
        if delivered && open.url.starts_with("https://live-page.atrium.invalid/") && open.url != limen::web_island::LIVE_STORAGE && !idb {
            let (status, kind, body): (u16, Option<&str>, std::borrow::Cow<[u8]>) = match (reporter_request(&open.url), post) {
                (Some(Reporter::Script), None) => (200, Some("text/javascript"), REPORTER.into()),
                // Frames and their messages (I6.5).
                (Some(Reporter::Frame(ask)), None) if !hooks.is_frame() => {
                    hooks.frame(ask);
                    (204, None, b"".into())
                }
                (Some(Reporter::Message { to }), Some(b)) if to.is_some() != hooks.is_frame() => {
                    let text = String::from_utf8(b.bytes.clone()).ok().filter(|t| t.len() <= crate::live_frames::MESSAGE_MAX);
                    let target = reporter_target(&open.url);
                    match (text, target) {
                        (Some(text), Some(target)) => match hooks.post_message(to, &target, text) {
                            Ok(()) => (204, None, b"".into()),
                            Err(why) => {
                                eprintln!("navigatord: a live page's message not delivered: {why}");
                                (204, None, b"".into())
                            }
                        },
                        _ => {
                            refuse("a message out of shape", ErrorCode::Policy, &mut served, hooks)?;
                            continue;
                        }
                    }
                }
                (Some(Reporter::Messages), None) => (200, Some("application/json"), hooks.messages().into()),
                (Some(Reporter::Print), None) => {
                    hooks.print();
                    (204, None, b"".into())
                }
                (Some(Reporter::Refused(api)), None) => {
                    hooks.refused_api(api);
                    (204, None, b"".into())
                }
                // The file the person picked (I6.4), once.
                (Some(Reporter::PickedFile), None) => match hooks.picked() {
                    Some(bytes) if bytes.len() as u64 <= open.limits.max_bytes => (200, Some("application/octet-stream"), bytes.into()),
                    _ => {
                        refuse("no picked file waiting", ErrorCode::Policy, &mut served, hooks)?;
                        continue;
                    }
                },
                (Some(Reporter::Download { name, kind }), Some(b)) => match hooks.made(&delivered_url, &name, &kind, b.bytes.clone()) {
                    Ok(()) => (204, None, b"".into()),
                    Err(why) => {
                        eprintln!("navigatord: a live page's file not offered: {why}");
                        refuse("a file the page made, not offered", ErrorCode::Policy, &mut served, hooks)?;
                        continue;
                    }
                },
                _ => {
                    refuse("not one of the reporter's requests", ErrorCode::Policy, &mut served, hooks)?;
                    continue;
                }
            };
            output.send(Event::Head { id, status, final_url: open.url.clone(), redirects: 0, content_type: kind.map(str::to_string),
                length: Some(body.len() as u64), accept_ranges: false, validator: None }).map_err(|e| e.to_string())?;
            // The page's script reads its messages from another origin
            // (navigatord's own name): the answer allows any, without
            // credentials (I6.5).
            if open.url == limen::web_island::LIVE_MESSAGES {
                output.send(Event::Cors { id, allow_origin: "*".into(), allow_credentials: false }).map_err(|e| e.to_string())?;
            }
            for (i, bytes) in body.chunks(nuntius::DATA_MAX).enumerate() {
                output.send(Event::Data { id, offset: (i * nuntius::DATA_MAX) as u64, bytes: bytes.to_vec() }).map_err(|e| e.to_string())?;
            }
            output.send(Event::Done { id, bytes: body.len() as u64 }).map_err(|e| e.to_string())?;
            continue;
        }
        // The engine's first request is for the page; it names no address.
        let url = if !delivered {
            if open.url != LIVE_PAGE {
                return Err("the engine's first request was not for its page".into());
            }
            page.to_string()
        } else {
            if let Err(why) = scope(page, &open.url) {
                refuse(why, ErrorCode::Policy, &mut served, hooks)?;
                continue;
            }
            open.url.clone()
        };
        let got = match post {
            Some(b) => upstream.post(&url, &b.content_type, &b.bytes),
            None => upstream.get(&url, !delivered),
        };
        let Got { status, kind, body, cors } = match got {
            Ok(r) => r,
            Err(why) if delivered => {
                eprintln!("navigatord: a live page's resource failed: {why}");
                output.send(Event::Error { id, code: ErrorCode::Connect }).map_err(|e| e.to_string())?;
                continue;
            }
            Err(why) => return Err(format!("the page: {why}")),
        };
        if !delivered && (status != 200 || kind != "text/html") {
            return Err(format!("the page answered {status} {kind}, not an HTML page"));
        }
        let body = if delivered { body } else { with_reporter_for(&body, hooks.is_frame()) };
        let length = body.len() as u64;
        if length > open.limits.max_bytes || served.bytes.saturating_add(length) > MAX_BYTES {
            refuse("over the page's byte bound", ErrorCode::TooBig, &mut served, hooks)?;
            continue;
        }
        served.bytes += length;
        output.send(Event::Head {
            id,
            status,
            final_url: url.clone(),
            redirects: 0,
            content_type: (kind != "-").then_some(kind),
            length: Some(length),
            accept_ranges: false,
            validator: None,
        }).map_err(|e| e.to_string())?;
        if let Some((allow_origin, allow_credentials)) = cors.filter(|_| delivered) {
            output.send(Event::Cors { id, allow_origin, allow_credentials }).map_err(|e| e.to_string())?;
        }
        for (i, bytes) in body.chunks(nuntius::DATA_MAX).enumerate() {
            output.send(Event::Data { id, offset: (i * nuntius::DATA_MAX) as u64, bytes: bytes.to_vec() }).map_err(|e| e.to_string())?;
        }
        output.send(Event::Done { id, bytes: length }).map_err(|e| e.to_string())?;
        if !delivered {
            delivered = true;
            delivered_url = url.clone();
            hooks.started(&url);
        }
    }
}

/// The script put first in a live page (I6.4), [`LIVE_REPORTER`]'s body.
///
/// [`LIVE_REPORTER`]: limen::web_island::LIVE_REPORTER
const REPORTER: &[u8] = include_bytes!("live_reporter.js");

/// One of the reporter's requests.
#[derive(Debug, PartialEq)]
enum Reporter {
    Script,
    Frame(crate::live_frames::FrameAsk),
    /// A message to frame `to`, or to the page (`None`), for the target in
    /// the address's `target`.
    Message { to: Option<u32> },
    Messages,
    Print,
    PickedFile,
    Refused(limen::web_island::Api),
    Download { name: String, kind: String },
}

fn reporter_request(url: &str) -> Option<Reporter> {
    use limen::web_island::{Api, LIVE_DOWNLOAD, LIVE_REFUSED, LIVE_REPORTER};
    if url == LIVE_REPORTER || url == FRAME_REPORTER { return Some(Reporter::Script); }
    if url == limen::web_island::LIVE_FILE { return Some(Reporter::PickedFile); }
    if url == limen::web_island::LIVE_PRINT { return Some(Reporter::Print); }
    if url == limen::web_island::LIVE_MESSAGES { return Some(Reporter::Messages); }
    let (at, query) = url.split_once('?')?;
    if query.len() > navigator_control::URL_MAX + 256 { return None; }
    let u = url::Url::parse(url).ok()?;
    let pairs: Vec<(String, String)> = u.query_pairs().take(8).map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    let get = |k: &str| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    let number = |k: &str| get(k).and_then(|v| (v.len() <= 10 && v.bytes().all(|b| b.is_ascii_digit() || b == b'-')).then(|| v.parse::<i32>().ok()).flatten());
    if at == limen::web_island::LIVE_FRAME {
        use crate::live_frames::FrameAsk;
        let id = get("id")?.parse::<u32>().ok().filter(|v| get("id").is_some_and(|s| s.len() <= 10 && !s.starts_with('+')) && *v > 0)?;
        if get("gone") == Some("1") { return Some(Reporter::Frame(FrameAsk::Gone { id })); }
        let url = get("url").filter(|u| navigator_control::clean(u, navigator_control::URL_MAX))?.to_string();
        return Some(Reporter::Frame(FrameAsk::At { id, url, rect: [number("x")?, number("y")?, number("w")?, number("h")?] }));
    }
    if at == limen::web_island::LIVE_MESSAGE {
        reporter_target(url)?;
        return match get("to") {
            Some(t) => Some(Reporter::Message { to: Some(t.parse::<u32>().ok().filter(|v| *v > 0 && t.len() <= 10 && !t.starts_with('+'))?) }),
            None => Some(Reporter::Message { to: None }),
        };
    }
    if query.len() > 1024 { return None; }
    match at {
        _ if at == LIVE_REFUSED => Api::from_token(get("api")?).map(Reporter::Refused),
        _ if at == LIVE_DOWNLOAD => {
            let kind = get("type").unwrap_or("").split(';').next().unwrap_or("").trim().to_ascii_lowercase();
            let kind = if kind.len() <= 128 && kind.bytes().all(|b| b.is_ascii_graphic()) { kind } else { String::new() };
            Some(Reporter::Download { name: get("name").unwrap_or("").chars().take(255).collect(), kind })
        }
        _ => None,
    }
}

/// The reporter as a frame's engine asks for it (I6.5): the same script,
/// which then works as the frame's side.
const FRAME_REPORTER: &str = "https://live-page.atrium.invalid/reporter.js?frame=1";

/// A message's target origin, from its address's `target`: `*`, `/` or
/// an origin (`postMessage`'s own rule; navigatord compares it).
fn reporter_target(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    let t = u.query_pairs().take(8).find(|(k, _)| k == "target").map(|(_, v)| v.into_owned())?;
    (t == "*" || t == "/" || (t.len() <= 300 && crate::origin(&t).is_some())).then_some(t)
}

/// `html` with the reporter's script first: after a doctype (and any
/// comments before it), so the page keeps its mode.
#[cfg(test)]
pub fn with_reporter(html: &[u8]) -> Vec<u8> { with_reporter_for(html, false) }

/// As [`with_reporter`], for a page's frame when `frame` (I6.5).
pub fn with_reporter_for(html: &[u8], frame: bool) -> Vec<u8> {
    let src = if frame { FRAME_REPORTER } else { limen::web_island::LIVE_REPORTER };
    let tag = format!("<script src=\"{src}\"></script>");
    let mut at = if html.starts_with(b"\xef\xbb\xbf") { 3 } else { 0 };
    let start = at;
    loop {
        while html.get(at).is_some_and(u8::is_ascii_whitespace) { at += 1; }
        let rest = &html[at..];
        if rest.starts_with(b"<!--") {
            match rest.windows(3).position(|w| w == b"-->") { Some(end) => { at += end + 3; continue; } None => { at = start; break; } }
        }
        if rest.get(..9).is_some_and(|d| d.eq_ignore_ascii_case(b"<!doctype")) {
            match rest.iter().position(|&b| b == b'>') { Some(end) => at += end + 1, None => at = start }
        } else {
            at = start;
        }
        break;
    }
    let mut out = Vec::with_capacity(html.len() + tag.len());
    out.extend_from_slice(&html[..at]);
    out.extend_from_slice(tag.as_bytes());
    out.extend_from_slice(&html[at..]);
    out
}

/// Where a live page's proposed navigation may go (I6.2): a plain HTTPS
/// address in its normal spelling, without credentials, of any origin
/// (another origin opens in the document lane). `None` refuses it.
pub fn navigation(page: Option<&str>, to: &str) -> Option<String> {
    page?;
    if !navigator_control::clean(to, navigator_control::URL_MAX) { return None; }
    let u = url::Url::parse(to).ok()?;
    (u.scheme() == "https" && u.username().is_empty() && u.password().is_none() && u.as_str() == to).then(|| to.to_string())
}

/// The page changed its own address (I6.2): taken only as a plain HTTPS
/// address at the page's own origin.
pub fn address_change(page: Option<&str>, to: &str) -> Option<String> {
    let to = navigation(page, to)?;
    (page.and_then(crate::origin).is_some() && page.and_then(crate::origin) == crate::origin(&to)).then_some(to)
}

/// How a page may open windows beside it (I6.8): each on a press of the
/// person's in the page that frescod routed and Limen spends, so one press
/// opens at most one, and at most [`Beside::MAX`] in a run besides.
#[derive(Default)]
pub struct Beside {
    opened: usize,
    /// The refusals the UI was told of: each once a run (its popover
    /// merges repeats anyway), the rest only logged.
    told: Vec<&'static str>,
}

impl Beside {
    pub const MAX: usize = 64;
    /// `spend_press` asks Limen to spend the person's press in the page;
    /// it is asked last, so a window refused anyway spends none.
    pub fn allow(&mut self, spend_press: impl FnOnce() -> bool) -> Result<(), &'static str> {
        if self.opened >= Self::MAX { return Err("a new window past the run's 64"); }
        if !spend_press() { return Err("a new window with no press in the page"); }
        self.opened += 1;
        Ok(())
    }

    /// Whether to tell the UI of the refusal `why`: the first time only.
    pub fn first(&mut self, why: &'static str) -> bool {
        if self.told.contains(&why) { return false; }
        self.told.push(why);
        true
    }
}

/// One running live page: its island, placed over the page area.
pub struct Live {
    pub url: String,
    pub press: navigator_control::Press,
    parent: Arc<Mutex<Option<Parent>>>,
    embed: u64,
    /// Whether the engine started, and the latest placement: the engine
    /// lays the page out at its size (I6.2), told once it runs.
    placed: Arc<Mutex<(bool, [u32; 4])>>,
    /// The broker's view for this run, closed with it (its cookies stay in Arca).
    jar: Option<LiveJar>,
}

/// Tell the engine its page area's size; a refusal is logged, not fatal.
fn tell_size(parent: &mut Parent, embed: u64, rect: [u32; 4]) {
    let Some(size) = limen::web_island::PageSize::new(rect[2], rect[3]) else { return };
    if let Err(e) = parent.send(embed, size.encode()) {
        eprintln!("navigatord: live page size {}x{} not sent: {e:?}", rect[2], rect[3]);
    }
}

/// What a run tells the UI, from its threads.
pub trait Tell: Send + Sync + 'static {
    /// The page was delivered and the engine reported `origin`, the page's.
    fn started(&self, origin: String);
    /// A trail entry for `url` (I6.2): a link, a form's answer.
    fn navigate(&self, url: String);
    /// The page changed its own address within its origin (I6.2).
    fn url_changed(&self, url: String);
    /// A new window the page asked for (I6.2): an entry beside it.
    fn open_beside(&self, url: String);
    /// A file the page offered (I6.4): navigatord holds it and asks.
    fn file(&self, file: Offered);
    /// Something the page was refused (I6.4), in the popover's words.
    fn refused(&self, what: String);
    /// The page asked to be printed (I6.4).
    fn print(&self);
    /// The person copied `text` in the page with the press `token` (I6.4).
    fn copy(&self, text: String, token: [u8; 16]);
    /// The person pressed Ctrl-V in the page, `token` (I6.4).
    fn paste(&self, token: [u8; 16]);
    /// The run ended by itself (`None`: the engine closed) or failed;
    /// once, and never after [`Live::stop`].
    fn finished(&self, why: Option<String>);
    /// What the page's site keeps in its stores, of what it may (bytes).
    fn usage(&self, _stored: u64, _max: u64) {}
}

/// The host of a refused request's target, as `refused_target` spells it.
fn refused_host(target: &str) -> &str {
    target.strip_prefix("host ").and_then(|t| t.split(',').next()).unwrap_or("an unreadable address")
}

/// The largest file a live page is handed from the picker: one resource.
const PICK_MAX: u64 = navigator_prerender::fetch::LIVE_RESOURCE_MAX as u64;
/// After a pick ends, how long before the page may open another: a page
/// can't hold the picker up over the window.
const PICK_AGAIN: Duration = Duration::from_secs(2);

/// A live page's file pick (I6.4): Scrinium's picker over the window,
/// one at a time, one read-only file.
#[derive(Default)]
struct Pick {
    embed: Option<limen::proto::EmbedId>,
    /// The file handed over, until the picker says it is done.
    file: Option<(String, Vec<u8>)>,
    ended: Option<Instant>,
}

impl Pick {
    /// The page asked: whether a picker may open now.
    fn may_open(&self, now: Instant) -> Result<(), &'static str> {
        if self.embed.is_some() { return Err("a file picker is already open"); }
        if self.ended.is_some_and(|t| now.duration_since(t) < PICK_AGAIN) { return Err("a file picker asked for again at once"); }
        Ok(())
    }

    /// A message from the picker, and its descriptor if it came with one.
    /// `Some` when the pick is over: the file, or none.
    fn message(&mut self, bytes: &[u8], fd: Option<std::os::fd::OwnedFd>) -> Option<Option<(String, Vec<u8>)>> {
        use limen::picker::ToParent;
        match (ToParent::decode(bytes), fd) {
            (Some(ToParent::File { name, size, write: false }), Some(fd)) if self.file.is_none() && size <= PICK_MAX => {
                use std::io::Read;
                let mut bytes = Vec::new();
                match std::fs::File::from(fd).take(PICK_MAX + 1).read_to_end(&mut bytes) {
                    Ok(n) if n as u64 <= PICK_MAX => self.file = Some((name, bytes)),
                    _ => eprintln!("navigatord: the picked file could not be read whole"),
                }
                None
            }
            (Some(ToParent::File { .. }), _) => {
                eprintln!("navigatord: the picker handed over a file the page can't take (too big, writable, or a second)");
                None
            }
            (Some(ToParent::Done { .. } | ToParent::Cancelled | ToParent::Failed { .. }), None) => Some(self.end()),
            _ => None,
        }
    }

    /// The pick is over: the file, if one came.
    fn end(&mut self) -> Option<(String, Vec<u8>)> {
        self.embed = None;
        self.ended = Some(Instant::now());
        self.file.take()
    }
}

/// Open Scrinium's picker over `window` for the page at `page`.
fn open_picker(connection: &mut Parent, window: [u8; 16], rect: [u32; 4], page: &str) -> Result<limen::proto::EmbedId, String> {
    let host = url::Url::parse(page).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
    let mut title = format!("Choose a file for {host}");
    while title.len() > limen::picker::MAX_TITLE { title.pop(); }
    let filter = limen::picker::Filter { title, ..limen::picker::Filter::file() };
    let e = connection.request("picker", window).map_err(|e| format!("the picker: {e:?}"))?;
    let bounds = crate::widget::bounds(rect).unwrap_or([0.0, 0.0, 640.0, 440.0]);
    let opened = connection.place(e, Some(bounds)).and_then(|_| connection.send(e, limen::picker::ToChild::Open(filter).encode()));
    if let Err(err) = opened {
        let _ = connection.detach(e);
        return Err(format!("the picker: {err:?}"));
    }
    Ok(e)
}

/// The pick is over: the file waits for the engine's GET, and the engine
/// is told its name, or that there is none.
fn finish_pick(connection: &mut Parent, embed: u64, file: Option<(String, Vec<u8>)>, picked: &Mutex<Option<Vec<u8>>>) {
    use limen::web_island::FilePicked;
    let answer = match file {
        Some((name, bytes)) => {
            let name: String = name.chars().filter(|c| !c.is_control() && *c != '/').take(limen::web_island::PICKED_NAME_MAX / 4).collect();
            let name = if name.is_empty() { "file".to_string() } else { name };
            eprintln!("navigatord: the person picked a file for a live page ({} bytes)", bytes.len());
            *picked.lock().unwrap() = Some(bytes);
            FilePicked::Chosen { name }
        }
        None => FilePicked::Cancelled,
    };
    tell_picked(connection, embed, &answer);
}

/// Tell the engine how its pick ended.
fn tell_picked(connection: &mut Parent, embed: u64, answer: &limen::web_island::FilePicked) {
    if let Some(bytes) = answer.encode() {
        if let Err(e) = connection.send(embed, bytes) { eprintln!("navigatord: the picked file not told: {e:?}"); }
    }
}

/// The serve loop's hooks for a running page.
struct RunHooks {
    delivered: std::sync::mpsc::Sender<String>,
    storage: Option<JarStores>,
    tell: Arc<dyn Tell>,
    /// The APIs told already, each once.
    apis: std::collections::HashSet<limen::web_island::Api>,
    /// The file the person picked, waiting for the engine's GET.
    picked: Arc<Mutex<Option<Vec<u8>>>>,
    /// When the page last asked to be printed.
    printed: Option<Instant>,
    /// Its frames' reports, for the watcher (I6.5).
    asks: Arc<Mutex<VecDeque<FrameAsk>>>,
    mail: Arc<Mutex<Mailbox>>,
    /// The stores' count last told to the UI.
    told_usage: Option<(u64, u64)>,
}

/// How long after a print a page may ask for another.
const PRINT_AGAIN: Duration = Duration::from_secs(5);

impl Hooks for RunHooks {
    fn started(&mut self, page: &str) {
        if let Some(o) = crate::origin(page) { self.mail.lock().unwrap().page_started(o); }
        let _ = self.delivered.send(page.to_string());
    }
    fn frame(&mut self, ask: FrameAsk) {
        // The latest place of a frame replaces one not yet looked at.
        let mut asks = self.asks.lock().unwrap();
        let id = match &ask { FrameAsk::At { id, .. } | FrameAsk::Gone { id } => *id };
        if let Some(at) = asks.iter().position(|a| matches!(a, FrameAsk::At { id: i, .. } if *i == id)) {
            asks[at] = ask;
        } else if asks.len() < crate::live_frames::ASKS_MAX {
            asks.push_back(ask);
        }
    }
    fn post_message(&mut self, to: Option<u32>, target: &str, data: String) -> Result<(), &'static str> {
        self.mail.lock().unwrap().from_page(to.ok_or("from the page, a message names its frame")?, target, data)
    }
    fn messages(&mut self) -> Vec<u8> { self.mail.lock().unwrap().take_for_page() }
    fn storage(&mut self) -> Option<&mut dyn LocalStorage> { self.storage.as_mut().map(|s| s as &mut dyn LocalStorage) }
    fn stored(&mut self, page: &str) {
        if let Some(u) = self.storage.as_mut().and_then(|s| s.used(page)) {
            // Told only when it changed.
            if self.told_usage != Some(u) {
                self.told_usage = Some(u);
                self.tell.usage(u.0, u.1);
            }
        }
    }
    fn made(&mut self, page: &str, name: &str, kind: &str, bytes: Vec<u8>) -> Result<(), String> {
        if bytes.is_empty() { return Err("an empty file".into()); }
        let (what, ext) = crate::files::kind(&bytes).unwrap_or(match kind {
            "text/csv" => ("CSV file", "csv"),
            "text/plain" => ("text file", "txt"),
            "application/json" => ("JSON file", "json"),
            "text/html" => ("web page", "html"),
            _ => ("file", "bin"),
        });
        let name = crate::files::safe_name(name, ext);
        self.tell.file(Offered { url: page.to_string(), name, kind: what.to_string(), bytes });
        Ok(())
    }
    fn refused(&mut self, target: &str) { self.tell.refused(format!("a request to {}", refused_host(target))); }
    fn picked(&mut self) -> Option<Vec<u8>> { self.picked.lock().unwrap().take() }
    fn print(&mut self) {
        // Prelum's sheet over the window, at most once every PRINT_AGAIN:
        // a page can't keep the window under print sheets.
        let now = Instant::now();
        if self.printed.is_some_and(|t| now.duration_since(t) < PRINT_AGAIN) {
            self.tell.refused("a print asked for again at once".into());
            return;
        }
        self.printed = Some(now);
        self.tell.print();
    }
    fn refused_api(&mut self, api: limen::web_island::Api) {
        if self.apis.insert(api) {
            eprintln!("navigatord: a live page was refused {}", api.label());
            self.tell.refused(api.label().to_string());
        }
    }
}

/// A frame's engine, served (I6.5): its own site's scope, a throwaway
/// jar, no storage, no files; its messages go through the page's mailbox.
struct FrameHooks {
    id: u32,
    mail: Arc<Mutex<Mailbox>>,
    tell: Arc<dyn Tell>,
    apis: std::collections::HashSet<limen::web_island::Api>,
}

impl Hooks for FrameHooks {
    fn is_frame(&self) -> bool { true }
    fn started(&mut self, page: &str) {
        if let Some(o) = crate::origin(page) { self.mail.lock().unwrap().frame_started(self.id, o); }
    }
    fn refused(&mut self, target: &str) { self.tell.refused(format!("a request to {} from a frame", refused_host(target))); }
    fn refused_api(&mut self, api: limen::web_island::Api) {
        if self.apis.insert(api) { self.tell.refused(format!("{} (a frame)", api.label())); }
    }
    fn post_message(&mut self, to: Option<u32>, target: &str, data: String) -> Result<(), &'static str> {
        if to.is_some() { return Err("a frame posts only to its page"); }
        self.mail.lock().unwrap().from_frame(self.id, target, data)
    }
    fn messages(&mut self) -> Vec<u8> { self.mail.lock().unwrap().take_for_frame(self.id) }
}

/// One of a live page's frames, as the watcher keeps it (I6.5).
struct FrameRun {
    url: String,
    /// Where the page's script said it is, in the page's viewport.
    rect: [i32; 4],
    embed: u64,
    /// Its fetch channel was taken and served.
    served: bool,
    started: bool,
    reports: limen::web_island::Lifecycle,
    /// Its throwaway jar's view, closed and forgotten when it ends.
    view: Option<LiveJar>,
}

/// Throwaway jar ids for frames, apart from the trails' (I6.5).
static FRAME_JARS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1 << 62);

impl FrameRun {
    fn end(self, connection: &mut Parent, mail: &Mutex<Mailbox>, id: u32) {
        let _ = connection.detach(self.embed);
        mail.lock().unwrap().frame_gone(id);
        if let Some(j) = self.view { j.cookies.lock().unwrap().close_throwaway(j.view); }
    }

    /// Put its island where the page's script said, in the page's place;
    /// hidden while none of it shows.
    fn place(&self, connection: &mut Parent, page: [u32; 4]) {
        match crate::live_frames::frame_rect(page, self.rect).and_then(|r| crate::widget::bounds(r).ok().map(|b| (r, b))) {
            Some((r, b)) => {
                let _ = connection.set_visible(self.embed, true);
                let _ = connection.place(self.embed, Some(b));
                if self.started { tell_size(connection, self.embed, r); }
            }
            None => { let _ = connection.set_visible(self.embed, false); }
        }
    }
}

/// What the watcher does with one of the page script's frame reports.
#[allow(clippy::too_many_arguments)]
fn frame_ask(connection: &mut Parent, window: [u8; 16], frames: &mut HashMap<u32, FrameRun>, starts: &mut usize, ask: FrameAsk,
             page: Option<&str>, page_rect: [u32; 4], mail: &Mutex<Mailbox>, tell: &Arc<dyn Tell>, can_run: bool) {
    use crate::live_frames::{frame_address, FRAMES_MAX, FRAME_MIN, FRAME_STARTS_MAX};
    let (id, url, rect) = match ask {
        FrameAsk::Gone { id } => {
            if let Some(run) = frames.remove(&id) { run.end(connection, mail, id); }
            return;
        }
        FrameAsk::At { id, url, rect } => (id, url, rect),
    };
    if let Some(run) = frames.get_mut(&id) {
        if run.url == url {
            run.rect = rect;
            if run.served { run.place(connection, page_rect); }
            return;
        }
        // The frame went somewhere else: its engine goes, a new one starts.
        if let Some(run) = frames.remove(&id) { run.end(connection, mail, id); }
    }
    let Some(page) = page else { return };
    let host = crate::refused_target(&url);
    let why = match frame_address(page, &url) {
        _ if !can_run => Some("no jar for a frame here"),
        None => Some("not a plain https address of another origin"),
        Some(_) if frames.len() >= FRAMES_MAX || *starts >= FRAME_STARTS_MAX => Some("more frames than a live page runs"),
        Some(_) if rect[2] < FRAME_MIN as i32 || rect[3] < FRAME_MIN as i32 => {
            // Too small to show (a tracking pixel): not started, not told.
            eprintln!("navigatord: a live page's frame too small to run ({host})");
            return;
        }
        Some(_) => None,
    };
    if let Some(why) = why {
        eprintln!("navigatord: a live page's frame not run: {why} ({host})");
        tell.refused(format!("a frame from {}", refused_host(&host)));
        return;
    }
    match connection.request_island(window, 2) {
        Ok(embed) => {
            *starts += 1;
            limen::start_trace("navigatord", "live-frame", &url);
            mail.lock().unwrap().frame_added(id);
            frames.insert(id, FrameRun { url, rect, embed, served: false, started: false, reports: Default::default(), view: None });
        }
        Err(e) => eprintln!("navigatord: a live page's frame engine: {e:?}"),
    }
}

impl Live {
    /// Ask Limen for the island in the UI's `window`, place it at `rect`,
    /// and serve it, telling `tell` what happens.
    pub fn start(
        socket: &Path,
        window: [u8; 16],
        url: String,
        press: navigator_control::Press,
        rect: [u32; 4],
        cfg: Arc<crate::Config>,
        jar: Option<LiveJar>,
        storage: Option<JarStores>,
        tell: Arc<dyn Tell>,
    ) -> Result<Self, String> {
        scope(&url, &url).map_err(|why| format!("this page can't run live: {why}"))?;
        let placed = Arc::new(Mutex::new((false, rect)));
        let rect = crate::widget::bounds(rect)?;
        limen::start_trace("navigatord", "live-start", &url);
        let mut parent = Parent::at(socket).map_err(|e| format!("Limen: {e:?}"))?;
        let embed = parent.request_page_island(window, 1).map_err(|e| format!("engine launch: {e:?}"))?;
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err("the engine gave no fetch channel".into());
            }
            match parent.poll(left).map_err(|e| format!("engine: {e:?}"))? {
                Some(ParentEvent::IslandFetch { embed: id }) if id == embed => break,
                Some(ParentEvent::ChildLost { .. }) => return Err("the engine stopped at launch".into()),
                _ => {}
            }
        }
        let channel = parent.take_island_fetch(embed).ok_or("missing fetch channel")?;
        parent.place(embed, Some(rect)).map_err(|e| format!("place: {e:?}"))?;
        let parent = Arc::new(Mutex::new(Some(parent)));
        let (done, fetched) = std::sync::mpsc::channel();
        let page = url.clone();
        let (delivered_tx, delivered) = std::sync::mpsc::channel::<String>();
        let forms = jar.clone();
        let closing = jar.clone();
        let picked = Arc::new(Mutex::new(None));
        let asks = Arc::new(Mutex::new(VecDeque::new()));
        let mail = Arc::new(Mutex::new(Mailbox::default()));
        let mut hooks = RunHooks { delivered: delivered_tx, storage, tell: tell.clone(), apis: Default::default(), picked: picked.clone(), printed: None,
                                   asks: asks.clone(), mail: mail.clone(), told_usage: None };
        std::thread::spawn(move || {
            let result = match jar {
                Some(mut jar) => serve(channel, &page, &mut jar, &mut hooks),
                None => crate::upstream(&cfg, &crate::instance()).and_then(|mut up| serve(channel, &page, &mut up, &mut hooks)),
            };
            match &result {
                Ok(s) => eprintln!("navigatord: live page served {} requests, {} bytes, {} refused", s.requests, s.bytes, s.refused.len()),
                Err(e) => eprintln!("navigatord: live page: {e}"),
            }
            let _ = done.send(result.map(|_| ()));
        });
        let watched = parent.clone();
        let size = placed.clone();
        std::thread::spawn(move || {
            use limen::web_island::{EndReason, Lifecycle, Report};
            let mut reports = Lifecycle::default();
            let mut started = false;
            let mut pick = Pick::default();
            let mut page = None::<String>;
            let mut frames: HashMap<u32, FrameRun> = HashMap::new();
            let mut beside = Beside::default();
            let mut frame_starts = 0usize;
            let mut page_rect = size.lock().unwrap().1;
            let cookies = forms.as_ref().map(|j| j.cookies.clone());
            let why = loop {
                let mut slot = watched.lock().unwrap();
                let Some(connection) = slot.as_mut() else { return };
                if let Ok(u) = delivered.try_recv() { page = Some(u); }
                // The page's frames (I6.5): started, moved or ended as its
                // script reports them, and moved with the page.
                let now_rect = size.lock().unwrap().1;
                if now_rect != page_rect {
                    page_rect = now_rect;
                    for run in frames.values().filter(|r| r.served) { run.place(connection, page_rect); }
                }
                // Reports wait for the page's engine to have started.
                let waiting: Vec<FrameAsk> = if started { asks.lock().unwrap().drain(..).collect() } else { vec![] };
                for ask in waiting {
                    frame_ask(connection, window, &mut frames, &mut frame_starts, ask, page.as_deref(), page_rect, &mail, &tell, cookies.is_some());
                }
                match connection.poll(Duration::from_millis(40)) {
                    Ok(Some(ParentEvent::Message { embed: id, bytes })) if id == embed => match reports.receive(&bytes) {
                        Some(Report::Started { origin }) => {
                            // The engine's origin must be the page's, as delivered.
                            // The serve thread records the delivery just after the
                            // page's last byte, so an engine that reports at once
                            // can be here first: wait for it briefly.
                            if page.is_none() { page = delivered.recv_timeout(Duration::from_secs(2)).ok(); }
                            let expected = page.as_deref().and_then(crate::origin);
                            if expected.as_deref() != Some(origin) {
                                break Some(format!("the engine reported {origin}, not the page's origin"));
                            }
                            limen::start_trace("navigatord", "live-started", origin);
                            if !std::mem::replace(&mut started, true) { tell.started(origin.to_string()); }
                            let rect = { let mut p = size.lock().unwrap(); p.0 = true; p.1 };
                            tell_size(connection, embed, rect);
                        }
                        Some(Report::Title(_) | Report::Resize { .. } | Report::MediaReady) => {}
                        // The page asked to go somewhere (I6.2): never followed in
                        // the engine. navigatord hands it to the UI, which opens
                        // it as a trail entry: live again if it is this origin's
                        // (`ContinueLive`), in the document lane if not. Within
                        // the origin it is looked at through the jar first, off
                        // this thread: a file is offered, not run (I6.4).
                        // A new window (I6.2): beside the page in the trail, on
                        // a real press in the page, spent (I6.8): the engine's
                        // word that the person pressed is not proof.
                        Some(Report::OpenBeside(to)) => match navigation(page.as_deref(), to) {
                            Some(to) => match beside.allow(|| match connection.spend_press(embed) {
                                Ok(()) => true,
                                Err(e) => { eprintln!("navigatord: no press to spend for a new window: {e:?}"); false }
                            }) {
                                Ok(()) => tell.open_beside(to),
                                Err(why) => {
                                    eprintln!("navigatord: a live page's new window refused: {why} ({})", crate::refused_target(&to));
                                    if beside.first(why) { tell.refused(why.into()); }
                                }
                            },
                            None => {
                                eprintln!("navigatord: a live page's new window refused ({})", crate::refused_target(to));
                                tell.refused("an address it could not open".into());
                            }
                        },
                        Some(Report::Navigate(to)) => match (navigation(page.as_deref(), to), &forms) {
                            (Some(to), Some(jar)) if page.as_deref().and_then(crate::origin) == crate::origin(&to) => {
                                let (jar, tell) = (jar.clone(), tell.clone());
                                std::thread::spawn(move || match jar.peek(&to) {
                                    Ok(Answer::Page(at)) => tell.navigate(at),
                                    Ok(Answer::File(f)) => tell.file(f),
                                    Err(why) => {
                                        eprintln!("navigatord: a live page's navigation to {} failed: {why}", crate::refused_target(&to));
                                        tell.navigate(to);
                                    }
                                });
                            }
                            (Some(to), _) => tell.navigate(to),
                            (None, _) => {
                                eprintln!("navigatord: a live page's navigation refused ({})", crate::refused_target(to));
                                tell.refused("an address it could not go to".into());
                            }
                        },
                        // The page changed its own address (pushState): a trail
                        // entry in the UI. An engine that claims another
                        // origin's address is ended.
                        Some(Report::UrlChanged(to)) => match address_change(page.as_deref(), to) {
                            Some(to) => tell.url_changed(to),
                            None => break Some("the engine claimed another origin's address".into()),
                        },
                        // A form (I6.3): only to the page's own site, sent
                        // through its jar off this thread; the page that
                        // answers opens as the next trail entry.
                        Some(Report::Submit { url: to, body }) => match (page.as_deref().map(|p| scope(p, to)), &forms) {
                            (Some(Ok(())), Some(jar)) => {
                                let (jar, to, body, tell) = (jar.clone(), to.to_string(), body.to_vec(), tell.clone());
                                std::thread::spawn(move || match jar.submit(&to, &body) {
                                    Ok(Answer::Page(at)) => tell.navigate(at),
                                    Ok(Answer::File(f)) => tell.file(f),
                                    Err(why) => eprintln!("navigatord: a live page's form to {} failed: {why}", crate::refused_target(&to)),
                                });
                            }
                            (Some(Err(why)), _) => {
                                eprintln!("navigatord: a live page's form refused: {why} ({})", crate::refused_target(to));
                                tell.refused(format!("a form to {}", refused_host(&crate::refused_target(to))));
                            }
                            _ => eprintln!("navigatord: a live page's form refused: no jar ({})", crate::refused_target(to)),
                        },

                        // A file input (I6.4): Scrinium's picker over the window;
                        // only what the person picks reaches the page.
                        Some(Report::PickFile) => {
                            use limen::web_island::FilePicked;
                            let rect = size.lock().unwrap().1;
                            match pick.may_open(Instant::now()).map_err(String::from).and_then(|_| open_picker(connection, window, rect, page.as_deref().unwrap_or(""))) {
                                Ok(e) => pick.embed = Some(e),
                                Err(why) => {
                                    eprintln!("navigatord: a live page's file picker: {why}");
                                    if why.contains("again") { tell.refused(why.clone()); }
                                    tell_picked(connection, embed, &FilePicked::Cancelled);
                                }
                            }
                        }
                        // The clipboard (I6.4): the press's token goes to the
                        // UI, which spends it on Tabula; the engine has no
                        // clipboard of its own.
                        Some(Report::Copy { text, token }) => tell.copy(text.to_string(), token),
                        Some(Report::Paste { token }) => tell.paste(token),
                        Some(Report::Ended(EndReason::Closed)) => break None,
                        Some(Report::Ended(EndReason::Failed)) => break Some("the page's engine failed".into()),
                        // A live page has no unload question in I6.1: Stop is the person's.
                        Some(Report::BeforeUnload { .. }) | None => break Some("the engine sent a report out of order".into()),
                    },
                    Ok(Some(ParentEvent::IslandStopped { embed: id })) if id == embed => break None,
                    // A frame's engine (I6.5).
                    Ok(Some(ParentEvent::IslandFetch { embed: id })) if frames.values().any(|r| r.embed == id) => {
                        let Some((&fid, run)) = frames.iter_mut().find(|(_, r)| r.embed == id) else { continue };
                        let jar = cookies.clone().ok_or("no jar".to_string())
                            .and_then(|c| LiveJar::open(c, &run.url, Some(FRAME_JARS.fetch_add(1, std::sync::atomic::Ordering::Relaxed))));
                        match (connection.take_island_fetch(id), jar) {
                            (Some(channel), Ok(jar)) => {
                                run.view = Some(jar.clone());
                                run.served = true;
                                let (url, mail, tell) = (run.url.clone(), mail.clone(), tell.clone());
                                std::thread::spawn(move || {
                                    let mut jar = jar;
                                    let mut hooks = FrameHooks { id: fid, mail, tell, apis: Default::default() };
                                    match serve(channel, &url, &mut jar, &mut hooks) {
                                        Ok(s) => eprintln!("navigatord: a live page's frame served {} requests, {} refused", s.requests, s.refused.len()),
                                        Err(e) => eprintln!("navigatord: a live page's frame: {e}"),
                                    }
                                });
                                run.place(connection, page_rect);
                            }
                            (_, why) => {
                                eprintln!("navigatord: a live page's frame not served: {:?}", why.err());
                                if let Some(run) = frames.remove(&fid) { run.end(connection, &mail, fid); }
                            }
                        }
                    }
                    Ok(Some(ParentEvent::Message { embed: id, bytes })) if frames.values().any(|r| r.embed == id) => {
                        let Some((&fid, run)) = frames.iter_mut().find(|(_, r)| r.embed == id) else { continue };
                        let ended = match run.reports.receive(&bytes) {
                            Some(Report::Started { origin }) if crate::origin(&run.url).as_deref() == Some(origin) => {
                                run.started = true;
                                run.place(connection, page_rect);
                                false
                            }
                            Some(Report::Started { .. }) => {
                                eprintln!("navigatord: a live page's frame reported another origin; ended");
                                true
                            }
                            Some(Report::Copy { text, token }) => { tell.copy(text.to_string(), token); false }
                            Some(Report::Paste { token }) => { tell.paste(token); false }
                            Some(Report::Title(_) | Report::Resize { .. } | Report::MediaReady | Report::UrlChanged(_)) => false,
                            // A frame does not navigate the trail, send forms
                            // through a jar, or pick files in I6.5.
                            Some(Report::Navigate(_) | Report::Submit { .. } | Report::OpenBeside(_)) => {
                                tell.refused("a frame's navigation or form".into());
                                false
                            }
                            Some(Report::PickFile) => {
                                tell.refused("a file for a frame".into());
                                tell_picked(connection, id, &limen::web_island::FilePicked::Cancelled);
                                false
                            }
                            Some(Report::Ended(_)) | Some(Report::BeforeUnload { .. }) | None => true,
                        };
                        if ended { if let Some(run) = frames.remove(&fid) { run.end(connection, &mail, fid); } }
                    }
                    Ok(Some(ParentEvent::ChildLost { embed: id } | ParentEvent::Detached { embed: id } | ParentEvent::IslandStopped { embed: id }))
                        if frames.values().any(|r| r.embed == id) =>
                    {
                        if let Some(fid) = frames.iter().find(|(_, r)| r.embed == id).map(|(f, _)| *f) {
                            eprintln!("navigatord: a live page's frame ended");
                            if let Some(run) = frames.remove(&fid) { run.end(connection, &mail, fid); }
                        }
                    }
                    // The picker (I6.4): its file, then its end; or it went.
                    Ok(Some(ParentEvent::Message { embed: id, bytes } | ParentEvent::MessageFd { embed: id, bytes })) if Some(id) == pick.embed => {
                        let fd = connection.take_fd();
                        if let Some(file) = pick.message(&bytes, fd) {
                            let _ = connection.detach(id);
                            finish_pick(connection, embed, file, &picked);
                        }
                    }
                    Ok(Some(ParentEvent::ChildLost { embed: id } | ParentEvent::Detached { embed: id })) if Some(id) == pick.embed => {
                        let file = pick.end();
                        finish_pick(connection, embed, file, &picked);
                    }
                    // Only the island's own end ends the run: a picker this
                    // run already finished (and detached) reports its detach
                    // too, after `pick.embed` is cleared.
                    Ok(Some(ParentEvent::ChildLost { embed: id } | ParentEvent::Detached { embed: id })) if id != embed => {}
                    ref ev @ (Ok(Some(ParentEvent::ChildLost { .. } | ParentEvent::Detached { .. })) | Err(_)) => {
                        // The engine exited by itself: most often it reached
                        // its memory limit (rctl kills it; Limen can't say).
                        let lost = matches!(ev, Ok(Some(ParentEvent::ChildLost { .. })));
                        let why = match fetched.recv_timeout(Duration::from_millis(100)) {
                            Ok(Err(e)) => e,
                            _ if lost => "the page's engine stopped; it may have used more memory than a live page may".into(),
                            _ => "the page's engine stopped".into(),
                        };
                        break Some(why);
                    }
                    Ok(Some(ParentEvent::MessageFd { .. })) => { drop(connection.take_fd()); }
                    _ => {}
                }
                if let Ok(Err(e)) = fetched.try_recv() { break Some(e); }
                drop(slot);
                std::thread::sleep(Duration::from_millis(5));
            };
            for (_, run) in frames.drain() {
                if let Some(j) = run.view { j.cookies.lock().unwrap().close_throwaway(j.view); }
            }
            if watched.lock().unwrap().take().is_some() {
                tell.finished(why);
            }
        });
        Ok(Live { url, press, parent, embed, placed, jar: closing })
    }

    /// Move the island with the page area (the window resized).
    pub fn place(&self, rect: [u32; 4]) -> Result<(), String> {
        let bounds = crate::widget::bounds(rect)?;
        let mut parent = self.parent.lock().unwrap();
        let Some(p) = parent.as_mut() else { return Err("the live page has ended".into()) };
        p.place(self.embed, Some(bounds)).map_err(|e| format!("place: {e:?}"))?;
        let running = {
            let mut placed = self.placed.lock().unwrap();
            let resized = placed.1[2..] != rect[2..];
            placed.1 = rect;
            placed.0 && resized
        };
        if running { tell_size(p, self.embed, rect); }
        Ok(())
    }

    /// The text the person pasted (I6.4), for the engine to insert where
    /// its focus is.
    pub fn paste(&self, text: &str) -> Result<(), String> {
        let bytes = limen::web_island::Pasted(text.to_string()).encode().ok_or("nothing to paste, or too much")?;
        let mut parent = self.parent.lock().unwrap();
        let p = parent.as_mut().ok_or("the live page has ended")?;
        p.send(self.embed, bytes).map_err(|e| format!("paste: {e:?}"))
    }

    /// End it: the island goes, and `finished` is not called.
    pub fn stop(self) {}
}

impl Drop for Live {
    fn drop(&mut self) {
        // Dropping Limen's connection ends the embed and its engine.
        self.parent.lock().unwrap().take();
        if let Some(j) = &self.jar { j.cookies.lock().unwrap().close(j.view); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuntius::{Open};

    #[test]
    fn a_proposed_navigation_is_one_plain_https_address() {
        let page = Some("https://app.example/app.html");
        assert_eq!(navigation(page, "https://app.example/second.html").as_deref(), Some("https://app.example/second.html"));
        assert_eq!(navigation(page, "https://elsewhere.example/").as_deref(), Some("https://elsewhere.example/"), "another origin opens in the document lane");
        assert_eq!(navigation(None, "https://app.example/second.html"), None, "not before the page was delivered");
        for bad in ["http://app.example/", "https://u:p@app.example/", "https://APP.example/", "file:///etc/passwd", "https://a.example/\u{7f}"] {
            assert_eq!(navigation(page, bad), None, "{bad}");
        }
    }

    #[test]
    fn a_page_changes_its_address_only_within_its_origin() {
        let page = Some("https://app.example/app.html");
        assert_eq!(address_change(page, "https://app.example/route/1").as_deref(), Some("https://app.example/route/1"));
        for bad in ["https://api.app.example/route", "https://app.example:8443/route", "http://app.example/route",
                    "https://user@app.example/route", "https://APP.example/route"] {
            assert_eq!(address_change(page, bad), None, "{bad}");
        }
        assert_eq!(address_change(None, "https://app.example/route/1"), None, "not before the page was delivered");
    }

    #[test]
    fn a_live_page_reaches_its_own_site_and_nothing_else() {
        let page = "https://app.example/app.html";
        assert_eq!(scope(page, "https://app.example/app.js"), Ok(()));
        assert_eq!(scope(page, "https://api.app.example/status"), Ok(()), "a sibling host on the page's site");
        assert!(scope(page, "https://cdn.thirdparty.example/lib.js").is_err());
        assert!(scope(page, "http://app.example/app.js").is_err(), "never plain http");
        assert!(scope(page, "https://app.example:8443/app.js").is_err(), "another port");
        assert!(scope(page, "https://u:p@app.example/app.js").is_err(), "no credentials");
        assert!(scope(page, "https://app.example/app.js#x").is_err(), "no fragment");
        assert!(scope(page, "https://APP.example/app.js").is_err(), "one spelling");
        assert!(scope(page, "https://app.example.evil.example/").is_err());
        assert!(scope("http://app.example/", "http://app.example/a").is_err(), "an http page has no live scope");
        // A public suffix is not a site boundary of the page's choosing.
        assert!(scope("https://a.github.io/", "https://b.github.io/x.js").is_err());
    }

    struct Fake(Vec<String>);
    impl Resources for Fake {
        fn post(&mut self, url: &str, content_type: &str, body: &[u8]) -> Result<Got, String> {
            self.0.push(format!("POST {url} {content_type} {}", String::from_utf8_lossy(body)));
            Ok(Got::new(201, "application/json", b"{\"count\":1}".to_vec()))
        }
        fn get(&mut self, url: &str, first: bool) -> Result<Got, String> {
            assert_eq!(first, self.0.is_empty(), "only the page is the first");
            self.0.push(url.to_string());
            match url {
                "https://app.example/app.html" => Ok(Got::new(200, "text/html", b"<p>hi".to_vec())),
                "https://app.example/app.js" => Ok(Got::new(200, "text/javascript", b"x=1".to_vec())),
                "https://api.app.example/status" => Ok(Got { cors: Some(("https://app.example".into(), true)), ..Got::new(200, "application/json", b"{}".to_vec()) }),
                _ => Err("not here".into()),
            }
        }
    }

    /// The stash is one slot for the process: its tests take turns.
    static STASH_TESTS: Mutex<()> = Mutex::new(());

    /// What the serve loop told its run.
    #[derive(Default)]
    struct Seen { started: Vec<String>, local: Option<JarStores>, made: Vec<(String, String, String, Vec<u8>)>, refused: Vec<String>, apis: Vec<limen::web_island::Api> }
    impl Hooks for Seen {
        fn started(&mut self, page: &str) { self.started.push(page.into()); }
        fn storage(&mut self) -> Option<&mut dyn LocalStorage> { self.local.as_mut().map(|s| s as &mut dyn LocalStorage) }
        fn made(&mut self, page: &str, name: &str, kind: &str, bytes: Vec<u8>) -> Result<(), String> {
            self.made.push((page.into(), name.into(), kind.into(), bytes));
            Ok(())
        }
        fn refused(&mut self, target: &str) { self.refused.push(target.into()); }
        fn refused_api(&mut self, api: limen::web_island::Api) { self.apis.push(api); }
    }

    fn body(client: &mut nuntius::client::Client) -> (Option<(u16, String, Option<String>)>, Vec<u8>, Option<ErrorCode>) {
        let (head, bytes, error, _) = body_and_cors(client);
        (head, bytes, error)
    }

    type Cors = Option<(String, bool)>;
    fn body_and_cors(client: &mut nuntius::client::Client) -> (Option<(u16, String, Option<String>)>, Vec<u8>, Option<ErrorCode>, Cors) {
        let (mut head, mut bytes, mut cors) = (None, vec![], None);
        loop {
            match client.recv().unwrap() {
                Event::Head { status, final_url, content_type, .. } => head = Some((status, final_url, content_type)),
                Event::Cors { allow_origin, allow_credentials, .. } => {
                    assert!(head.is_some() && bytes.is_empty(), "after the head, before the body");
                    cors = Some((allow_origin, allow_credentials));
                }
                Event::Data { bytes: b, .. } => bytes.extend(b),
                Event::Done { .. } => return (head, bytes, None, cors),
                Event::Error { code, .. } => return (head, bytes, Some(code), cors),
                _ => {}
            }
        }
    }

    #[test]
    fn the_engine_gets_its_page_then_only_resources_in_scope() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let mut fake = Fake(vec![]);
            let mut seen = Seen::default();
            let r = serve(ours, "https://app.example/app.html", &mut fake, &mut seen);
            tx.send((r, fake.0, seen)).unwrap();
        });
        let mut c = nuntius::client::Client::from_stream(theirs).unwrap();
        c.open(Open::get(1, LIVE_PAGE)).unwrap();
        let (head, page, err) = body(&mut c);
        assert_eq!(err, None);
        assert_eq!(head, Some((200, "https://app.example/app.html".into(), Some("text/html".into()))), "the page's address is in the answer");
        assert_eq!(page, with_reporter(b"<p>hi"), "the reporter first (I6.4)");
        c.open(Open::get(2, "https://app.example/app.js")).unwrap();
        assert_eq!(body(&mut c).1, b"x=1");
        c.open(Open::get(3, "https://api.app.example/status")).unwrap();
        let (head, _, _, cors) = body_and_cors(&mut c);
        assert_eq!(head.map(|h| h.0), Some(200), "the site's own answer");
        assert_eq!(cors, Some(("https://app.example".into(), true)), "with its CORS answer for the engine to check (I6.5)");
        c.open(Open::get(4, "https://cdn.thirdparty.example/lib.js")).unwrap();
        assert_eq!(body(&mut c).2, Some(ErrorCode::Policy));
        let mut post = Open::get(5, "https://app.example/api/items");
        post.method = Method::Post;
        post.body = Some(nuntius::Body { content_type: "application/json".into(), bytes: b"{}".to_vec() });
        c.open(post.clone()).unwrap();
        let (head, answer, _) = body(&mut c);
        assert_eq!((head.map(|h| h.0), answer.as_slice()), (Some(201), &b"{\"count\":1}"[..]), "a script's POST, with its body (I6.3)");
        // Not to another site; never a range.
        let mut away = post.clone();
        (away.id, away.url) = (6, "https://cdn.thirdparty.example/collect".into());
        let mut part = Open::get(7, "https://app.example/app.js");
        part.range = Some((0, Some(1)));
        for o in [away, part] {
            c.open(o).unwrap();
            assert_eq!(body(&mut c).2, Some(ErrorCode::Policy));
        }
        drop(c);
        let (r, asked, seen) = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        server.join().unwrap();
        let served = r.unwrap();
        assert_eq!(seen.started, vec!["https://app.example/app.html".to_string()]);
        // Each refused host is told once, for the popover.
        assert_eq!(seen.refused, ["host cdn.thirdparty.example, site thirdparty.example", "host app.example, site app.example"]);
        // Refused requests never reached the network.
        assert_eq!(asked, ["https://app.example/app.html", "https://app.example/app.js", "https://api.app.example/status",
                           "POST https://app.example/api/items application/json {}"]);
        assert_eq!(served.refused, ["host cdn.thirdparty.example, site thirdparty.example", "host cdn.thirdparty.example, site thirdparty.example",
                                    "host app.example, site app.example"]);
        assert_eq!(served.requests, 7);
    }

    #[test]
    fn the_page_loads_and_changes_its_sites_storage_after_it_is_delivered() {
        use crate::live_storage::{encode_ops, Op, Stores};
        let stores = Arc::new(Mutex::new(Stores::new(None)));
        let (ours, theirs) = UnixStream::pair().unwrap();
        let s2 = stores.clone();
        let idb = Arc::new(Mutex::new(crate::live_idb::Stores::new(None)));
        let i2 = idb.clone();
        let server = std::thread::spawn(move || {
            let mut seen = Seen { local: Some(JarStores { stores: s2, idb: i2, jar: None }), ..Seen::default() };
            serve(ours, "https://app.example/app.html", &mut Fake(vec![]), &mut seen)
        });
        let storage = limen::web_island::LIVE_STORAGE;
        let mut c = nuntius::client::Client::from_stream(theirs).unwrap();
        c.open(Open::get(1, LIVE_PAGE)).unwrap();
        assert_eq!(body(&mut c).2, None);
        c.open(Open::get(2, storage)).unwrap();
        assert_eq!(body(&mut c), (Some((200, storage.into(), Some("application/octet-stream".into()))), vec![], None), "empty at first");
        c.open(Open::post(3, storage, "application/octet-stream", encode_ops(&[Op::Set("visits".into(), "1".into())]))).unwrap();
        assert_eq!(body(&mut c).0.map(|h| h.0), Some(204));
        c.open(Open::post(4, storage, "application/octet-stream", vec![9, 9])).unwrap();
        assert_eq!(body(&mut c).2, Some(ErrorCode::Policy), "a batch out of shape");
        c.open(Open::get(5, storage)).unwrap();
        assert_eq!(body(&mut c).1, crate::live_storage::encode(&[("visits".to_string(), "1".to_string())].into()));
        drop(c);
        server.join().unwrap().unwrap();
        // The store is the page's site's, in the persona's jar.
        let key = crate::live_storage::Key::of(None, "https://api.app.example/").unwrap();
        assert_ne!(stores.lock().unwrap().load(&key), b"");
        let _ = idb;
    }

    #[test]
    fn the_page_loads_and_commits_its_sites_indexeddb_after_it_is_delivered() {
        use crate::live_idb::{parts, Op, Stores};
        let idb = Arc::new(Mutex::new(Stores::new(None)));
        let (ours, theirs) = UnixStream::pair().unwrap();
        let i2 = idb.clone();
        let server = std::thread::spawn(move || {
            let local = JarStores { stores: Arc::new(Mutex::new(crate::live_storage::Stores::new(None))), idb: i2, jar: None };
            let mut seen = Seen { local: Some(local), ..Seen::default() };
            serve(ours, "https://app.example/app.html", &mut Fake(vec![]), &mut seen)
        });
        let at = limen::web_island::LIVE_IDB;
        let mut c = nuntius::client::Client::from_stream(theirs).unwrap();
        c.open(Open::get(1, LIVE_PAGE)).unwrap();
        assert_eq!(body(&mut c).2, None);
        c.open(Open::get(2, at)).unwrap();
        assert_eq!(body(&mut c), (Some((200, at.into(), Some("application/octet-stream".into()))), vec![], None), "empty at first");
        let commit = [Op::Version(b"mail".to_vec(), 1), Op::CreateStore(b"m".to_vec(), vec![]), Op::Put(b"k".to_vec(), b"v".to_vec())];
        let p = parts(7, &commit);
        assert_eq!(p.len(), 1);
        c.open(Open::post(3, at, "application/octet-stream", p[0].clone())).unwrap();
        assert_eq!(body(&mut c).0.map(|h| h.0), Some(204));
        // A commit of more parts than navigatord's own rate a second: every
        // part is taken (the run is bounded by its quota, not the rate).
        let many = parts(9, &[Op::Version(b"mail".to_vec(), 1), Op::Store(b"m".to_vec()), Op::Put(b"big".to_vec(), vec![7; (OWN_PER_SECOND as usize + 5) * 32 * 1024])]);
        assert!(many.len() > OWN_PER_SECOND as usize + 2, "{} parts", many.len());
        for (i, part) in many.iter().enumerate() {
            c.open(Open::post(100 + i as u32, at, "application/octet-stream", part.clone())).unwrap();
            assert_eq!(body(&mut c).0.map(|h| h.0), Some(204), "part {i} of {}", many.len());
        }
        let bad = parts(8, &[Op::Store(b"nope".to_vec())]);
        c.open(Open::post(4, at, "application/octet-stream", bad[0].clone())).unwrap();
        assert_eq!(body(&mut c).2, Some(ErrorCode::Policy), "a commit refused");
        c.open(Open::get(5, at)).unwrap();
        let loaded = body(&mut c).1;
        let back = crate::live_idb::decode_ops(&loaded, crate::live_idb::LOAD_MAX).unwrap();
        assert!(back.contains(&Op::Put(b"k".to_vec(), b"v".to_vec())), "the first commit read back");
        let big: usize = back.iter().map(|o| match o { Op::Put(k, v) | Op::Append(k, v) if k == b"big" => v.len(), _ => 0 }).sum();
        assert_eq!(big, (OWN_PER_SECOND as usize + 5) * 32 * 1024, "the many-part commit read back whole");
        // localStorage is a store of its own, and so is the Cache API's,
        // which takes the same commits.
        c.open(Open::get(6, limen::web_island::LIVE_STORAGE)).unwrap();
        assert_eq!(body(&mut c).1, b"");
        let caches = limen::web_island::LIVE_CACHE;
        c.open(Open::get(7, caches)).unwrap();
        assert_eq!(body(&mut c).1, b"", "the Cache API store is apart from IndexedDB's");
        let entry = [Op::Version(b"v1".to_vec(), 0), Op::CreateStore(b"entries".to_vec(), vec![]), Op::Put(1u64.to_be_bytes().to_vec(), b"GET /app.js".to_vec())];
        c.open(Open::post(8, caches, "application/octet-stream", parts(10, &entry)[0].clone())).unwrap();
        assert_eq!(body(&mut c).0.map(|h| h.0), Some(204));
        c.open(Open::get(9, caches)).unwrap();
        assert_eq!(crate::live_idb::decode_ops(&body(&mut c).1, crate::live_idb::LOAD_MAX).unwrap(), entry);
        c.open(Open::get(11, at)).unwrap();
        assert!(!crate::live_idb::decode_ops(&body(&mut c).1, crate::live_idb::LOAD_MAX).unwrap().contains(&entry[2]), "and IndexedDB's holds none of it");
        drop(c);
        server.join().unwrap().unwrap();
    }

    #[test]
    fn storage_is_not_asked_for_before_the_page() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            let mut seen = Seen { local: Some(JarStores { stores: Arc::new(Mutex::new(crate::live_storage::Stores::new(None))), idb: Arc::new(Mutex::new(crate::live_idb::Stores::new(None))), jar: None }), ..Seen::default() };
            serve(ours, "https://app.example/app.html", &mut Fake(vec![]), &mut seen)
        });
        let mut c = nuntius::client::Client::from_stream(theirs).unwrap();
        c.open(Open::get(1, limen::web_island::LIVE_STORAGE)).unwrap();
        assert!(server.join().unwrap().is_err_and(|e| e.contains("first request")));
    }

    #[test]
    fn an_engine_that_names_its_own_page_is_ended() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            serve(ours, "https://app.example/app.html", &mut Fake(vec![]), &mut Seen::default())
        });
        let mut c = nuntius::client::Client::from_stream(theirs).unwrap();
        c.open(Open::get(1, "https://app.example/app.html")).unwrap();
        assert!(server.join().unwrap().is_err_and(|e| e.contains("first request")));
    }

    #[test]
    fn a_page_that_is_not_html_does_not_start() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            let mut seen = Seen::default();
            let r = serve(ours, "https://app.example/app.js", &mut Fake(vec![]), &mut seen);
            assert!(seen.started.is_empty());
            r
        });
        let mut c = nuntius::client::Client::from_stream(theirs).unwrap();
        c.open(Open::get(1, LIVE_PAGE)).unwrap();
        assert!(server.join().unwrap().is_err_and(|e| e.contains("not an HTML page")));
    }

    #[test]
    fn the_reporter_hands_over_files_and_refusals_after_the_page() {
        use limen::web_island::{Api, LIVE_DOWNLOAD, LIVE_REFUSED, LIVE_REPORTER};
        let (ours, theirs) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            let mut seen = Seen::default();
            serve(ours, "https://app.example/app.html", &mut Fake(vec![]), &mut seen).map(|_| seen)
        });
        let mut c = nuntius::client::Client::from_stream(theirs).unwrap();
        c.open(Open::get(1, LIVE_PAGE)).unwrap();
        assert_eq!(body(&mut c).2, None);
        c.open(Open::get(2, LIVE_REPORTER)).unwrap();
        let (head, script, _) = body(&mut c);
        assert_eq!((head.and_then(|h| h.2).as_deref(), script.as_slice()), (Some("text/javascript"), REPORTER));
        c.open(Open::post(3, &format!("{LIVE_DOWNLOAD}?name=notes.txt&type=text%2Fplain"), "text/plain", b"I6 fixture notes\n".to_vec())).unwrap();
        assert_eq!(body(&mut c).0.map(|h| h.0), Some(204));
        c.open(Open::get(4, &format!("{LIVE_REFUSED}?api=notifications"))).unwrap();
        assert_eq!(body(&mut c).0.map(|h| h.0), Some(204));
        // A file only as a POST with its bytes; an API only by its token.
        c.open(Open::get(5, &format!("{LIVE_DOWNLOAD}?name=x.txt"))).unwrap();
        assert_eq!(body(&mut c).2, Some(ErrorCode::Policy));
        c.open(Open::get(6, &format!("{LIVE_REFUSED}?api=everything"))).unwrap();
        assert_eq!(body(&mut c).2, Some(ErrorCode::Policy));
        c.open(Open::get(7, "https://live-page.atrium.invalid/other")).unwrap();
        assert_eq!(body(&mut c).2, Some(ErrorCode::Policy));
        drop(c);
        let seen = server.join().unwrap().unwrap();
        assert_eq!(seen.made, [("https://app.example/app.html".to_string(), "notes.txt".to_string(), "text/plain".to_string(), b"I6 fixture notes\n".to_vec())]);
        assert_eq!(seen.apis, [Api::Notifications]);
        assert_eq!(seen.started.len(), 1);
    }

    #[test]
    fn a_page_opens_a_window_beside_it_only_on_a_spent_press() {
        let mut b = Beside::default();
        assert_eq!(b.allow(|| false), Err("a new window with no press in the page"));
        assert!(b.allow(|| true).is_ok());
        assert!(b.allow(|| true).is_ok(), "two presses, two windows, however close");
        // At most MAX in a run, and past it no press is spent.
        let mut b = Beside::default();
        for _ in 0..Beside::MAX { assert!(b.allow(|| true).is_ok()); }
        assert_eq!(b.allow(|| panic!("spent a press on a refused window")), Err("a new window past the run's 64"));
        assert!(b.first("a new window past the run's 64"));
        assert!(!b.first("a new window past the run's 64"), "the UI is told once a run");
    }

    /// A run's `Tell` that keeps what it was told was refused.
    #[derive(Default)]
    struct Told(Mutex<Vec<String>>);
    impl Tell for Told {
        fn started(&self, _: String) {}
        fn navigate(&self, _: String) {}
        fn url_changed(&self, _: String) {}
        fn open_beside(&self, url: String) { self.0.lock().unwrap().push(format!("beside {url}")); }
        fn file(&self, _: Offered) {}
        fn refused(&self, what: String) { self.0.lock().unwrap().push(what); }
        fn print(&self) {}
        fn copy(&self, _: String, _: [u8; 16]) {}
        fn paste(&self, _: [u8; 16]) {}
        fn finished(&self, _: Option<String>) {}
    }

    struct PayFrame;
    impl Resources for PayFrame {
        fn get(&mut self, url: &str, first: bool) -> Result<Got, String> {
            match (url, first) {
                ("https://pay.example/frame.html", true) => Ok(Got::new(200, "text/html", b"<!doctype html><p>pay".to_vec())),
                _ => Err("not here".into()),
            }
        }
    }

    #[test]
    fn a_page_and_its_frame_run_apart_and_talk_only_through_the_mailbox() {
        use limen::web_island::{LIVE_FRAME, LIVE_MESSAGE, LIVE_MESSAGES};
        let mail = Arc::new(Mutex::new(Mailbox::default()));
        let asks = Arc::new(Mutex::new(VecDeque::new()));
        let told = Arc::new(Told::default());
        let (page_end, page_engine) = UnixStream::pair().unwrap();
        let (delivered, _) = std::sync::mpsc::channel();
        let mut page_hooks = RunHooks { delivered, storage: None, tell: told.clone(), apis: Default::default(), picked: Default::default(),
                                        printed: None, asks: asks.clone(), mail: mail.clone(), told_usage: None };
        let page_server = std::thread::spawn(move || serve(page_end, "https://app.example/app.html", &mut Fake(vec![]), &mut page_hooks));
        let mut page = nuntius::client::Client::from_stream(page_engine).unwrap();
        page.open(Open::get(1, LIVE_PAGE)).unwrap();
        assert_eq!(body(&mut page).2, None);
        // The page's script reports its frame; the watcher would start it.
        page.open(Open::get(2, &format!("{LIVE_FRAME}?id=1&url=https%3A%2F%2Fpay.example%2Fframe.html&x=10&y=20&w=320&h=80"))).unwrap();
        assert_eq!(body(&mut page).0.map(|h| h.0), Some(204));
        assert_eq!(asks.lock().unwrap().front(), Some(&FrameAsk::At { id: 1, url: "https://pay.example/frame.html".into(), rect: [10, 20, 320, 80] }));
        mail.lock().unwrap().frame_added(1);
        // The frame's engine: its own document, the reporter as the frame's.
        let (frame_end, frame_engine) = UnixStream::pair().unwrap();
        let mut frame_hooks = FrameHooks { id: 1, mail: mail.clone(), tell: told.clone(), apis: Default::default() };
        let frame_server = std::thread::spawn(move || serve(frame_end, "https://pay.example/frame.html", &mut PayFrame, &mut frame_hooks));
        let mut frame = nuntius::client::Client::from_stream(frame_engine).unwrap();
        frame.open(Open::get(1, LIVE_PAGE)).unwrap();
        let (_, html, _) = body(&mut frame);
        assert!(html.starts_with(format!("<!doctype html><script src=\"{FRAME_REPORTER}\"></script>").as_bytes()), "{}", String::from_utf8_lossy(&html));
        frame.open(Open::get(2, FRAME_REPORTER)).unwrap();
        assert_eq!(body(&mut frame).1, REPORTER);
        // The frame to its page, for the page's origin: stamped with the frame's.
        let post = |id, url: String, text: &str| Open::post(id, &url, "text/plain", text.as_bytes().to_vec());
        frame.open(post(3, format!("{LIVE_MESSAGE}?target=https%3A%2F%2Fapp.example"), "\"pay.example running\"")).unwrap();
        assert_eq!(body(&mut frame).0.map(|h| h.0), Some(204));
        page.open(Open::get(3, LIVE_MESSAGES)).unwrap();
        let (_, answer, _, cors) = body_and_cors(&mut page);
        assert_eq!(cors, Some(("*".into(), false)), "readable by the page's script");
        let got: serde_json::Value = serde_json::from_slice(&answer).unwrap();
        assert_eq!(got, serde_json::json!([{ "from": 1, "origin": "https://pay.example", "data": "\"pay.example running\"" }]));
        // The page to its frame.
        page.open(post(4, format!("{LIVE_MESSAGE}?to=1&target=*"), "{\"total\":12}")).unwrap();
        assert_eq!(body(&mut page).0.map(|h| h.0), Some(204));
        frame.open(Open::get(4, LIVE_MESSAGES)).unwrap();
        let got: serde_json::Value = serde_json::from_slice(&body(&mut frame).1).unwrap();
        assert_eq!(got, serde_json::json!([{ "from": null, "origin": "https://app.example", "data": "{\"total\":12}" }]));
        // A frame names no frame and reports none; the page names one.
        frame.open(post(5, format!("{LIVE_MESSAGE}?to=1&target=*"), "1")).unwrap();
        assert_eq!(body(&mut frame).2, Some(ErrorCode::Policy));
        frame.open(Open::get(6, &format!("{LIVE_FRAME}?id=2&url=https%3A%2F%2Fevil.example%2F&x=0&y=0&w=99&h=99"))).unwrap();
        assert_eq!(body(&mut frame).2, Some(ErrorCode::Policy));
        page.open(post(5, format!("{LIVE_MESSAGE}?target=*"), "1")).unwrap();
        assert_eq!(body(&mut page).2, Some(ErrorCode::Policy));
        // navigatord's own addresses are held to a rate: past it, refused,
        // and the page runs on.
        let refused = (10..40).filter(|&i| { page.open(Open::get(i, LIVE_MESSAGES)).unwrap(); body(&mut page).2.is_some() }).count();
        assert!(refused >= 30 - OWN_PER_SECOND as usize, "{refused} refused");
        // Messages are not network requests.
        drop((page, frame));
        let (page, frame) = (page_server.join().unwrap().unwrap(), frame_server.join().unwrap().unwrap());
        assert_eq!((page.network, frame.network), (1, 1));
    }

    #[test]
    fn the_reporters_requests_are_parsed_strictly() {
        use limen::web_island::{Api, LIVE_DOWNLOAD, LIVE_REFUSED, LIVE_REPORTER};
        assert_eq!(reporter_request(LIVE_REPORTER), Some(Reporter::Script));
        assert_eq!(reporter_request(limen::web_island::LIVE_PRINT), Some(Reporter::Print));
        assert_eq!(reporter_request(&format!("{}?x", limen::web_island::LIVE_PRINT)), None);
        assert_eq!(reporter_request(&format!("{LIVE_REFUSED}?api=service-worker")), Some(Reporter::Refused(Api::ServiceWorker)));
        assert_eq!(reporter_request(&format!("{LIVE_DOWNLOAD}?name=a%2Fb.csv&type=Text%2FCSV%3Bcharset%3Dutf-8")),
                   Some(Reporter::Download { name: "a/b.csv".into(), kind: "text/csv".into() }), "the name is made safe when it is held");
        assert_eq!(reporter_request(&format!("{LIVE_DOWNLOAD}?name={}", "a".repeat(5000))), None, "over the query bound");
        assert_eq!(reporter_request(&format!("{LIVE_REPORTER}?x=1")), None);
        assert_eq!(reporter_request(&format!("{LIVE_REFUSED}x?api=push")), None);
        assert_eq!(reporter_request(&format!("{LIVE_REFUSED}?api=push%0a")), None);
        // Frames and messages (I6.5).
        use limen::web_island::{LIVE_FRAME, LIVE_MESSAGE, LIVE_MESSAGES};
        assert_eq!(reporter_request(FRAME_REPORTER), Some(Reporter::Script));
        assert_eq!(reporter_request(LIVE_MESSAGES), Some(Reporter::Messages));
        assert_eq!(reporter_request(&format!("{LIVE_FRAME}?id=3&url=https%3A%2F%2Fpay.example%2F&x=-5&y=7&w=320&h=80")),
                   Some(Reporter::Frame(FrameAsk::At { id: 3, url: "https://pay.example/".into(), rect: [-5, 7, 320, 80] })));
        assert_eq!(reporter_request(&format!("{LIVE_FRAME}?id=3&gone=1")), Some(Reporter::Frame(FrameAsk::Gone { id: 3 })));
        for bad in ["id=0&gone=1", "id=%2B3&gone=1", "id=3&url=https%3A%2F%2Fpay.example%2F&x=1&y=1&w=99999999999&h=1",
                    "id=3&url=https%3A%2F%2Fpay.example%2F%0a&x=1&y=1&w=1&h=1", "id=3&url=https%3A%2F%2Fpay.example%2F&x=1&y=1&w=1"] {
            assert_eq!(reporter_request(&format!("{LIVE_FRAME}?{bad}")), None, "{bad}");
        }
        assert_eq!(reporter_request(&format!("{LIVE_MESSAGE}?to=2&target=https%3A%2F%2Fpay.example")), Some(Reporter::Message { to: Some(2) }));
        assert_eq!(reporter_request(&format!("{LIVE_MESSAGE}?target=*")), Some(Reporter::Message { to: None }));
        for bad in ["to=2", "to=2&target=pay.example", "to=x&target=*", "to=0&target=*"] {
            assert_eq!(reporter_request(&format!("{LIVE_MESSAGE}?{bad}")), None, "{bad}");
        }
    }

    #[test]
    fn the_reporter_goes_first_without_changing_the_pages_mode() {
        let tag = format!("<script src=\"{}\"></script>", limen::web_island::LIVE_REPORTER);
        let put = |html: &str| String::from_utf8(with_reporter(html.as_bytes())).unwrap();
        assert_eq!(put("<!doctype html><p>x"), format!("<!doctype html>{tag}<p>x"));
        assert_eq!(put("\n <!-- a -->\n<!DOCTYPE html PUBLIC \"x\">\n<html>"), format!("\n <!-- a -->\n<!DOCTYPE html PUBLIC \"x\">{tag}\n<html>"));
        assert_eq!(put("<p>no doctype"), format!("{tag}<p>no doctype"));
        assert_eq!(put("\u{feff}<!doctype html>"), format!("\u{feff}<!doctype html>{tag}"), "after the byte order mark");
        assert_eq!(put("<!-- never closed <!doctype html>"), format!("{tag}<!-- never closed <!doctype html>"));
        assert_eq!(put("<!doctype html"), format!("{tag}<!doctype html"));
    }

    #[test]
    fn a_made_file_is_not_taken_before_the_page() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || serve(ours, "https://app.example/app.html", &mut Fake(vec![]), &mut Seen::default()));
        let mut c = nuntius::client::Client::from_stream(theirs).unwrap();
        c.open(Open::post(1, &format!("{}?name=a.txt", limen::web_island::LIVE_DOWNLOAD), "text/plain", b"x".to_vec())).unwrap();
        assert!(server.join().unwrap().is_err(), "the first request is the page's");
    }

    #[test]
    fn a_page_the_jar_looked_at_waits_briefly_for_its_own_jar_and_address() {
        let _one = STASH_TESTS.lock().unwrap();
        stash(Some(7), "https://app.example/answer", Got::new(200, "text/html", b"<p>ok".to_vec()));
        assert_eq!(unstash(None, "https://app.example/answer"), None, "another jar");
        assert_eq!(unstash(Some(7), "https://app.example/other"), None, "another address");
        assert_eq!(unstash(Some(7), "https://app.example/answer").map(|a| a.body), Some(b"<p>ok".to_vec()));
        assert_eq!(unstash(Some(7), "https://app.example/answer"), None, "taken once");
    }

    #[test]
    fn a_navigation_within_the_origin_is_a_page_to_run_or_a_file_to_save() {
        let _one = STASH_TESTS.lock().unwrap();
        let fetched = |url: &str, ctype: &str, extra: Option<(&str, &str)>, body: &[u8]| navigator_cookies::Fetched {
            status: 200, url: url.into(), redirects: 0, body: body.to_vec(),
            headers: [("Content-Type", ctype)].into_iter().chain(extra).map(|(a, b)| (a.to_string(), b.to_string())).collect(),
        };
        let page = answered(fetched("https://app.example/second.html", "text/html", None, b"<p>2"), Some(9)).unwrap();
        assert_eq!(page, Answer::Page("https://app.example/second.html".into()));
        assert_eq!(unstash(Some(9), "https://app.example/second.html").map(|a| a.body), Some(b"<p>2".to_vec()), "kept for the run that shows it");
        let file = answered(fetched("https://app.example/report.csv", "text/csv", Some(("Content-Disposition", "attachment; filename=\"report.csv\"")), b"a,b\n"), Some(9)).unwrap();
        assert_eq!(file, Answer::File(Offered { url: "https://app.example/report.csv".into(), name: "report.csv".into(), kind: "CSV file".into(), bytes: b"a,b\n".to_vec() }));
        assert_eq!(unstash(Some(9), "https://app.example/report.csv"), None, "a file is not a page to run");
    }

    #[test]
    fn a_pick_is_one_read_only_file_and_the_page_cant_hold_the_picker_up() {
        use limen::picker::ToParent;
        use std::io::Write;
        let file = |bytes: &[u8]| {
            let path = std::env::temp_dir().join(format!("navigatord-pick-{}-{}", std::process::id(), bytes.len()));
            std::fs::File::create(&path).unwrap().write_all(bytes).unwrap();
            let f = std::fs::File::open(&path).unwrap();
            std::fs::remove_file(&path).unwrap();
            std::os::fd::OwnedFd::from(f)
        };
        let now = Instant::now();
        let mut pick = Pick::default();
        assert_eq!(pick.may_open(now), Ok(()));
        pick.embed = Some(5);
        assert!(pick.may_open(now).is_err(), "one at a time");
        let picked = ToParent::File { name: "notes.txt".into(), size: 5, write: false }.encode();
        assert_eq!(pick.message(&picked, Some(file(b"hello"))), None, "not over until Done");
        // A second file, a writable one, or one without its descriptor is not taken.
        assert_eq!(pick.message(&ToParent::File { name: "b".into(), size: 1, write: false }.encode(), Some(file(b"b"))), None);
        assert_eq!(pick.message(&ToParent::File { name: "c".into(), size: 1, write: true }.encode(), Some(file(b"c"))), None);
        assert_eq!(pick.message(&ToParent::Done { files: 1 }.encode(), None), Some(Some(("notes.txt".into(), b"hello".to_vec()))));
        assert!(pick.embed.is_none());
        assert!(pick.may_open(Instant::now()).is_err(), "not again at once");
        assert_eq!(pick.may_open(Instant::now() + PICK_AGAIN), Ok(()));
        // Cancelled: no file.
        pick.embed = Some(6);
        assert_eq!(pick.message(&ToParent::Cancelled.encode(), None), Some(None));
        // A file the picker said was bigger than a page may take.
        pick.embed = Some(7);
        pick.message(&ToParent::File { name: "big".into(), size: PICK_MAX + 1, write: false }.encode(), Some(file(b"x")));
        assert_eq!(pick.message(&ToParent::Done { files: 1 }.encode(), None), Some(None));
    }

    #[test]
    fn the_picked_file_is_served_once() {
        use limen::web_island::LIVE_FILE;
        struct Picked(Option<Vec<u8>>);
        impl Hooks for Picked {
            fn picked(&mut self) -> Option<Vec<u8>> { self.0.take() }
        }
        let (ours, theirs) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || serve(ours, "https://app.example/app.html", &mut Fake(vec![]), &mut Picked(Some(b"hello".to_vec()))));
        let mut c = nuntius::client::Client::from_stream(theirs).unwrap();
        c.open(Open::get(1, LIVE_FILE)).unwrap();
        assert!(server.join().unwrap().is_err(), "not before the page");
        let (ours, theirs) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || serve(ours, "https://app.example/app.html", &mut Fake(vec![]), &mut Picked(Some(b"hello".to_vec()))));
        let mut c = nuntius::client::Client::from_stream(theirs).unwrap();
        c.open(Open::get(1, LIVE_PAGE)).unwrap();
        assert_eq!(body(&mut c).2, None);
        c.open(Open::get(2, LIVE_FILE)).unwrap();
        assert_eq!(body(&mut c).1, b"hello");
        c.open(Open::get(3, LIVE_FILE)).unwrap();
        assert_eq!(body(&mut c).2, Some(ErrorCode::Policy), "once");
        drop(c);
        server.join().unwrap().unwrap();
    }

    #[test]
    fn refusals_are_told_in_the_popovers_words() {
        assert_eq!(refused_host("host cdn.thirdparty.example, site thirdparty.example"), "cdn.thirdparty.example");
        assert_eq!(refused_host("host unreadable"), "unreadable");
        assert_eq!(refused_host("garbage"), "an unreadable address");
    }
}

#[cfg(test)]
mod cors_tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> { pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() }

    #[test]
    fn a_servers_cors_answer_is_passed_on_only_in_its_one_shape() {
        assert_eq!(cors(&h(&[("access-control-allow-origin", "https://app.example"), ("access-control-allow-credentials", "true")])),
                   Some(("https://app.example".into(), true)));
        assert_eq!(cors(&h(&[("Access-Control-Allow-Origin", " * ")])), Some(("*".into(), false)));
        assert_eq!(cors(&h(&[("access-control-allow-origin", "https://app.example"), ("access-control-allow-credentials", "TRUE")])),
                   Some(("https://app.example".into(), false)), "credentials only on a literal true");
        assert_eq!(cors(&h(&[])), None);
        assert_eq!(cors(&h(&[("access-control-allow-origin", "https://a.example"), ("access-control-allow-origin", "https://b.example")])), None,
                   "two answers are none");
        assert_eq!(cors(&h(&[("access-control-allow-origin", "https://a.example, https://b.example")])), None);
    }
}

/// The engine's side of the reporter channel, fuzzed (I6.6): every address
/// and body it sends navigatord is hostile. A fixed seed, so a failure
/// repeats; each case is a mutation of an honest request.
#[cfg(test)]
mod channel_fuzz {
    use super::*;
    use crate::live_frames::{FrameAsk, Mailbox, MESSAGE_MAX, QUEUE_MAX, FRAME_MIN};

    pub(crate) struct Rng(u64);
    impl Rng {
        pub(crate) fn next(&mut self) -> u64 { self.0 ^= self.0 << 13; self.0 ^= self.0 >> 7; self.0 ^= self.0 << 17; self.0 }
        pub(crate) fn below(&mut self, n: usize) -> usize { (self.next() % n.max(1) as u64) as usize }
        pub(crate) fn bytes(&mut self, max: usize) -> Vec<u8> { let n = self.below(max + 1); (0..n).map(|_| self.next() as u8).collect() }
    }

    const PIECES: &[&str] = &["id", "url", "x", "y", "w", "h", "gone", "to", "target", "api", "name", "type", "=", "&", "?", "%", "%00",
        "%ff", "-", "+", "0", "1", "4294967296", "-2147483649", "2147483647", "*", "/", "https://pay.example", "https://evil.example/x",
        "javascript:1", "\u{202e}", " ", "#", "frame=1", "1;base64"];
    const SEEDS: &[&str] = &[
        "https://live-page.atrium.invalid/frame?id=1&url=https%3A%2F%2Fpay.example%2Ff.html&x=10&y=20&w=320&h=80",
        "https://live-page.atrium.invalid/frame?id=3&gone=1",
        "https://live-page.atrium.invalid/message?to=1&target=https%3A%2F%2Fpay.example",
        "https://live-page.atrium.invalid/message?target=*",
        "https://live-page.atrium.invalid/messages",
        "https://live-page.atrium.invalid/refused?api=geolocation",
        "https://live-page.atrium.invalid/download?name=report.csv&type=text%2Fcsv",
        "https://live-page.atrium.invalid/reporter.js?frame=1",
    ];

    fn mutate(r: &mut Rng, s: &str) -> String {
        let mut b = s.as_bytes().to_vec();
        for _ in 0..1 + r.below(4) {
            let at = r.below(b.len());
            match r.below(4) {
                0 => { b.truncate(at); }
                1 => { let p = PIECES[r.below(PIECES.len())].as_bytes(); b.splice(at..at, p.iter().copied()); }
                2 => { if at < b.len() { b[at] = r.next() as u8; } }
                _ => { let n = r.below(3000); b.extend(std::iter::repeat_n(b'9', n)); }
            }
        }
        String::from_utf8_lossy(&b).into_owned()
    }

    #[test]
    fn hostile_reporter_addresses_are_refused_or_taken_within_bounds() {
        let mut r = Rng(0x9e37_79b9_7f4a_7c15);
        let mut taken = 0;
        for i in 0..20_000 {
            let url = mutate(&mut r, SEEDS[i % SEEDS.len()]);
            match reporter_request(&url) {
                Some(Reporter::Frame(FrameAsk::At { id, url, rect })) => {
                    taken += 1;
                    assert!(id > 0 && navigator_control::clean(&url, navigator_control::URL_MAX), "{url}");
                    let _ = (crate::live_frames::frame_rect([0, 0, 800, 600], rect), crate::live_frames::frame_address("https://app.example/", &url), FRAME_MIN);
                }
                Some(Reporter::Frame(FrameAsk::Gone { id })) => { taken += 1; assert!(id > 0); }
                Some(Reporter::Message { to }) => { taken += 1; assert!(to != Some(0)); assert!(reporter_target(&url).is_some()); }
                Some(Reporter::Download { name, kind }) => {
                    taken += 1;
                    assert!(name.chars().count() <= 255 && kind.len() <= 128 && kind.bytes().all(|b| b.is_ascii_graphic()));
                }
                Some(_) => taken += 1,
                None => {}
            }
            if let Some(t) = reporter_target(&url) { assert!(t == "*" || t == "/" || t.len() <= 300); }
        }
        // The mutations are not all refused: the seeds' shapes still get through.
        assert!(taken > 300, "{taken}");
    }

    #[test]
    fn hostile_messages_between_a_page_and_its_frames_stay_bounded() {
        let mut r = Rng(0x2545_f491_4f6c_dd1d);
        let origins = ["https://app.example", "https://pay.example", "https://evil.example"];
        let targets = ["*", "/", "https://app.example", "https://pay.example/p", "https://evil.example", "", "app.example", "https://app.example:1"];
        for _ in 0..200 {
            let mut m = Mailbox::default();
            for _ in 0..400 {
                let id = r.below(5) as u32;
                let data = if r.below(10) == 0 { "x".repeat(MESSAGE_MAX + r.below(2)) } else { String::from_utf8_lossy(&r.bytes(64)).into_owned() };
                let target = targets[r.below(targets.len())];
                match r.below(9) {
                    0 => m.page_started(origins[r.below(3)].into()),
                    1 => m.frame_added(id),
                    2 => m.frame_started(id, origins[r.below(3)].into()),
                    3 => m.frame_gone(id),
                    4 | 5 => { if m.from_page(id, target, data.clone()).is_ok() { assert!(!data.is_empty() && data.len() <= MESSAGE_MAX); } }
                    6 | 7 => { if m.from_frame(id, target, data.clone()).is_ok() { assert!(!data.is_empty() && data.len() <= MESSAGE_MAX); } }
                    _ => {
                        for got in [m.take_for_page(), m.take_for_frame(id)] {
                            let v: Vec<serde_json::Value> = serde_json::from_slice(&got).expect("always JSON");
                            assert!(v.len() <= QUEUE_MAX);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_hostile_document_gets_the_reporter_once_and_first() {
        let mut r = Rng(0x1234_5678_9abc_def1);
        let seeds: [&[u8]; 4] = [b"<!doctype html><html><head></head><body>x</body></html>", b"<!-- a --><!DOCTYPE html>", b"<html>", b""];
        for i in 0..5_000 {
            let mut html = seeds[i % seeds.len()].to_vec();
            let extra = r.bytes(200);
            let at = r.below(html.len());
            html.splice(at..at, extra);
            for frame in [false, true] {
                let out = with_reporter_for(&html, frame);
                assert!(out.len() > html.len(), "the script was added");
            }
        }
    }
}
