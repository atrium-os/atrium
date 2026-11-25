//! One request, in the order arca.md §5 fixes: resolve and connect through
//! the fetch rules, finish TLS to the bound host, and only then ask Arca
//! for the slots; attach, send, strip `Set-Cookie` into records, hand the
//! rest back, wipe.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use arca_store::secret::LockedBuf;
use navigator_fetch::{addr, Connect, Conversation};

use crate::schema::{Encoding, Method, Place, Refusal, Request, Response, Segment};

/// Arca, as the courier sees it.
pub trait Vault {
    /// The values for these (source, ref) pairs, all or nothing.
    fn fill(&mut self, refs: &[(u8, [u8; 16])]) -> Result<Vec<LockedBuf>, Refusal>;
    /// A `Set-Cookie` from the endpoint, for a request to `path`. Arca
    /// validates it; errors are Arca's to log.
    /// `ticket` is the request's, for a web courier.
    fn set_cookie(&mut self, name: &str, value: &[u8], attrs: &[u8], path: &str, ticket: Option<&[u8; 16]>);
    /// The `Cookie` header Arca matched for this request (`method`, `path`,
    /// `query` without its `?`), if any. A web courier's request redeems its
    /// ticket here, for exactly this request. `origin`: the request's
    /// `Origin` header, which Arca allows only from the ticket's initiator's
    /// site; `Err` refuses the request.
    fn cookies(&mut self, method: Method, path: &str, query: &str, ticket: Option<&[u8; 16]>, origin: Option<&str>) -> Result<Option<LockedBuf>, Refusal>;
}

/// The one endpoint this courier serves, from its label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
}

type Tls = rustls::StreamOwned<rustls::ClientConnection, TcpStream>;

/// A courier: one endpoint, a kept-alive connection while the server allows.
pub struct Courier<'a> {
    pub endpoint: Endpoint,
    conv: Conversation<'a>,
    tls: Arc<rustls::ClientConfig>,
    net: &'a (dyn Connect + Sync),
    conn: Option<Tls>,
    /// TCP connections made, for tests and logs.
    pub connects: usize,
}

impl<'a> Courier<'a> {
    pub fn new(endpoint: Endpoint, net: &'a (dyn Connect + Sync), tls: Arc<rustls::ClientConfig>, policy: addr::Policy) -> Self {
        Courier { endpoint, conv: Conversation::new(net, tls.clone(), policy), tls, net, conn: None, connects: 0 }
    }

    /// Connect and finish the handshake: the server has proved it is the
    /// bound host before anything is asked of Arca.
    fn connect(&mut self) -> Result<Tls, Refusal> {
        let Endpoint { host, port } = self.endpoint.clone();
        let addrs = self.conv.resolve_checked(&host, port).map_err(|_| Refusal::Connect)?;
        let tcp = addrs
            .iter()
            .find_map(|a| self.net.connect_addr(*a).ok())
            .ok_or(Refusal::Connect)?;
        self.connects += 1;
        tcp.set_read_timeout(Some(navigator_fetch::TIMEOUT)).ok();
        tcp.set_write_timeout(Some(navigator_fetch::TIMEOUT)).ok();
        let name = rustls_pki_types::ServerName::try_from(host).map_err(|_| Refusal::Tls)?;
        let conn = rustls::ClientConnection::new(self.tls.clone(), name).map_err(|_| Refusal::Tls)?;
        let mut s = rustls::StreamOwned::new(conn, tcp);
        while s.conn.is_handshaking() {
            s.conn.complete_io(&mut s.sock).map_err(|_| Refusal::Tls)?;
        }
        Ok(s)
    }

    /// Handle one request frame from the client. Malformed bytes are
    /// refused before any network or Arca call.
    pub fn handle(&mut self, frame: &[u8], vault: &mut dyn Vault) -> Response {
        let req = match Request::decode(frame) {
            Ok(r) => r,
            Err(_) => return Response::Refused(Refusal::Malformed),
        };
        match self.send(&req, vault) {
            Ok(r) => r,
            Err(e) => {
                self.conn = None;
                Response::Refused(e)
            }
        }
    }

