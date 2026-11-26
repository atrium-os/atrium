//! The person's own server in Navigator (background-apps.md §4.4).
//!
//! A local address (`navigator_control::local`, the one parser) opens only
//! as a navigation the person started: an address they typed, or a bookmark
//! or history entry they opened (`Request::Typed`, from the UI's own
//! connection), or a link in a page of that same local origin. Each such
//! grant is for one address and is spent by the one `Open` of it.
//!
//! Its bytes come over sockets portcullisd connects to the app
//! (`OpenLocalPort`), one per request, handed to a fetcher started on a
//! Unix socket (`navigator_fetch::local`), which dials nothing. navigatord
//! checks every request against its own record of the page's origin, never
//! what the converter claims, and checks each socket's peer is the address
//! portcullisd named.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read};
use std::net::{SocketAddr, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use navigator_control::local::{self, Local, Origin};
use navigator_prerender::fetch::Reply;

/// How long a typed grant waits for its `Open`.
pub const GRANT_LIFE: Duration = Duration::from_secs(30);
/// Grants held at once (a burst of Enter presses).
pub const GRANTS_MAX: usize = 16;
/// Redirects followed within the origin.
pub const REDIRECTS_MAX: usize = 5;
/// Requests one page may make of the server (the page and its resources).
pub const REQUESTS_MAX: usize = 256;
/// The longest answer head line from the fetcher.
const HEAD_MAX: usize = 64 + navigator_control::URL_MAX;

/// Addresses the person started a navigation to, each good for one `Open`.
#[derive(Default)]
pub struct Typed {
    held: VecDeque<(String, Instant)>,
}

impl Typed {
    /// Hold `url` if it is a local address; ignored otherwise.
    pub fn grant(&mut self, url: &str, now: Instant) {
        let Some(l) = local::parse(url) else { return };
        self.held.retain(|(_, at)| now.duration_since(*at) < GRANT_LIFE);
        if self.held.len() >= GRANTS_MAX {
            self.held.pop_front();
        }
        self.held.push_back((l.url(), now));
    }

    /// Spend the grant for `url`, if one is held and fresh.
    pub fn take(&mut self, url: &str, now: Instant) -> bool {
        let Some(l) = local::parse(url) else { return false };
        let want = l.url();
        let Some(i) = self.held.iter().position(|(u, at)| *u == want && now.duration_since(*at) < GRANT_LIFE) else { return false };
        self.held.remove(i);
        true
    }
}

/// A link `href` led to `to` from the page at `page`: whether it stays on
/// that page's local origin (so it may be granted).
pub fn same_origin_link(page: &str, to: &str) -> bool {
    matches!((local::parse(page), local::parse(to)), (Some(a), Some(b)) if a.origin == b.origin)
}

/// The app that served each local origin last, for the address bar's chip.
fn apps() -> &'static Mutex<HashMap<Origin, String>> {
    static A: OnceLock<Mutex<HashMap<Origin, String>>> = OnceLock::new();
    A.get_or_init(Default::default)
}

/// The app that served `url`'s origin in the page just opened.
pub fn app_of(url: &str) -> Option<String> {
    let l = local::parse(url)?;
    apps().lock().unwrap_or_else(|e| e.into_inner()).get(&l.origin).cloned()
}

/// A socket to the person's own server on `port`, from portcullisd, and the
/// app's name.
fn open_port(port: u16) -> Result<(TcpStream, String), String> {
    use portcullis_ipc::{read_response, round_trip, Request, Response};
    let mut s = crate::worker::portcullisd_socket().map_err(|e| format!("portcullisd: {e}"))?;
    s.set_read_timeout(Some(Duration::from_secs(15))).map_err(|e| e.to_string())?;
    match round_trip(&mut s, &Request::Hello { version: portcullis_ipc::PROTO_VERSION }).map_err(|e| e.to_string())? {
        Response::Hello { .. } => {}
        other => return Err(format!("portcullisd hello: {other:?}")),
    }
    portcullis_ipc::write_request(&mut s, &Request::OpenLocalPort { port }).map_err(|e| e.to_string())?;
    let (app_name, addr) = match read_response(&mut s).map_err(|e| e.to_string())? {
        Response::LocalPort { app_name, addr, .. } => (app_name, addr),
        Response::Error { message } => return Err(message),
        other => return Err(format!("portcullisd: unexpected {other:?}")),
    };
    let mut fds = portcullis_ipc::recv_fds(&s, 1).map_err(|e| format!("portcullisd: no socket: {e}"))?;
    let fd = fds.pop().ok_or("portcullisd sent no socket")?;
    let sock = TcpStream::from(fd);
    let want: SocketAddr = addr.parse().map_err(|_| "portcullisd named no address".to_string())?;
    // The socket must be to the address portcullisd named, on the port asked.
    match sock.peer_addr() {
        Ok(p) if p == want && p.port() == port => Ok((sock, app_name)),
        other => Err(format!("the socket is to {other:?}, not {want}")),
    }
}

