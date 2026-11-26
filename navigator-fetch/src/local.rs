//! The fetcher for the person's own server (background-apps.md §4.4).
//!
//! It dials nothing. navigatord hands it, with each request, one TCP
//! socket portcullisd connected to the app; this speaks one HTTP/1.1
//! exchange on it and closes it. A request is one message on the channel
//! (a Unix socket that is the fetcher's stdin): `G <url>\n`, then
//! optionally `C <cookie header>\n` (navigatord's jar for this app and
//! port), with exactly one descriptor riding on it. The URL must be a
//! local address by the one parser (`navigator_control::local`), else
//! nothing is sent.
//!
//! Answers, on the same channel: first, for each `Set-Cookie` the server
//! sent that is well formed, `cookie <path> <max-age or -> <name>=<value>\n`
//! (at most [`SET_COOKIES_MAX`](navigator_control::local::SET_COOKIES_MAX); the jar is navigatord's, in memory), then
//! - `ok <status> <type> <len> <url>\n` and `len` body bytes (`type` as
//!   [`crate::live_content_type`] spells it);
//! - `redirect <status> <url>\n`: a redirect to the same origin, which
//!   navigatord follows with a new socket;
//! - `err <why>\n`, also for a redirect off the origin.

use std::io::{self, Write};
use std::net::TcpStream;
use std::os::fd::{AsRawFd, OwnedFd};

use navigator_control::local::{self, cookie_octet, path_ok, token, Local, SetCookie, SET_COOKIES_MAX, SET_COOKIE_MAX};

/// The longest cookie header navigatord sends.
pub const COOKIE_MAX: usize = 8192;
/// The longest request message: `G `, an address, the newline, and a
/// cookie line.
pub const REQUEST_MAX: usize = 2 + navigator_control::URL_MAX + 1 + 2 + COOKIE_MAX + 1;
/// RFC 6265 §5.1.4's default path for a request to `path`.
fn default_path(path: &str) -> String {
    let p = path.split_once('?').map_or(path, |(p, _)| p);
    match p.rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => p[..i].into(),
    }
}

/// Seconds since 1970 of an HTTP date (`Sun, 06 Nov 1994 08:49:37 GMT`,
/// also with `-` between day, month and year), or `None`.
fn http_date(v: &str) -> Option<i64> {
    let v = v.split_once(',').map_or(v, |(_, d)| d).trim().replace('-', " ");
    let f: Vec<&str> = v.split_whitespace().collect();
    let day: i64 = f.first()?.parse().ok()?;
    let mon = f.get(1)?.to_ascii_lowercase();
    let mon = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"]
        .iter().position(|m| mon.starts_with(m))? as i64 + 1;
    let mut year: i64 = f.get(2)?.parse().ok()?;
    if year < 100 { year += if year < 70 { 2000 } else { 1900 } }
    let hms: Vec<i64> = f.get(3)?.split(':').map(|x| x.parse().ok()).collect::<Option<_>>()?;
    let [h, m, sec] = hms[..] else { return None };
    if !(1..=31).contains(&day) || !(0..=23).contains(&h) || !(0..=59).contains(&m) || !(0..=60).contains(&sec) { return None }
    // Days from the civil date (H. Hinnant's algorithm).
    let y = if mon <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (mon + if mon > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146097 + doe - 719468) * 86400 + h * 3600 + m * 60 + sec)
}

