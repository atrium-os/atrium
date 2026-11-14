//! A small HTTP/1.1 client for the WebDAV share server (S2): one request at
//! a time on one kept-alive connection, a fresh connection whenever the
//! server closes one. Bodies are bounded by the caller, headers by
//! `HEADERS_MAX`; nothing here follows redirects or speaks anything but
//! HTTP/1.1 (the share server talks to one server, at one address).

use std::io::{self, BufRead, BufReader, Read, Write};

use zeroize::Zeroizing;

/// The most header bytes a response may carry.
pub const HEADERS_MAX: usize = 64 * 1024;
/// The most headers.
pub const HEADER_COUNT_MAX: usize = 100;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// The response broke the protocol, or a limit.
    Protocol(&'static str),
    /// The body is longer than the caller allows.
    TooBig,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Protocol(m) => write!(f, "HTTP: {m}"),
            Error::TooBig => write!(f, "HTTP: the answer is too big"),
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

/// Opens a new connection to the one server (TLS included).
pub trait Dial {
    type Stream: Read + Write;
    fn dial(&mut self) -> io::Result<Self::Stream>;
}

#[derive(Debug, Clone, Default)]
pub struct Response {
    pub status: u16,
    /// Lowercased names.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

pub struct Request<'a> {
    pub method: &'a str,
    /// Already percent-encoded, starting with `/`.
    pub path: &'a str,
    pub headers: Vec<(&'a str, String)>,
    pub body: &'a [u8],
    /// The longest body accepted in answer.
    pub limit: usize,
}

pub struct Client<D: Dial> {
    dial: D,
    host: String,
    conn: Option<BufReader<D::Stream>>,
    /// `Basic ...`, kept for every request (HTTP has no login).
    auth: Option<Zeroizing<String>>,
}

/// One token-safe header value: no CR, LF or NUL.
fn clean(v: &str) -> bool {
    !v.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0)
}

impl<D: Dial> Client<D> {
    /// `host` is the Host header (with `:port` when not the default).
    pub fn new(dial: D, host: &str) -> Self {
        Client { dial, host: host.to_string(), conn: None, auth: None }
    }

    pub fn set_basic(&mut self, user: &str, password: &[u8]) {
        let mut raw = Zeroizing::new(Vec::with_capacity(user.len() + 1 + password.len()));
        raw.extend_from_slice(user.as_bytes());
        raw.push(b':');
        raw.extend_from_slice(password);
        let mut v = Zeroizing::new(String::from("Basic "));
        base64_into(&raw, &mut v);
        self.auth = Some(v);
    }

    pub fn has_auth(&self) -> bool {
        self.auth.is_some()
    }

    /// Send `r`; a connection the server had closed is replaced once.
    pub fn send(&mut self, r: &Request<'_>) -> Result<Response, Error> {
        let fresh = self.conn.is_none();
        match self.once(r) {
            Ok(resp) => Ok(resp),
            // A kept-alive connection the server closed meanwhile: try
            // again on a new one (only when nothing of the answer came).
            Err(Error::Io(e)) if !fresh && matches!(e.kind(), io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionAborted) => {
                self.conn = None;
                let again = self.once(r);
                if again.is_err() {
                    self.conn = None;
                }
                again
            }
            Err(e) => {
                self.conn = None;
                Err(e)
            }
        }
    }

    fn once(&mut self, r: &Request<'_>) -> Result<Response, Error> {
        if !r.path.starts_with('/') || !clean(r.path) || r.path.contains(' ') || !r.method.bytes().all(|b| b.is_ascii_uppercase()) {
            return Err(Error::Protocol("bad request line"));
        }
        if r.headers.iter().any(|(k, v)| !clean(k) || !clean(v) || k.contains(':')) {
            return Err(Error::Protocol("bad header"));
        }
        if self.conn.is_none() {
            self.conn = Some(BufReader::with_capacity(64 * 1024, self.dial.dial()?));
        }
        let mut head = Zeroizing::new(String::with_capacity(256));
        head.push_str(r.method);
        head.push(' ');
        head.push_str(r.path);
        head.push_str(" HTTP/1.1\r\nHost: ");
        head.push_str(&self.host);
        head.push_str("\r\nUser-Agent: Atrium\r\n");
        if let Some(a) = &self.auth {
            head.push_str("Authorization: ");
            head.push_str(a);
            head.push_str("\r\n");
        }
        for (k, v) in &r.headers {
            if !clean(k) || !clean(v) || k.contains(':') {
                return Err(Error::Protocol("bad header"));
            }
            head.push_str(k);
            head.push_str(": ");
            head.push_str(v);
            head.push_str("\r\n");
        }
        if !r.body.is_empty() || matches!(r.method, "PUT" | "POST" | "PROPFIND" | "PROPPATCH") {
            head.push_str(&format!("Content-Length: {}\r\n", r.body.len()));
        }
        head.push_str("\r\n");
        let conn = self.conn.as_mut().unwrap();
        {
            let w = conn.get_mut();
            w.write_all(head.as_bytes())?;
            w.write_all(r.body)?;
            w.flush()?;
        }
        let resp = read_response(conn, r.method == "HEAD", r.limit)?;
        let close = resp.header("connection").is_some_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case("close")));
        if close || resp.header("content-length").is_none() && resp.header("transfer-encoding").is_none() && has_body(resp.status, r.method == "HEAD") {
            self.conn = None;
        }
        Ok(resp)
    }
}

