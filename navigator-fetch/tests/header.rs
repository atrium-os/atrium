//! Headers ahead of bodies (legacy-web §5.4.5.7, cut A): ranged fetches and
//! the prefetcher's `peek`, against a loopback server that serves a large
//! body, honours `Range` or ignores it, and is slow on the full body.

use navigator_fetch::{addr::Policy, prefetch::{Prefetcher, HEAD_BYTES, MAX_PEEK}, Connect, Conversation};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct ToServer(SocketAddr);
impl Connect for ToServer {
    fn resolve(&self, _: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> { Ok(vec![SocketAddr::new(self.0.ip(), port)]) }
    fn connect_addr(&self, _: SocketAddr) -> std::io::Result<TcpStream> { TcpStream::connect(self.0) }
}

const BODY: usize = 100_000;
fn byte(i: usize) -> u8 { (i * 7 % 251) as u8 }

#[derive(Clone, Copy, PartialEq)]
enum Mode { Honour, Ignore, Lie }

/// `/slow…` waits `slow` before a FULL body; ranges are answered at once.
fn server(mode: Mode, slow: Duration) -> (SocketAddr, Arc<AtomicUsize>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let conns = Arc::new(AtomicUsize::new(0));
    let c = conns.clone();
    std::thread::spawn(move || for s in l.incoming() {
        let Ok(mut s) = s else { return };
        c.fetch_add(1, Ordering::SeqCst);
        std::thread::spawn(move || loop {
            let mut head = Vec::new();
            let mut b = [0u8; 1024];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match s.read(&mut b) { Ok(0) | Err(_) => return, Ok(n) => head.extend_from_slice(&b[..n]) }
            }
            let h = String::from_utf8_lossy(&head).to_string();
            let path = h.split(' ').nth(1).unwrap_or("/").to_string();
            let range = h.lines().find_map(|l| l.strip_prefix("Range: bytes=")).and_then(|r| {
                let (a, z) = r.trim().split_once('-')?; Some((a.parse::<usize>().ok()?, z.parse::<usize>().ok()?))
            });
            // `/small…` is 50 bytes: a body that ends inside a header read.
            let n = if path.starts_with("/small") { 50 } else { BODY };
            let body: Vec<u8> = (0..n).map(byte).collect();
            let mut out = Vec::new();
            match (range, mode) {
                (Some((a, z)), Mode::Honour | Mode::Lie) => {
                    let z = z.min(n - 1);
                    let a2 = if mode == Mode::Lie { a + 1 } else { a };
                    write!(out, "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {a2}-{z}/{n}\r\nContent-Length: {}\r\n\r\n", z + 1 - a).unwrap();
                    out.extend_from_slice(&body[a..=z]);
                }
                _ => {
                    if path.starts_with("/slow") { std::thread::sleep(slow) }
                    write!(out, "HTTP/1.1 200 OK\r\nContent-Length: {n}\r\n\r\n").unwrap();
                    out.extend_from_slice(&body);
                }
            }
            if s.write_all(&out).is_err() { return }
        });
    });
    (addr, conns)
}

fn tls() -> Arc<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    Arc::new(rustls::ClientConfig::builder_with_provider(provider).with_safe_default_protocol_versions().unwrap()
        .with_root_certificates(rustls::RootCertStore::empty()).with_no_client_auth())
}
const GRANT: Policy = Policy { grant_private_network: true };
fn want(a: usize, z: usize) -> Vec<u8> { (a..z.min(BODY)).map(byte).collect() }

#[test]
fn a_range_is_the_bytes_asked_for_and_says_where_the_body_ends() {
    let (addr, _) = server(Mode::Honour, Duration::ZERO);
    let net = ToServer(addr);
    let mut c = Conversation::new(&net, tls(), GRANT).with_reuse(true);
    let r = c.fetch_range("http://img.test/a.jpg", 0, HEAD_BYTES).unwrap();
    assert_eq!((r.status, r.bytes.clone(), r.end), (206, want(0, HEAD_BYTES as usize), false));
    let r = c.fetch_range("http://img.test/a.jpg", 90_000, 16_384).unwrap();
    assert_eq!((r.bytes.clone(), r.end), (want(90_000, BODY), true));
}

