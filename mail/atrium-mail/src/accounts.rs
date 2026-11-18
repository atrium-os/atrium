//! Accounts in the app (mail.md §5.1, §6.1): adding one (settings found from
//! the address, the password kept by Arca from its own field), and the
//! background sync that keeps each account current and sends what is due.
//!
//! Every connection is an Arca courier, so Mail never holds a password or
//! a token: IMAP and SMTP through the mail courier (logged in by Arca),
//! JMAP through the HTTP courier with the token filled in as a bearer.
//! Settings discovery fetches through Mail's own Nuntius fetcher.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use arca_courier::schema::{Header, Method, Place, Request, Response, Slot};
use arcad::proto::{courier_op, op, status};
use mail_engine::accounts::Connect;
use mail_engine::{Engine, Parser};
use mail_store::{AccountConfig, AccountId, Protocol, Reached, Server, Store};

pub fn courierd_socket() -> String {
    std::env::var("ARCA_COURIERD_SOCKET").unwrap_or_else(|_| arca_courierd::DEFAULT_SOCKET.into())
}

pub fn arca_socket() -> PathBuf {
    PathBuf::from(std::env::var("ARCA_SOCKET").unwrap_or_else(|_| "/atrium/sockets/arca/arcad.sock".into()))
}

fn scheme(name: &str) -> std::io::Result<arca_store::Scheme> {
    arca_store::Scheme::from_name(name).ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "unknown scheme"))
}

/// The real couriers, from arca-courierd.
pub struct Couriers {
    pub socket: String,
}

impl Connect for Couriers {
    fn mail(&self, s: &Server, item: &[u8; 16]) -> std::io::Result<UnixStream> {
        let mut c = arca_courierd::open_scheme(&self.socket, scheme(&s.scheme)?, &s.host, s.port, None).map_err(|code| std::io::Error::other(format!("courierd refused ({code})")))?;
        c.write_all(&arca_courier::mail::encode_hello(item))?;
        c.set_read_timeout(Some(Duration::from_secs(120)))?;
        Ok(c)
    }

    fn jmap(&self, session: &str, item: &[u8; 16]) -> Result<Box<dyn mail_sync::jmap::Http>, String> {
        let (host, port, _) = split_url(session).ok_or("the JMAP address isn't an https URL")?;
        Ok(Box::new(CourierHttp { socket: self.socket.clone(), host, port, item: *item, conn: None }))
    }
}

/// `https://host[:port]/path?query` → (host, port, path-and-query).
pub fn split_url(url: &str) -> Option<(String, u16, String)> {
    let rest = url.strip_prefix("https://")?;
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().ok()?),
        None => (hostport, 443),
    };
    if host.is_empty() || host.contains('@') {
        return None;
    }
    Some((host.to_ascii_lowercase(), port, path.to_string()))
}

/// JMAP over the HTTP courier to one host, the token as a bearer slot.
/// A JMAP server whose API, download or upload URL is on another host is
/// refused: the token is bound to the session's host.
struct CourierHttp {
    socket: String,
    host: String,
    port: u16,
    item: [u8; 16],
    conn: Option<UnixStream>,
}

impl CourierHttp {
    fn request(&mut self, url: &str, method: Method, content_type: Option<&str>, body: &[u8], max: usize) -> Result<Vec<u8>, String> {
        let (host, port, pq) = split_url(url).ok_or("not an https URL")?;
        if host != self.host || port != self.port {
            return Err(format!("the server sends Mail to another host ({host}) the sign-in isn't for"));
        }
        if body.len() > arca_courier::schema::BODY_MAX {
            return Err("the message is larger than Mail can send through Arca yet (1 MiB)".into());
        }
        let (path, query) = match pq.split_once('?') {
            Some((p, q)) => (p.to_string(), q.to_string()),
            None => (pq, String::new()),
        };
        let mut headers = vec![(Header::Accept, b"application/json".to_vec())];
        if let Some(ct) = content_type {
            headers.push((Header::ContentType, ct.as_bytes().to_vec()));
        }
        let req = Request {
            method,
            path: path.into_bytes(),
            query: query.into_bytes(),
            headers,
            slots: vec![Slot { source: courier_op::SOURCE_ITEM, reference: self.item, place: Place::Bearer }],
            body: if body.is_empty() { vec![] } else { vec![arca_courier::schema::Segment::Literal(body.to_vec())] },
            ticket: None,
        };
        let frame = req.encode();
        for attempt in 0..2 {
            if self.conn.is_none() {
                let s = arca_courierd::open_scheme(&self.socket, arca_store::Scheme::Https, &self.host, self.port, None).map_err(|c| format!("courierd refused ({c})"))?;
                s.set_read_timeout(Some(Duration::from_secs(120))).ok();
                self.conn = Some(s);
            }
            let c = self.conn.as_mut().unwrap();
            let r = arca_courier::send::write_frame(c, &frame).and_then(|_| arca_courier::send::read_frame(c));
            match r {
                Ok(Some(b)) => {
                    return match Response::decode(&b) {
                        Some(Response::Http { status, body, .. }) if (200..300).contains(&status) => {
                            if body.len() > max {
                                Err("the answer is too large".into())
                            } else {
                                Ok(body)
                            }
                        }
                        Some(Response::Http { status: 401 | 403, .. }) => Err("AUTHENTICATIONFAILED: the server refused the sign-in".into()),
                        Some(Response::Http { status, .. }) => Err(format!("the server said {status}")),
                        Some(Response::Refused(r)) => Err(format!("Arca refused: {r:?}")),
                        None => Err("a bad answer from the courier".into()),
                    };
                }
                // The courier ended (idle, or one request per courier):
                // a new one, once.
                _ if attempt == 0 => self.conn = None,
                _ => return Err("couldn't reach the server".into()),
            }
        }
        Err("couldn't reach the server".into())
    }
}