impl<D: Dial> Client<D> {
    /// A PUT whose body is sent as it comes (chunked), on a connection of
    /// its own so that other requests can go on meanwhile.
    pub fn upload(&mut self, path: &str, headers: &[(&str, String)]) -> Result<Upload<D::Stream>, Error> {
        if !path.starts_with('/') || !clean(path) || path.contains(' ') {
            return Err(Error::Protocol("bad request line"));
        }
        let mut s = self.dial.dial()?;
        let mut head = Zeroizing::new(format!("PUT {path} HTTP/1.1\r\nHost: {}\r\nUser-Agent: Atrium\r\n", self.host));
        if let Some(a) = &self.auth {
            head.push_str("Authorization: ");
            head.push_str(a);
            head.push_str("\r\n");
        }
        for (k, v) in headers {
            if !clean(k) || !clean(v) || k.contains(':') {
                return Err(Error::Protocol("bad header"));
            }
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
        s.write_all(head.as_bytes())?;
        Ok(Upload { s })
    }
}

/// A PUT in progress: `chunk` the body, then `finish` for the answer.
pub struct Upload<S: Read + Write> {
    s: S,
}

impl<S: Read + Write> Upload<S> {
    pub fn chunk(&mut self, data: &[u8]) -> Result<(), Error> {
        if data.is_empty() {
            return Ok(());
        }
        self.s.write_all(format!("{:x}\r\n", data.len()).as_bytes())?;
        self.s.write_all(data)?;
        self.s.write_all(b"\r\n")?;
        Ok(())
    }

    pub fn finish(mut self) -> Result<Response, Error> {
        self.s.write_all(b"0\r\n\r\n")?;
        self.s.flush()?;
        read_response(&mut BufReader::new(self.s), false, 64 * 1024)
    }
}

fn has_body(status: u16, head: bool) -> bool {
    !(head || status == 204 || status == 304 || (100..200).contains(&status))
}

fn line<R: BufRead>(r: &mut R, budget: &mut usize) -> Result<String, Error> {
    let mut v = Vec::new();
    loop {
        let buf = r.fill_buf()?;
        if buf.is_empty() {
            return Err(Error::Io(io::ErrorKind::UnexpectedEof.into()));
        }
        let (take, done) = match buf.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (buf.len(), false),
        };
        if take > *budget {
            return Err(Error::Protocol("headers too long"));
        }
        *budget -= take;
        v.extend_from_slice(&buf[..take]);
        r.consume(take);
        if done {
            break;
        }
    }
    while v.last().is_some_and(|&b| b == b'\n' || b == b'\r') {
        v.pop();
    }
    String::from_utf8(v).map_err(|_| Error::Protocol("header not UTF-8"))
}

