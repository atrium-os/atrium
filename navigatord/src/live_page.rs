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
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use limen::{client::Parent, proto::ParentEvent, web_island::LIVE_PAGE};
use nuntius::{Deliver, ErrorCode, Event, Method, Request};

/// The most requests one live page makes before its engine is ended: a
/// map or an editor makes hundreds; thousands is a page that loops.
pub const MAX_REQUESTS: usize = 4096;
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

/// Where a live page's resources come from: the page's jar through Arca
/// ([`LiveJar`], D9), the plain fetcher (`L`) where there is no Arca, or a
/// test's stand-in.
pub trait Resources {
    /// One GET (`first`: the page itself): status, content type token
    /// (`-` for none), body.
    fn get(&mut self, url: &str, first: bool) -> Result<(u16, String, Vec<u8>), String>;
    /// A script's POST (I6.3) of `body` as `content_type`.
    fn post(&mut self, _url: &str, _content_type: &str, _body: &[u8]) -> Result<(u16, String, Vec<u8>), String> {
        Err("a POST needs the page's jar".into())
    }
}

impl Resources for crate::Upstream {
    fn get(&mut self, url: &str, _first: bool) -> Result<(u16, String, Vec<u8>), String> {
        match self {
            crate::Upstream::Fetchd(f, _) => f.live_resource(url),
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
}

impl LiveJar {
    /// A new view for the page at `url` in the jar `jar` (`None`: the
    /// persona's own).
    pub fn open(cookies: crate::Cookies, url: &str, jar: Option<u64>) -> Result<Self, String> {
        let host = url::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_ascii_lowercase)).ok_or("no host")?;
        let view = crate::VIEWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        cookies.lock().unwrap().show_live(view, url, jar).map_err(|e| format!("the page's jar: {e:?}"))?;
        Ok(LiveJar { cookies, view, host })
    }

    fn answer(got: Result<navigator_cookies::Fetched, navigator_cookies::Refused>) -> Result<(u16, String, Vec<u8>), String> {
        let r = got.map_err(|e| format!("through Arca: {e:?}"))?;
        if r.body.len() > navigator_prerender::fetch::LIVE_RESOURCE_MAX { return Err("the answer is too big".into()); }
        Ok((r.status, type_token(&r.headers), r.body))
    }

    /// A form the page submitted (I6.3): sent through the jar, its answer's
    /// redirects followed; the address of the page that answered.
    pub fn submit(&self, url: &str, body: &[u8]) -> Result<String, String> {
        let f = navigator_cookies::Fetch::live_form(url, &self.host, body.to_vec());
        let r = self.cookies.lock().unwrap().fetch(self.view, &f).map_err(|e| format!("through Arca: {e:?}"))?;
        Ok(r.url)
    }
}

impl Resources for LiveJar {
    fn get(&mut self, url: &str, first: bool) -> Result<(u16, String, Vec<u8>), String> {
        let f = if first { navigator_cookies::Fetch::navigation(url, None, true) } else { navigator_cookies::Fetch::subresource(url, &self.host) };
        Self::answer(self.cookies.lock().unwrap().fetch_once(self.view, &f))
    }

    fn post(&mut self, url: &str, content_type: &str, body: &[u8]) -> Result<(u16, String, Vec<u8>), String> {
        let f = navigator_cookies::Fetch::script_post(url, &self.host, content_type, body.to_vec());
        Self::answer(self.cookies.lock().unwrap().fetch_once(self.view, &f))
    }
}

/// Where a live page's `localStorage` is kept (I6.3): navigatord's
/// [`crate::live_storage::Stores`], or a test's stand-in. `page` is the
/// delivered page's address, whose site names the store.
pub trait LocalStorage {
    fn load(&mut self, page: &str) -> Result<Vec<u8>, String>;
    fn apply(&mut self, page: &str, batch: &[u8]) -> Result<(), String>;
}

/// The stores of one jar.
pub struct JarStores {
    pub stores: Arc<Mutex<crate::live_storage::Stores>>,
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
}

/// What the serve loop saw, for the log and the UI.
#[derive(Debug, Default, PartialEq)]
pub struct Served {
    pub requests: usize,
    pub bytes: u64,
    /// Refused requests' hosts and sites, as `refused_target` spells them.
    pub refused: Vec<String>,
}