impl mail_sync::jmap::Http for CourierHttp {
    fn get(&mut self, url: &str, max: usize) -> Result<Vec<u8>, String> {
        self.request(url, Method::Get, None, &[], max)
    }
    fn post(&mut self, url: &str, content_type: &str, body: &[u8], max: usize) -> Result<Vec<u8>, String> {
        self.request(url, Method::Post, Some(content_type), body, max)
    }
}

// ── settings discovery through Nuntius ──────────────────────────────

/// One lookup through a fetcher (nuntius.md §3.6): its records, waiting
/// at most 8 s; other events (none are expected) are passed over.
pub fn lookup(c: &mut nuntius::Client, id: u32, name: &str, kind: nuntius::RecordKind) -> Result<Vec<nuntius::Record>, String> {
    use nuntius::{Event, Lookup};
    c.lookup(Lookup { id, name: name.to_string(), kind }).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("no answer in time".into());
        }
        match c.recv_timeout(left) {
            Ok(Some(Event::Records { id: i, records, .. })) if i == id => return Ok(records),
            Ok(Some(Event::Error { id: i, code })) if i == id => return Err(format!("{code:?}")),
            Ok(_) => {}
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// DKIM and DMARC lookups (§7.5) through a fetcher of Mail's own, opened
/// the first time one is needed and again if it stops.
#[derive(Default)]
pub struct NuntiusDns {
    fetcher: std::cell::RefCell<Option<(nuntius::Client, nuntius::launch::FetcherGuard)>>,
    next: std::cell::Cell<u32>,
}

impl mail_engine::dns::Dns for NuntiusDns {
    fn txt(&self, name: &str) -> Result<Vec<String>, String> {
        let mut f = self.fetcher.borrow_mut();
        if f.as_ref().is_some_and(|(_, g)| g.exited()) {
            *f = None;
        }
        if f.is_none() {
            *f = Some(nuntius::launch::open().map_err(|e| e.to_string())?.into_parts());
        }
        let id = self.next.get().wrapping_add(1);
        self.next.set(id);
        let (c, _) = f.as_mut().unwrap();
        match lookup(c, id, name, nuntius::RecordKind::Txt) {
            Ok(r) => Ok(r.into_iter().filter_map(|r| match r { nuntius::Record::Txt(t) => Some(t), _ => None }).collect()),
            Err(e) => {
                // A fetcher that went quiet is not asked again.
                *f = None;
                Err(e)
            }
        }
    }
}

/// HTTP, SRV and MX through Mail's fetcher.
pub struct NuntiusLookup<'a>(pub &'a mut nuntius::client::Client);