    /// Handle one request frame, writing the reply frames to `out`: one
    /// frame, or for an event stream ([`wants_events`]) a frame per event
    /// until the stream ends. `Err` only when `out` fails.
    pub fn handle_to(&mut self, frame: &[u8], vault: &mut dyn Vault, out: &mut dyn Write) -> std::io::Result<()> {
        match Request::decode(frame) {
            Ok(req) if wants_events(&req) => {
                let r = self.events(&req, vault, out);
                // A stream's connection is never reused.
                self.conn = None;
                match r {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(e)) => write_frame(out, &Response::Refused(e).encode()),
                    Err(e) => Err(e),
                }
            }
            _ => write_frame(out, &self.handle(frame, vault).encode()),
        }
    }

    /// An event stream: the head, then each event as it comes, masked like
    /// a body. The outer `Err` is the client gone; the inner one a refusal
    /// to send in place of the rest.
    fn events(&mut self, req: &Request, vault: &mut dyn Vault, out: &mut dyn Write) -> std::io::Result<Result<(), Refusal>> {
        let (mut s, values, cookie, path, _) = match self.start(req, vault) {
            Ok(x) => x,
            Err(e) => return Ok(Err(e)),
        };
        s.sock.set_read_timeout(Some(EVENTS_WAIT)).ok();
        let Ok((head, rest)) = navigator_fetch::http::read_head(&mut s, false) else { return Ok(Err(Refusal::Http)) };
        let is_events = head.header("content-type").is_some_and(|t| t.to_ascii_lowercase().starts_with("text/event-stream"));
        let mut body = navigator_fetch::http::Body::new(&head, rest);
        if !(200..300).contains(&head.status) || !is_events {
            // An ordinary answer (a refusal, a redirect): whole, as usual.
            let mut b = Vec::new();
            loop {
                match body.next(&mut s, 64 * 1024) {
                    Ok(Some(c)) if b.len() + c.len() <= crate::schema::BODY_MAX => b.extend_from_slice(&c),
                    Ok(None) => break,
                    _ => return Ok(Err(Refusal::Http)),
                }
            }
            let r = scrub(navigator_fetch::http::Response { status: head.status, headers: head.headers, body: b }, &values, cookie.as_ref(), &path, req.ticket.as_ref(), vault);
            return write_frame(out, &r.encode()).map(|_| Ok(()));
        }
        let Response::Http { status, headers, .. } = scrub(navigator_fetch::http::Response { status: head.status, headers: head.headers, body: Vec::new() }, &values, cookie.as_ref(), &path, req.ticket.as_ref(), vault) else {
            return Ok(Err(Refusal::Internal));
        };
        write_frame(out, &Response::Events { status, headers }.encode())?;
        let mut buf: Vec<u8> = Vec::new();
        loop {
            // Each whole event (up to a blank line), masked, on its own.
            while let Some(end) = event_end(&buf) {
                let mut e: Vec<u8> = buf.drain(..end).collect();
                mask_body(&mut e, &values, cookie.as_ref());
                write_frame(out, &Response::Event(e).encode())?;
            }
            if buf.len() > crate::schema::EVENT_MAX {
                return Ok(Err(Refusal::Http));
            }
            match body.next(&mut s, 16 * 1024) {
                Ok(Some(c)) => buf.extend_from_slice(&c),
                Ok(None) => return write_frame(out, &Response::EventsEnd.encode()).map(|_| Ok(())),
                // Quiet past the wait, or cut off: the stream is over.
                Err(_) => return write_frame(out, &Response::EventsEnd.encode()).map(|_| Ok(())),
            }
        }
    }

    /// Connect (or reuse), fill the slots and write the request.
    fn start(&mut self, req: &Request, vault: &mut dyn Vault) -> Result<(Tls, Vec<LockedBuf>, Option<LockedBuf>, String, Option<LockedBuf>), Refusal> {
        // A kept-alive connection is already past TLS to this host; a
        // fresh one gets there before the vault is asked.
        let (mut s, reused) = match self.conn.take() {
            Some(s) => (s, true),
            None => (self.connect()?, false),
        };
        let values = if req.slots.is_empty() {
            Vec::new()
        } else {
            let refs: Vec<(u8, [u8; 16])> = req.slots.iter().map(|s| (s.source, s.reference)).collect();
            vault.fill(&refs)?
        };
        // Arca's matching, after TLS like the fill. No cookies is no
        // refusal: the request goes without. An Origin Arca doesn't allow
        // for the ticket is.
        let path = std::str::from_utf8(&req.path).map_err(|_| Refusal::Malformed)?;
        let query = std::str::from_utf8(&req.query).map_err(|_| Refusal::Malformed)?;
        let origin = match req.headers.iter().find(|(h, _)| *h == crate::schema::Header::Origin) {
            Some((_, v)) => Some(std::str::from_utf8(v).map_err(|_| Refusal::Malformed)?),
            None => None,
        };
        let cookie = vault.cookies(req.method, path, query, req.ticket.as_ref(), origin)?;
        let wire = build(req, &self.endpoint, &values, cookie.as_ref())?;
        let mut reused = reused;
        if s.write_all(wire.as_ref()).and_then(|_| s.flush()).is_err() {
            // The server closed a kept-alive connection: once more, fresh.
            // The values are already in hand; the new connection is past
            // TLS to the same host before they are written.
            s = self.connect()?;
            reused = false;
            s.write_all(wire.as_ref()).and_then(|_| s.flush()).map_err(|_| Refusal::Http)?;
        }
        // Kept while a GET or HEAD on a kept-alive connection may still be
        // sent again ([`Courier::send`]); dropped (and wiped) otherwise.
        let again = (reused && idempotent(req.method)).then_some(wire);
        Ok((s, values, cookie, path.to_string(), again))
    }

    fn send(&mut self, req: &Request, vault: &mut dyn Vault) -> Result<Response, Refusal> {
        let (mut s, values, cookie, path, again) = self.start(req, vault)?;
        let path = path.as_str();
        // A kept-alive connection the server closed after the write went
        // into it (a restarted server): the read fails. A GET or HEAD is
        // sent once more on a fresh connection, the same bytes, as
        // browsers do; nothing is asked of the vault twice.
        let (resp, keep) = match (read(&mut s, req.method), again) {
            (Ok(r), _) => r,
            (Err(_), Some(wire)) => {
                s = self.connect()?;
                s.write_all(wire.as_ref()).and_then(|_| s.flush()).map_err(|_| Refusal::Http)?;
                drop(wire);
                read(&mut s, req.method)?
            }
            (Err(e), None) => return Err(e),
        };
        if keep {
            self.conn = Some(s);
        }
        Ok(scrub(resp, &values, cookie.as_ref(), path, req.ticket.as_ref(), vault))
    }
}

