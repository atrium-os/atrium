//! ★ NUNTIUS v1 against a server that misbehaves (nuntius.md §6 step 1):
//! slow, stalling mid-body, dropping the connection, answering 5xx,
//! changing the resource under a resume — over a real loopback socket, with
//! the fetcher's own address policy (loopback needs the grant, which these
//! conversations hold; a URL that names loopback directly is still refused
//! without it).

use navigator_fetch::nuntius::*;
use navigator_fetch::{addr::Policy, Connect, Conversation};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SIZE: usize = 400_000;
fn byte(i: usize) -> u8 { (i % 251) as u8 }

/// What the server does with one request.
#[derive(Clone, Debug, Default)]
struct Plan {
    status: Option<u16>,
    /// Bytes per write and the pause between writes.
    chunk: usize,
    pause_ms: u64,
    /// After this many body bytes: stall (sleep this long), or drop.
    stall_after: Option<(usize, u64)>,
    drop_after: Option<usize>,
    /// The resource's ETag (the If-Range check uses it).
    etag: &'static str,
    /// Never answer at all.
    silent: bool,
    /// A redirect to this URL.
    redirect: Option<&'static str>,
}

#[derive(Clone, Debug)]
struct Seen { range: Option<String>, if_range: Option<String>, path: String }

struct Server { addr: SocketAddr, seen: Arc<Mutex<Vec<Seen>>>, closed: Arc<AtomicUsize> }

/// A server: `plan(n)` decides request `n` (0-based).
fn server(plan: impl Fn(usize) -> Plan + Send + Sync + 'static) -> Server {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(vec![]));
    let closed = Arc::new(AtomicUsize::new(0));
    let (s2, c2, plan) = (seen.clone(), closed.clone(), Arc::new(plan));
    std::thread::spawn(move || for s in l.incoming() {
        let Ok(mut s) = s else { return };
        let (seen, closed, plan) = (s2.clone(), c2.clone(), plan.clone());
        std::thread::spawn(move || {
            let mut head = Vec::new();
            let mut b = [0u8; 4096];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match s.read(&mut b) { Ok(0) | Err(_) => return, Ok(n) => head.extend_from_slice(&b[..n]) }
            }
            let text = String::from_utf8_lossy(&head).to_string();
            let hdr = |n: &str| text.lines().find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, v)| v.trim().to_string()));
            let path = text.split(' ').nth(1).unwrap_or("/").to_string();
            let n = { let mut v = seen.lock().unwrap(); v.push(Seen { range: hdr("range"), if_range: hdr("if-range"), path: path.clone() }); v.len() - 1 };
            let p = plan(n);
            if p.silent { std::thread::sleep(Duration::from_secs(30)); return }
            if let Some(to) = p.redirect {
                let _ = s.write_all(format!("HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\n\r\n").as_bytes());
                return;
            }
            if let Some(st) = p.status.filter(|s| *s != 200) {
                let _ = s.write_all(format!("HTTP/1.1 {st} Nope\r\nContent-Length: 0\r\n\r\n").as_bytes());
                return;
            }
            // Range, honoured when If-Range (if any) matches the ETag.
            let range = hdr("range").and_then(|r| r.strip_prefix("bytes=").map(str::to_string)).and_then(|r| {
                let (a, b) = r.split_once('-')?;
                let a: usize = a.parse().ok()?;
                let b: usize = if b.is_empty() { SIZE - 1 } else { b.parse().ok()? };
                Some((a, b.min(SIZE - 1)))
            }).filter(|_| hdr("if-range").is_none_or(|v| v == p.etag));
            let (from, to) = range.unwrap_or((0, SIZE - 1));
            let head = match range {
                Some(_) => format!("HTTP/1.1 206 Partial\r\nContent-Range: bytes {from}-{to}/{SIZE}\r\nContent-Length: {}\r\nETag: {}\r\nAccept-Ranges: bytes\r\nContent-Type: video/webm\r\n\r\n", to - from + 1, p.etag),
                None => format!("HTTP/1.1 200 OK\r\nContent-Length: {SIZE}\r\nETag: {}\r\nAccept-Ranges: bytes\r\nContent-Type: video/webm\r\n\r\n", p.etag),
            };
            if s.write_all(head.as_bytes()).is_err() { return }
            let body: Vec<u8> = (from..=to).map(byte).collect();
            let mut sent = 0;
            let chunk = if p.chunk == 0 { 16 * 1024 } else { p.chunk };
            while sent < body.len() {
                if let Some((at, ms)) = p.stall_after { if sent >= at { std::thread::sleep(Duration::from_millis(ms)); if s.write_all(&[]).is_err() { break } } }
                if p.drop_after.is_some_and(|d| sent >= d) { let _ = s.shutdown(std::net::Shutdown::Both); return }
                let n = chunk.min(body.len() - sent);
                let n = match (p.stall_after, p.drop_after) { (Some((at, _)), _) if sent < at => n.min(at - sent), (_, Some(d)) if sent < d => n.min(d - sent), _ => n };
                if s.write_all(&body[sent..sent + n]).is_err() { closed.fetch_add(1, Ordering::SeqCst); return }
                sent += n;
                if p.pause_ms > 0 { std::thread::sleep(Duration::from_millis(p.pause_ms)) }
            }
            // Wait for the client to close.
            let _ = s.read(&mut b);
            closed.fetch_add(1, Ordering::SeqCst);
        });
    });
    Server { addr, seen, closed }
}