/// Where sockets come from: portcullisd, or a test's stand-in.
pub type Opener = Box<dyn FnMut(u16) -> Result<(TcpStream, String), String> + Send>;

/// One page's way to its local origin: the fetcher's channel and the
/// origin navigatord recorded when the person opened it.
pub struct LocalUp {
    page: Local,
    chan: UnixStream,
    open: Opener,
    requests: usize,
    /// The fetcher's jail, kept while the channel is.
    _worker: Option<crate::worker::Worker>,
}

impl LocalUp {
    /// For the page at `url` (a local address), with a fetcher on `chan`.
    pub fn new(url: &str, chan: UnixStream, open: Opener, worker: Option<crate::worker::Worker>) -> Result<LocalUp, String> {
        let page = local::parse(url).ok_or("not one of your own server's addresses")?;
        Ok(LocalUp { page, chan, open, requests: 0, _worker: worker })
    }

    /// A fetcher started on a new socket pair, and portcullisd's sockets.
    pub fn start(url: &str, spec: &crate::worker::Spec, instance: &str) -> Result<LocalUp, String> {
        let (ours, theirs) = UnixStream::pair().map_err(|e| e.to_string())?;
        let w = crate::worker::Worker::start_on(spec, instance, theirs.into()).map_err(|e| format!("fetcher: {e}"))?;
        ours.set_read_timeout(Some(navigator_fetch_timeout())).map_err(|e| e.to_string())?;
        LocalUp::new(url, ours, Box::new(open_port), Some(w))
    }

    /// `url` fetched through the fetcher, as a converter's fetch is answered.
    pub fn fetch(&mut self, url: &str) -> Reply {
        match self.fetch_inner(url) {
            Ok((status, _kind, body, at, redirects)) => Reply::Ok { status, rest: format!("{redirects} {} {at}", body.len()), body },
            Err(e) => Reply::Err(e),
        }
    }

    fn fetch_inner(&mut self, url: &str) -> Result<(u16, String, Vec<u8>, String, usize), String> {
        let mut at = self.check(url)?;
        for redirects in 0..=REDIRECTS_MAX {
            if self.requests >= REQUESTS_MAX {
                return Err(format!("more than {REQUESTS_MAX} requests to your own server from one page"));
            }
            self.requests += 1;
            let (sock, app) = (self.open)(at.origin.port)?;
            apps().lock().unwrap_or_else(|e| e.into_inner()).insert(at.origin, app);
            let msg = format!("G {}\n", at.url());
            atrium_fdpass::send(self.chan.as_raw_fd(), msg.as_bytes(), &[sock.as_raw_fd()]).map_err(|e| format!("fetcher: {e}"))?;
            drop(sock);
            match self.answer()? {
                Answer::Ok { status, kind, body } => return Ok((status, kind, body, at.url(), redirects)),
                Answer::Redirect(to) => at = self.check(&to)?,
            }
        }
        Err("too many redirects on your own server".into())
    }

    /// ★ Every request is held to the page's own origin, as navigatord
    /// recorded it, whatever asked for it.
    fn check(&self, url: &str) -> Result<Local, String> {
        match local::parse(url) {
            Some(l) if l.origin == self.page.origin => Ok(l),
            _ => Err(format!("{url} is not {}", self.page.origin.spell())),
        }
    }