impl mail_sync::setup::Lookup for NuntiusLookup<'_> {
    fn get(&mut self, url: &str) -> Option<Vec<u8>> {
        use nuntius::{Event, Limits, Open};
        let mut o = Open::get(1, url);
        o.limits = Limits { max_bytes: 256 << 10, connect_ms: 5_000, idle_ms: 5_000, total_ms: 10_000 };
        self.0.open(o).ok()?;
        let deadline = Instant::now() + Duration::from_secs(12);
        let mut ok = false;
        let mut body = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                let _ = self.0.cancel(1);
                return None;
            }
            match self.0.recv_timeout(left.min(Duration::from_secs(1))) {
                Ok(Some(Event::Head { status, .. })) => ok = (200..300).contains(&status),
                Ok(Some(Event::Data { bytes, .. })) => {
                    let _ = self.0.budget(bytes.len() as u64);
                    body.extend_from_slice(&bytes);
                }
                Ok(Some(Event::Done { .. })) => return ok.then_some(body),
                Ok(Some(Event::Error { .. })) => return None,
                Ok(_) => {}
                Err(_) => return None,
            }
        }
    }
    fn srv(&mut self, name: &str) -> Vec<(u16, u16, u16, String)> {
        lookup(self.0, 2, name, nuntius::RecordKind::Srv)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|r| match r { nuntius::Record::Srv { priority, weight, port, target } => Some((priority, weight, port, target)), _ => None })
            .collect()
    }
    fn mx(&mut self, domain: &str) -> Vec<String> {
        let mut v: Vec<(u16, String)> = lookup(self.0, 3, domain, nuntius::RecordKind::Mx)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|r| match r { nuntius::Record::Mx { preference, host } => Some((preference, host)), _ => None })
            .collect();
        v.sort();
        v.into_iter().map(|(_, h)| h).collect()
    }
}

/// Find `address`'s settings, opening a fetcher for the lookups.
pub fn discover(address: &str) -> Result<mail_sync::setup::Found, String> {
    // A known provider needs no fetcher at all.
    let mut none = NoNet;
    let f = mail_sync::setup::discover(address, &mut none).map_err(str::to_string)?;
    if matches!(f.via, mail_sync::setup::Via::Provider(_)) {
        return Ok(f);
    }
    let fetcher = nuntius::launch::open().map_err(|e| format!("Mail couldn't look up the settings: {e}"))?;
    let (mut c, _guard) = fetcher.into_parts();
    mail_sync::setup::discover(address, &mut NuntiusLookup(&mut c)).map_err(str::to_string)
}

struct NoNet;

impl mail_sync::setup::Lookup for NoNet {
    fn get(&mut self, _: &str) -> Option<Vec<u8>> {
        None
    }
    fn srv(&mut self, _: &str) -> Vec<(u16, u16, u16, String)> {
        Vec::new()
    }
    fn mx(&mut self, _: &str) -> Vec<String> {
        Vec::new()
    }
}

/// The store's settings for what was found (items filled in by [`keep`]).
pub fn config_for(found: &mail_sync::setup::Found, address: &str, name: &str) -> AccountConfig {
    let s = |x: &mail_sync::setup::Server| Server { scheme: x.scheme.to_string(), host: x.host.clone(), port: x.port };
    let protocol = match &found.settings {
        mail_sync::setup::Settings::Imap { imap, smtp } => Protocol::Imap { imap: s(imap), smtp: s(smtp) },
        mail_sync::setup::Settings::Jmap { session } => Protocol::Jmap { session: session.clone() },
    };
    AccountConfig { address: address.trim().to_string(), name: name.trim().to_string(), protocol, incoming_item: None, outgoing_item: None, oauth: found.oauth.map(str::to_string), reached: Reached::Never }
}

/// The endpoints an account's password is kept for: IMAP and SMTP, or the
/// JMAP host.
pub fn endpoints(c: &AccountConfig) -> Result<Vec<(arca_store::Scheme, String, u16)>, String> {
    let e = |s: &Server| -> Result<_, String> { Ok((scheme(&s.scheme).map_err(|e| e.to_string())?, s.host.clone(), s.port)) };
    match &c.protocol {
        Protocol::Imap { imap, smtp } => Ok(vec![e(imap)?, e(smtp)?]),
        Protocol::Jmap { session } => {
            let (h, p, _) = split_url(session).ok_or("not an https URL")?;
            Ok(vec![(arca_store::Scheme::Https, h, p)])
        }
    }
}