/// Read one response from `r`.
pub fn read_response<R: BufRead>(r: &mut R, head: bool, limit: usize) -> Result<Response, Error> {
    let mut budget = HEADERS_MAX;
    let mut resp;
    // 1xx answers come before the real one.
    loop {
        let status_line = line(r, &mut budget)?;
        let mut it = status_line.splitn(3, ' ');
        let ver = it.next().unwrap_or("");
        if ver != "HTTP/1.1" && ver != "HTTP/1.0" {
            return Err(Error::Protocol("not HTTP/1.x"));
        }
        let status: u16 = it.next().and_then(|s| (s.len() == 3).then(|| s.parse().ok()).flatten()).ok_or(Error::Protocol("bad status"))?;
        resp = Response { status, ..Default::default() };
        loop {
            let l = line(r, &mut budget)?;
            if l.is_empty() {
                break;
            }
            if resp.headers.len() >= HEADER_COUNT_MAX {
                return Err(Error::Protocol("too many headers"));
            }
            let (k, v) = l.split_once(':').ok_or(Error::Protocol("bad header"))?;
            if k.is_empty() || k.bytes().any(|b| b <= b' ' || b >= 0x7f) {
                return Err(Error::Protocol("bad header name"));
            }
            resp.headers.push((k.to_ascii_lowercase(), v.trim().to_string()));
        }
        if !(100..200).contains(&status) {
            break;
        }
    }
    if !has_body(resp.status, head) {
        return Ok(resp);
    }
    let chunked = resp.header("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked"));
    if chunked {
        loop {
            let l = line(r, &mut budget)?;
            let size = usize::from_str_radix(l.split(';').next().unwrap_or("").trim(), 16).map_err(|_| Error::Protocol("bad chunk size"))?;
            if size == 0 {
                // Trailers, then the empty line.
                while !line(r, &mut budget)?.is_empty() {}
                break;
            }
            if resp.body.len() + size > limit {
                return Err(Error::TooBig);
            }
            let start = resp.body.len();
            resp.body.resize(start + size, 0);
            r.read_exact(&mut resp.body[start..])?;
            if !line(r, &mut budget)?.is_empty() {
                return Err(Error::Protocol("bad chunk end"));
            }
        }
    } else if resp.header("transfer-encoding").is_some() {
        return Err(Error::Protocol("unknown transfer encoding"));
    } else if let Some(n) = resp.header("content-length") {
        let n: usize = n.parse().map_err(|_| Error::Protocol("bad content length"))?;
        if n > limit {
            return Err(Error::TooBig);
        }
        resp.body.resize(n, 0);
        r.read_exact(&mut resp.body)?;
    } else {
        // Delimited by the close.
        let mut take = r.take(limit as u64 + 1);
        take.read_to_end(&mut resp.body)?;
        if resp.body.len() > limit {
            return Err(Error::TooBig);
        }
    }
    Ok(resp)
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_into(data: &[u8], out: &mut String) {
    for c in data.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
}