struct ToServer(SocketAddr);
impl Connect for ToServer {
    fn resolve(&self, _: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> { Ok(vec![SocketAddr::new(self.0.ip(), port)]) }
    fn connect_addr(&self, _: SocketAddr) -> std::io::Result<TcpStream> { TcpStream::connect(self.0) }
}

fn tls() -> Arc<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    Arc::new(rustls::ClientConfig::builder_with_provider(provider).with_safe_default_protocol_versions().unwrap()
        .with_root_certificates(rustls::RootCertStore::empty()).with_no_client_auth())
}

fn instance(s: &Server, budget: u64, policy: Policy) -> (Nuntius, Receiver<Event>) {
    let net: &'static ToServer = Box::leak(Box::new(ToServer(s.addr)));
    let (tx, rx) = channel();
    (Nuntius::new(Conversation::new(net, tls(), policy), tx, budget), rx)
}
const GRANT: Policy = Policy { grant_private_network: true };

fn open(id: u32) -> Open {
    let mut o = Open::get(id, "http://media.test/v.webm");
    o.limits = Limits { max_bytes: 10 << 20, connect_ms: 1_000, idle_ms: 300, total_ms: 20_000 };
    o.retry = Retry { max_attempts: 4, backoff_ms: 20 };
    o
}

/// Collect a request's events until Done or Error; the bytes, in order.
fn collect(rx: &Receiver<Event>, id: u32) -> (Vec<String>, Vec<u8>, Option<ErrorCode>) { collect_from(rx, id, 0) }

fn collect_from(rx: &Receiver<Event>, id: u32, start: usize) -> (Vec<String>, Vec<u8>, Option<ErrorCode>) {
    let (mut kinds, mut bytes, mut err) = (vec![], vec![], None);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let e = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).expect("an event in time");
        assert_eq!(e.id(), id);
        match e {
            Event::Head { status, .. } => kinds.push(format!("head {status}")),
            Event::Data { offset, bytes: b, .. } => { assert_eq!(offset as usize, start + bytes.len(), "offsets are contiguous"); bytes.extend(b) }
            Event::Retrying { reason, .. } => kinds.push(format!("retry {reason}")),
            Event::Done { .. } => { kinds.push("done".into()); break }
            Event::Error { code, .. } => { err = Some(code); kinds.push("error".into()); break }
            Event::Progress { .. } | Event::Fd { .. } => {}
        }
    }
    (kinds, bytes, err)
}

fn whole() -> Vec<u8> { (0..SIZE).map(byte).collect() }

#[test]
fn a_resource_streams_in_order_and_whole() {
    let s = server(|_| Plan { etag: "\"v1\"", ..Default::default() });
    let (n, rx) = instance(&s, u64::MAX, GRANT);
    n.request(Request::Open(open(1)));
    let (k, b, err) = collect(&rx, 1);
    assert_eq!(err, None);
    assert_eq!(k, vec!["head 200", "done"]);
    assert_eq!(b, whole());
}