/// How long an event stream may be quiet: servers ping well within it
/// (Mail asks for a ping a minute).
pub const EVENTS_WAIT: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// A `GET` asking for `text/event-stream`.
pub fn wants_events(req: &Request) -> bool {
    req.method == Method::Get && req.headers.iter().any(|(h, v)| *h == crate::schema::Header::Accept && v.eq_ignore_ascii_case(b"text/event-stream"))
}

/// Where the first whole event in `b` ends (after its blank line).
fn event_end(b: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i + 1 < b.len() {
        match (b[i], b[i + 1]) {
            (b'\n', b'\n') => return Some(i + 2),
            (b'\r', b'\r') if b.get(i + 2) != Some(&b'\n') => return Some(i + 2),
            (b'\r', b'\n') if b.get(i + 2..i + 4) == Some(b"\r\n") => return Some(i + 4),
            _ => {}
        }
        i += 1;
    }
    None
}

/// A request that may be sent twice: the server keeps no change from it.
fn idempotent(m: Method) -> bool { matches!(m, Method::Get | Method::Head) }

fn read(s: &mut Tls, method: Method) -> Result<(navigator_fetch::http::Response, bool), Refusal> {
    // HEAD has no body whatever the length says.
    let cap = (method == Method::Head).then_some(0);
    navigator_fetch::http::read_response_capped(s, cap).map_err(|_| Refusal::Http)
}