/// Percent-encode one path segment (RFC 3986 unreserved kept).
pub fn encode_segment(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

/// Decode `%XX`; None for a broken escape or a result that isn't UTF-8.
pub fn decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut o = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            o.push(u8::from_str_radix(h, 16).ok()?);
            i += 3;
        } else {
            o.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(o).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resp(s: &str, limit: usize) -> Result<Response, Error> {
        read_response(&mut io::Cursor::new(s.as_bytes().to_vec()), false, limit)
    }

    #[test]
    fn bodies() {
        let r = resp("HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello", 10).unwrap();
        assert_eq!((r.status, r.body.as_slice()), (200, &b"hello"[..]));
        let r = resp("HTTP/1.1 207 Multi-Status\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;x=y\r\nde\r\n0\r\nT: 1\r\n\r\n", 10).unwrap();
        assert_eq!(r.body, b"abcde");
        let r = resp("HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 204 No Content\r\n\r\n", 10).unwrap();
        assert_eq!(r.status, 204);
        let r = resp("HTTP/1.0 200 OK\r\n\r\nto the end", 20).unwrap();
        assert_eq!(r.body, b"to the end");
    }

    #[test]
    fn limits_and_breakage() {
        assert!(matches!(resp("HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\nhello world", 10), Err(Error::TooBig)));
        assert!(matches!(resp("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nb\r\nhello world\r\n0\r\n\r\n", 10), Err(Error::TooBig)));
        assert!(matches!(resp("HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\n\r\n", 10), Err(Error::Protocol(_))));
        assert!(matches!(resp("HTTP/2 200 OK\r\n\r\n", 10), Err(Error::Protocol(_))));
        assert!(matches!(resp("HTTP/1.1 2000 OK\r\n\r\n", 10), Err(Error::Protocol(_))));
        assert!(matches!(resp("HTTP/1.1 200 OK\r\nBad Name: x\r\n\r\n", 10), Err(Error::Protocol(_))));
        let long = format!("HTTP/1.1 200 OK\r\nX: {}\r\n\r\n", "a".repeat(HEADERS_MAX));
        assert!(matches!(resp(&long, 10), Err(Error::Protocol(_))));
        assert!(matches!(resp("HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhel", 10), Err(Error::Io(_))));
    }

    #[test]
    fn segments() {
        assert_eq!(encode_segment("My Files/ä"), "My%20Files%2F%C3%A4");
        assert_eq!(decode("My%20Files%2F%C3%A4").as_deref(), Some("My Files/ä"));
        assert_eq!(decode("%zz"), None);
        assert_eq!(decode("%C3"), None);
        let mut s = String::new();
        base64_into(b"alice:pw", &mut s);
        assert_eq!(s, "YWxpY2U6cHc=");
    }

    /// A scripted server: each dial gets the next canned conversation.
    struct Script(Vec<Vec<u8>>, std::rc::Rc<std::cell::RefCell<Vec<u8>>>);
    struct Pipe(io::Cursor<Vec<u8>>, std::rc::Rc<std::cell::RefCell<Vec<u8>>>);
    impl Read for Pipe {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            self.0.read(b)
        }
    }
    impl Write for Pipe {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.1.borrow_mut().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl Dial for Script {
        type Stream = Pipe;
        fn dial(&mut self) -> io::Result<Pipe> {
            if self.0.is_empty() {
                return Err(io::ErrorKind::ConnectionRefused.into());
            }
            Ok(Pipe(io::Cursor::new(self.0.remove(0)), self.1.clone()))
        }
    }

    #[test]
    fn a_closed_keepalive_is_redialled_once() {
        let sent = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let first = b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\na".to_vec();
        let second = b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\nb".to_vec();
        let mut c = Client::new(Script(vec![first, second], sent.clone()), "nas.lan");
        c.set_basic("alice", b"pw");
        let get = Request { method: "GET", path: "/a", headers: vec![], body: b"", limit: 10 };
        assert_eq!(c.send(&get).unwrap().body, b"a");
        // The first connection has nothing more: EOF, so a new one.
        assert_eq!(c.send(&get).unwrap().body, b"b");
        assert!(c.send(&get).is_err(), "no third connection to be had");
        let s = String::from_utf8(sent.borrow().clone()).unwrap();
        assert!(s.starts_with("GET /a HTTP/1.1\r\nHost: nas.lan\r\nUser-Agent: Atrium\r\nAuthorization: Basic YWxpY2U6cHc=\r\n\r\n"));
        let bad = Request { method: "GET", path: "/a\r\nX: y", headers: vec![], body: b"", limit: 10 };
        assert!(matches!(c.send(&bad), Err(Error::Protocol(_))));
        let bad = Request { method: "GET", path: "/a", headers: vec![("X", "a\nb".into())], body: b"", limit: 10 };
        assert!(matches!(c.send(&bad), Err(Error::Protocol(_))));
    }
}