#[test]
fn a_slow_server_is_waited_for_within_the_idle_timeout() {
    let s = server(|_| Plan { etag: "\"v1\"", chunk: 40_000, pause_ms: 100, ..Default::default() });
    let (n, rx) = instance(&s, u64::MAX, GRANT);
    n.request(Request::Open(open(1)));
    let (k, b, _) = collect(&rx, 1);
    assert_eq!(k, vec!["head 200", "done"], "slow but never idle 300 ms: no retry");
    assert_eq!(b, whole());
}

#[test]
fn a_stall_mid_body_times_out_and_resumes_with_range_and_if_range() {
    let s = server(|n| Plan { etag: "\"v1\"", stall_after: (n == 0).then_some((150_000, 5_000)), ..Default::default() });
    let (n, rx) = instance(&s, u64::MAX, GRANT);
    let t = Instant::now();
    n.request(Request::Open(open(1)));
    let (k, b, err) = collect(&rx, 1);
    assert_eq!(err, None);
    assert_eq!(k, vec!["head 200", "retry timeout", "done"]);
    assert_eq!(b, whole(), "the resumed bytes continue the first");
    let seen = s.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[1].range.as_deref(), Some("bytes=150000-"));
    assert_eq!(seen[1].if_range.as_deref(), Some("\"v1\""));
    assert!(t.elapsed() < Duration::from_secs(3), "resumed after the idle timeout, not the server's stall");
}

#[test]
fn a_dropped_connection_resumes() {
    let s = server(|n| Plan { etag: "\"v1\"", drop_after: (n < 2).then_some(100_000 + n * 50_000), ..Default::default() });
    let (n, rx) = instance(&s, u64::MAX, GRANT);
    n.request(Request::Open(open(1)));
    let (k, b, err) = collect(&rx, 1);
    assert_eq!(err, None);
    assert_eq!(k, vec!["head 200", "retry reset", "retry reset", "done"]);
    assert_eq!(b, whole());
    let seen = s.seen.lock().unwrap();
    assert_eq!(seen[1].range.as_deref(), Some("bytes=100000-"));
    assert_eq!(seen[2].range.as_deref(), Some("bytes=250000-"), "each resume starts where the bytes stopped");
}

#[test]
fn server_errors_are_retried_and_client_errors_are_not() {
    let s = server(|n| Plan { etag: "\"v1\"", status: (n < 2).then_some(503), ..Default::default() });
    let (n, rx) = instance(&s, u64::MAX, GRANT);
    n.request(Request::Open(open(1)));
    let (k, b, _) = collect(&rx, 1);
    assert_eq!(k, vec!["retry server", "retry server", "head 200", "done"]);
    assert_eq!(b.len(), SIZE);
    let s = server(|_| Plan { status: Some(404), ..Default::default() });
    let (n, rx) = instance(&s, u64::MAX, GRANT);
    n.request(Request::Open(open(2)));
    let (k, _, err) = collect(&rx, 2);
    assert_eq!((k, err), (vec!["error".to_string()], Some(ErrorCode::Http(404))));
    assert_eq!(s.seen.lock().unwrap().len(), 1, "a 404 is not retried");
}

#[test]
fn a_resource_that_changed_is_not_spliced() {
    // The resume finds another version: the server answers 200 (If-Range
    // failed), and the fetcher refuses rather than join two files.
    let s = server(|n| Plan { etag: if n == 0 { "\"v1\"" } else { "\"v2\"" }, drop_after: (n == 0).then_some(100_000), ..Default::default() });
    let (n, rx) = instance(&s, u64::MAX, GRANT);
    n.request(Request::Open(open(1)));
    let (k, b, err) = collect(&rx, 1);
    assert_eq!(err, Some(ErrorCode::Malformed));
    assert_eq!(k, vec!["head 200", "retry reset", "error"]);
    assert_eq!(b.len(), 100_000, "only the first version's bytes");
}