fn form_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"*-._".contains(&c)
}

/// The value as it goes on the wire, in locked memory.
fn encode(v: &[u8], e: Encoding) -> Result<LockedBuf, Refusal> {
    let len = match e {
        Encoding::Raw => v.len(),
        Encoding::Form => v.iter().map(|&c| if form_byte(c) || c == b' ' { 1 } else { 3 }).sum(),
        Encoding::JsonString => v
            .iter()
            .map(|&c| match c {
                b'"' | b'\\' | b'\n' | b'\r' | b'\t' => 2,
                c if c < 0x20 => 6,
                _ => 1,
            })
            .sum(),
    };
    let mut out = LockedBuf::zeroed(len).map_err(|_| Refusal::Internal)?;
    let o = out.as_mut();
    let hex = b"0123456789ABCDEF";
    let mut i = 0;
    for &c in v {
        match e {
            Encoding::Raw => {
                o[i] = c;
                i += 1;
            }
            Encoding::Form if form_byte(c) => {
                o[i] = c;
                i += 1;
            }
            Encoding::Form if c == b' ' => {
                o[i] = b'+';
                i += 1;
            }
            Encoding::Form => {
                o[i..i + 3].copy_from_slice(&[b'%', hex[(c >> 4) as usize], hex[(c & 15) as usize]]);
                i += 3;
            }
            Encoding::JsonString => {
                let esc: &[u8] = match c {
                    b'"' => b"\\\"",
                    b'\\' => b"\\\\",
                    b'\n' => b"\\n",
                    b'\r' => b"\\r",
                    b'\t' => b"\\t",
                    _ => &[],
                };
                if !esc.is_empty() {
                    o[i..i + 2].copy_from_slice(esc);
                    i += 2;
                } else if c < 0x20 {
                    o[i..i + 6].copy_from_slice(&[b'\\', b'u', b'0', b'0', hex[(c >> 4) as usize], hex[(c & 15) as usize]]);
                    i += 6;
                } else {
                    o[i] = c;
                    i += 1;
                }
            }
        }
    }
    Ok(out)
}

/// The whole request in locked memory: the only place a value is written.
pub fn build(req: &Request, ep: &Endpoint, values: &[LockedBuf], cookie: Option<&LockedBuf>) -> Result<LockedBuf, Refusal> {
    let mut body: Vec<LockedBuf> = Vec::new();
    for g in &req.body {
        body.push(match g {
            Segment::Literal(b) => LockedBuf::from_slice(b).map_err(|_| Refusal::Internal)?,
            Segment::Slot { slot, encoding } => encode(values.get(*slot as usize).ok_or(Refusal::Internal)?.as_ref(), *encoding)?,
        });
    }
    let body_len: usize = body.iter().map(|b| b.len()).sum();
    let mut head = Vec::new();
    head.extend_from_slice(req.method.as_str().as_bytes());
    head.push(b' ');
    head.extend_from_slice(&req.path);
    if !req.query.is_empty() {
        head.push(b'?');
        head.extend_from_slice(&req.query);
    }
    head.extend_from_slice(b" HTTP/1.1\r\nHost: ");
    head.extend_from_slice(ep.host.as_bytes());
    if ep.port != 443 {
        head.extend_from_slice(format!(":{}", ep.port).as_bytes());
    }
    head.extend_from_slice(b"\r\nAccept-Encoding: identity\r\nConnection: keep-alive\r\n");
    for (h, v) in &req.headers {
        head.extend_from_slice(h.name().as_bytes());
        head.extend_from_slice(b": ");
        head.extend_from_slice(v);
        head.extend_from_slice(b"\r\n");
    }
    if req.method.has_body() {
        head.extend_from_slice(format!("Content-Length: {body_len}\r\n").as_bytes());
    }
    let bearer = req.slots.iter().position(|s| s.place == Place::Bearer);
    let auth_len = match bearer {
        Some(i) => {
            let v = values.get(i).ok_or(Refusal::Internal)?.as_ref();
            // A header value: visible ASCII only, so no value can end the
            // header and start another.
            if v.is_empty() || !v.iter().all(|c| (0x21..=0x7e).contains(c)) {
                return Err(Refusal::Unsendable);
            }
            "Authorization: Bearer ".len() + v.len() + 2
        }
        None => 0,
    };
    // Arca's header: names are tokens, values have no controls or ';'.
    let cookie = cookie.filter(|c| !c.is_empty());
    if cookie.is_some_and(|c| !c.as_ref().iter().all(|&b| b == b'\t' || (0x20..0x7f).contains(&b))) {
        return Err(Refusal::Internal);
    }
    let cookie_len = cookie.map_or(0, |c| "Cookie: ".len() + c.len() + 2);
    let total = head.len() + auth_len + cookie_len + 2 + body_len;
    let mut out = LockedBuf::zeroed(total).map_err(|_| Refusal::Internal)?;
    let o = out.as_mut();
    let mut at = 0;
    let mut put = |b: &[u8]| {
        o[at..at + b.len()].copy_from_slice(b);
        at += b.len();
    };
    put(&head);
    if let Some(i) = bearer {
        put(b"Authorization: Bearer ");
        put(values[i].as_ref());
        put(b"\r\n");
    }
    if let Some(c) = cookie {
        put(b"Cookie: ");
        put(c.as_ref());
        put(b"\r\n");
    }
    put(b"\r\n");
    for b in &body {
        put(b.as_ref());
    }
    Ok(out)
}

