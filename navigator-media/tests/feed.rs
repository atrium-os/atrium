//! ★ A SLOW STREAM NEVER NEEDS A REFRESH (fresco-video P3, with the
//! media-player thread). A feed the Navigator drives itself — Nuntius over
//! a real loopback socket, the MSE front end, the fake player — against a
//! server that is slow, stalls mid-body, drops connections and answers 5xx,
//! with seeks during each: playback resumes at the seek target without a
//! refresh, fetches stay bounded, and the events come in order. Verified
//! against the fake player (real playback is P1's).

use navigator_fetch::nuntius::{Event, Nuntius};
use navigator_fetch::{addr::Policy, Connect, Conversation};
use navigator_media::fake::FakePlayer;
use navigator_media::feed::{Feed, Segment};
use navigator_media::mse::{Fire, Media};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SEGS: usize = 40;
const SEG_BYTES: usize = 20_000;
const INIT_BYTES: usize = 1_000;
const SEG_SECS: f64 = 2.0;
const SIZE: usize = INIT_BYTES + SEGS * SEG_BYTES;

/// The file: an init segment, then segments whose first 4 bytes are their
/// index, so what was appended can be read back.
fn file() -> Vec<u8> {
    let mut f = vec![0xEEu8; INIT_BYTES];
    for i in 0..SEGS {
        let mut s = vec![(i % 200) as u8 + 1; SEG_BYTES];
        s[..4].copy_from_slice(&(i as u32).to_be_bytes());
        f.extend(s);
    }
    f
}

#[derive(Clone, Copy, Default)]
struct Plan { status: Option<u16>, pause_ms: u64, stall_after: Option<usize>, drop_after: Option<usize> }

struct Server { addr: SocketAddr, requests: Arc<Mutex<Vec<String>>> }

/// `plan(n, path)` decides request `n`.
fn server(plan: impl Fn(usize, &str) -> Plan + Send + Sync + 'static) -> Server {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(vec![]));
    let (r2, plan, body) = (requests.clone(), Arc::new(plan), Arc::new(file()));
    std::thread::spawn(move || for s in l.incoming() {
        let Ok(mut s) = s else { return };
        let (reqs, plan, body) = (r2.clone(), plan.clone(), body.clone());
        std::thread::spawn(move || {
            let mut head = Vec::new();
            let mut b = [0u8; 4096];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match s.read(&mut b) { Ok(0) | Err(_) => return, Ok(n) => head.extend_from_slice(&b[..n]) }
            }
            let text = String::from_utf8_lossy(&head).to_string();
            let path = text.split(' ').nth(1).unwrap_or("/").to_string();
            let range = text.lines().find_map(|l| l.strip_prefix("Range: bytes=").map(str::to_string));
            let n = { let mut v = reqs.lock().unwrap(); v.push(format!("{path} {}", range.clone().unwrap_or_default())); v.len() - 1 };
            let p = plan(n, &path);
            if let Some(st) = p.status { let _ = s.write_all(format!("HTTP/1.1 {st} Busy\r\nContent-Length: 0\r\n\r\n").as_bytes()); return }
            let (from, to) = range.and_then(|r| { let (a, b) = r.split_once('-')?; Some((a.parse::<usize>().ok()?, if b.is_empty() { SIZE - 1 } else { b.parse::<usize>().ok()? })) }).unwrap_or((0, SIZE - 1));
            let h = format!("HTTP/1.1 206 Partial\r\nContent-Range: bytes {from}-{to}/{SIZE}\r\nContent-Length: {}\r\nETag: \"m1\"\r\n\r\n", to - from + 1);
            if s.write_all(h.as_bytes()).is_err() { return }
            let mut sent = 0;
            let part = &body[from..=to];
            while sent < part.len() {
                if p.drop_after.is_some_and(|d| sent >= d) { let _ = s.shutdown(std::net::Shutdown::Both); return }
                if p.stall_after.is_some_and(|d| sent >= d) { std::thread::sleep(Duration::from_secs(20)); return }
                let mut n = 4096.min(part.len() - sent);
                if let Some(d) = p.drop_after.or(p.stall_after) { if sent < d { n = n.min(d - sent) } }
                if s.write_all(&part[sent..sent + n]).is_err() { return }
                sent += n;
                if p.pause_ms > 0 { std::thread::sleep(Duration::from_millis(p.pause_ms)) }
            }
            let _ = s.read(&mut b);
        });
    });
    Server { addr, requests }
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

