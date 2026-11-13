//! The courier's request and response, as the client sends and gets them
//! (arca.md §5 step 1, §5.1). A fixed binary schema with a cap on every
//! field: no header text, no URL to parse, nothing the client can use to
//! name another host. The courier's endpoint comes from its label alone.
//!
//! ```text
//! request  = u8 version(1) | u8 method | bytes path | bytes query
//!          | u8 n | n × (u8 header | bytes value)
//!          | u8 n | n × (u8 source | ref[16] | u8 place)
//!          | u16 n | n × segment
//!          | u8 0 | u8 1 ticket[16]       -- the web broker's ticket (arca.md §8.2)
//! segment  = u8 0 | bytes literal          -- body bytes as given
//!          | u8 1 | u8 slot | u8 encoding  -- a slot's value, encoded
//! response = u8 0 | u16 status | u8 n | n × (str name | str value) | bytes body
//!          | u8 1 | u8 reason              -- refused, nothing sent
//! ```
//!
//! `bytes` is a u32 length then the bytes (arca_store::codec).

use arca_store::codec::{Reader, Writer};

pub const VERSION: u8 = 1;
pub const PATH_MAX: usize = 8 * 1024;
pub const QUERY_MAX: usize = 8 * 1024;
pub const HEADERS_MAX: usize = 16;
pub const HEADER_VALUE_MAX: usize = 1024;
pub const SLOTS_MAX: usize = arcad::proto::courier_op::MAX_SLOTS;
pub const SEGMENTS_MAX: usize = 256;
pub const BODY_MAX: usize = 1024 * 1024;
/// The whole request frame.
pub const FRAME_MAX: usize = BODY_MAX + 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get = 1,
    Head = 2,
    Post = 3,
    Put = 4,
    Patch = 5,
    Delete = 6,
}

impl Method {
    fn from_u8(v: u8) -> Option<Method> {
        Some(match v {
            1 => Method::Get,
            2 => Method::Head,
            3 => Method::Post,
            4 => Method::Put,
            5 => Method::Patch,
            6 => Method::Delete,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Head => "HEAD",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
        }
    }

    pub fn has_body(self) -> bool {
        matches!(self, Method::Post | Method::Put | Method::Patch)
    }
}

/// The headers a client may set. Everything else (Host, Cookie,
/// Authorization, Content-Length, Connection, …) is the courier's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Header {
    Accept = 1,
    AcceptLanguage = 2,
    ContentType = 3,
    IfNoneMatch = 4,
    IfModifiedSince = 5,
    Range = 6,
    CacheControl = 7,
}