/// Replace every occurrence of `needle` in `hay` with '*'.
fn mask(hay: &mut [u8], needle: &[u8]) -> bool {
    if needle.len() < 4 || hay.len() < needle.len() {
        return false;
    }
    let mut hit = false;
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        if &hay[i..i + needle.len()] == needle {
            hay[i..i + needle.len()].fill(b'*');
            hit = true;
            i += needle.len();
        } else {
            i += 1;
        }
    }
    hit
}

/// Cookie values shorter than this aren't masked: a session id is long,
/// and masking every `true` in a page would break it.
const COOKIE_MASK_MIN: usize = 16;

/// What the client gets: no `Set-Cookie` (those go to Arca), and no echo
/// of a value this request carried (arca.md §8.3), nor of a cookie's.
fn scrub(
    resp: navigator_fetch::http::Response,
    values: &[LockedBuf],
    cookie: Option<&LockedBuf>,
    path: &str,
    ticket: Option<&[u8; 16]>,
    vault: &mut dyn Vault,
) -> Response {
    let navigator_fetch::http::Response { status, mut headers, mut body } = resp;
    // A GlobalProtect portal's SAML answer (vpn.md §9.3): its token goes to
    // Arca (kept for Crypta only in a VPN sign-in window's jar, refused
    // anywhere else), never to the client, as a header or in the page.
    if let Some((value, token)) = gp_saml_token(&headers, &body) {
        vault.set_cookie(GP_SAML_TOKEN, value.as_bytes(), b"", path, ticket);
        mask(&mut body, token.as_bytes());
        headers.retain(|(k, _)| !GP_SAML_HEADERS.contains(&k.as_str()));
    }
    let mut out = Vec::with_capacity(headers.len());
    for (k, v) in headers {
        if k == "set-cookie" {
            if let Some((nv, attrs)) = Some(v.split_once(';').unwrap_or((v.as_str(), ""))) {
                if let Some((n, val)) = nv.split_once('=') {
                    vault.set_cookie(n.trim(), val.trim().as_bytes(), attrs.trim().as_bytes(), path, ticket);
                }
            }
            continue;
        }
        let mut vb = v.into_bytes();
        for val in values {
            mask(&mut vb, val.as_ref());
        }
        out.push((k, String::from_utf8(vb).unwrap_or_default()));
    }
    let cookie_values: Vec<&[u8]> = cookie
        .map(|c| {
            c.as_ref()
                .split(|&b| b == b';')
                .filter_map(|p| p.iter().position(|&b| b == b'=').map(|i| &p[i + 1..]))
                .filter(|v| v.len() >= COOKIE_MASK_MIN)
                .collect()
        })
        .unwrap_or_default();
    for (_, v) in out.iter_mut() {
        let mut vb = std::mem::take(v).into_bytes();
        for c in &cookie_values {
            mask(&mut vb, c);
        }
        *v = String::from_utf8(vb).unwrap_or_default();
    }
    mask_body(&mut body, values, cookie);
    Response::Http { status, headers: out, body }
}

