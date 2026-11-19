//! Just enough HTTP/1.1 for a gateway (vpn.md §9.2): requests we write,
//! and response heads and bodies read with fixed caps. Bodies by
//! `Content-Length` or chunked; nothing else (no compression asked for).

/// The largest response head.
pub const MAX_HEAD: usize = 16 * 1024;
pub const MAX_HEADERS: usize = 96;
/// The largest body read (a config-auth document and a little).
pub const MAX_BODY: usize = crate::xml::MAX_DOC;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Head {
    pub status: u16,
    /// Names lower-cased, values trimmed, in order (repeats kept).
    pub headers: Vec<(String, String)>,
}

impl Head {
    pub fn get(&self, k: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
    }

    pub fn all<'a>(&'a self, k: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.headers.iter().filter(move |(n, _)| n == k).map(|(_, v)| v.as_str())
    }

    /// A cookie the response sets, by name (`Set-Cookie: name=value; ...`).
    /// An empty value or `deleted` means the gateway cleared it.
    pub fn set_cookie(&self, name: &str) -> Option<&str> {
        self.all("set-cookie").filter_map(|c| c.split(';').next()).filter_map(|kv| kv.trim().split_once('=')).find(|(k, _)| *k == name).map(|(_, v)| v)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    TooLarge,
    Syntax(&'static str),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::TooLarge => f.write_str("response too large"),
            Error::Syntax(s) => write!(f, "HTTP: {s}"),
        }
    }
}

