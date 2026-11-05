//! A bounded HTTP/1.1 GET — request writer and response reader.
//!
//! ★ BOUNDED BEFORE IT IS BELIEVED. The peer is untrusted: every length it
//! states is checked against a limit before a byte is allocated, a header
//! section that never ends is a refusal after `MAX_HEADER_BYTES`, and a body
//! is read to the limit and no further whatever the peer claims.

use std::io::{self, Read, Write};

pub const MAX_HEADER_BYTES: usize = 64 * 1024;
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub status: u16,
    /// Header names lowercased; order preserved.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, PartialEq)]
pub enum HttpError {
    Io(String),
    HeaderTooLarge,
    BodyTooLarge,
    Malformed(&'static str),
    /// We asked for `identity`; a compressed body is refused, not decoded —
    /// a decompressor is one more parser of hostile input for no gain here.
    Encoded(String),
}

impl From<io::Error> for HttpError {
    fn from(e: io::Error) -> Self { HttpError::Io(e.to_string()) }
}

/// The request. ★ What is NOT sent is the point: no Cookie, no Referer, no
/// Authorization, no client hints — the origin learns the resource was
/// fetched and nothing about by whom or from where.
pub fn write_request(w: &mut impl Write, host_header: &str, path_and_query: &str) -> io::Result<()> {
    write_request_conn(w, host_header, path_and_query, false)
}

/// `write_request`, asking to keep the connection open (`keep_alive`) for
/// the next request to the same origin (legacy-web §5.4.5.7, step 4).
pub fn write_request_conn(w: &mut impl Write, host_header: &str, path_and_query: &str, keep_alive: bool) -> io::Result<()> {
    write_request_range(w, host_header, path_and_query, keep_alive, None)
}

/// `write_request_conn`, asking for bytes `start .. start + len` of the
/// resource only (`Range`, RFC 9110 §14), when `range` is given: an image's
/// header (legacy-web §5.4.5.7, cut A). A server may ignore it and send the
/// whole body; `read_response_capped` reads no more than it was asked for
/// either way.
pub fn write_request_range(w: &mut impl Write, host_header: &str, path_and_query: &str, keep_alive: bool,
                           range: Option<(u64, u64)>) -> io::Result<()> {
    let conn = if keep_alive { "keep-alive" } else { "close" };
    let range = match range { Some((s, n)) if n > 0 => format!("Range: bytes={s}-{}\r\n", s + n - 1), _ => String::new() };
    write!(w, "GET {path_and_query} HTTP/1.1\r\nHost: {host_header}\r\n\
               User-Agent: atrium-navigator-fetch/0.1\r\nAccept: */*\r\n\
               Accept-Encoding: identity\r\n{range}Connection: {conn}\r\n\r\n")?;
    w.flush()
}

fn read_until_headers_end(r: &mut impl Read) -> Result<(Vec<u8>, Vec<u8>), HttpError> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let rest = buf.split_off(i + 4);
            return Ok((buf, rest));
        }
        if buf.len() > MAX_HEADER_BYTES { return Err(HttpError::HeaderTooLarge) }
        let n = r.read(&mut chunk)?;
        if n == 0 { return Err(HttpError::Malformed("connection closed inside headers")) }
        buf.extend_from_slice(&chunk[..n]);
    }
}

pub fn read_response(r: &mut impl Read) -> Result<Response, HttpError> {
    read_response_framed(r).map(|(resp, _)| resp)
}

/// `read_response`, and whether the connection may carry another request:
/// ★ only when the body's end was DELIMITED (a length, or chunked with its
/// trailers read), nothing past it was read, the server did not say
/// `close`, and it speaks HTTP/1.1. Anything else is read to its end and
/// the connection is not reused — a byte left over would become the start
/// of the next response.
pub fn read_response_framed(r: &mut impl Read) -> Result<(Response, bool), HttpError> {
    read_response_capped(r, None)
}