/// arcad's `jar::VPN_GP_TOKEN`.
const GP_SAML_TOKEN: &str = "gpSamlToken";
const GP_SAML_HEADERS: [&str; 4] = ["prelogin-cookie", "portal-userauthcookie", "saml-username", "saml-auth-status"];
const GP_SAML_MAX: usize = 4096;

/// A GlobalProtect portal's answer at the end of a SAML sign-in: its
/// `prelogin-cookie` (or `portal-userauthcookie`) and `saml-username`, as
/// response headers or as the same-named elements in the page (PAN puts
/// them in an HTML comment), with `saml-auth-status` 1. As Arca files it:
/// `<kind>\n<user>\n<token>`, and the token alone.
fn gp_saml_token(headers: &[(String, String)], body: &[u8]) -> Option<(zeroize::Zeroizing<String>, zeroize::Zeroizing<String>)> {
    let header = |n: &str| headers.iter().find(|(k, _)| k == n).map(|(_, v)| v.trim().to_string());
    let element = |n: &str| {
        let text = std::str::from_utf8(body).ok()?;
        let (open, close) = (format!("<{n}>"), format!("</{n}>"));
        let start = text.find(&open)? + open.len();
        let end = start + text[start..].find(&close)?;
        Some(text[start..end].trim().to_string())
    };
    let field = |n: &str| header(n).filter(|v| !v.is_empty()).or_else(|| element(n).filter(|v| !v.is_empty()));
    if field("saml-auth-status").as_deref() != Some("1") {
        return None;
    }
    let (kind, token) = ["prelogin-cookie", "portal-userauthcookie"].into_iter().find_map(|k| field(k).map(|t| (k, t)))?;
    let user = field("saml-username")?;
    let ok = |v: &str| v.len() <= GP_SAML_MAX && v.bytes().all(|b| (b' '..0x7f).contains(&b));
    if !ok(&token) || !ok(&user) {
        return None;
    }
    let value = zeroize::Zeroizing::new(format!("{kind}\n{user}\n{token}"));
    Some((value, zeroize::Zeroizing::new(token)))
}

/// The filled values and the cookies sent, out of a body (as sent, form
/// or JSON-escaped).
fn mask_body(body: &mut [u8], values: &[LockedBuf], cookie: Option<&LockedBuf>) {
    let cookie_values: Vec<&[u8]> = cookie
        .map(|c| {
            c.as_ref()
                .split(|&b| b == b';')
                .filter_map(|p| p.iter().position(|&b| b == b'=').map(|i| &p[i + 1..]))
                .filter(|v| v.len() >= COOKIE_MASK_MIN)
                .collect()
        })
        .unwrap_or_default();
    for c in &cookie_values {
        mask(body, c);
    }
    for val in values {
        mask(body, val.as_ref());
        for e in [Encoding::Form, Encoding::JsonString] {
            if let Ok(enc) = encode(val.as_ref(), e) {
                mask(body, enc.as_ref());
            }
        }
    }
}

/// Read one client frame (`u32 LE length | bytes`) from `r`.
pub fn read_frame(r: &mut impl Read) -> std::io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let n = u32::from_le_bytes(len) as usize;
    if n > crate::schema::FRAME_MAX {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut b = vec![0u8; n];
    r.read_exact(&mut b)?;
    Ok(Some(b))
}

pub fn write_frame<W: Write + ?Sized>(w: &mut W, b: &[u8]) -> std::io::Result<()> {
    w.write_all(&(b.len() as u32).to_le_bytes())?;
    w.write_all(b)?;
    w.flush()
}