/// One `Set-Cookie` value, for a request to `path`, at `now` (seconds
/// since 1970); `None` when it is out of shape. Domain is ignored: the
/// jar is the app's and port's alone. A `Secure` one is dropped (this is
/// http).
pub fn parse_set_cookie(v: &str, path: &str, now: i64) -> Option<SetCookie> {
    let mut parts = v.split(';');
    let (name, value) = parts.next()?.split_once('=')?;
    let (name, mut value) = (name.trim(), value.trim());
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        value = &value[1..value.len() - 1];
    }
    if !token(name) || !value.bytes().all(cookie_octet) || name.len() + value.len() > SET_COOKIE_MAX {
        return None;
    }
    let (mut cpath, mut max_age, mut expires) = (None, None, None);
    for a in parts {
        let (k, val) = a.split_once('=').map_or((a.trim(), ""), |(k, v)| (k.trim(), v.trim()));
        match k.to_ascii_lowercase().as_str() {
            "path" if path_ok(val) => cpath = Some(val.to_string()),
            "max-age" => max_age = Some(u64::try_from(val.parse::<i64>().ok()?.max(0)).unwrap_or(0)),
            "expires" => expires = http_date(val).map(|t| u64::try_from((t - now).max(0)).unwrap_or(0)),
            "secure" => return None,
            _ => {}
        }
    }
    Some(SetCookie { name: name.into(), value: value.into(), path: cpath.unwrap_or_else(|| default_path(path)), max_age: max_age.or(expires) })
}

#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
    Ok { status: u16, kind: String, body: Vec<u8>, url: String, cookies: Vec<SetCookie> },
    Redirect { status: u16, to: String, cookies: Vec<SetCookie> },
    Err(String),
}

/// A request message's address and cookie header, or why it is refused.
pub fn parse_request(msg: &[u8]) -> Result<(Local, Option<String>), String> {
    let text = std::str::from_utf8(msg).map_err(|_| "not text")?;
    let text = text.strip_suffix('\n').ok_or("not a request")?;
    let (first, cookie) = match text.split_once('\n') {
        Some((a, c)) => (a, Some(c.strip_prefix("C ").ok_or("not a request")?)),
        None => (text, None),
    };
    let url = first.strip_prefix("G ").ok_or("not a request")?;
    if let Some(c) = cookie {
        // name=value pairs joined by "; ", nothing else.
        if c.len() > COOKIE_MAX || c.split("; ").any(|p| !p.split_once('=').is_some_and(|(n, v)| token(n) && v.bytes().all(cookie_octet))) {
            return Err("a cookie header out of shape".into());
        }
    }
    let l = local::parse(url).ok_or_else(|| "not one of your own server's addresses".to_string())?;
    Ok((l, cookie.map(String::from)))
}

/// One exchange for `l` on `sock`, closed after; `cookie` is sent as the
/// request's `Cookie` header.
pub fn exchange(mut sock: TcpStream, l: &Local, cookie: Option<&str>) -> Answer {
    let _ = sock.set_read_timeout(Some(crate::TIMEOUT));
    let _ = sock.set_write_timeout(Some(crate::TIMEOUT));
    let cookie = cookie.map_or(String::new(), |c| format!("Cookie: {c}\r\n"));
    // One write, as the web fetcher's.
    let req = format!("GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: atrium-navigator-fetch/0.1\r\nAccept: */*\r\n\
                       Accept-Encoding: identity\r\n{cookie}Connection: close\r\n\r\n", l.path, l.origin.host_header());
    if let Err(e) = sock.write_all(req.as_bytes()) {
        return Answer::Err(format!("send: {e}"));
    }
    let r = match crate::http::read_response_capped(&mut sock, None) {
        Ok((r, _)) => r,
        Err(e) => return Answer::Err(format!("{e:?}")),
    };
    let _ = sock.shutdown(std::net::Shutdown::Both);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    let cookies: Vec<SetCookie> = r.headers.iter().filter(|(k, _)| k == "set-cookie")
        .filter_map(|(_, v)| parse_set_cookie(v, &l.path, now)).take(SET_COOKIES_MAX).collect();
    if matches!(r.status, 301 | 302 | 303 | 307 | 308) {
        let Some(loc) = r.header("location") else { return Answer::Err("a redirect without Location".into()) };
        return match local::join_same_origin(l, loc) {
            Some(to) => Answer::Redirect { status: r.status, to: to.url(), cookies },
            None => Answer::Err("your server redirected away from itself".into()),
        };
    }
    Answer::Ok { status: r.status, kind: crate::live_content_type(r.header("content-type")), body: r.body, url: l.url(), cookies }
}