#[test]
fn a_server_that_ignores_range_is_read_no_further_than_asked() {
    let (addr, conns) = server(Mode::Ignore, Duration::ZERO);
    let net = ToServer(addr);
    let mut c = Conversation::new(&net, tls(), GRANT).with_reuse(true);
    let r = c.fetch_range("http://img.test/a.jpg", 0, 1000).unwrap();
    assert_eq!((r.status, r.bytes.clone(), r.end), (200, want(0, 1000), false));
    let r = c.fetch_range("http://img.test/a.jpg", 20_000, 500).unwrap();
    assert_eq!((r.bytes.clone(), r.end), (want(20_000, 20_500), false));
    // The rest of each body was still on the wire: neither connection is reused.
    c.fetch("http://img.test/b").unwrap();
    assert_eq!(conns.load(Ordering::SeqCst), 3);
}

#[test]
fn a_206_for_another_range_is_refused() {
    let (addr, _) = server(Mode::Lie, Duration::ZERO);
    let net = ToServer(addr);
    let mut c = Conversation::new(&net, tls(), GRANT);
    let e = c.fetch_range("http://img.test/a.jpg", 0, 100).unwrap_err();
    assert!(format!("{e:?}").contains("another range"), "{e:?}");
}

#[test]
fn a_header_does_not_wait_behind_slow_bodies() {
    let (addr, _) = server(Mode::Honour, Duration::from_millis(800));
    let net = ToServer(addr);
    let conv = Conversation::new(&net, tls(), GRANT).with_reuse(true);
    std::thread::scope(|s| {
        let mut pf = Prefetcher::start(s, &conv, 1);
        for i in 0..3 { pf.hint(&format!("http://img.test/slow{i}")) }
        pf.hint_header("http://img.test/slow9");
        let t = Instant::now();
        let p = pf.peek("http://img.test/slow9", 0, 4096, Duration::from_secs(5)).unwrap();
        assert_eq!(p.bytes, want(0, 4096));
        assert!(t.elapsed() < Duration::from_millis(500), "the header waited {:?}", t.elapsed());
        // Past the head: a new range job, also ahead of the bodies.
        let p = pf.peek("http://img.test/slow9", 16_384, 16_384, Duration::from_secs(5)).unwrap();
        assert_eq!(p.bytes, want(16_384, 32_768));
        // A peek does not consume a body: the request still finds its prefetch.
        let b = pf.fetch("http://img.test/slow0").unwrap();
        let p = pf.peek("http://img.test/slow1", 0, 10, Duration::from_secs(5)).unwrap();
        assert_eq!((b.response.body.len(), p.bytes), (BODY, want(0, 10)));
        pf.close();
    });
}

#[test]
fn a_peek_is_answered_from_a_finished_body_and_bounded() {
    let (addr, _) = server(Mode::Ignore, Duration::ZERO);
    let net = ToServer(addr);
    let conv = Conversation::new(&net, tls(), GRANT);
    std::thread::scope(|s| {
        let mut pf = Prefetcher::start(s, &conv, 2);
        pf.hint("http://img.test/x");
        std::thread::sleep(Duration::from_millis(200));
        let p = pf.peek("http://img.test/x", 60_000, 5_000, Duration::ZERO).unwrap();
        assert_eq!(p.bytes, want(60_000, 65_000), "from the finished body, with no job and no wait");
        assert!(pf.peek("http://img.test/x", 0, MAX_PEEK + 1, Duration::ZERO).unwrap_err().contains("refused"));
        // A body that ends inside the read: fewer bytes than asked, which is how the end is said.
        let p = pf.peek("http://img.test/small", 0, 16_384, Duration::from_secs(5)).unwrap();
        assert_eq!(p.bytes, want(0, 50));
        assert_eq!(pf.fetch("http://img.test/x").unwrap().response.body.len(), BODY);
        pf.close();
    });
}

#[test]
fn a_peek_that_runs_out_of_time_says_so() {
    let (addr, _) = server(Mode::Ignore, Duration::from_millis(600));
    let net = ToServer(addr);
    let conv = Conversation::new(&net, tls(), GRANT);
    std::thread::scope(|s| {
        let mut pf = Prefetcher::start(s, &conv, 1);
        // The server ignores Range and is slow on every full body.
        assert_eq!(pf.peek("http://img.test/slow", 0, 100, Duration::from_millis(50)).unwrap_err(), "not in time");
        assert_eq!(pf.peek_stats(), (0, 1));
        // The job carried on; a later peek finds it.
        let p = pf.peek("http://img.test/slow", 0, 100, Duration::from_secs(5)).unwrap();
        assert_eq!(p.bytes, want(0, 100));
        pf.close();
    });
}