impl Header {
    fn from_u8(v: u8) -> Option<Header> {
        Some(match v {
            1 => Header::Accept,
            2 => Header::AcceptLanguage,
            3 => Header::ContentType,
            4 => Header::IfNoneMatch,
            5 => Header::IfModifiedSince,
            6 => Header::Range,
            7 => Header::CacheControl,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Header::Accept => "Accept",
            Header::AcceptLanguage => "Accept-Language",
            Header::ContentType => "Content-Type",
            Header::IfNoneMatch => "If-None-Match",
            Header::IfModifiedSince => "If-Modified-Since",
            Header::Range => "Range",
            Header::CacheControl => "Cache-Control",
        }
    }
}

/// Where a slot's value goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// `Authorization: Bearer <value>`.
    Bearer = 1,
    /// Wherever body segments name it.
    Body = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Raw = 1,
    /// application/x-www-form-urlencoded.
    Form = 2,
    /// Inside a JSON string (without the quotes).
    JsonString = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    /// arcad::proto::courier_op::SOURCE_ITEM or SOURCE_HANDLE.
    pub source: u8,
    pub reference: [u8; 16],
    pub place: Place,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Literal(Vec<u8>),
    Slot { slot: u8, encoding: Encoding },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: Method,
    pub path: Vec<u8>,
    pub query: Vec<u8>,
    pub headers: Vec<(Header, Vec<u8>)>,
    pub slots: Vec<Slot>,
    pub body: Vec<Segment>,
    /// A web courier's ticket from the broker, for this request; an app's
    /// courier sends none.
    pub ticket: Option<[u8; 16]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Malformed {
    Truncated,
    Trailing,
    Version,
    Method,
    Path,
    Query,
    Header,
    Slot,
    Body,
    Ticket,
}

/// Visible ASCII, no '#'; the path also no '?'.
fn uri_bytes(b: &[u8], path: bool) -> bool {
    b.iter().all(|&c| (0x21..=0x7e).contains(&c) && c != b'#' && !(path && c == b'?'))
}

fn header_value(b: &[u8]) -> bool {
    !b.is_empty() && b.iter().all(|&c| (0x20..=0x7e).contains(&c)) && b[0] != b' ' && b[b.len() - 1] != b' '
}

impl Request {
    pub fn decode(b: &[u8]) -> Result<Request, Malformed> {
        use Malformed::*;
        let mut r = Reader::new(b);
        let t = |_| Truncated;
        if r.u8().map_err(t)? != VERSION {
            return Err(Version);
        }
        let method = self::Method::from_u8(r.u8().map_err(t)?).ok_or(Malformed::Method)?;
        let path = r.bytes(PATH_MAX).map_err(|_| Path)?.to_vec();
        if path.first() != Some(&b'/') || !uri_bytes(&path, true) {
            return Err(Path);
        }
        let query = r.bytes(QUERY_MAX).map_err(|_| Query)?.to_vec();
        if !uri_bytes(&query, false) {
            return Err(Query);
        }
        let n = r.u8().map_err(t)? as usize;
        if n > HEADERS_MAX {
            return Err(Header);
        }
        let mut headers: Vec<(self::Header, Vec<u8>)> = Vec::with_capacity(n);
        for _ in 0..n {
            let h = self::Header::from_u8(r.u8().map_err(t)?).ok_or(Header)?;
            let v = r.bytes(HEADER_VALUE_MAX).map_err(|_| Header)?;
            if !header_value(v) || headers.iter().any(|(k, _)| *k == h) {
                return Err(Header);
            }
            headers.push((h, v.to_vec()));
        }
        let n = r.u8().map_err(t)? as usize;
        if n > SLOTS_MAX {
            return Err(Slot);
        }
        let mut slots = Vec::with_capacity(n);
        for _ in 0..n {
            let source = r.u8().map_err(t)?;
            let reference = r.array::<16>().map_err(t)?;
            let place = match r.u8().map_err(t)? {
                1 => Place::Bearer,
                2 => Place::Body,
                _ => return Err(Slot),
            };
            use arcad::proto::courier_op::{SOURCE_HANDLE, SOURCE_ITEM};
            if source != SOURCE_ITEM && source != SOURCE_HANDLE {
                return Err(Slot);
            }
            slots.push(self::Slot { source, reference, place });
        }
        if slots.iter().filter(|s| s.place == Place::Bearer).count() > 1 {
            return Err(Slot);
        }
        let n = r.u16().map_err(t)? as usize;
        if n > SEGMENTS_MAX || (n > 0 && !method.has_body()) {
            return Err(Body);
        }
        let mut body = Vec::with_capacity(n);
        let mut total = 0usize;
        for _ in 0..n {
            match r.u8().map_err(t)? {
                0 => {
                    let l = r.bytes(BODY_MAX).map_err(|_| Body)?;
                    total += l.len();
                    if total > BODY_MAX {
                        return Err(Body);
                    }
                    body.push(Segment::Literal(l.to_vec()));
                }
                1 => {
                    let slot = r.u8().map_err(t)?;
                    let encoding = match r.u8().map_err(t)? {
                        1 => Encoding::Raw,
                        2 => Encoding::Form,
                        3 => Encoding::JsonString,
                        _ => return Err(Body),
                    };
                    if slots.get(slot as usize).map(|s| s.place) != Some(Place::Body) {
                        return Err(Body);
                    }
                    body.push(Segment::Slot { slot, encoding });
                }
                _ => return Err(Body),
            }
        }
        // Every body slot is used: a slot fetched for nothing is a read the
        // request had no need for.
        for (i, s) in slots.iter().enumerate() {
            if s.place == Place::Body && !body.iter().any(|g| matches!(g, Segment::Slot { slot, .. } if *slot as usize == i)) {
                return Err(Slot);
            }
        }
        let ticket = match r.u8().map_err(t)? {
            0 => None,
            1 => Some(r.array::<16>().map_err(t)?),
            _ => return Err(Ticket),
        };
        r.end().map_err(|_| Trailing)?;
        Ok(Request { method, path, query, headers, slots, body, ticket })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.u8(VERSION);
        w.u8(self.method as u8);
        w.bytes(&self.path);
        w.bytes(&self.query);
        w.u8(self.headers.len() as u8);
        for (h, v) in &self.headers {
            w.u8(*h as u8);
            w.bytes(v);
        }
        w.u8(self.slots.len() as u8);
        for s in &self.slots {
            w.u8(s.source);
            w.raw(&s.reference);
            w.u8(s.place as u8);
        }
        w.u16(self.body.len() as u16);
        for g in &self.body {
            match g {
                Segment::Literal(b) => {
                    w.u8(0);
                    w.bytes(b);
                }
                Segment::Slot { slot, encoding } => {
                    w.u8(1);
                    w.u8(*slot);
                    w.u8(*encoding as u8);
                }
            }
        }
        match &self.ticket {
            None => w.u8(0),
            Some(t) => {
                w.u8(1);
                w.raw(t);
            }
        }
        w.buf
    }
}

/// Why the courier sent nothing (or stopped).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Malformed = 1,
    /// Resolution, the address policy, or the TCP connect.
    Connect = 2,
    Tls = 3,
    /// Arca refused a slot: not this endpoint's, not this app's, unknown.
    Denied = 4,
    Locked = 5,
    /// The server's response was out of bounds or not HTTP.
    Http = 6,
    /// A slot value that can't go where it was placed (a control byte in a
    /// header value).
    Unsendable = 7,
    Internal = 8,
    /// A verify-each-use slot whose check the user refused or didn't
    /// finish (arca.md §6.2): nothing was sent.
    NotVerified = 9,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    Http { status: u16, headers: Vec<(String, String)>, body: Vec<u8> },
    Refused(Refusal),
}

