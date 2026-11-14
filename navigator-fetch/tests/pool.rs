//! Connection reuse and the prefetcher (legacy-web §5.4.5.7, step 4),
//! against a loopback server that counts connections and can be slow.
//! Loopback is a private address, so these conversations hold the grant.

use navigator_fetch::{addr::Policy, prefetch::Prefetcher, Connect, Conversation};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Resolves every name to the test server and counts lookups.
struct ToServer { addr: SocketAddr, lookups: AtomicUsize }
impl Connect for ToServer {
    fn resolve(&self, _host: &str, _port: u16) -> std::io::Result<Vec<SocketAddr>> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        // A pause, so concurrent first lookups would overlap if unlocked.
        std::thread::sleep(Duration::from_millis(20));
        Ok(vec![self.addr])
    }
    fn connect_addr(&self, _a: SocketAddr) -> std::io::Result<TcpStream> { TcpStream::connect(self.addr) }
}

#[derive(Clone, Copy)]
enum Style { KeepAlive, CloseAfterFirst, Chunked, NoLength }

/// Serves `/<n>` with body `body-<n>`, `delay` per response. Returns the
/// address and the count of connections accepted.
fn server(style: Style, delay: Duration) -> (SocketAddr, Arc<AtomicUsize>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let conns = Arc::new(AtomicUsize::new(0));
    let c = conns.clone();
    std::thread::spawn(move || for s in l.incoming() {
        let Ok(mut s) = s else { return };
        c.fetch_add(1, Ordering::SeqCst);
        std::thread::spawn(move || {
            let mut served = 0;
            loop {
                let mut head = Vec::new();
                let mut b = [0u8; 1024];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut b) { Ok(0) | Err(_) => return, Ok(n) => head.extend_from_slice(&b[..n]) }
                }
                let line = String::from_utf8_lossy(&head).lines().next().unwrap_or("").to_string();
                let path = line.split(' ').nth(1).unwrap_or("/").trim_start_matches('/').to_string();
                let body = format!("body-{path}");
                std::thread::sleep(delay);
                let r = match style {
                    Style::Chunked => format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{body}\r\n0\r\nX-T: 1\r\n\r\n", body.len()),
                    Style::NoLength => format!("HTTP/1.1 200 OK\r\n\r\n{body}"),
                    _ => format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len()),
                };
                if s.write_all(r.as_bytes()).is_err() { return }
                served += 1;
                if matches!(style, Style::NoLength) || (matches!(style, Style::CloseAfterFirst) && served == 1) { return }
            }
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

fn fetch_three(style: Style, reuse: bool) -> (Vec<String>, usize) {
    let (addr, conns) = server(style, Duration::ZERO);
    let net = ToServer { addr, lookups: AtomicUsize::new(0) };
    let mut c = Conversation::new(&net, tls(), GRANT).with_reuse(reuse);
    let bodies = (0..3).map(|i| String::from_utf8(c.fetch(&format!("http://site.test/{i}")).unwrap().response.body).unwrap()).collect();
    (bodies, conns.load(Ordering::SeqCst))
}

#[test]
fn reuse_carries_requests_to_one_origin_on_one_connection() {
    assert_eq!(fetch_three(Style::KeepAlive, true), (vec!["body-0".into(), "body-1".into(), "body-2".into()], 1));
    // Off, as fetchd was: one connection per request.
    assert_eq!(fetch_three(Style::KeepAlive, false).1, 3);
}

#[test]
fn a_chunked_body_with_trailers_leaves_the_connection_reusable() {
    assert_eq!(fetch_three(Style::Chunked, true), (vec!["body-0".into(), "body-1".into(), "body-2".into()], 1));
}

#[test]
fn a_body_that_ends_with_the_connection_is_never_reused() {
    assert_eq!(fetch_three(Style::NoLength, true), (vec!["body-0".into(), "body-1".into(), "body-2".into()], 3));
}

#[test]
fn a_connection_the_server_closed_while_idle_is_retried_once_fresh() {
    let (bodies, conns) = fetch_three(Style::CloseAfterFirst, true);
    assert_eq!(bodies, vec!["body-0".to_string(), "body-1".into(), "body-2".into()]);
    assert_eq!(conns, 3);
}

#[test]
fn hinted_fetches_run_in_parallel_and_are_handed_over_once() {
    let (addr, _) = server(Style::KeepAlive, Duration::from_millis(150));
    let net = ToServer { addr, lookups: AtomicUsize::new(0) };
    let conv = Conversation::new(&net, tls(), GRANT).with_reuse(true);
    let urls: Vec<String> = (0..6).map(|i| format!("http://site.test/{i}")).collect();
    let t = Instant::now();
    std::thread::scope(|s| {
        let mut p = Prefetcher::start(s, &conv, 6);
        for u in &urls { p.hint(u) }
        for (i, u) in urls.iter().enumerate() {
            assert_eq!(p.fetch(u).unwrap().response.body, format!("body-{i}").into_bytes());
        }
        let (hits, waits, direct, hinted) = p.stats();
        assert_eq!((hits + waits + direct, hinted), (6, 6));
        assert!(direct <= 1, "{:?}", p.stats());
        // ★ Handed over ONCE: asking again fetches again, on this thread.
        assert_eq!(p.fetch(&urls[0]).unwrap().response.body, b"body-0".to_vec());
        assert_eq!(p.stats().2, direct + 1);
    });
    // Six 150 ms responses: serial would be 900 ms.
    assert!(t.elapsed() < Duration::from_millis(600), "{:?}", t.elapsed());
    // ★ One name, one resolution, however many threads asked at once.
    assert_eq!(net.lookups.load(Ordering::SeqCst), 1);
}

#[test]
fn a_request_never_waits_behind_the_hint_queue() {
    let (addr, _) = server(Style::KeepAlive, Duration::from_millis(100));
    let net = ToServer { addr, lookups: AtomicUsize::new(0) };
    let conv = Conversation::new(&net, tls(), GRANT).with_reuse(true);
    std::thread::scope(|s| {
        // One worker, twenty hints: 2 s of queue.
        let mut p = Prefetcher::start(s, &conv, 1);
        for i in 0..20 { p.hint(&format!("http://site.test/{i}")) }
        let t = Instant::now();
        assert_eq!(p.fetch("http://site.test/script").unwrap().response.body, b"body-script".to_vec());
        assert!(t.elapsed() < Duration::from_millis(500), "{:?}", t.elapsed());
        // A hinted URL not yet started is taken out of the queue and fetched now.
        let t = Instant::now();
        assert_eq!(p.fetch("http://site.test/19").unwrap().response.body, b"body-19".to_vec());
        assert!(t.elapsed() < Duration::from_millis(500), "{:?}", t.elapsed());
        p.close();
    });
}

#[test]
fn siblings_keep_the_address_rule() {
    // Without the grant, loopback is refused — from a sibling too.
    let (addr, conns) = server(Style::KeepAlive, Duration::ZERO);
    let net = ToServer { addr, lookups: AtomicUsize::new(0) };
    let conv = Conversation::new(&net, tls(), Policy::default()).with_reuse(true);
    let refused = Mutex::new(0);
    std::thread::scope(|s| {
        let mut p = Prefetcher::start(s, &conv, 2);
        p.hint("http://site.test/a");
        if p.fetch("http://site.test/a").is_err() { *refused.lock().unwrap() += 1 }
        if p.fetch("http://site.test/b").is_err() { *refused.lock().unwrap() += 1 }
    });
    assert_eq!(*refused.lock().unwrap(), 2);
    assert_eq!(conns.load(Ordering::SeqCst), 0);
}

/// fetchd's serve loop: hints answered with nothing, requests in order, one
/// frame each, from the prefetch when there was one.
#[test]
fn the_serve_loop_answers_requests_in_order_and_hints_with_nothing() {
    let (addr, conns) = server(Style::KeepAlive, Duration::from_millis(50));
    let net = ToServer { addr, lookups: AtomicUsize::new(0) };
    let conv = Conversation::new(&net, tls(), GRANT).with_reuse(true);
    let input = b"H http://site.test/a\nH http://site.test/b\nhttp://site.test/b\nhttp://site.test/a\nhttp://site.test/c\n";
    let mut out = Vec::new();
    navigator_fetch::serve(&conv, 4, &input[..], &mut out, &mut |_| {});
    let text = String::from_utf8(out).unwrap();
    let mut frames = vec![];
    let mut rest = text.as_str();
    while let Some((head, tail)) = rest.split_once('\n') {
        let f: Vec<&str> = head.split(' ').collect();
        assert_eq!(f[0], "ok", "{head}");
        let n: usize = f[3].parse().unwrap();
        frames.push((f[4].to_string(), tail[..n].to_string()));
        rest = &tail[n..];
    }
    assert_eq!(frames, vec![("http://site.test/b".into(), "body-b".into()), ("http://site.test/a".into(), "body-a".into()),
                            ("http://site.test/c".into(), "body-c".into())]);
    assert!(conns.load(Ordering::SeqCst) <= 3, "{}", conns.load(Ordering::SeqCst));
}

/// ★ Hints are bounded in BYTES: once the prefetcher has pulled its budget,
/// what is still queued is dropped, and those URLs are fetched when asked.
#[test]
fn hints_stop_at_the_byte_budget() {
    let (addr, conns) = server(Style::KeepAlive, Duration::from_millis(20));
    let net = ToServer { addr, lookups: AtomicUsize::new(0) };
    let conv = Conversation::new(&net, tls(), GRANT).with_reuse(true);
    std::thread::scope(|s| {
        let mut p = Prefetcher::start(s, &conv, 1);
        p.set_budget(6); // one body: "body-0" is 6 bytes
        for i in 0..10 { p.hint(&format!("http://site.test/{i}")) }
        assert_eq!(p.fetch("http://site.test/0").unwrap().response.body, b"body-0".to_vec());
        std::thread::sleep(Duration::from_millis(200));
        let (pulled, over) = p.pulled();
        assert!(pulled <= 6 + 6, "{pulled}");
        assert!(over >= 8, "{over}");
        // Dropped hints are still fetched when asked.
        assert_eq!(p.fetch("http://site.test/9").unwrap().response.body, b"body-9".to_vec());
    });
    assert!(conns.load(Ordering::SeqCst) >= 1);
}

/// A site whose cookie check goes round without the cookie: `/start` sets
/// one and sends you to `/check`, which sends you back to `/start`.
fn cookie_check_server() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || for s in l.incoming() {
        let Ok(mut s) = s else { return };
        std::thread::spawn(move || loop {
            let mut head = Vec::new();
            let mut b = [0u8; 1024];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match s.read(&mut b) { Ok(0) | Err(_) => return, Ok(n) => head.extend_from_slice(&b[..n]) }
            }
            let line = String::from_utf8_lossy(&head).lines().next().unwrap_or("").to_string();
            let r = match line.split(' ').nth(1).unwrap_or("/") {
                "/start" => "HTTP/1.1 302 Found\r\nSet-Cookie: c=1; Path=/\r\nLocation: /check\r\nContent-Length: 0\r\n\r\n".to_string(),
                "/check" => "HTTP/1.1 302 Found\r\nLocation: /start\r\nContent-Length: 0\r\n\r\n".to_string(),
                _ => "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_string(),
            };
            if s.write_all(r.as_bytes()).is_err() { return }
        });
    });
    addr
}

/// `N <url>`: the page with its chain, also when the chain loops.
#[test]
fn a_navigation_is_answered_with_its_chain() {
    let net = ToServer { addr: cookie_check_server(), lookups: AtomicUsize::new(0) };
    let conv = Conversation::new(&net, tls(), GRANT);
    let mut out = Vec::new();
    navigator_fetch::serve(&conv, 1, &b"N http://site.test/start\nN http://site.test/plain\n"[..], &mut out, &mut |_| {});
    let text = String::from_utf8(out).unwrap();
    assert_eq!(text, "err TooManyRedirects\nhops 3\n302 1 http://site.test/start\n302 0 http://site.test/check\n0 0 http://site.test/start\n\
                      ok 200 0 2 http://site.test/plain\nokhops 1\n200 0 http://site.test/plain\n");
}