#[test]
fn retries_are_bounded_and_a_later_request_starts_fresh() {
    let s = server(|n| Plan { etag: "\"v1\"", status: (n < 10).then_some(502), ..Default::default() });
    let (n, rx) = instance(&s, u64::MAX, GRANT);
    n.request(Request::Open(open(1)));
    let (k, _, err) = collect(&rx, 1);
    assert_eq!(err, Some(ErrorCode::Http(502)));
    assert_eq!(k.iter().filter(|k| k.starts_with("retry")).count(), 3, "four attempts in all");
    assert_eq!(s.seen.lock().unwrap().len(), 4);
    // Nothing wedged: a later request is a new one (and the server, now
    // past its bad patch, answers it).
    for id in 2..=3 { n.request(Request::Open(open(id))); let _ = collect(&rx, id); }
    n.request(Request::Open(open(4)));
    let (_, b, err) = collect(&rx, 4);
    assert_eq!((err, b.len()), (None, SIZE));
    assert_eq!(n.live(), 0);
}

#[test]
fn flow_control_reads_only_with_credit() {
    let s = server(|_| Plan { etag: "\"v1\"", ..Default::default() });
    let (n, rx) = instance(&s, 100_000, GRANT);
    n.request(Request::Open(open(1)));
    let mut got = 0usize;
    let deadline = Instant::now() + Duration::from_secs(5);
    while let Ok(e) = rx.recv_timeout(Duration::from_millis(300)) {
        if let Event::Data { bytes, .. } = e { got += bytes.len() }
        assert!(Instant::now() < deadline);
    }
    assert_eq!(got, 100_000, "exactly the credit, then it waits");
    n.request(Request::Budget { bytes: u64::MAX / 2 });
    let (_, rest, err) = collect_from(&rx, 1, got);
    assert_eq!(err, None);
    assert_eq!(got + rest.len(), SIZE);
}

#[test]
fn cancel_is_immediate_and_closes_the_connection() {
    let s = server(|_| Plan { etag: "\"v1\"", stall_after: Some((50_000, 10_000)), ..Default::default() });
    let (n, rx) = instance(&s, u64::MAX, GRANT);
    let mut o = open(1);
    o.limits.idle_ms = 10_000;
    n.request(Request::Open(o));
    // Wait for some data, then cancel during the stall.
    loop { if let Event::Data { offset, bytes, .. } = rx.recv_timeout(Duration::from_secs(5)).unwrap() { if offset as usize + bytes.len() >= 50_000 { break } } }
    std::thread::sleep(Duration::from_millis(100));
    let t = Instant::now();
    n.request(Request::Cancel { id: 1 });
    let (_, _, err) = collect(&rx, 1);
    assert_eq!(err, Some(ErrorCode::Cancelled));
    assert!(t.elapsed() < Duration::from_millis(500), "cancelled at once: {:?}", t.elapsed());
    assert_eq!(n.live(), 0);
}

#[test]
fn a_silent_server_hits_the_connect_timeout_and_is_retried() {
    let s = server(|n| Plan { etag: "\"v1\"", silent: n == 0, ..Default::default() });
    let (n, rx) = instance(&s, u64::MAX, GRANT);
    let mut o = open(1);
    o.limits.connect_ms = 300;
    n.request(Request::Open(o));
    let (k, b, err) = collect(&rx, 1);
    assert_eq!(err, None);
    assert_eq!(k, vec!["retry timeout", "head 200", "done"]);
    assert_eq!(b.len(), SIZE);
}

#[test]
fn a_range_is_asked_for_and_checked() {
    let s = server(|_| Plan { etag: "\"v1\"", ..Default::default() });
    let (n, rx) = instance(&s, u64::MAX, GRANT);
    let mut o = open(1);
    o.range = Some((1000, Some(5000)));
    n.request(Request::Open(o));
    let mut first_offset = None;
    let (mut b, mut done) = (vec![], false);
    while !done {
        match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            Event::Data { offset, bytes, .. } => { first_offset.get_or_insert(offset); b.extend(bytes) }
            Event::Done { .. } => done = true,
            Event::Error { code, .. } => panic!("{code:?}"),
            _ => {}
        }
    }
    assert_eq!(first_offset, Some(1000), "offsets are absolute in the resource");
    assert_eq!(b, (1000..6000).map(byte).collect::<Vec<_>>());
    assert_eq!(s.seen.lock().unwrap()[0].range.as_deref(), Some("bytes=1000-5999"));
}

