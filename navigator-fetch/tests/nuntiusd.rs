//! ★ NUNTIUS ON ITS CHANNEL (nuntius.md §6 step 2): the `nuntius` crate's
//! client against the fetcher's channel loop (`serve_channel`, what
//! `nuntiusd` runs), over a real socket pair and a loopback server — Stream
//! with flow control, Fd with its descriptor, Cancel, a client that hangs
//! up, one that sends garbage — and the `nuntiusd` process itself.

use navigator_fetch::nuntius::serve_channel;
use navigator_fetch::{addr::Policy, Connect, Conversation};
use nuntius::{Client, Deliver, ErrorCode, Event, Open};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

const SIZE: usize = 300_000;
fn byte(i: usize) -> u8 { (i % 251) as u8 }

/// A server that sends SIZE bytes, `pause_ms` between 16 KiB writes.
fn server(pause_ms: u64) -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || for s in l.incoming() {
        let Ok(mut s) = s else { return };
        std::thread::spawn(move || {
            let mut head = Vec::new();
            let mut b = [0u8; 4096];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match s.read(&mut b) { Ok(0) | Err(_) => return, Ok(n) => head.extend_from_slice(&b[..n]) }
            }
            let _ = s.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {SIZE}\r\nETag: \"v1\"\r\nContent-Type: video/webm\r\n\r\n").as_bytes());
            let body: Vec<u8> = (0..SIZE).map(byte).collect();
            for c in body.chunks(16 * 1024) {
                if s.write_all(c).is_err() { return }
                if pause_ms > 0 { std::thread::sleep(Duration::from_millis(pause_ms)) }
            }
            let _ = s.read(&mut b);
        });
    });
    addr
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

/// A fetcher serving one channel; the client's end, and the loop's result.
fn fetcher(addr: SocketAddr, credit: u64) -> (Client, std::thread::JoinHandle<std::io::Result<()>>) {
    let (a, b) = UnixStream::pair().unwrap();
    let net: &'static ToServer = Box::leak(Box::new(ToServer(addr)));
    let conv = Conversation::new(net, tls(), Policy { grant_private_network: true });
    let h = std::thread::spawn(move || serve_channel(b, conv, credit));
    (Client::from_stream(a).unwrap(), h)
}

fn open(id: u32, deliver: Deliver) -> Open {
    let mut o = Open::get(id, "http://media.test/v.webm");
    o.deliver = deliver;
    o.limits.idle_ms = 2_000;
    o
}

fn next(c: &mut Client) -> Event { c.recv_timeout(Duration::from_secs(20)).unwrap().expect("an event in time") }

#[test]
fn stream_over_the_channel_waits_for_credit_and_numbers_absolutely() {
    let (mut c, _) = fetcher(server(0), 0);
    c.open(open(7, Deliver::Stream)).unwrap();
    match next(&mut c) {
        Event::Head { id: 7, status: 200, length: Some(l), validator: Some(v), content_type: Some(t), .. } => {
            assert_eq!(l, SIZE as u64);
            assert_eq!(v.value(), "\"v1\"");
            assert_eq!(t, "video/webm");
        }
        other => panic!("{other:?}"),
    }
    // No credit: no bytes.
    assert!(c.recv_timeout(Duration::from_millis(400)).unwrap().is_none_or(|e| !matches!(e, Event::Data { .. })), "no Data without credit");
    // Exactly the resource's credit: it must end at its length, not wait for
    // credit to read the server's end of body.
    c.budget(SIZE as u64).unwrap();
    let mut got = vec![];
    loop {
        match next(&mut c) {
            Event::Data { id: 7, offset, bytes } => {
                assert_eq!(offset as usize, got.len(), "absolute, contiguous offsets");
                assert!(bytes.len() <= nuntius::DATA_MAX);
                got.extend_from_slice(&bytes);
            }
            Event::Progress { .. } => {}
            Event::Done { id: 7, bytes } => { assert_eq!(bytes, SIZE as u64); break }
            other => panic!("{other:?}"),
        }
    }
    assert!(got.iter().enumerate().all(|(i, b)| *b == byte(i)), "the bytes, in order");
}

#[test]
fn fd_delivery_hands_over_a_read_only_descriptor() {
    let (mut c, _) = fetcher(server(0), nuntius_credit());
    c.open(open(1, Deliver::Fd)).unwrap();
    assert!(matches!(next(&mut c), Event::Head { id: 1, .. }));
    let fd = match next(&mut c) { Event::Fd { id: 1, fd } => fd, other => panic!("{other:?}") };
    loop {
        match next(&mut c) {
            Event::Progress { .. } => {}
            Event::Done { id: 1, bytes } => { assert_eq!(bytes, SIZE as u64); break }
            other => panic!("{other:?}"),
        }
    }
    let mut f = std::fs::File::from(fd);
    let mut got = vec![];
    f.read_to_end(&mut got).unwrap();
    assert_eq!(got.len(), SIZE);
    assert!(got.iter().enumerate().all(|(i, b)| *b == byte(i)));
    assert!(f.write_all(b"x").is_err(), "the client's descriptor is read-only");
}