/// The answer's frame.
pub fn write_answer(out: &mut impl Write, a: &Answer) -> io::Result<()> {
    let cookies = match a {
        Answer::Ok { cookies, .. } | Answer::Redirect { cookies, .. } => cookies.as_slice(),
        Answer::Err(_) => &[],
    };
    for c in cookies {
        writeln!(out, "{}", c.line())?;
    }
    match a {
        Answer::Ok { status, kind, body, url, .. } => {
            write!(out, "ok {status} {kind} {} {url}\n", body.len())?;
            out.write_all(body)?;
        }
        Answer::Redirect { status, to, .. } => write!(out, "redirect {status} {to}\n")?,
        Answer::Err(why) => write!(out, "err {}\n", why.replace(['\n', '\r'], " "))?,
    }
    out.flush()
}

/// Serve requests on `chan` until it closes or a message is malformed.
pub fn serve(chan: &std::os::unix::net::UnixStream) -> io::Result<()> {
    let mut buf = vec![0u8; REQUEST_MAX];
    loop {
        let got = atrium_fdpass::recv(chan.as_raw_fd(), &mut buf, 1)?;
        if got.n == 0 {
            return Ok(());
        }
        let mut fds = got.fds;
        let (Some(fd), true) = (fds.pop(), fds.is_empty()) else {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "a request without its socket"));
        };
        let answer = match parse_request(&buf[..got.n]) {
            Ok((l, cookie)) => exchange(tcp(fd)?, &l, cookie.as_deref()),
            Err(why) => Answer::Err(why),
        };
        write_answer(&mut &*chan, &answer)?;
    }
}