    fn answer(&mut self) -> Result<Answer, String> {
        let mut r = BufReader::new(&self.chan);
        let mut head = Vec::new();
        (&mut r).take(HEAD_MAX as u64).read_until(b'\n', &mut head).map_err(|e| format!("fetcher: {e}"))?;
        let head = String::from_utf8(head).map_err(|_| "fetcher: not text")?;
        let head = head.strip_suffix('\n').ok_or("fetcher: a cut answer")?;
        if let Some(why) = head.strip_prefix("err ") {
            return Err(why.to_string());
        }
        if let Some(rest) = head.strip_prefix("redirect ") {
            let to = rest.split_once(' ').map(|(_, u)| u).ok_or("fetcher: a bad redirect")?;
            return Ok(Answer::Redirect(to.to_string()));
        }
        let f: Vec<&str> = head.strip_prefix("ok ").ok_or("fetcher: a bad answer")?.splitn(4, ' ').collect();
        let (Some(status), Some(kind), Some(len)) = (f.first().and_then(|s| s.parse::<u16>().ok()), f.get(1), f.get(2).and_then(|l| l.parse::<usize>().ok())) else {
            return Err("fetcher: a bad answer".into());
        };
        if len > navigator_fetch_body_max() {
            return Err("fetcher: an answer over its bound".into());
        }
        let mut body = vec![0u8; len];
        // The BufReader may hold the start of the body.
        r.read_exact(&mut body).map_err(|e| format!("fetcher: {e}"))?;
        Ok(Answer::Ok { status, kind: kind.to_string(), body })
    }
}

enum Answer {
    Ok { status: u16, kind: String, body: Vec<u8> },
    Redirect(String),
}

/// The fetcher's own bounds, mirrored here (it is a separate process).
fn navigator_fetch_timeout() -> Duration { Duration::from_secs(30) }
fn navigator_fetch_body_max() -> usize { 8 * 1024 * 1024 }

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;

    #[test]
    fn a_grant_is_one_address_once_and_fresh() {
        let t = Instant::now();
        let mut g = Typed::default();
        g.grant("http://localhost:8080", t);
        g.grant("https://example.com/", t);
        assert!(!g.take("http://localhost:9090/", t), "another port");
        assert!(!g.take("http://127.0.0.1:8080/", t), "another spelling is another origin");
        assert!(g.take("http://LOCALHOST:8080/#x", t), "the same address in another spelling of case and fragment");
        assert!(!g.take("http://localhost:8080/", t), "spent");
        g.grant("http://localhost:8080/", t);
        assert!(!g.take("http://localhost:8080/", t + GRANT_LIFE), "stale");
        assert!(!g.take("https://example.com/", t), "not a local address");
    }

    #[test]
    fn links_are_granted_only_within_the_origin() {
        assert!(same_origin_link("http://localhost:8080/a", "http://localhost:8080/b"));
        assert!(!same_origin_link("http://localhost:8080/a", "http://localhost:9090/b"));
        assert!(!same_origin_link("https://evil.example/", "http://localhost:8080/"));
    }

    /// A stand-in fetcher (the real one's serve loop) and a server.
    fn rig(replies: Vec<&'static str>) -> (u16, LocalUp, std::sync::Arc<Mutex<Vec<u16>>>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for reply in replies {
                let (mut c, _) = l.accept().unwrap();
                let mut b = [0u8; 4096];
                let _ = c.read(&mut b);
                let _ = c.write_all(reply.as_bytes());
            }
        });
        let (ours, theirs) = UnixStream::pair().unwrap();
        std::thread::spawn(move || navigator_fetch::local::serve(&theirs));
        let asked = std::sync::Arc::new(Mutex::new(vec![]));
        let a = asked.clone();
        let open: Opener = Box::new(move |p| {
            a.lock().unwrap().push(p);
            Ok((TcpStream::connect(("127.0.0.1", port)).map_err(|e| e.to_string())?, "Photo Server".into()))
        });
        let up = LocalUp::new(&format!("http://localhost:{port}/"), ours, open, None).unwrap();
        (port, up, asked)
    }

    #[test]
    fn pages_and_their_redirects_come_from_their_own_origin_only() {
        let (port, mut up, asked) = rig(vec![
            "HTTP/1.1 302 Found\r\nLocation: /home\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 2\r\n\r\nhi",
        ]);
        let base = format!("http://localhost:{port}");
        match up.fetch(&format!("{base}/")) {
            Reply::Ok { status, rest, body } => {
                assert_eq!((status, body.as_slice()), (200, &b"hi"[..]));
                assert_eq!(rest, format!("1 2 {base}/home"));
            }
            Reply::Err(e) => panic!("{e}"),
        }
        assert_eq!(asked.lock().unwrap().as_slice(), &[port, port], "one socket per request");
        assert_eq!(app_of(&format!("{base}/x")).as_deref(), Some("Photo Server"));
        // Another origin is refused before any socket is asked for.
        for other in ["http://localhost:1/", "http://127.0.0.1:8080/", "https://example.com/"] {
            assert!(matches!(up.fetch(other), Reply::Err(_)), "{other}");
        }
        assert_eq!(asked.lock().unwrap().len(), 2);
    }
}
