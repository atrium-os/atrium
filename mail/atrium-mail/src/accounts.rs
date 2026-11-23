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

/// Ask Arca to unwrap an OAEP content key using this launch’s persona.
pub fn unwrap_smime(socket: &std::path::Path, fingerprint: &[u8; 32], wrapped: &[u8]) -> Result<Option<Vec<u8>>, String> {
    use arcad::proto::{self, AppReply};
    use arca_store::codec::Writer;

    let call = || -> std::io::Result<_> {
        let mut stream = UnixStream::connect(socket)?;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        stream.set_write_timeout(Some(Duration::from_secs(3)))?;
        let mut body = Writer::new();
        body.raw(fingerprint);
        body.bytes(wrapped);
        proto::write_request(&mut stream, op::MAIL_SMIME_UNWRAP, &body.buf)?;
        proto::read_app_reply(&mut stream, AppReply::SmimeContentKey)
    };
    match call().map_err(|_| "Arca couldn’t be reached".to_string())? {
        (status::OK, key) => Ok(Some(key)),
        (status::DENIED, _) => Ok(None),
        (status::LOCKED, _) => Err("Unlock your Atrium session to read this message".into()),
        _ => Err("Arca couldn’t unwrap the content key".into()),
    }
}

/// Public certificate discovery is scoped to this Mail launch by Arca.
pub fn smime_certificates(socket: &std::path::Path) -> Result<Vec<Vec<u8>>, String> {
    use arcad::proto::{self, AppReply, SMIME_CERT_MAX, SMIME_CERTS_MAX};
    use arca_store::codec::Reader;

    let call = || -> std::io::Result<_> {
        let mut stream = UnixStream::connect(socket)?;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        stream.set_write_timeout(Some(Duration::from_secs(3)))?;
        proto::write_request(&mut stream, op::MAIL_SMIME_CERTS, &[])?;
        proto::read_app_reply(&mut stream, AppReply::SmimeCertificates)
    };
    let (status_code, body) = call().map_err(|_| "Arca couldn’t be reached".to_string())?;
    match status_code {
        status::DENIED | status::UNSUPPORTED => return Ok(Vec::new()),
        status::LOCKED => return Err("Unlock your Atrium session to read this message".into()),
        status::OK => {}
        _ => return Err("Arca couldn’t list S/MIME certificates".into()),
    }
    let invalid = |_| "Arca returned an invalid S/MIME certificate list".to_string();
    let mut r = Reader::new(&body);
    let count = r.u32().map_err(invalid)? as usize;
    if count > SMIME_CERTS_MAX {
        return Err("Arca returned too many S/MIME certificates".into());
    }
    let mut certificates = Vec::with_capacity(count);
    for _ in 0..count {
        let cert = r.bytes(SMIME_CERT_MAX).map_err(invalid)?;
        if cert.is_empty() {
            return Err("Arca returned an empty S/MIME certificate".into());
        }
        certificates.push(cert.to_vec());
    }
    r.end().map_err(invalid)?;
    Ok(certificates)
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

    fn account_mail(&self, cfg: &AccountConfig, server: &Server, item: &[u8; 16]) -> std::io::Result<UnixStream> {
        if cfg.oauth.as_deref() != Some("microsoft") { return self.mail(server, item); }
        let invalid = || std::io::Error::other("Microsoft account has incomplete server references");
        let (incoming, outgoing) = (cfg.incoming_item.ok_or_else(invalid)?, cfg.outgoing_item.ok_or_else(invalid)?);
        let p = &arcad::oauth::MICROSOFT;
        let direction = match (server.scheme.as_str(), server.host.as_str(), server.port) {
            ("imaps", h, n) if (h, n) == p.imap && item == &incoming => 0,
            ("submission", h, n) if (h, n) == p.smtp && item == &outgoing => 1,
            _ => return Err(invalid()),
        };
        let mut token = arca_courierd::open(&self.socket, p.token_host, 443, None)
            .map_err(|code| std::io::Error::other(format!("token courier refused ({code})")))?;
        token.set_read_timeout(Some(Duration::from_secs(120)))?;
        token.set_write_timeout(Some(Duration::from_secs(10)))?;
        let mut request = arca_courier::schema::MAIL_REFRESH.to_vec();
        request.extend_from_slice(&incoming);
        request.extend_from_slice(&outgoing);
        request.push(direction);
        arca_courier::send::write_frame(&mut token, &request)?;
        let reply = arca_courier::send::read_frame(&mut token)?.ok_or(std::io::ErrorKind::UnexpectedEof)?;
        drop(token);
        if reply.as_slice() == [arca_courier::schema::Refusal::SignInAgain as u8] {
            return Err(std::io::Error::other("sign in again"));
        }
        let ticket = reply.as_slice().try_into().map_err(|_| std::io::Error::other("Arca could not refresh the Microsoft grant"))?;
        self.mail(server, ticket)
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
        if body.len() > arca_courier::schema::BODY_MAX {
            return Err("the message is larger than Mail can send through Arca yet (1 MiB)".into());
        }
        let frame = self.frame(url, method, "application/json", content_type, body)?;
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
                        _ => Err("a bad answer from the courier".into()),
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

impl CourierHttp {
    /// The request for `url` (on this courier's host), as courier frames.
    fn frame(&self, url: &str, method: Method, accept: &str, content_type: Option<&str>, body: &[u8]) -> Result<Vec<u8>, String> {
        let (host, port, pq) = split_url(url).ok_or("not an https URL")?;
        if host != self.host || port != self.port {
            return Err(format!("the server sends Mail to another host ({host}) the sign-in isn't for"));
        }
        let (path, query) = match pq.split_once('?') {
            Some((p, q)) => (p.to_string(), q.to_string()),
            None => (pq, String::new()),
        };
        let mut headers = vec![(Header::Accept, accept.as_bytes().to_vec())];
        if let Some(ct) = content_type {
            headers.push((Header::ContentType, ct.as_bytes().to_vec()));
        }
        Ok(Request {
            method,
            path: path.into_bytes(),
            query: query.into_bytes(),
            headers,
            slots: vec![Slot { source: courier_op::SOURCE_ITEM, reference: self.item, place: Place::Bearer }],
            body: if body.is_empty() { vec![] } else { vec![arca_courier::schema::Segment::Literal(body.to_vec())] },
            ticket: None,
        }
        .encode())
    }
}

/// How long Mail waits on a quiet event stream: past the courier's own
/// wait, which ends it first.
const EVENTS_READ: Duration = Duration::from_secs(6 * 60);

/// An event stream's frames from a courier, to `on_event`.
pub fn read_events(c: &mut impl std::io::Read, on_event: &mut dyn FnMut(&[u8]) -> bool) -> Result<(), String> {
    let mut begun = false;
    loop {
        let f = arca_courier::send::read_frame(c).map_err(|e| format!("the event stream broke: {e}"))?.ok_or("the courier ended")?;
        match Response::decode(&f) {
            Some(Response::Events { status, .. }) if !begun && (200..300).contains(&status) => begun = true,
            Some(Response::Event(e)) if begun => {
                if !on_event(&e) {
                    return Ok(());
                }
            }
            Some(Response::EventsEnd) if begun => return Ok(()),
            Some(Response::Http { status: 401 | 403, .. }) => return Err("AUTHENTICATIONFAILED: the server refused the sign-in".into()),
            Some(Response::Http { status, .. }) => return Err(format!("the server said {status} to the event stream")),
            Some(Response::Refused(r)) => return Err(format!("Arca refused: {r:?}")),
            _ => return Err("a bad answer from the courier".into()),
        }
    }
}

impl mail_sync::jmap::Http for CourierHttp {
    fn events(&mut self, url: &str, on_event: &mut dyn FnMut(&[u8]) -> bool) -> Result<(), String> {
        let frame = self.frame(url, Method::Get, "text/event-stream", None, &[])?;
        // A courier of its own: the stream holds it until it ends.
        let mut c = arca_courierd::open_scheme(&self.socket, arca_store::Scheme::Https, &self.host, self.port, None).map_err(|c| format!("courierd refused ({c})"))?;
        c.set_read_timeout(Some(EVENTS_READ)).ok();
        arca_courier::send::write_frame(&mut c, &frame).map_err(|e| e.to_string())?;
        read_events(&mut c, on_event)
    }
    fn get(&mut self, url: &str, max: usize) -> Result<Vec<u8>, String> {
        self.request(url, Method::Get, None, &[], max)
    }
    fn post(&mut self, url: &str, content_type: &str, body: &[u8], max: usize) -> Result<Vec<u8>, String> {
        self.request(url, Method::Post, Some(content_type), body, max)
    }
}

// ── settings discovery through Nuntius ──────────────────────────────

/// One lookup through a fetcher (nuntius.md §3.6): records and their TTL, waiting
/// at most 8 s; other events (none are expected) are passed over.
pub fn lookup(c: &mut nuntius::Client, id: u32, name: &str, kind: nuntius::RecordKind) -> Result<(Vec<nuntius::Record>, u32), String> {
    use nuntius::{Event, Lookup};
    let request = Lookup { id, name: name.to_string(), kind };
    match kind {
        nuntius::RecordKind::Txt => c.lookup_with_ttl(request),
        _ => c.lookup(request),
    }.map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("no answer in time".into());
        }
        match c.recv_timeout(left) {
            Ok(Some(Event::Records { id: i, records, ttl, .. })) if i == id => return Ok((records, ttl)),
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
    fn txt(&self, name: &str) -> Result<(Vec<String>, u32), String> {
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
            Ok((records, ttl)) => Ok((records.into_iter().filter_map(|r| match r { nuntius::Record::Txt(t) => Some(t), _ => None }).collect(), ttl)),
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
            .map(|(records, _)| records)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|r| match r { nuntius::Record::Srv { priority, weight, port, target } => Some((priority, weight, port, target)), _ => None })
            .collect()
    }
    fn mx(&mut self, domain: &str) -> Vec<String> {
        let mut v: Vec<(u16, String)> = lookup(self.0, 3, domain, nuntius::RecordKind::Mx)
            .map(|(records, _)| records)
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

/// IMAP servers known to have no ManageSieve.
const NO_SIEVE: &[&str] = &["imap.gmail.com", "outlook.office365.com", "imap.mail.yahoo.com", "imap.mail.me.com", "imap.aol.com", "imap.gmx.com", "imap.gmx.net"];

/// Where an IMAP account's filters are set (ManageSieve on the IMAP host,
/// mail.md §11.6), unless its provider is known to have none. A sign-in
/// by OAuth has none: no provider that signs in that way offers it.
pub fn sieve_server(c: &AccountConfig) -> Option<Server> {
    match &c.protocol {
        Protocol::Imap { imap, .. } if c.oauth.is_none() && !NO_SIEVE.contains(&imap.host.to_ascii_lowercase().as_str()) => Some(Server { scheme: "sieve".into(), host: imap.host.clone(), port: mail_engine::sieve::PORT }),
        _ => None,
    }
}

/// Start a PKCE session. Only the sheet ticket crosses into Mail.
pub fn microsoft_sign_in(arca: &std::path::Path, address: &str, renewal: Option<([u8; 16], [u8; 16])>) -> Result<[u8; 16], String> {
    let mut socket = UnixStream::connect(arca).map_err(|e| format!("Arca is unavailable: {e}"))?;
    let timeout = Some(Duration::from_secs(10));
    socket.set_read_timeout(timeout).and_then(|_| socket.set_write_timeout(timeout)).map_err(|e| e.to_string())?;
    let mut body = Vec::new();
    let op = if let Some((incoming, outgoing)) = renewal {
        body.extend_from_slice(&incoming);
        body.extend_from_slice(&outgoing);
        op::MAIL_REAUTH
    } else { op::MAIL_SIGN_IN };
    body.extend_from_slice(address.as_bytes());
    arcad::proto::write_request(&mut socket, op, &body).map_err(|e| e.to_string())?;
    let (code, body) = arcad::proto::read_app_reply(&mut socket, arcad::proto::AppReply::Ticket).map_err(|e| e.to_string())?;
    match code {
        status::OK => body.as_slice().try_into().map_err(|_| "Arca returned an invalid sign-in ticket.".into()),
        status::UNSUPPORTED => Err("Atrium Mail has no Microsoft app registration configured.".into()),
        status::LOCKED => Err("Unlock Arca and try again.".into()),
        _ => Err(format!("Arca refused Microsoft sign-in ({code}).")),
    }
}

pub fn cancel_microsoft(arca: &std::path::Path, ticket: &[u8; 16]) -> std::io::Result<()> {
    let mut socket = UnixStream::connect(arca)?;
    let timeout = Some(Duration::from_secs(2));
    socket.set_read_timeout(timeout)?;
    socket.set_write_timeout(timeout)?;
    arcad::proto::write_request(&mut socket, op::MAIL_SIGN_IN_CANCEL, ticket)?;
    let (code, _) = arcad::proto::read_app_reply(&mut socket, arcad::proto::AppReply::Empty)?;
    if code != status::OK { return Err(std::io::Error::other(format!("Arca refused cancellation ({code})"))); }
    Ok(())
}

pub fn microsoft_result(arca: &std::path::Path, ticket: &[u8; 16], cfg: &mut AccountConfig) -> Result<(), String> {
    let mut socket = UnixStream::connect(arca).map_err(|e| e.to_string())?;
    let timeout = Some(Duration::from_secs(10));
    socket.set_read_timeout(timeout).and_then(|_| socket.set_write_timeout(timeout)).map_err(|e| e.to_string())?;
    let mut request = arca_store::codec::Writer::new();
    request.raw(ticket);
    request.str(&cfg.address);
    arcad::proto::write_request(&mut socket, op::MAIL_SIGN_IN_RESULT, &request.buf).map_err(|e| e.to_string())?;
    let (status, items) = arcad::proto::read_app_reply(&mut socket, arcad::proto::AppReply::MailItems).map_err(|e| e.to_string())?;
    if status != status::OK || items.len() != 32 { return Err("Arca could not complete this account’s sign-in.".into()); }
    cfg.incoming_item = Some(items[..16].try_into().unwrap());
    cfg.outgoing_item = Some(items[16..].try_into().unwrap());
    Ok(())
}

/// Have Arca keep what the person typed into its field (`handle`) for the
/// account's servers (arcad `KEEP_FOR`); fills in the item ids, and gives
/// the filters server's with its item. Mail never sees the password.
pub fn keep(arca: &std::path::Path, handle: &[u8; 16], c: &mut AccountConfig) -> Result<Option<(Server, [u8; 16])>, String> {
    use arca_store::codec::Writer;
    let mut eps = endpoints(c)?;
    let sieve = sieve_server(c);
    if let Some(s) = &sieve {
        eps.push((arca_store::Scheme::Sieve, s.host.clone(), s.port));
    }
    let mut w = Writer::new();
    w.raw(handle);
    w.u8(arca_store::model::Class::AfterFirstUnlock as u8); // sync runs in the background
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
            Ok(sieve.zip(ids.get(2).copied()))
        }
        status::LOCKED => Err("Arca is locked: unlock it and try again.".into()),
        _ => Err("Arca didn't keep the password: type it again.".into()),
    }
}

/// The Arca items an account signs in with, each with its server.
pub fn items_of(c: &AccountConfig) -> Vec<((arca_store::Scheme, String, u16), [u8; 16])> {
    let Ok(eps) = endpoints(c) else { return Vec::new() };
    let mut out = Vec::new();
    for (i, ep) in eps.into_iter().enumerate() {
        let item = if i == 0 { c.incoming_item } else { c.outgoing_item };
        if let Some(item) = item.filter(|it| !out.iter().any(|(_, o)| o == it)) {
            out.push((ep, item));
        }
    }
    out
}

/// Have Arca delete an account's items (arcad `DELETE_FOR`), as the
/// account is removed: its servers' and its filters' (`sieve`). Each is
/// tried; the first failure is said.
pub fn forget(arca: &std::path::Path, c: &AccountConfig, sieve: Option<&(Server, [u8; 16])>) -> Result<(), String> {
    use arca_store::codec::Writer;
    let mut first = None;
    let mut items = items_of(c);
    if let Some((s, item)) = sieve {
        if let Ok(sc) = scheme(&s.scheme) {
            items.push(((sc, s.host.clone(), s.port), *item));
        }
    }
    for ((scheme, host, port), item) in items {
        let mut w = Writer::new();
        arcad::proto::write_endpoint_scheme(&mut w, scheme, &host, port);
        w.raw(&item);
        let r = (|| {
            let mut s = UnixStream::connect(arca).map_err(|e| format!("Arca isn't reachable: {e}"))?;
            s.set_read_timeout(Some(Duration::from_secs(10))).ok();
            arcad::proto::write_request(&mut s, op::DELETE_FOR, &w.buf).map_err(|e| e.to_string())?;
            let (st, _) = arcad::proto::read_reply(&mut s).map_err(|e| e.to_string())?;
            match st {
                status::OK => Ok(()),
                status::LOCKED => Err("Arca is locked".to_string()),
                // Already gone, or never Mail's: nothing left to remove.
                status::DENIED => Ok(()),
                _ => Err("Arca didn't remove it".to_string()),
            }
        })();
        if let Err(e) = r {
            first.get_or_insert(e);
        }
    }
    first.map_or(Ok(()), Err)
}

// ── background sync ─────────────────────────────────────────────────

/// How often each account is looked at when nothing is due and it has
/// no push (§6.1).
pub const IDLE_SECS: i64 = 5 * 60;

/// With push for every account, a pass still runs this often (sends,
/// drafts, folders other than the inbox).
pub const PUSHED_SECS: i64 = 30 * 60;

/// What the window (or a watcher) tells the sync thread.
pub enum Wake {
    /// Look now (a send, an action, a new account, "Get mail").
    Now,
    /// The server said this account's inbox changed.
    Changed(AccountId),
    /// Search every account's server for this (§11.2).
    Search(mail_sync::ServerQuery),
    Stop,
}

/// The sync thread's handle: wake it, and see when it changed the index.
pub struct Syncer {
    pub wake: mpsc::Sender<Wake>,
    /// Set after a pass changed or may have changed the index.
    pub changed: Arc<AtomicBool>,
    /// Per account, how the last pass went.
    pub status: Arc<Mutex<Vec<(AccountId, Result<mail_engine::accounts::Pass, Reached>)>>>,
    /// What the last server search found in all, or why it failed; taken
    /// by the window.
    pub searched: Arc<Mutex<Option<Result<usize, String>>>>,
}

/// One account's IDLE watch (§6.1), on a thread and a connection of its
/// own.
pub struct Watcher {
    stop: Arc<AtomicBool>,
    conn: Arc<Mutex<Option<UnixStream>>>,
    /// Connected for push (cleared when the connection ends or the
    /// server turns out to have no IDLE).
    pub idling: Arc<AtomicBool>,
    /// The server has no IDLE: this account is polled.
    pub no_idle: Arc<AtomicBool>,
}

/// Seconds to wait after a watch's connection fails: doubling from one
/// minute to fifteen.
pub fn watch_backoff(failures: u32) -> u64 {
    (60u64 << failures.min(4)).min(15 * 60)
}

impl Watcher {
    /// Watch `account`'s inbox, waking `wake` with [`Wake::Changed`] on news.
    pub fn start(account: AccountId, cfg: AccountConfig, net: Arc<dyn Connect + Send + Sync>, wake: mpsc::Sender<Wake>) -> Watcher {
        let w = Watcher { stop: Arc::default(), conn: Arc::default(), idling: Arc::default(), no_idle: Arc::default() };
        let (stop, conn, idling, no_idle) = (w.stop.clone(), w.conn.clone(), w.idling.clone(), w.no_idle.clone());
        std::thread::spawn(move || {
            let mut failures = 0;
            while !stop.load(Ordering::Acquire) {
                let r = mail_engine::accounts::watch_inbox(
                    &cfg,
                    net.as_ref(),
                    &mut |s| {
                        *conn.lock().unwrap() = Some(s);
                        // Stopped while connecting: end it now.
                        if stop.load(Ordering::Acquire) {
                            if let Some(s) = conn.lock().unwrap().take() {
                                let _ = s.shutdown(std::net::Shutdown::Both);
                            }
                        }
                        idling.store(true, Ordering::Release);
                    },
                    &mut |changed| {
                        failures = 0;
                        idling.store(true, Ordering::Release);
                        !stop.load(Ordering::Acquire) && (!changed || wake.send(Wake::Changed(account)).is_ok())
                    },
                );
                idling.store(false, Ordering::Release);
                conn.lock().unwrap().take();
                match r {
                    Ok(mail_engine::accounts::Watched::NoIdle) => {
                        no_idle.store(true, Ordering::Release);
                        return;
                    }
                    Ok(mail_engine::accounts::Watched::Stopped) => return,
                    Err(e) => {
                        if stop.load(Ordering::Acquire) {
                            return;
                        }
                        eprintln!("atrium-mail: account {account}: push: {e}");
                        let until = Instant::now() + Duration::from_secs(watch_backoff(failures));
                        failures += 1;
                        while Instant::now() < until && !stop.load(Ordering::Acquire) {
                            std::thread::sleep(Duration::from_millis(500));
                        }
                    }
                }
            }
        });
        w
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        if let Some(s) = self.conn.lock().unwrap().take() {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start syncing every configured account, on a thread with its own
/// engine over the same store; IMAP accounts whose servers take IDLE are
/// watched for new mail in their inbox.
pub fn start(store: Store, jail: PathBuf, roots: Vec<Vec<u8>>, net: Arc<dyn Connect + Send + Sync>) -> Syncer {
    let (wake, rx) = mpsc::channel();
    let changed = Arc::new(AtomicBool::new(false));
    let status = Arc::new(Mutex::new(Vec::new()));
    let searched = Arc::new(Mutex::new(None));
    let (ch, st, waker, found) = (changed.clone(), status.clone(), wake.clone(), searched.clone());
    std::thread::spawn(move || {
        let mut engine = Engine::new(store, Parser { program: jail });
        engine.set_roots(roots);
        engine.set_dns(Box::new(NuntiusDns::default()));
        let now = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        let mut watchers: std::collections::HashMap<AccountId, (AccountConfig, Watcher)> = Default::default();
        // `None`: every account.
        let mut only: Option<AccountId> = None;
        loop {
            let configured = engine.configured();
            let mut out = if only.is_some() { st.lock().unwrap().clone() } else { Vec::new() };
            for (id, cfg) in &configured {
                if only.is_some_and(|o| o != *id) {
                    continue;
                }
                let r = engine.pass(*id, net.as_ref(), now());
                if let Err(e) = &r {
                    eprintln!("atrium-mail: account {id}: {e:?}");
                }
                // A watch for each IMAP account that signs in, restarted
                // when its settings change.
                let wanted = !matches!(r, Err(Reached::SignInAgain { .. }));
                let stale = watchers.get(id).is_some_and(|(c, _)| !same_servers(c, cfg));
                if stale || !wanted {
                    watchers.remove(id);
                }
                if wanted && !watchers.contains_key(id) {
                    watchers.insert(*id, (cfg.clone(), Watcher::start(*id, cfg.clone(), net.clone(), waker.clone())));
                }
                out.retain(|(a, _)| a != id);
                out.push((*id, r));
                ch.store(true, Ordering::Release);
            }
            watchers.retain(|id, _| configured.iter().any(|(c, _)| c == id));
            *st.lock().unwrap() = out;
            // Every account pushed: passes can be rarer.
            let pushed = !configured.is_empty() && configured.iter().all(|(id, _)| watchers.get(id).is_some_and(|(_, w)| w.idling.load(Ordering::Acquire)));
            let idle = if pushed { PUSHED_SECS } else { IDLE_SECS };
            let next = engine.next_pass(now(), idle);
            let wait = Duration::from_secs((next - now()).clamp(1, idle) as u64);
            only = None;
            // A server search runs alone, no pass with it (account 0 is
            // none: no account has id 0).
            let search = |words: &mail_sync::ServerQuery| {
                let mut total = 0;
                let mut failed = None;
                for (id, _) in engine.configured() {
                    match engine.search_server(id, words, net.as_ref()) {
                        Ok(n) => total += n,
                        Err(e) => {
                            eprintln!("atrium-mail: account {id}: server search: {e}");
                            failed = Some(e);
                        }
                    }
                }
                *found.lock().unwrap() = Some(match failed {
                    Some(e) if total == 0 => Err(e),
                    _ => Ok(total),
                });
                ch.store(true, Ordering::Release);
            };
            match rx.recv_timeout(wait) {
                Ok(Wake::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                Ok(w) => {
                    // Several wakes at once are one pass, of one account
                    // when only its inbox changed.
                    let mut one = match &w { Wake::Changed(a) => Some(*a), _ => None };
                    let mut all = matches!(w, Wake::Now);
                    let mut pass = !matches!(w, Wake::Search(_));
                    if let Wake::Search(words) = &w {
                        search(words);
                    }
                    while let Ok(w) = rx.try_recv() {
                        match w {
                            Wake::Stop => return,
                            Wake::Search(words) => search(&words),
                            Wake::Now => (all, pass) = (true, true),
                            Wake::Changed(a) if one.is_none_or(|o| o == a) => (one, pass) = (Some(a), true),
                            Wake::Changed(_) => (all, pass) = (true, true),
                        }
                    }
                    only = if !pass { Some(0) } else if all { None } else { one };
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    });
    Syncer { wake, changed, status, searched }
}

/// Whether two settings reach the same servers with the same sign-in.
fn same_servers(a: &AccountConfig, b: &AccountConfig) -> bool {
    a.protocol == b.protocol && a.incoming_item == b.incoming_item
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

    /// An IMAP server on one end of a pair: `idle` says whether it takes
    /// IDLE; the first IDLE reports a new message.
    struct FakeImap {
        idle: bool,
    }

    impl Connect for FakeImap {
        fn mail(&self, _: &Server, _: &[u8; 16]) -> std::io::Result<UnixStream> {
            use std::io::{BufRead, BufReader};
            let (mine, theirs) = UnixStream::pair()?;
            let idle = self.idle;
            std::thread::spawn(move || {
                let mut w = theirs.try_clone().unwrap();
                let caps = if idle { "IMAP4rev1 IDLE" } else { "IMAP4rev1" };
                let _ = write!(w, "* PREAUTH [CAPABILITY {caps}] ready\r\n");
                let mut first = true;
                let mut tag = String::new();
                for l in BufReader::new(theirs).lines() {
                    let Ok(l) = l else { return };
                    if l == "DONE" {
                        let _ = write!(w, "{tag} OK idle done\r\n");
                        continue;
                    }
                    let mut p = l.splitn(3, ' ');
                    tag = p.next().unwrap_or("").to_string();
                    match p.next().unwrap_or("") {
                        "IDLE" if first => {
                            first = false;
                            let _ = write!(w, "+ idling\r\n* 3 EXISTS\r\n");
                        }
                        "IDLE" => {
                            let _ = write!(w, "+ idling\r\n");
                        }
                        "LOGOUT" => {
                            let _ = write!(w, "* BYE\r\n{tag} OK bye\r\n");
                            return;
                        }
                        _ => {
                            let _ = write!(w, "* 2 EXISTS\r\n{tag} OK [READ-WRITE] done\r\n");
                        }
                    }
                }
            });
            Ok(mine)
        }
        fn jmap(&self, _: &str, _: &[u8; 16]) -> Result<Box<dyn mail_sync::jmap::Http>, String> {
            Err("no".into())
        }
    }

    fn imap_account() -> AccountConfig {
        let server = |h: &str, p| Server { scheme: "imaps".into(), host: h.into(), port: p };
        AccountConfig {
            address: "ada@example.org".into(),
            name: "Ada".into(),
            protocol: Protocol::Imap { imap: server("imap.example.org", 993), smtp: server("smtp.example.org", 465) },
            incoming_item: Some([1; 16]),
            outgoing_item: Some([2; 16]),
            oauth: None,
            reached: Reached::Never,
        }
    }

    #[test]
    fn a_watcher_wakes_the_sync_for_its_account_and_stops() {
        let (tx, rx) = mpsc::channel();
        let w = Watcher::start(7, imap_account(), Arc::new(FakeImap { idle: true }), tx);
        match rx.recv_timeout(Duration::from_secs(10)).unwrap() {
            Wake::Changed(a) => assert_eq!(a, 7),
            _ => panic!("not a change"),
        }
        assert!(w.idling.load(Ordering::Acquire));
        w.stop();
        let until = Instant::now() + Duration::from_secs(10);
        while w.idling.load(Ordering::Acquire) {
            assert!(Instant::now() < until, "still watching");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!w.no_idle.load(Ordering::Acquire));
    }

    #[test]
    fn a_server_without_idle_is_polled() {
        let (tx, rx) = mpsc::channel();
        let w = Watcher::start(7, imap_account(), Arc::new(FakeImap { idle: false }), tx);
        let until = Instant::now() + Duration::from_secs(10);
        while !w.no_idle.load(Ordering::Acquire) {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(rx.try_recv().is_err());
        assert_eq!((watch_backoff(0), watch_backoff(1), watch_backoff(3), watch_backoff(9)), (60, 120, 480, 900));
    }

    #[test]
    fn an_event_stream_from_the_courier() {
        let mut b = Vec::new();
        for r in [Response::Events { status: 200, headers: vec![] }, Response::Event(b"event: ping\n\n".to_vec()), Response::Event(b"event: state\n\n".to_vec()), Response::EventsEnd] {
            arca_courier::send::write_frame(&mut b, &r.encode()).unwrap();
        }
        let mut got = Vec::new();
        read_events(&mut &b[..], &mut |e| {
            got.push(e.to_vec());
            true
        })
        .unwrap();
        assert_eq!(got, vec![b"event: ping\n\n".to_vec(), b"event: state\n\n".to_vec()]);
        // Stopping at the first.
        let mut n = 0;
        read_events(&mut &b[..], &mut |_| {
            n += 1;
            false
        })
        .unwrap();
        assert_eq!(n, 1);
        // A refused sign-in reads as one, so the account says "sign in again".
        let mut b = Vec::new();
        arca_courier::send::write_frame(&mut b, &Response::Http { status: 401, headers: vec![], body: vec![] }.encode()).unwrap();
        assert!(read_events(&mut &b[..], &mut |_| true).unwrap_err().starts_with("AUTHENTICATIONFAILED"));
    }

    /// arcad on a socket: answers each request with the next status, and
    /// keeps what it was asked.
    fn fake_arcad(dir: &std::path::Path, answers: Vec<u8>) -> (PathBuf, Arc<Mutex<Vec<(u16, Vec<u8>)>>>) {
        let path = dir.join("arcad.sock");
        let l = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        std::thread::spawn(move || {
            for (st, c) in answers.into_iter().zip(l.incoming()) {
                let mut c = c.unwrap();
                let f = arcad::proto::read_frame(&mut c).unwrap().unwrap();
                let b = f.as_ref();
                s2.lock().unwrap().push((u16::from_le_bytes([b[0], b[1]]), b[2..].to_vec()));
                arcad::proto::write_reply(&mut c, st, &[]).unwrap();
            }
        });
        (path, seen)
    }

    #[test]
    fn removing_an_account_asks_arca_to_delete_each_item() {
        let d = tempfile::tempdir().unwrap();
        let (sock, seen) = fake_arcad(d.path(), vec![status::OK, status::LOCKED]);
        let c = imap_account();
        assert_eq!(items_of(&c).len(), 2);
        assert_eq!(forget(&sock, &c, None), Err("Arca is locked".into()));
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2);
        let want = |sc, h: &str, p, item: [u8; 16]| {
            let mut w = arca_store::codec::Writer::new();
            arcad::proto::write_endpoint_scheme(&mut w, sc, h, p);
            w.raw(&item);
            (op::DELETE_FOR, w.buf)
        };
        assert_eq!(seen[0], want(arca_store::Scheme::Imaps, "imap.example.org", 993, [1; 16]));
        assert_eq!(seen[1], want(arca_store::Scheme::Imaps, "smtp.example.org", 465, [2; 16]));
        // One item for both servers is deleted once; none, nothing asked.
        let one = AccountConfig { outgoing_item: Some([1; 16]), ..imap_account() };
        assert_eq!(items_of(&one).len(), 1);
        let none = AccountConfig { incoming_item: None, outgoing_item: None, ..imap_account() };
        assert!(items_of(&none).is_empty());
        assert_eq!(forget(&d.path().join("none.sock"), &none, None), Ok(()));
    }

    #[test]
    fn an_imap_account_keeps_a_sign_in_for_its_filters() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("arcad.sock");
        let l = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let asked = std::thread::spawn(move || {
            let mut c = l.incoming().next().unwrap().unwrap();
            let f = arcad::proto::read_frame(&mut c).unwrap().unwrap();
            let ids: Vec<u8> = [[1u8; 16], [2; 16], [3; 16]].concat();
            arcad::proto::write_reply(&mut c, status::OK, &ids).unwrap();
            f.as_ref().to_vec()
        });
        let mut c = AccountConfig { incoming_item: None, outgoing_item: None, ..imap_account() };
        let sieve = keep(&path, &[7; 16], &mut c).unwrap();
        assert_eq!((c.incoming_item, c.outgoing_item), (Some([1; 16]), Some([2; 16])));
        let (server, item) = sieve.unwrap();
        assert_eq!((server.scheme.as_str(), server.host.as_str(), server.port, item), ("sieve", "imap.example.org", 4190, [3; 16]));
        let req = asked.join().unwrap();
        let mut w = arca_store::codec::Writer::new();
        arcad::proto::write_endpoint_scheme(&mut w, arca_store::Scheme::Sieve, "imap.example.org", 4190);
        assert!(req.ends_with(&w.buf), "the third endpoint is the filters'");
        // Known to have none, or signing in by OAuth: no filters' sign-in.
        let gmail = |h: &str| AccountConfig { protocol: Protocol::Imap { imap: Server { scheme: "imaps".into(), host: h.into(), port: 993 }, smtp: Server { scheme: "submissions".into(), host: "smtp.gmail.com".into(), port: 465 } }, ..imap_account() };
        assert!(sieve_server(&gmail("imap.gmail.com")).is_none());
        assert!(sieve_server(&AccountConfig { oauth: Some("microsoft".into()), ..imap_account() }).is_none());
        // Removed with the account.
        let d2 = tempfile::tempdir().unwrap();
        let (sock, seen) = fake_arcad(d2.path(), vec![status::OK; 3]);
        assert_eq!(forget(&sock, &c, Some(&(server, item))), Ok(()));
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 3);
        assert!(seen[2].1.starts_with(&w.buf) && seen[2].1.ends_with(&[3; 16]));
    }
}