/// The descriptor as a connected TCP socket, or an error.
fn tcp(fd: OwnedFd) -> io::Result<TcpStream> {
    let s = TcpStream::from(fd);
    s.peer_addr()?;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read};
    use std::net::TcpListener;
    use std::os::unix::net::UnixStream;

    /// A one-shot server: answers each connection with `reply`, after
    /// checking the request line and Host header it got.
    fn server(replies: Vec<&'static str>) -> (u16, std::thread::JoinHandle<Vec<String>>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let t = std::thread::spawn(move || {
            let mut seen = vec![];
            for reply in replies {
                let (mut c, _) = l.accept().unwrap();
                let mut r = BufReader::new(c.try_clone().unwrap());
                let mut first = String::new();
                r.read_line(&mut first).unwrap();
                let mut host = String::new();
                loop {
                    let mut h = String::new();
                    r.read_line(&mut h).unwrap();
                    if h == "\r\n" || h.is_empty() { break }
                    if h.to_ascii_lowercase().starts_with("host:") { host = h.trim().to_string() }
                }
                seen.push(format!("{} | {host}", first.trim()));
                let _ = c.write_all(reply.as_bytes());
            }
            seen
        });
        (port, t)
    }

    fn ask(chan: &UnixStream, url: &str, port: u16) -> String {
        let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        atrium_fdpass::send(chan.as_raw_fd(), format!("G {url}\n").as_bytes(), &[s.as_raw_fd()]).unwrap();
        drop(s);
        let mut r = BufReader::new(chan);
        let mut head = String::new();
        loop {
            let mut l = String::new();
            r.read_line(&mut l).unwrap();
            head.push_str(&l);
            if !l.starts_with("cookie ") { break }
        }
        let last = head.lines().last().unwrap_or("").to_string();
        if let Some(rest) = last.strip_prefix("ok ") {
            let len: usize = rest.split(' ').nth(2).unwrap().parse().unwrap();
            let mut body = vec![0; len];
            r.read_exact(&mut body).unwrap();
            return format!("{}{}", head, String::from_utf8(body).unwrap());
        }
        head
    }

    #[test]
    fn one_exchange_per_socket_and_redirects_stay_home() {
        let (port, t) = server(vec![
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nSet-Cookie: s=1; Path=/\r\nSet-Cookie: bad cookie\r\nContent-Length: 5\r\n\r\nhello",
            "HTTP/1.1 302 Found\r\nLocation: /next?a=1\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 302 Found\r\nLocation: http://localhost:9999/\r\nContent-Length: 0\r\n\r\n",
            "",
        ]);
        let (ours, theirs) = UnixStream::pair().unwrap();
        let fetchd = std::thread::spawn(move || serve(&theirs));
        let page = format!("http://localhost:{port}/a/b");
        assert_eq!(ask(&ours, &page, port), format!("cookie / - s=1\nok 200 text/html 5 http://localhost:{port}/a/b\nhello"));
        assert_eq!(ask(&ours, &page, port), format!("redirect 302 http://localhost:{port}/next?a=1\n"));
        assert_eq!(ask(&ours, &page, port), "err your server redirected away from itself\n");
        // Not a local address: refused before anything is sent on the socket.
        let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        atrium_fdpass::send(ours.as_raw_fd(), b"G http://127.0.0.2:80/\n", &[s.as_raw_fd()]).unwrap();
        drop(s);
        let mut head = String::new();
        BufReader::new(&ours).read_line(&mut head).unwrap();
        assert_eq!(head, "err not one of your own server's addresses\n");
        drop(ours);
        fetchd.join().unwrap().unwrap();
        let seen = t.join().unwrap();
        assert_eq!(seen[0], format!("GET /a/b HTTP/1.1 | Host: localhost:{port}"));
        assert_eq!(seen[3], " | ", "nothing is sent for a refused address");
    }

    #[test]
    fn set_cookies_are_checked_and_dated() {
        let now = 784_111_777; // Sun, 06 Nov 1994 08:49:37 GMT
        assert_eq!(http_date("Sun, 06 Nov 1994 08:49:37 GMT"), Some(now));
        assert_eq!(http_date("Sunday, 06-Nov-94 08:49:37 GMT"), Some(now));
        assert_eq!(http_date("garbage"), None);
        let c = parse_set_cookie("sid=abc; Path=/app; HttpOnly", "/x", now).unwrap();
        assert_eq!(c, SetCookie { name: "sid".into(), value: "abc".into(), path: "/app".into(), max_age: None });
        assert_eq!(parse_set_cookie("a=\"q\"", "/dir/page?x", now).unwrap().path, "/dir", "the default path");
        assert_eq!(parse_set_cookie("a=1; Max-Age=60", "/", now).unwrap().max_age, Some(60));
        assert_eq!(parse_set_cookie("a=1; Expires=Thu, 01 Jan 1970 00:00:00 GMT", "/", now).unwrap().max_age, Some(0), "a past date removes it");
        for bad in ["noequals", "a b=1", "a=1 2", "a=x,y", "a=1; Secure", "=1", "a=\\"] {
            assert!(parse_set_cookie(bad, "/", now).is_none(), "{bad}");
        }
        let line = c.line();
        assert_eq!(line, "cookie /app - sid=abc");
        assert_eq!(SetCookie::from_line(&line), Some(c));
        assert!(SetCookie::from_line("cookie app - a=1").is_none() && SetCookie::from_line("cookie / x a=1").is_none());
    }

    #[test]
    fn a_cookie_header_rides_with_the_request_and_is_checked() {
        let (l, c) = parse_request(b"G http://localhost:8080/\nC a=1; b=2\n").unwrap();
        assert_eq!((l.url().as_str(), c.as_deref()), ("http://localhost:8080/", Some("a=1; b=2")));
        assert!(parse_request(b"G http://localhost:8080/\nC a=1\r\nX: y\n").is_err());
        assert!(parse_request(b"G http://localhost:8080/\nX a=1\n").is_err());
        assert!(parse_request(b"G http://localhost:8080/\nC a b\n").is_err());
    }

    #[test]
    fn a_request_without_a_socket_ends_the_channel() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        (&ours).write_all(b"G http://localhost:8080/\n").unwrap();
        assert!(serve(&theirs).is_err());
    }
}
