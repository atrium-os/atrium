//! The fetcher for the person's own server (background-apps.md §4.4).
//!
//! It dials nothing. navigatord hands it, with each request, one TCP
//! socket portcullisd connected to the app; this speaks one HTTP/1.1
//! exchange on it and closes it. A request is one message on the channel
//! (a Unix socket that is the fetcher's stdin): `G <url>\n` with exactly
//! one descriptor riding on it. The URL must be a local address by the
//! one parser (`navigator_control::local`), else nothing is sent.
//!
//! Answers, on the same channel:
//! - `ok <status> <type> <len> <url>\n` and `len` body bytes (`type` as
//!   [`crate::live_content_type`] spells it);
//! - `redirect <status> <url>\n`: a redirect to the same origin, which
//!   navigatord follows with a new socket;
//! - `err <why>\n`, also for a redirect off the origin.

use std::io::{self, Write};
use std::net::TcpStream;
use std::os::fd::{AsRawFd, OwnedFd};

use navigator_control::local::{self, Local};

/// The longest request message: `G `, an address, the newline.
pub const REQUEST_MAX: usize = 2 + navigator_control::URL_MAX + 1;

#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
    Ok { status: u16, kind: String, body: Vec<u8>, url: String },
    Redirect { status: u16, to: String },
    Err(String),
}

/// A request message's address, or why it is refused.
pub fn parse_request(msg: &[u8]) -> Result<Local, String> {
    let line = msg.strip_prefix(b"G ").and_then(|m| m.strip_suffix(b"\n")).ok_or("not a request")?;
    let url = std::str::from_utf8(line).map_err(|_| "not text")?;
    local::parse(url).ok_or_else(|| "not one of your own server's addresses".to_string())
}

/// One exchange for `l` on `sock`, closed after.
pub fn exchange(mut sock: TcpStream, l: &Local) -> Answer {
    let _ = sock.set_read_timeout(Some(crate::TIMEOUT));
    let _ = sock.set_write_timeout(Some(crate::TIMEOUT));
    if let Err(e) = crate::http::write_request_range(&mut sock, &l.origin.host_header(), &l.path, false, None) {
        return Answer::Err(format!("send: {e}"));
    }
    let r = match crate::http::read_response_capped(&mut sock, None) {
        Ok((r, _)) => r,
        Err(e) => return Answer::Err(format!("{e:?}")),
    };
    let _ = sock.shutdown(std::net::Shutdown::Both);
    if matches!(r.status, 301 | 302 | 303 | 307 | 308) {
        let Some(loc) = r.header("location") else { return Answer::Err("a redirect without Location".into()) };
        return match local::join_same_origin(l, loc) {
            Some(to) => Answer::Redirect { status: r.status, to: to.url() },
            None => Answer::Err("your server redirected away from itself".into()),
        };
    }
    Answer::Ok { status: r.status, kind: crate::live_content_type(r.header("content-type")), body: r.body, url: l.url() }
}

/// The answer's frame.
pub fn write_answer(out: &mut impl Write, a: &Answer) -> io::Result<()> {
    match a {
        Answer::Ok { status, kind, body, url } => {
            write!(out, "ok {status} {kind} {} {url}\n", body.len())?;
            out.write_all(body)?;
        }
        Answer::Redirect { status, to } => write!(out, "redirect {status} {to}\n")?,
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
            Ok(l) => exchange(tcp(fd)?, &l),
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
        r.read_line(&mut head).unwrap();
        if let Some(rest) = head.strip_prefix("ok ") {
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
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 5\r\n\r\nhello",
            "HTTP/1.1 302 Found\r\nLocation: /next?a=1\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 302 Found\r\nLocation: http://localhost:9999/\r\nContent-Length: 0\r\n\r\n",
            "",
        ]);
        let (ours, theirs) = UnixStream::pair().unwrap();
        let fetchd = std::thread::spawn(move || serve(&theirs));
        let page = format!("http://localhost:{port}/a/b");
        assert_eq!(ask(&ours, &page, port), format!("ok 200 text/html 5 http://localhost:{port}/a/b\nhello"));
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
    fn a_request_without_a_socket_ends_the_channel() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        (&ours).write_all(b"G http://localhost:8080/\n").unwrap();
        assert!(serve(&theirs).is_err());
    }
}