#[test]
fn fd_delivery_fills_a_read_only_file() {
    let s = server(|_| Plan { etag: "\"v1\"", ..Default::default() });
    let (n, rx) = instance(&s, 0, GRANT); // no Stream credit needed
    let mut o = open(1);
    o.deliver = Deliver::Fd;
    n.request(Request::Open(o));
    let mut fd = None;
    loop {
        match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            Event::Head { length, .. } => assert_eq!(length, Some(SIZE as u64)),
            Event::Fd { fd: f, .. } => fd = Some(f),
            Event::Data { .. } => panic!("an Fd request sends no Data"),
            Event::Done { bytes, .. } => { assert_eq!(bytes, SIZE as u64); break }
            Event::Error { code, .. } => panic!("{code:?}"),
            _ => {}
        }
    }
    let mut f = std::fs::File::from(fd.expect("the descriptor came with Fd"));
    let mut all = vec![];
    f.read_to_end(&mut all).unwrap();
    assert_eq!(all, whole());
    assert!(f.write_all(b"x").is_err(), "the descriptor is read-only");
}

#[test]
fn the_address_rule_and_redirect_rules_still_hold() {
    let s = server(|_| Plan { etag: "\"v1\"", ..Default::default() });
    // A URL that names loopback, without the grant: refused, never retried.
    let (n, rx) = instance(&s, u64::MAX, Policy::default());
    n.request(Request::Open(Open::get(1, "http://127.0.0.1/v.webm")));
    assert_eq!(collect(&rx, 1).2, Some(ErrorCode::Policy));
    // A name that resolves to loopback, without the grant: refused too.
    n.request(Request::Open(Open::get(2, "http://media.test/v.webm")));
    assert_eq!(collect(&rx, 2).2, Some(ErrorCode::Policy));
    assert!(s.seen.lock().unwrap().is_empty(), "nothing reached the server");
    // Redirects are followed; a downgrade from https is refused.
    let s = server(|n| Plan { etag: "\"v1\"", redirect: (n == 0).then_some("/moved.webm"), ..Default::default() });
    let (n, rx) = instance(&s, u64::MAX, GRANT);
    n.request(Request::Open(open(3)));
    let (k, b, _) = collect(&rx, 3);
    assert_eq!((k, b.len()), (vec!["head 200".to_string(), "done".to_string()], SIZE));
    assert_eq!(s.seen.lock().unwrap()[1].path, "/moved.webm");
}

#[test]
fn concurrent_requests_are_bounded() {
    let s = server(|_| Plan { etag: "\"v1\"", silent: true, ..Default::default() });
    let (n, rx) = instance(&s, u64::MAX, GRANT);
    for id in 0..(MAX_REQUESTS as u32 + 3) {
        let mut o = open(id);
        o.limits.connect_ms = 3_000;
        o.retry.max_attempts = 1;
        n.request(Request::Open(o));
    }
    let refused: Vec<u32> = (0..3).map(|_| rx.recv_timeout(Duration::from_secs(2)).unwrap()).map(|e| match e { Event::Error { id, code: ErrorCode::Malformed } => id, other => panic!("{other:?}") }).collect();
    assert_eq!(refused.len(), 3, "past 64 in flight, refused");
    for id in 0..MAX_REQUESTS as u32 { n.request(Request::Cancel { id }) }
}