/// Have Arca keep what the person typed into its field (`handle`) for the
/// account's servers (arcad `KEEP_FOR`); fills in the item ids. Mail never
/// sees the password.
pub fn keep(arca: &std::path::Path, handle: &[u8; 16], c: &mut AccountConfig) -> Result<(), String> {
    use arca_store::codec::Writer;
    let eps = endpoints(c)?;
    let mut w = Writer::new();
    w.raw(handle);
    w.u8(2); // after first unlock: sync runs in the background
    w.str(&c.address);
    w.u8(eps.len() as u8);
    for (s, h, p) in &eps {
        arcad::proto::write_endpoint_scheme(&mut w, *s, h, *p);
    }
    let mut s = UnixStream::connect(arca).map_err(|e| format!("Arca isn't reachable: {e}"))?;
    s.set_read_timeout(Some(Duration::from_secs(10))).ok();
    arcad::proto::write_request(&mut s, op::KEEP_FOR, &w.buf).map_err(|e| e.to_string())?;
    let (st, body) = arcad::proto::read_reply(&mut s).map_err(|e| e.to_string())?;
    match st {
        status::OK if body.len() == 16 * eps.len() => {
            let ids: Vec<[u8; 16]> = body.as_ref().chunks(16).map(|c| c.try_into().unwrap()).collect();
            c.incoming_item = Some(ids[0]);
            c.outgoing_item = Some(*ids.get(1).unwrap_or(&ids[0]));
            Ok(())
        }
        status::LOCKED => Err("Arca is locked: unlock it and try again.".into()),
        _ => Err("Arca didn't keep the password: type it again.".into()),
    }
}

// ── background sync ─────────────────────────────────────────────────

/// How often each account is looked at when nothing is due (§6.1; IDLE
/// and EventSource push come later).
pub const IDLE_SECS: i64 = 5 * 60;

/// What the window tells the sync thread.
pub enum Wake {
    /// Look now (a send, an action, a new account, "Get mail").
    Now,
    Stop,
}

/// The sync thread's handle: wake it, and see when it changed the index.
pub struct Syncer {
    pub wake: mpsc::Sender<Wake>,
    /// Set after a pass changed or may have changed the index.
    pub changed: Arc<AtomicBool>,
    /// Per account, how the last pass went.
    pub status: Arc<Mutex<Vec<(AccountId, Result<mail_engine::accounts::Pass, Reached>)>>>,
}

/// Start syncing every configured account, on a thread with its own
/// engine over the same store.
pub fn start(store: Store, jail: PathBuf, roots: Vec<Vec<u8>>, net: Box<dyn Connect + Send>) -> Syncer {
    let (wake, rx) = mpsc::channel();
    let changed = Arc::new(AtomicBool::new(false));
    let status = Arc::new(Mutex::new(Vec::new()));
    let (ch, st) = (changed.clone(), status.clone());
    std::thread::spawn(move || {
        let mut engine = Engine::new(store, Parser { program: jail });
        engine.set_roots(roots);
        engine.set_dns(Box::new(NuntiusDns::default()));
        let now = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        loop {
            let mut out = Vec::new();
            for (id, _) in engine.configured() {
                let r = engine.pass(id, net.as_ref(), now());
                if let Err(e) = &r {
                    eprintln!("atrium-mail: account {id}: {e:?}");
                }
                out.push((id, r));
                ch.store(true, Ordering::Release);
            }
            *st.lock().unwrap() = out;
            let next = engine.next_pass(now(), IDLE_SECS);
            let wait = Duration::from_secs((next - now()).clamp(1, IDLE_SECS) as u64);
            match rx.recv_timeout(wait) {
                Ok(Wake::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                Ok(Wake::Now) => {
                    // Several wakes at once are one pass.
                    while let Ok(w) = rx.try_recv() {
                        if matches!(w, Wake::Stop) {
                            return;
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    });
    Syncer { wake, changed, status }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert_eq!(split_url("https://API.fastmail.com/jmap/session"), Some(("api.fastmail.com".into(), 443, "/jmap/session".into())));
        assert_eq!(split_url("https://h.example:8443/a?b=c"), Some(("h.example".into(), 8443, "/a?b=c".into())));
        assert_eq!(split_url("http://h.example/"), None);
        assert_eq!(split_url("https://u@h.example/"), None);
    }

    #[test]
    fn found_to_config_and_endpoints() {
        let f = mail_sync::setup::discover("ada@outlook.com", &mut NoNet).unwrap();
        let c = config_for(&f, " ada@outlook.com ", "Ada");
        assert_eq!(c.oauth.as_deref(), Some("microsoft"));
        let e = endpoints(&c).unwrap();
        assert_eq!(e, vec![(arca_store::Scheme::Imaps, "outlook.office365.com".into(), 993), (arca_store::Scheme::Submission, "smtp.office365.com".into(), 587)]);
        let j = AccountConfig { protocol: Protocol::Jmap { session: "https://api.fastmail.com/jmap/session".into() }, ..c };
        assert_eq!(endpoints(&j).unwrap(), vec![(arca_store::Scheme::Https, "api.fastmail.com".into(), 443)]);
    }
}