/// A request head: `METHOD path`, `Host`, our fixed headers, then `extra`.
pub fn request(method: &str, path: &str, host: &str, extra: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut s = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: {}\r\nAccept: */*\r\nAccept-Encoding: identity\r\n", crate::USER_AGENT);
    for (k, v) in extra {
        s.push_str(k);
        s.push_str(": ");
        s.push_str(v);
        s.push_str("\r\n");
    }
    if !body.is_empty() || method == "POST" {
        s.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    s.push_str("\r\n");
    let mut b = s.into_bytes();
    b.extend_from_slice(body);
    b
}

/// The head, if `buf` holds all of it: it and the bytes it took.
pub fn head(buf: &[u8]) -> Result<Option<(Head, usize)>, Error> {
    let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        return if buf.len() > MAX_HEAD { Err(Error::TooLarge) } else { Ok(None) };
    };
    if end + 4 > MAX_HEAD {
        return Err(Error::TooLarge);
    }
    let text = std::str::from_utf8(&buf[..end]).map_err(|_| Error::Syntax("not text"))?;
    let mut lines = text.split("\r\n");
    let status = lines.next().ok_or(Error::Syntax("no status"))?;
    let mut sp = status.splitn(3, ' ');
    let v = sp.next().unwrap_or("");
    if v != "HTTP/1.1" && v != "HTTP/1.0" {
        return Err(Error::Syntax("not HTTP/1.x"));
    }
    let code = sp.next().filter(|c| c.len() == 3).and_then(|c| c.parse::<u16>().ok()).ok_or(Error::Syntax("bad status"))?;
    let mut headers = Vec::new();
    for l in lines {
        if headers.len() >= MAX_HEADERS {
            return Err(Error::TooLarge);
        }
        let (k, v) = l.split_once(':').ok_or(Error::Syntax("bad header"))?;
        if k.is_empty() || k.bytes().any(|c| !(c.is_ascii_alphanumeric() || c == b'-' || c == b'_')) {
            return Err(Error::Syntax("bad header name"));
        }
        if v.bytes().any(|c| c < 0x20 && c != b'\t') {
            return Err(Error::Syntax("control byte in header"));
        }
        headers.push((k.to_ascii_lowercase(), v.trim().to_string()));
    }
    Ok(Some((Head { status: code, headers }, end + 4)))
}

/// The body after `h`, if `buf` (what followed the head) holds all of it:
/// the body and the bytes it took. A response with neither length nor
/// chunking has none (we never read to close).
pub fn body(h: &Head, buf: &[u8]) -> Result<Option<(Vec<u8>, usize)>, Error> {
    if h.get("transfer-encoding").is_some_and(|t| t.eq_ignore_ascii_case("chunked")) {
        let mut out = Vec::new();
        let mut i = 0;
        loop {
            let Some(e) = buf[i..].windows(2).position(|w| w == b"\r\n") else {
                return if buf.len() - i > 32 { Err(Error::Syntax("bad chunk size")) } else { Ok(None) };
            };
            let line = std::str::from_utf8(&buf[i..i + e]).map_err(|_| Error::Syntax("bad chunk size"))?;
            let hex = line.split(';').next().unwrap_or("").trim();
            if hex.is_empty() || hex.len() > 8 {
                return Err(Error::Syntax("bad chunk size"));
            }
            let n = usize::from_str_radix(hex, 16).map_err(|_| Error::Syntax("bad chunk size"))?;
            if out.len() + n > MAX_BODY {
                return Err(Error::TooLarge);
            }
            let start = i + e + 2;
            if n == 0 {
                // No trailers accepted: the final CRLF.
                return match buf.get(start..start + 2) {
                    None => Ok(None),
                    Some(b"\r\n") => Ok(Some((out, start + 2))),
                    Some(_) => Err(Error::Syntax("trailers")),
                };
            }
            if buf.len() < start + n + 2 {
                return Ok(None);
            }
            out.extend_from_slice(&buf[start..start + n]);
            if &buf[start + n..start + n + 2] != b"\r\n" {
                return Err(Error::Syntax("bad chunk end"));
            }
            i = start + n + 2;
        }
    }
    let n = match h.get("content-length") {
        Some(v) => v.parse::<usize>().map_err(|_| Error::Syntax("bad length"))?,
        None => 0,
    };
    if n > MAX_BODY {
        return Err(Error::TooLarge);
    }
    if buf.len() < n {
        return Ok(None);
    }
    Ok(Some((buf[..n].to_vec(), n)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heads_and_bodies() {
        let r = b"HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nSet-Cookie: webvpn=abc; Secure\r\nSet-Cookie: webvpnc=; expires=x\r\nContent-Length: 5\r\n\r\nhello+more";
        let (h, n) = head(r).unwrap().unwrap();
        assert_eq!(h.status, 200);
        assert_eq!(h.set_cookie("webvpn"), Some("abc"));
        assert_eq!(h.set_cookie("webvpnc"), Some(""));
        let (b, m) = body(&h, &r[n..]).unwrap().unwrap();
        assert_eq!((b.as_slice(), m), (&b"hello"[..], 5));
        assert_eq!(head(b"HTTP/1.1 200 OK\r\n").unwrap(), None);
        let c = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;x\r\nde\r\n0\r\n\r\n";
        let (h, n) = head(c).unwrap().unwrap();
        assert_eq!(body(&h, &c[n..]).unwrap().unwrap().0, b"abcde");
        assert_eq!(body(&h, &c[n..c.len() - 1]).unwrap(), None);
    }

    #[test]
    fn refuses() {
        assert!(head(b"FOO 200 OK\r\n\r\n").is_err());
        assert!(head(b"HTTP/1.1 2000 OK\r\n\r\n").is_err());
        assert!(head(b"HTTP/1.1 200 OK\r\nBad Header: x\r\n\r\n").is_err());
        assert!(head(&vec![b'a'; MAX_HEAD + 1]).is_err());
        let h = Head { status: 200, headers: vec![("content-length".into(), (MAX_BODY + 1).to_string())] };
        assert_eq!(body(&h, b""), Err(Error::TooLarge));
        let h = Head { status: 200, headers: vec![("transfer-encoding".into(), "chunked".into())] };
        assert!(body(&h, b"zz\r\n").is_err());
        assert!(body(&h, b"ffffffff\r\n").is_err());
    }
}
