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
    /// A `Set-Cookie` from the endpoint. Errors are Arca's to log.
    fn set_cookie(&mut self, name: &str, value: &[u8], attrs: &[u8]);
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

    fn send(&mut self, req: &Request, vault: &mut dyn Vault) -> Result<Response, Refusal> {
        // A kept-alive connection is already past TLS to this host; a
        // fresh one gets there before the vault is asked.
        let mut s = match self.conn.take() {
            Some(s) => s,
            None => self.connect()?,
        };
        let values = if req.slots.is_empty() {
            Vec::new()
        } else {
            let refs: Vec<(u8, [u8; 16])> = req.slots.iter().map(|s| (s.source, s.reference)).collect();
            vault.fill(&refs)?
        };
        let wire = build(req, &self.endpoint, &values)?;
        if s.write_all(wire.as_ref()).and_then(|_| s.flush()).is_err() {
            // The server closed a kept-alive connection: once more, fresh.
            // The values are already in hand; the new connection is past
            // TLS to the same host before they are written.
            s = self.connect()?;
            s.write_all(wire.as_ref()).and_then(|_| s.flush()).map_err(|_| Refusal::Http)?;
        }
        drop(wire);
        let (resp, keep) = read(&mut s, req.method)?;
        if keep {
            self.conn = Some(s);
        }
        Ok(scrub(resp, &values, vault))
    }
}

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
pub fn build(req: &Request, ep: &Endpoint, values: &[LockedBuf]) -> Result<LockedBuf, Refusal> {
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
    let total = head.len() + auth_len + 2 + body_len;
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

/// What the client gets: no `Set-Cookie` (those go to Arca), and no echo
/// of a value this request carried (arca.md §8.3).
fn scrub(resp: navigator_fetch::http::Response, values: &[LockedBuf], vault: &mut dyn Vault) -> Response {
    let navigator_fetch::http::Response { status, headers, mut body } = resp;
    let mut out = Vec::with_capacity(headers.len());
    for (k, v) in headers {
        if k == "set-cookie" {
            if let Some((nv, attrs)) = Some(v.split_once(';').unwrap_or((v.as_str(), ""))) {
                if let Some((n, val)) = nv.split_once('=') {
                    vault.set_cookie(n.trim(), val.trim().as_bytes(), attrs.trim().as_bytes());
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
    for val in values {
        mask(&mut body, val.as_ref());
        for e in [Encoding::Form, Encoding::JsonString] {
            if let Ok(enc) = encode(val.as_ref(), e) {
                mask(&mut body, enc.as_ref());
            }
        }
    }
    Response::Http { status, headers: out, body }
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

pub fn write_frame(w: &mut impl Write, b: &[u8]) -> std::io::Result<()> {
    w.write_all(&(b.len() as u32).to_le_bytes())?;
    w.write_all(b)?;
    w.flush()
}