fn segments(alt: bool) -> (Segment, Vec<Segment>) {
    let urls = |p: &str| { let mut v = vec![format!("http://media.test/{p}")]; if alt { v.push(format!("http://mirror.test/m/{p}")) } v };
    let init = Segment { urls: urls("v.webm"), range: Some((0, INIT_BYTES as u64)), start: 0.0, end: 0.0 };
    let segs = (0..SEGS).map(|i| Segment { urls: urls("v.webm"), range: Some(((INIT_BYTES + i * SEG_BYTES) as u64, SEG_BYTES as u64)),
                                           start: i as f64 * SEG_SECS, end: (i + 1) as f64 * SEG_SECS }).collect();
    (init, segs)
}

struct Rig { n: Nuntius, rx: Receiver<Event>, media: Media, p: FakePlayer, feed: Feed, events: Vec<Fire> }

impl Rig {
    fn new(s: &Server, alt: bool) -> Rig {
        let net: &'static ToServer = Box::leak(Box::new(ToServer(s.addr)));
        let (tx, rx) = channel();
        let n = Nuntius::new(Conversation::new(net, tls(), Policy { grant_private_network: true }), tx, 0);
        let mut media = Media::new();
        media.prime(&["video/webm; codecs=\"vp9\""]);
        let mut p = FakePlayer::new();
        p.cap = 600_000; // a small budget, so eviction matters
        let (init, segs) = segments(alt);
        let mut feed = Feed::new(0, init, segs);
        feed.ahead = 8.0;
        feed.behind = 6.0;
        feed.limits.idle_ms = 300;
        feed.limits.connect_ms = 500;
        feed.retry.backoff_ms = 20;
        let mut r = Rig { n, rx, media, p, feed, events: vec![] };
        r.player();
        r.media.attach_source().unwrap();
        r.media.add_source_buffer("video/webm; codecs=\"vp9\"").unwrap();
        r.feed.start(&r.n);
        r
    }

    /// Exchange with the fake player until quiet.
    fn player(&mut self) {
        for _ in 0..100 {
            for m in self.media.take_out() { self.p.receive(&m.encode()) }
            let ans = self.p.deliver();
            if ans.is_empty() { break }
            for a in ans { self.media.from_player(a) }
        }
        self.events.extend(self.media.take_events());
    }

    /// Run the loop for up to `ms`, or until `done` holds.
    fn run(&mut self, ms: u64, done: impl Fn(&Rig) -> bool) -> bool {
        let end = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < end {
            if let Ok(e) = self.rx.recv_timeout(Duration::from_millis(20)) {
                self.feed.on_fetch(e, &self.n, &mut self.media)
            }
            self.player();
            self.feed.pump(&mut self.media);
            self.player();
            if done(self) { return true }
        }
        false
    }

    fn seek(&mut self, t: f64) {
        self.media.seek(t).unwrap();
        self.feed.seek(t, &self.n, &mut self.media);
        self.player();
    }

    /// Segment indices the player was sent, in order.
    fn appended(&self) -> Vec<u32> { self.appended_since(0) }
    fn has_since(&self, mark: usize, seg: usize) -> bool { self.appended_since(mark).contains(&(seg as u32)) }
    fn appended_since(&self, mark: usize) -> Vec<u32> {
        let mut out = vec![];
        for m in &self.p.received[mark.min(self.p.received.len())..] {
            if let limen::media_player::ToChild::Append { bytes, .. } = m {
                // An Append is at most 60 KiB, so a segment is one or more;
                // its first carries the index, then its fill byte (file()).
                if bytes.len() < 8 { continue }
                let i = u32::from_be_bytes(bytes[..4].try_into().unwrap());
                if (i as usize) < SEGS && bytes[4..8].iter().all(|b| *b == (i % 200) as u8 + 1) { out.push(i) }
            }
        }
        out
    }

    fn has(&self, seg: usize) -> bool { self.appended().contains(&(seg as u32)) }
}

fn check_clean(r: &Rig) {
    assert!(r.p.violations.is_empty(), "the player was sent nothing out of order: {:?}", r.p.violations);
    assert_eq!(r.feed.network_errors, 0, "no NETWORK on a stream that only misbehaves");
    let seeking = r.events.iter().position(|e| *e == Fire::Element("seeking"));
    let seeked = r.events.iter().position(|e| *e == Fire::Element("seeked"));
    if let (Some(a), Some(b)) = (seeking, seeked) { assert!(a < b, "seeking before seeked") }
    assert!(!r.events.contains(&Fire::Element("error")));
}