/// Serve one live page's engine on its fetch channel until it closes it.
/// `page` is the address the person pressed on; `started` is called once
/// the page itself has been delivered, with its address.
pub fn serve(socket: UnixStream, page: &str, upstream: &mut dyn Resources, mut local: Option<&mut dyn LocalStorage>, started: &mut dyn FnMut(&str)) -> Result<Served, String> {
    let mut delivered_url = String::new();
    socket.set_write_timeout(Some(Duration::from_secs(30))).map_err(|e| e.to_string())?;
    socket.set_read_timeout(Some(IDLE_POLL)).map_err(|e| e.to_string())?;
    let (mut input, mut output) = nuntius::server::channel(socket).map_err(|e| e.to_string())?;
    let mut served = Served::default();
    let first = Instant::now();
    let mut delivered = false;
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
        served.requests += 1;
        if served.requests > MAX_REQUESTS {
            return Err("the page made too many requests".into());
        }
        let id = open.id;
        let mut refuse = |why: &str, code: ErrorCode, served: &mut Served| -> Result<(), String> {
            let target = crate::refused_target(&open.url);
            eprintln!("navigatord: a live page's request refused: {why} ({target})");
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
                refuse("only a whole GET, or a POST with its body", ErrorCode::Policy, &mut served)?;
                continue;
            }
        };
        if open.range.is_some() || open.if_range.is_some() || open.deliver != Deliver::Stream {
            refuse("only a whole resource", ErrorCode::Policy, &mut served)?;
            continue;
        }
        // The page's storage (I6.3): loaded once, changed in batches, kept
        // by navigatord for the page's site.
        if delivered && open.url == limen::web_island::LIVE_STORAGE {
            let answer = match (local.as_deref_mut(), post) {
                (None, _) => Err("no storage here".to_string()),
                (Some(l), None) => l.load(&delivered_url).map(|b| (200, b)),
                (Some(l), Some(b)) => l.apply(&delivered_url, &b.bytes).map(|_| (204, vec![])),
            };
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
                Ok(_) => refuse("the store is over the engine's bound", ErrorCode::TooBig, &mut served)?,
                Err(why) => {
                    eprintln!("navigatord: a live page's storage: {why}");
                    output.send(Event::Error { id, code: ErrorCode::Policy }).map_err(|e| e.to_string())?;
                }
            }
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
                refuse(why, ErrorCode::Policy, &mut served)?;
                continue;
            }
            open.url.clone()
        };
        let got = match post {
            Some(b) => upstream.post(&url, &b.content_type, &b.bytes),
            None => upstream.get(&url, !delivered),
        };
        let (status, kind, body) = match got {
            Ok(r) => r,
            Err(why) if delivered => {
                eprintln!("navigatord: a live page's resource failed: {why}");
                output.send(Event::Error { id, code: ErrorCode::Connect }).map_err(|e| e.to_string())?;
                continue;
            }
            Err(why) => return Err(format!("the page: {why}")),
        };
        let length = body.len() as u64;
        if !delivered && (status != 200 || kind != "text/html") {
            return Err(format!("the page answered {status} {kind}, not an HTML page"));
        }
        if length > open.limits.max_bytes || served.bytes.saturating_add(length) > MAX_BYTES {
            refuse("over the page's byte bound", ErrorCode::TooBig, &mut served)?;
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
        for (i, bytes) in body.chunks(nuntius::DATA_MAX).enumerate() {
            output.send(Event::Data { id, offset: (i * nuntius::DATA_MAX) as u64, bytes: bytes.to_vec() }).map_err(|e| e.to_string())?;
        }
        output.send(Event::Done { id, bytes: length }).map_err(|e| e.to_string())?;
        if !delivered {
            delivered = true;
            delivered_url = url.clone();
            started(&url);
        }
    }
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

