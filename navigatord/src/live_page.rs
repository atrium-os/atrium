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

/// Where a live page's resources come from: the plain fetcher (`L`), or a
/// test's stand-in.
pub trait Resources {
    /// One GET: status, content type token (`-` for none), body.
    fn get(&mut self, url: &str) -> Result<(u16, String, Vec<u8>), String>;
}

impl Resources for crate::Upstream {
    fn get(&mut self, url: &str) -> Result<(u16, String, Vec<u8>), String> {
        match self {
            crate::Upstream::Fetchd(f, _) => f.live_resource(url),
            _ => Err("a live page needs the plain fetcher".into()),
        }
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
pub fn serve(socket: UnixStream, page: &str, upstream: &mut dyn Resources, started: &mut dyn FnMut(&str)) -> Result<Served, String> {
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
        if open.method != Method::Get || open.body.is_some() || open.range.is_some() || open.if_range.is_some() || open.deliver != Deliver::Stream {
            refuse("only a whole GET in I6.1", ErrorCode::Policy, &mut served)?;
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
        let (status, kind, body) = match upstream.get(&url) {
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
            started(&url);
        }
    }
}

/// One running live page: its island, placed over the page area.
pub struct Live {
    pub url: String,
    pub press: navigator_control::Press,
    parent: Arc<Mutex<Option<Parent>>>,
    embed: u64,
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
        started: impl FnOnce(String) + Send + 'static,
        finished: impl FnOnce(Option<String>) + Send + 'static,
    ) -> Result<Self, String> {
        scope(&url, &url).map_err(|why| format!("this page can't run live: {why}"))?;
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
        std::thread::spawn(move || {
            let result = crate::upstream(&cfg, &crate::instance()).and_then(|mut up| {
                serve(channel, &page, &mut up, &mut |u| { let _ = delivered_tx.send(u.to_string()); })
            });
            match &result {
                Ok(s) => eprintln!("navigatord: live page served {} requests, {} bytes, {} refused", s.requests, s.bytes, s.refused.len()),
                Err(e) => eprintln!("navigatord: live page: {e}"),
            }
            let _ = done.send(result.map(|_| ()));
        });
        let watched = parent.clone();
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
                            let expected = page.as_deref().and_then(crate::origin);
                            if expected.as_deref() != Some(origin) {
                                break Some(format!("the engine reported {origin}, not the page's origin"));
                            }
                            limen::start_trace("navigatord", "live-started", origin);
                            if let Some(s) = started.take() { s(origin.to_string()); }
                        }
                        Some(Report::Title(_) | Report::Resize { .. } | Report::MediaReady) => {}
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
        Ok(Live { url, press, parent, embed })
    }

    /// Move the island with the page area (the window resized).
    pub fn place(&self, rect: [u32; 4]) -> Result<(), String> {
        let rect = crate::widget::bounds(rect)?;
        match self.parent.lock().unwrap().as_mut() {
            Some(p) => p.place(self.embed, Some(rect)).map_err(|e| format!("place: {e:?}")),
            None => Err("the live page has ended".into()),
        }
    }

    /// End it: the island goes, and `finished` is not called.
    pub fn stop(self) {}
}

impl Drop for Live {
    fn drop(&mut self) {
        // Dropping Limen's connection ends the embed and its engine.
        self.parent.lock().unwrap().take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuntius::{Open};

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
        fn get(&mut self, url: &str) -> Result<(u16, String, Vec<u8>), String> {
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
            let r = serve(ours, "https://app.example/app.html", &mut fake, &mut |u| started.push(u.to_string()));
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
        c.open(post).unwrap();
        assert_eq!(body(&mut c).2, Some(ErrorCode::Policy), "no POST in I6.1");
        drop(c);
        let (r, asked, started) = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        server.join().unwrap();
        let served = r.unwrap();
        assert_eq!(started, vec!["https://app.example/app.html".to_string()]);
        // Refused requests never reached the network.
        assert_eq!(asked, ["https://app.example/app.html", "https://app.example/app.js", "https://api.app.example/status"]);
        assert_eq!(served.refused, ["host cdn.thirdparty.example, site thirdparty.example", "host app.example, site app.example"]);
        assert_eq!(served.requests, 5);
    }

    #[test]
    fn an_engine_that_names_its_own_page_is_ended() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            serve(ours, "https://app.example/app.html", &mut Fake(vec![]), &mut |_| {})
        });
        let mut c = nuntius::client::Client::from_stream(theirs).unwrap();
        c.open(Open::get(1, "https://app.example/app.html")).unwrap();
        assert!(server.join().unwrap().is_err_and(|e| e.contains("first request")));
    }

    #[test]
    fn a_page_that_is_not_html_does_not_start() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            serve(ours, "https://app.example/app.js", &mut Fake(vec![]), &mut |_| panic!("started"))
        });
        let mut c = nuntius::client::Client::from_stream(theirs).unwrap();
        c.open(Open::get(1, LIVE_PAGE)).unwrap();
        assert!(server.join().unwrap().is_err_and(|e| e.contains("not an HTML page")));
    }
}