impl Response {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        match self {
            Response::Http { status, headers, body } => {
                w.u8(0);
                w.u16(*status);
                w.u8(headers.len().min(255) as u8);
                for (k, v) in headers.iter().take(255) {
                    w.str(k);
                    w.str(v);
                }
                w.bytes(body);
            }
            Response::Refused(r) => {
                w.u8(1);
                w.u8(*r as u8);
            }
        }
        w.buf
    }

    pub fn decode(b: &[u8]) -> Option<Response> {
        let mut r = Reader::new(b);
        let out = match r.u8().ok()? {
            0 => {
                let status = r.u16().ok()?;
                let n = r.u8().ok()? as usize;
                let mut headers = Vec::with_capacity(n);
                for _ in 0..n {
                    let k = r.str(64 * 1024).ok()?.to_string();
                    let v = r.str(64 * 1024).ok()?.to_string();
                    headers.push((k, v));
                }
                let body = r.bytes(navigator_fetch::http::MAX_BODY_BYTES).ok()?.to_vec();
                Response::Http { status, headers, body }
            }
            1 => Response::Refused(match r.u8().ok()? {
                1 => Refusal::Malformed,
                2 => Refusal::Connect,
                3 => Refusal::Tls,
                4 => Refusal::Denied,
                5 => Refusal::Locked,
                6 => Refusal::Http,
                7 => Refusal::Unsendable,
                9 => Refusal::NotVerified,
                _ => Refusal::Internal,
            }),
            _ => return None,
        };
        r.end().ok()?;
        Some(out)
    }
}