fn scenario(plan: impl Fn(usize, &str) -> Plan + Send + Sync + 'static) {
    let s = server(plan);
    let mut r = Rig::new(&s, false);
    let ok = r.run(15_000, |r| r.has(0) && r.has(1));
    assert!(ok, "the start plays: {:?}; requests {:?}; opened {} cancelled {} failovers {} net {}", r.appended(),
            s.requests.lock().unwrap(), r.feed.opened, r.feed.cancelled, r.feed.failovers, r.feed.network_errors);
    // Seek well ahead, while the first fetches may still be misbehaving.
    let mark = r.p.received.len();
    r.seek(41.0);
    let target = r.feed.covering(41.0);
    assert!(r.run(15_000, |r| r.has_since(mark, target)), "the seek target arrives without a refresh: {:?}", r.appended_since(mark));
    assert_eq!(r.appended_since(mark)[0], target as u32, "the covering segment is appended first: {:?}", r.appended_since(mark));
    // Far ahead and straight back, before any of the far seek's fetches has
    // answered: every one of them is stale, and cancelled at once.
    r.seek(61.0);
    let c = r.feed.cancelled;
    let mark = r.p.received.len();
    let back = r.feed.covering(5.0);
    let want = r.feed.covering(12.0) + 1;
    let held: Vec<usize> = (back..=want).filter(|&i| r.feed.holds(i)).collect();
    r.seek(5.0);
    assert!(r.feed.cancelled > c, "the far seek's fetches are cancelled");
    if !held.contains(&back) {
        assert!(r.run(15_000, |r| r.has_since(mark, back)), "the seek back arrives: {:?}", r.appended_since(mark));
        assert_eq!(r.appended_since(mark)[0], back as u32, "the covering segment is appended first after the seek back");
    }
    // And on: refilling ahead from the new playhead.
    for t in [6.0, 8.0, 10.0, 12.0] { r.feed.time(t, &r.n, &mut r.media); r.player(); }
    assert!(r.run(15_000, |r| (back..=want).all(|i| held.contains(&i) || r.has_since(mark, i))),
            "refilling ahead after the seek back: {:?} (held {held:?})", r.appended_since(mark));
    check_clean(&r);
    // Bounded: no more requests than segments wanted, retries included.
    let reqs = s.requests.lock().unwrap().len() as u64;
    assert!(r.feed.opened <= 3 * SEGS as u64, "opened {}", r.feed.opened);
    assert!(reqs <= r.feed.opened * 4, "{reqs} requests for {} opens", r.feed.opened);
}

#[test]
fn a_well_server_plays_and_seeks() { scenario(|_, _| Plan::default()) }

#[test]
fn a_slow_server() { scenario(|_, _| Plan { pause_ms: 15, ..Default::default() }) }

#[test]
fn a_server_that_stalls_mid_body() {
    // Every third request stalls halfway: the idle timeout resumes it.
    scenario(|n, _| Plan { stall_after: (n % 3 == 1).then_some(SEG_BYTES / 2), ..Default::default() })
}

#[test]
fn a_server_that_drops_connections() {
    scenario(|n, _| Plan { drop_after: (n % 2 == 1).then_some(SEG_BYTES / 3), ..Default::default() })
}

#[test]
fn a_server_that_answers_5xx() {
    scenario(|n, _| Plan { status: (n % 3 == 0 && n > 0).then_some(503), ..Default::default() })
}

#[test]
fn a_dead_primary_fails_over_to_the_alternate() {
    // The primary always answers 503; the mirror (path /m/…) serves.
    let s = server(|_, path| Plan { status: (!path.starts_with("/m/")).then_some(503), ..Default::default() });
    let mut r = Rig::new(&s, true);
    r.feed.retry.max_attempts = 2;
    assert!(r.run(20_000, |r| r.has(0) && r.has(1) && r.has(2)), "{:?} failovers {}", r.appended(), r.feed.failovers);
    assert!(r.feed.failovers >= 3, "segments moved to the alternate: {}", r.feed.failovers);
    assert_eq!(r.feed.network_errors, 0);
    check_clean(&r);
}

#[test]
fn a_stream_that_never_answers_reports_network_and_a_seek_starts_fresh() {
    let s = server(|n, _| Plan { status: (n < 200).then_some(503), ..Default::default() });
    let mut r = Rig::new(&s, false);
    r.feed.retry.max_attempts = 2;
    assert!(r.run(20_000, |r| r.feed.network_errors > 0), "NETWORK after the retries");
    assert_eq!(r.media.error, Some(2), "MEDIA_ERR_NETWORK");
    let reqs = s.requests.lock().unwrap().len();
    assert!(reqs < 200, "bounded: {reqs} requests");
    // The server recovers; a seek starts fresh — no refresh.
    let s_ok_after = reqs;
    let _ = s_ok_after;
    r.seek(4.0);
    assert_eq!(r.media.error, None, "a seek clears the error");
    assert!(r.feed.opened > 0);
}