/// `read_response_framed`, reading at most `cap` bytes of the body when a
/// cap is given: what a header request needs, however much the server
/// sends (it may ignore `Range`). A body cut at the cap ends the
/// connection's reuse — the rest of it is still on the wire.
pub fn read_response_capped(r: &mut impl Read, cap: Option<usize>) -> Result<(Response, bool), HttpError> {
    let (head, mut rest) = read_until_headers_end(r)?;
    let head = std::str::from_utf8(&head).map_err(|_| HttpError::Malformed("non-UTF-8 header"))?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().ok_or(HttpError::Malformed("no status line"))?;
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/1.") { return Err(HttpError::Malformed("not HTTP/1.x")) }
    let status: u16 = parts.next().and_then(|s| s.parse().ok()).ok_or(HttpError::Malformed("bad status"))?;
    let mut headers = vec![];
    for l in lines.filter(|l| !l.is_empty()) {
        let (k, v) = l.split_once(':').ok_or(HttpError::Malformed("header without colon"))?;
        headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
    }
    let resp_hdr = |n: &str| headers.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
    if let Some(enc) = resp_hdr("content-encoding") {
        if !enc.eq_ignore_ascii_case("identity") { return Err(HttpError::Encoded(enc)) }
    }
    let chunked = resp_hdr("transfer-encoding").is_some_and(|t| t.to_ascii_lowercase().contains("chunked"));
    let keep = version == "HTTP/1.1"
        && !resp_hdr("connection").is_some_and(|c| c.to_ascii_lowercase().split(',').any(|t| t.trim() == "close"));
    // A response that has no body by its status (RFC 9112 §6.3): reading one
    // to the end of the connection would wait out the timeout on a
    // connection the server is keeping open.
    let (body, delimited) = if (100..200).contains(&status) || status == 204 || status == 304 {
        (vec![], rest.is_empty())
    } else if chunked {
        let (b, whole) = read_chunked(&mut rest, r, cap.unwrap_or(MAX_BODY_BYTES))?;
        if !whole { return Ok((Response { status, headers, body: b }, false)) }
        let clean = read_trailers(&mut rest, r)? && rest.is_empty();
        (b, clean)
    } else if let Some(cl) = resp_hdr("content-length") {
        let n: usize = cl.parse().map_err(|_| HttpError::Malformed("bad content-length"))?;
        match cap {
            Some(c) if n > c => {
                // Only the asked-for bytes: the stated length is not read.
                fill(&mut rest, r, c)?;
                rest.truncate(c);
                return Ok((Response { status, headers, body: std::mem::take(&mut rest) }, false));
            }
            _ => {}
        }
        if n > MAX_BODY_BYTES { return Err(HttpError::BodyTooLarge) }
        fill(&mut rest, r, n)?;
        let clean = rest.len() == n;
        rest.truncate(n);
        (std::mem::take(&mut rest), clean)
    } else {
        match cap {
            Some(c) => { let b = read_to_cap(&mut rest, r, c)?; return Ok((Response { status, headers, body: b }, false)) }
            None => (read_to_limit(&mut rest, r)?, false),
        }
    };
    Ok((Response { status, headers, body }, keep && delimited))
}

/// After a chunked body's last chunk: the trailer section, up to its empty
/// line. False if it could not be read cleanly.
fn read_trailers(rest: &mut Vec<u8>, r: &mut impl Read) -> Result<bool, HttpError> {
    for _ in 0..64 {
        let end = loop {
            if let Some(i) = rest.windows(2).position(|w| w == b"\r\n") { break i }
            if rest.len() > 8 * 1024 { return Ok(false) }
            let want = rest.len() + 1;
            if fill(rest, r, want).is_err() { return Ok(false) }
        };
        let empty = end == 0;
        rest.drain(..end + 2);
        if empty { return Ok(true) }
    }
    Ok(false)
}

fn fill(rest: &mut Vec<u8>, r: &mut impl Read, want: usize) -> Result<(), HttpError> {
    let mut chunk = [0u8; 16 * 1024];
    while rest.len() < want {
        let n = r.read(&mut chunk)?;
        if n == 0 { return Err(HttpError::Malformed("connection closed inside body")) }
        rest.extend_from_slice(&chunk[..n]);
        if rest.len() > MAX_BODY_BYTES + MAX_HEADER_BYTES { return Err(HttpError::BodyTooLarge) }
    }
    Ok(())
}

fn read_to_limit(rest: &mut Vec<u8>, r: &mut impl Read) -> Result<Vec<u8>, HttpError> {
    let mut chunk = [0u8; 16 * 1024];
    loop {
        if rest.len() > MAX_BODY_BYTES { return Err(HttpError::BodyTooLarge) }
        let n = r.read(&mut chunk)?;
        if n == 0 { return Ok(std::mem::take(rest)) }
        rest.extend_from_slice(&chunk[..n]);
    }
}

/// At most `cap` bytes of a close-delimited body.
fn read_to_cap(rest: &mut Vec<u8>, r: &mut impl Read, cap: usize) -> Result<Vec<u8>, HttpError> {
    let mut chunk = [0u8; 16 * 1024];
    while rest.len() < cap {
        let n = r.read(&mut chunk)?;
        if n == 0 { break }
        rest.extend_from_slice(&chunk[..n]);
    }
    rest.truncate(cap);
    Ok(std::mem::take(rest))
}