impl Live {
    /// Ask Limen for the island in the UI's `window`, place it at `rect`,
    /// and serve it. `started(origin)` once the page is delivered and the
    /// engine reported its start; `finished(why)` once, when it ends for
    /// any reason other than [`Live::stop`] (`None`: it closed itself).
    pub fn start(
        socket: &Path,
        window: [u8; 16],
        url: String,
        press: navigator_control::Press,
        rect: [u32; 4],
        cfg: Arc<crate::Config>,
        jar: Option<LiveJar>,
        storage: Option<JarStores>,
        started: impl FnOnce(String) + Send + 'static,
        navigate: impl Fn(String) + Send + Sync + 'static,
        url_changed: impl Fn(String) + Send + 'static,
        finished: impl FnOnce(Option<String>) + Send + 'static,
    ) -> Result<Self, String> {
        scope(&url, &url).map_err(|why| format!("this page can't run live: {why}"))?;
        let placed = Arc::new(Mutex::new((false, rect)));
        let rect = crate::widget::bounds(rect)?;
        limen::start_trace("navigatord", "live-start", &url);
        let mut parent = Parent::at(socket).map_err(|e| format!("Limen: {e:?}"))?;
        let embed = parent.request_island(window, 1).map_err(|e| format!("engine launch: {e:?}"))?;
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
        std::thread::spawn(move || {
            let mut on_delivered = |u: &str| { let _ = delivered_tx.send(u.to_string()); };
            let mut storage = storage;
            let local = storage.as_mut().map(|s| s as &mut dyn LocalStorage);
            let result = match jar {
                Some(mut jar) => serve(channel, &page, &mut jar, local, &mut on_delivered),
                None => crate::upstream(&cfg, &crate::instance()).and_then(|mut up| serve(channel, &page, &mut up, local, &mut on_delivered)),
            };
            match &result {
                Ok(s) => eprintln!("navigatord: live page served {} requests, {} bytes, {} refused", s.requests, s.bytes, s.refused.len()),
                Err(e) => eprintln!("navigatord: live page: {e}"),
            }
            let _ = done.send(result.map(|_| ()));
        });
        let watched = parent.clone();
        let size = placed.clone();
        let navigate = Arc::new(navigate);
        std::thread::spawn(move || {
            use limen::web_island::{EndReason, Lifecycle, Report};
            let mut reports = Lifecycle::default();
            let mut started = Some(started);
            let mut page = None::<String>;
            let why = loop {
                let mut slot = watched.lock().unwrap();
                let Some(connection) = slot.as_mut() else { return };
                if let Ok(u) = delivered.try_recv() { page = Some(u); }
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
                            if let Some(s) = started.take() { s(origin.to_string()); }
                            let rect = { let mut p = size.lock().unwrap(); p.0 = true; p.1 };
                            tell_size(connection, embed, rect);
                        }
                        Some(Report::Title(_) | Report::Resize { .. } | Report::MediaReady) => {}
                        // The page asked to go somewhere (I6.2): never followed in
                        // the engine. navigatord hands it to the UI, which opens
                        // it as a trail entry: live again if it is this origin's
                        // (`ContinueLive`), in the document lane if not.
                        Some(Report::Navigate(to)) => match navigation(page.as_deref(), to) {
                            Some(to) => (*navigate)(to),
                            None => eprintln!("navigatord: a live page's navigation refused ({})", crate::refused_target(to)),
                        },
                        // The page changed its own address (pushState): a trail
                        // entry in the UI. An engine that claims another
                        // origin's address is ended.
                        Some(Report::UrlChanged(to)) => match address_change(page.as_deref(), to) {
                            Some(to) => url_changed(to),
                            None => break Some("the engine claimed another origin's address".into()),
                        },
                        // A form (I6.3): only to the page's own site, sent
                        // through its jar off this thread; the page that
                        // answers opens as the next trail entry.
                        Some(Report::Submit { url: to, body }) => match (page.as_deref().map(|p| scope(p, to)), &forms) {
                            (Some(Ok(())), Some(jar)) => {
                                let (jar, to, body, navigate) = (jar.clone(), to.to_string(), body.to_vec(), navigate.clone());
                                std::thread::spawn(move || match jar.submit(&to, &body) {
                                    Ok(answer) => (*navigate)(answer),
                                    Err(why) => eprintln!("navigatord: a live page's form to {} failed: {why}", crate::refused_target(&to)),
                                });
                            }
                            (Some(Err(why)), _) => eprintln!("navigatord: a live page's form refused: {why} ({})", crate::refused_target(to)),
                            _ => eprintln!("navigatord: a live page's form refused: no jar ({})", crate::refused_target(to)),
                        },
                        Some(Report::Ended(EndReason::Closed)) => break None,
                        Some(Report::Ended(EndReason::Failed)) => break Some("the page's engine failed".into()),
                        // A live page has no unload question in I6.1: Stop is the person's.
                        Some(Report::BeforeUnload { .. }) | None => break Some("the engine sent a report out of order".into()),
                    },
                    Ok(Some(ParentEvent::IslandStopped { embed: id })) if id == embed => break None,
                    Ok(Some(ParentEvent::ChildLost { .. } | ParentEvent::Detached { .. })) | Err(_) => {
                        let why = match fetched.recv_timeout(Duration::from_millis(100)) {
                            Ok(Err(e)) => e,
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
            if watched.lock().unwrap().take().is_some() {
                finished(why);
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
        fn post(&mut self, url: &str, content_type: &str, body: &[u8]) -> Result<(u16, String, Vec<u8>), String> {
            self.0.push(format!("POST {url} {content_type} {}", String::from_utf8_lossy(body)));
            Ok((201, "application/json".into(), b"{\"count\":1}".to_vec()))
        }
        fn get(&mut self, url: &str, first: bool) -> Result<(u16, String, Vec<u8>), String> {
            assert_eq!(first, self.0.is_empty(), "only the page is the first");
            self.0.push(url.to_string());
            match url {
                "https://app.example/app.html" => Ok((200, "text/html".into(), b"<p>hi".to_vec())),
                "https://app.example/app.js" => Ok((200, "text/javascript".into(), b"x=1".to_vec())),
                "https://api.app.example/status" => Ok((404, "-".into(), vec![])),
                _ => Err("not here".into()),
            }
        }
    }

    fn body(client: &mut nuntius::client::Client) -> (Option<(u16, String, Option<String>)>, Vec<u8>, Option<ErrorCode>) {
        let (mut head, mut bytes) = (None, vec![]);
        loop {
            match client.recv().unwrap() {
                Event::Head { status, final_url, content_type, .. } => head = Some((status, final_url, content_type)),
                Event::Data { bytes: b, .. } => bytes.extend(b),
                Event::Done { .. } => return (head, bytes, None),
                Event::Error { code, .. } => return (head, bytes, Some(code)),
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
            let mut started = vec![];
            let r = serve(ours, "https://app.example/app.html", &mut fake, None, &mut |u| started.push(u.to_string()));
            tx.send((r, fake.0, started)).unwrap();
        });
        let mut c = nuntius::client::Client::from_stream(theirs).unwrap();
        c.open(Open::get(1, LIVE_PAGE)).unwrap();
        let (head, page, err) = body(&mut c);
        assert_eq!(err, None);
        assert_eq!(head, Some((200, "https://app.example/app.html".into(), Some("text/html".into()))), "the page's address is in the answer");
        assert_eq!(page, b"<p>hi");
        c.open(Open::get(2, "https://app.example/app.js")).unwrap();
        assert_eq!(body(&mut c).1, b"x=1");
        c.open(Open::get(3, "https://api.app.example/status")).unwrap();
        assert_eq!(body(&mut c).0.map(|h| h.0), Some(404), "the site's own answer, whatever it is");
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
        let (r, asked, started) = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        server.join().unwrap();
        let served = r.unwrap();
        assert_eq!(started, vec!["https://app.example/app.html".to_string()]);
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
        let server = std::thread::spawn(move || {
            let mut local = JarStores { stores: s2, jar: None };
            serve(ours, "https://app.example/app.html", &mut Fake(vec![]), Some(&mut local), &mut |_| {})
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
    }

    #[test]
    fn storage_is_not_asked_for_before_the_page() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            let mut local = JarStores { stores: Arc::new(Mutex::new(crate::live_storage::Stores::new(None))), jar: None };
            serve(ours, "https://app.example/app.html", &mut Fake(vec![]), Some(&mut local), &mut |_| {})
        });
        let mut c = nuntius::client::Client::from_stream(theirs).unwrap();
        c.open(Open::get(1, limen::web_island::LIVE_STORAGE)).unwrap();
        assert!(server.join().unwrap().is_err_and(|e| e.contains("first request")));
    }

    #[test]
    fn an_engine_that_names_its_own_page_is_ended() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            serve(ours, "https://app.example/app.html", &mut Fake(vec![]), None, &mut |_| {})
        });
        let mut c = nuntius::client::Client::from_stream(theirs).unwrap();
        c.open(Open::get(1, "https://app.example/app.html")).unwrap();
        assert!(server.join().unwrap().is_err_and(|e| e.contains("first request")));
    }

    #[test]
    fn a_page_that_is_not_html_does_not_start() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            serve(ours, "https://app.example/app.js", &mut Fake(vec![]), None, &mut |_| panic!("started"))
        });
        let mut c = nuntius::client::Client::from_stream(theirs).unwrap();
        c.open(Open::get(1, LIVE_PAGE)).unwrap();
        assert!(server.join().unwrap().is_err_and(|e| e.contains("not an HTML page")));
    }
}