#[test]
fn a_ranged_resume_with_exactly_its_credit_completes_without_waiting() {
    // Regression (fresco-video P3 feed stall test): a ranged request whose
    // budget was granted exactly — the Navigator's feed grants each segment
    // its length — spent its last credit on its last byte and then waited
    // for more credit until the total deadline instead of finishing.
    let s = server(|n| Plan { etag: "\"v1\"", stall_after: (n == 0).then_some((40_000, 5_000)), ..Default::default() });
    let (n, rx) = instance(&s, 100_000, GRANT);
    let mut o = open(1);
    o.range = Some((0, Some(100_000)));
    let t = Instant::now();
    n.request(Request::Open(o));
    let (k, b, err) = collect(&rx, 1);
    assert_eq!(err, None);
    assert_eq!(k.last().map(String::as_str), Some("done"), "{k:?}");
    assert_eq!(b, (0..100_000).map(byte).collect::<Vec<_>>());
    assert_eq!(s.seen.lock().unwrap()[1].range.as_deref(), Some("bytes=40000-99999"));
    assert!(t.elapsed() < Duration::from_secs(3), "done when the length is in hand: {:?}", t.elapsed());
    assert_eq!(n.budget(), 0, "the credit was spent exactly");
}

/// A server for POSTs: records each request's method, content type and body, and answers
/// `status` with the body reversed (or a redirect for 302).
fn post_server(status: u16) -> (SocketAddr, Arc<Mutex<Vec<(String, String, Vec<u8>)>>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(vec![]));
    let s2 = seen.clone();
    std::thread::spawn(move || for s in l.incoming() {
        let Ok(mut s) = s else { return };
        let seen = s2.clone();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let mut b = [0u8; 4096];
            let end = loop {
                if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") { break p + 4 }
                match s.read(&mut b) { Ok(0) | Err(_) => return, Ok(n) => buf.extend_from_slice(&b[..n]) }
            };
            let text = String::from_utf8_lossy(&buf[..end]).to_string();
            let hdr = |n: &str| text.lines().find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, v)| v.trim().to_string()));
            let len: usize = hdr("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
            while buf.len() < end + len {
                match s.read(&mut b) { Ok(0) | Err(_) => return, Ok(n) => buf.extend_from_slice(&b[..n]) }
            }
            let body = buf[end..end + len].to_vec();
            let method = text.split(' ').next().unwrap_or("").to_string();
            seen.lock().unwrap().push((method, hdr("content-type").unwrap_or_default(), body.clone()));
            let r = match status {
                302 => "HTTP/1.1 302 Found\r\nLocation: http://media.test/elsewhere\r\nContent-Length: 0\r\n\r\n".to_string().into_bytes(),
                200 => {
                    let mut out = format!("HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
                    out.extend(body.iter().rev());
                    out
                }
                st => format!("HTTP/1.1 {st} Nope\r\nContent-Length: 0\r\n\r\n").into_bytes(),
            };
            let _ = s.write_all(&r);
            let _ = s.read(&mut b);
        });
    });
    (addr, seen)
}

fn post_instance(addr: SocketAddr) -> (Nuntius, Receiver<Event>) {
    let net: &'static ToServer = Box::leak(Box::new(ToServer(addr)));
    let (tx, rx) = channel();
    (Nuntius::new(Conversation::new(net, tls(), GRANT), tx, u64::MAX), rx)
}

#[test]
fn a_post_sends_its_body_once_and_streams_the_answer() {
    let (addr, seen) = post_server(200);
    let (n, rx) = post_instance(addr);
    let body: Vec<u8> = (0..5000u32).map(|i| (i * 7) as u8).collect();
    n.request(Request::Open(Open::post(1, "http://licence.test/wv", "application/octet-stream", body.clone())));
    let (k, got, err) = collect(&rx, 1);
    assert_eq!(err, None);
    assert_eq!(k, vec!["head 200", "done"]);
    assert_eq!(got, body.iter().rev().copied().collect::<Vec<_>>());
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0], ("POST".to_string(), "application/octet-stream".to_string(), body));
}

#[test]
fn a_post_is_neither_retried_nor_redirected() {
    for (status, code) in [(500, ErrorCode::Http(500)), (302, ErrorCode::Policy)] {
        let (addr, seen) = post_server(status);
        let (n, rx) = post_instance(addr);
        let mut o = Open::post(1, "http://licence.test/wv", "application/octet-stream", vec![1, 2, 3]);
        o.retry = Retry { max_attempts: 4, backoff_ms: 10 };
        n.request(Request::Open(o));
        let (_, _, err) = collect(&rx, 1);
        assert_eq!(err, Some(code), "status {status}");
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(seen.lock().unwrap().len(), 1, "status {status}: one request only");
    }
}