/// A chunked body, and whether it was read to its last chunk: reading stops
/// once `cap` bytes are in hand (`cap` is at most `MAX_BODY_BYTES`, the
/// refusal limit when there is no cap).
fn read_chunked(rest: &mut Vec<u8>, r: &mut impl Read, cap: usize) -> Result<(Vec<u8>, bool), HttpError> {
    let mut body = vec![];
    let capped = cap < MAX_BODY_BYTES;
    loop {
        if capped && body.len() >= cap { body.truncate(cap); return Ok((body, false)) }
        // Chunk-size line.
        let line_end = loop {
            if let Some(i) = rest.windows(2).position(|w| w == b"\r\n") { break i }
            if rest.len() > 1024 { return Err(HttpError::Malformed("chunk size line too long")) }
            let want = rest.len() + 1;
            fill(rest, r, want)?;
        };
        let line = std::str::from_utf8(&rest[..line_end]).map_err(|_| HttpError::Malformed("chunk size"))?;
        let size = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| HttpError::Malformed("chunk size"))?;
        rest.drain(..line_end + 2);
        if size == 0 { return Ok((body, true)) } // trailers ignored
        if capped && body.len() + size > cap {
            // Only what the cap still admits of this chunk.
            let take = cap - body.len();
            fill(rest, r, take)?;
            body.extend_from_slice(&rest[..take]);
            return Ok((body, false));
        }
        if body.len() + size > MAX_BODY_BYTES { return Err(HttpError::BodyTooLarge) }
        fill(rest, r, size + 2)?;
        body.extend_from_slice(&rest[..size]);
        if &rest[size..size + 2] != b"\r\n" { return Err(HttpError::Malformed("chunk not CRLF-terminated")) }
        rest.drain(..size + 2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &[u8]) -> Result<Response, HttpError> { read_response(&mut &raw[..]) }

    #[test]
    fn content_length_body() {
        let r = parse(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nX-A: b\r\n\r\nhelloEXTRA").unwrap();
        assert_eq!((r.status, r.body.as_slice(), r.header("x-a")), (200, &b"hello"[..], Some("b")));
    }

    #[test]
    fn chunked_body() {
        let r = parse(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nWiki\r\n5;x=y\r\npedia\r\n0\r\n\r\n").unwrap();
        assert_eq!(r.body, b"Wikipedia");
    }

    #[test]
    fn close_delimited_body() {
        assert_eq!(parse(b"HTTP/1.0 200 OK\r\n\r\nto the end").unwrap().body, b"to the end");
    }

    #[test]
    fn a_stated_length_over_the_limit_is_refused_before_reading() {
        let raw = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", MAX_BODY_BYTES + 1);
        assert_eq!(parse(raw.as_bytes()), Err(HttpError::BodyTooLarge));
    }

    #[test]
    fn a_chunk_that_overruns_the_limit_is_refused() {
        let raw = format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n", MAX_BODY_BYTES + 1);
        assert_eq!(parse(raw.as_bytes()), Err(HttpError::BodyTooLarge));
    }

    #[test]
    fn headers_that_never_end_are_refused() {
        let mut raw = b"HTTP/1.1 200 OK\r\n".to_vec();
        raw.extend(std::iter::repeat(b'a').take(MAX_HEADER_BYTES + 10));
        assert_eq!(parse(&raw), Err(HttpError::HeaderTooLarge));
    }

    #[test]
    fn a_compressed_body_is_refused_not_decoded() {
        let r = parse(b"HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 1\r\n\r\nx");
        assert_eq!(r, Err(HttpError::Encoded("gzip".into())));
    }

    fn capped(raw: &[u8], cap: usize) -> (Response, bool) { read_response_capped(&mut &raw[..], Some(cap)).unwrap() }

    #[test]
    fn a_capped_read_takes_only_the_asked_for_bytes_whatever_the_framing() {
        // A server that ignored Range and states a length past the limit: not refused, cut.
        let big = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\nabcdefghij", MAX_BODY_BYTES * 4);
        assert_eq!(capped(big.as_bytes(), 4), (Response { status: 200, headers: vec![("content-length".into(), (MAX_BODY_BYTES * 4).to_string())], body: b"abcd".to_vec() }, false));
        // A body within the cap is read whole and keeps the connection.
        let (r, keep) = capped(b"HTTP/1.1 206 Partial\r\nContent-Length: 3\r\n\r\nabc", 16);
        assert_eq!((r.body.as_slice(), keep), (&b"abc"[..], true));
        let (r, keep) = capped(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n", 6);
        assert_eq!((r.body.as_slice(), keep), (&b"Wikipe"[..], false));
        let (r, keep) = capped(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nWiki\r\n0\r\n\r\n", 6);
        assert_eq!((r.body.as_slice(), keep), (&b"Wiki"[..], true));
        let (r, keep) = capped(b"HTTP/1.0 200 OK\r\n\r\nto the end", 2);
        assert_eq!((r.body.as_slice(), keep), (&b"to"[..], false));
        // A huge chunk under a cap is cut, not refused.
        let raw = format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\nxyz", MAX_BODY_BYTES + 1);
        assert_eq!(capped(raw.as_bytes(), 2).0.body, b"xy");
    }

    #[test]
    fn a_range_request_says_which_bytes() {
        let mut out = vec![];
        write_request_range(&mut out, "example.com", "/i.jpg", true, Some((16384, 16384))).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("\r\nRange: bytes=16384-32767\r\nConnection: keep-alive\r\n\r\n"));
    }

    #[test]
    fn the_request_carries_no_identity() {
        let mut out = vec![];
        write_request(&mut out, "example.com", "/a?b").unwrap();
        let s = String::from_utf8(out).unwrap().to_ascii_lowercase();
        assert!(s.starts_with("get /a?b http/1.1\r\nhost: example.com\r\n"));
        for banned in ["cookie", "referer", "authorization", "sec-ch-"] { assert!(!s.contains(banned), "{banned}") }
        assert!(s.contains("accept-encoding: identity"));
    }
}