fn nuntius_credit() -> u64 { navigator_fetch::nuntius::CHANNEL_CREDIT }

#[test]
fn cancel_ends_a_request_and_a_split_client_works_from_two_threads() {
    let (c, _) = fetcher(server(50), nuntius_credit());
    let (mut tx, mut rx) = c.split();
    tx.send(&nuntius::Request::Open(open(3, Deliver::Stream))).unwrap();
    let waiter = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let e = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap().expect("in time");
            if e.is_last() { return e }
        }
    });
    std::thread::sleep(Duration::from_millis(200));
    tx.send(&nuntius::Request::Cancel { id: 3 }).unwrap();
    match waiter.join().unwrap() { Event::Error { id: 3, code: ErrorCode::Cancelled } => {}, other => panic!("{other:?}") }
}

#[test]
fn a_request_over_the_limits_is_refused_before_it_is_sent() {
    let (mut c, _) = fetcher(server(0), 0);
    let mut o = open(1, Deliver::Stream);
    o.url = format!("http://media.test/{}", "a".repeat(nuntius::URL_MAX));
    assert_eq!(c.open(o).unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
    let mut o = open(1, Deliver::Stream);
    o.url = "http://media.test/\r\nHost: evil".into();
    assert_eq!(c.open(o).unwrap_err().kind(), std::io::ErrorKind::InvalidInput, "no control characters");
}

#[test]
fn garbage_from_the_client_ends_the_channel() {
    let (a, b) = UnixStream::pair().unwrap();
    let net: &'static ToServer = Box::leak(Box::new(ToServer(server(0))));
    let conv = Conversation::new(net, tls(), Policy { grant_private_network: true });
    let h = std::thread::spawn(move || serve_channel(b, conv, 0));
    let mut raw = aqueduct::Connection::wrap(a.try_clone().unwrap()).unwrap();
    raw.send_message(nuntius::wire::CLASS, nuntius::wire::OP_REQUEST, 0, &[0xff, 0xff, 0xff]).unwrap();
    let r = h.join().unwrap();
    assert_eq!(r.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    let mut c = Client::from_stream(a).unwrap();
    assert!(c.recv().is_err(), "the channel is closed");
}

#[test]
fn a_client_that_hangs_up_ends_the_loop_and_its_requests() {
    let (mut c, h) = fetcher(server(100), nuntius_credit());
    c.open(open(1, Deliver::Stream)).unwrap();
    assert!(matches!(next(&mut c), Event::Head { .. }));
    drop(c);
    let t = Instant::now();
    h.join().unwrap().unwrap();
    assert!(t.elapsed() < Duration::from_secs(5));
}

/// The `nuntiusd` process: its channel is its standard input.
#[test]
fn nuntiusd_serves_its_channel_with_the_address_policy() {
    let exe = env!("CARGO_BIN_EXE_nuntiusd");
    // A trust store is read at start: the system's.
    let Some(pem) = ["/etc/ssl/cert.pem", "/etc/ssl/certs/ca-certificates.crt"].iter().map(std::path::Path::new).find(|p| p.exists()) else {
        eprintln!("no system trust store; skipped");
        return;
    };
    let (a, b) = UnixStream::pair().unwrap();
    let mut child = std::process::Command::new(exe).args(["--unconfined-for-testing", "--trust", pem.to_str().unwrap()])
        .env_remove("NAVIGATOR_FETCH_GRANT_PRIVATE_NETWORK")
        .stdin(std::process::Stdio::from(std::os::fd::OwnedFd::from(b))).stderr(std::process::Stdio::null()).spawn().unwrap();
    let mut c = Client::from_stream(a).unwrap();
    // Loopback without the grant: refused by policy, before any connect.
    c.open(Open::get(9, "http://127.0.0.1/")).unwrap();
    match next(&mut c) { Event::Error { id: 9, code: ErrorCode::Policy } => {}, other => panic!("{other:?}") }
    c.open(Open::get(10, "http://169.254.169.254/latest/meta-data/")).unwrap();
    match next(&mut c) { Event::Error { id: 10, code: ErrorCode::Policy } => {}, other => panic!("{other:?}") }
    drop(c);
    let st = child.wait().unwrap();
    assert!(st.success(), "exits cleanly when the client hangs up: {st:?}");
    // Not a socket on stdin: refused.
    let st = std::process::Command::new(exe).args(["--unconfined-for-testing", "--trust", pem.to_str().unwrap()])
        .stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().unwrap();
    assert_eq!(st.code(), Some(1));
}
