//! The worker against a fake LAN on loopback: a responder answers its
//! questions; then a flood of junk and of distinct instances, which must
//! leave it bounded and answering.

use std::net::UdpSocket;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use prelum_browse::{run, Opts};
use prelum_proto::browse::{Ev, Req};
use prelum_proto::frame;

fn name(s: &str) -> Vec<u8> {
    let mut v = Vec::new();
    for l in s.split('.').filter(|l| !l.is_empty()) {
        v.push(l.len() as u8);
        v.extend_from_slice(l.as_bytes());
    }
    v.push(0);
    v
}

struct Resp {
    out: Vec<u8>,
    n: u16,
}

impl Resp {
    fn new() -> Resp {
        Resp { out: vec![0, 0, 0x84, 0, 0, 0, 0, 0, 0, 0, 0, 0], n: 0 }
    }
    fn rr(&mut self, owner: &[u8], t: u16, ttl: u32, rdata: &[u8]) -> &mut Resp {
        self.out.extend_from_slice(owner);
        self.out.extend_from_slice(&t.to_be_bytes());
        self.out.extend_from_slice(&0x8001u16.to_be_bytes());
        self.out.extend_from_slice(&ttl.to_be_bytes());
        self.out.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        self.out.extend_from_slice(rdata);
        self.n += 1;
        self
    }
    fn done(&mut self) -> Vec<u8> {
        let mut o = self.out.clone();
        o[6..8].copy_from_slice(&self.n.to_be_bytes());
        o
    }
}

/// A printer `inst` under `_ipps._tcp.local` at `host`, 10.9.0.`last`:631.
fn printer(inst: &str, host: &str, last: u8) -> Vec<u8> {
    let full = format!("{inst}._ipps._tcp.local");
    let mut srv = vec![0, 0, 0, 0, 0x02, 0x77];
    srv.extend_from_slice(&name(host));
    let mut txt = Vec::new();
    for s in ["rp=ipp/print", "ty=Test Printer", "pdl=application/pdf"] {
        txt.push(s.len() as u8);
        txt.extend_from_slice(s.as_bytes());
    }
    Resp::new()
        .rr(&name("_ipps._tcp.local"), 12, 120, &name(&full))
        .rr(&name(&full), 33, 120, &srv)
        .rr(&name(&full), 16, 120, &txt)
        .rr(&name(host), 1, 120, &[10, 9, 0, last])
        .done()
}

/// The worker on loopback: its recv socket, its send socket connected to
/// `lan`; returns prelumd's end.
fn start(lan: &UdpSocket) -> (UnixStream, std::net::SocketAddr, std::thread::JoinHandle<prelum_browse::Stats>) {
    let recv = UdpSocket::bind("127.0.0.1:0").unwrap();
    let to = recv.local_addr().unwrap();
    let send = UdpSocket::bind("127.0.0.1:0").unwrap();
    send.connect(lan.local_addr().unwrap()).unwrap();
    let (ours, theirs) = UnixStream::pair().unwrap();
    let o = Opts { max_interval: Duration::from_secs(2), ..Opts::default() };
    let h = std::thread::spawn(move || run(theirs, recv, send, &o).unwrap());
    (ours, to, h)
}

fn services_until(c: &mut UnixStream, ok: impl Fn(&[prelum_dnssd::Service]) -> bool, secs: u64) -> Vec<prelum_dnssd::Service> {
    c.set_read_timeout(Some(Duration::from_secs(secs))).unwrap();
    let mut last = Vec::new();
    loop {
        match frame::read::<Ev>(c) {
            Ok(Some(Ev::Services(s))) => {
                if ok(&s) {
                    return s;
                }
                last = s;
            }
            Ok(Some(Ev::Failed(m))) => panic!("worker failed: {m}"),
            _ => panic!("no such list in {secs} s; last had {}", last.len()),
        }
    }
}

#[test]
fn finds_a_printer_then_stays_bounded_under_a_flood() {
    let lan = UdpSocket::bind("127.0.0.1:0").unwrap();
    lan.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let (mut c, to, h) = start(&lan);
    // It asks; the LAN answers.
    let mut q = [0u8; 1500];
    let n = lan.recv(&mut q).unwrap();
    assert_eq!(q[2] & 0x80, 0, "a query");
    assert!(n > 12 && q[5] == 1, "one question a datagram");
    lan.send_to(&printer("Office", "office.local", 20), to).unwrap();
    let s = services_until(&mut c, |s| !s.is_empty(), 5);
    assert_eq!(s[0].instance, "Office");
    assert_eq!(s[0].port, 631);
    assert_eq!(s[0].txt.rp.as_deref(), Some("ipp/print"));

    // The flood: garbage, answers with a compression loop, oversized and
    // empty datagrams, and 1000 distinct printers.
    let t0 = Instant::now();
    let mut junk = Resp::new().rr(&[0xC0, 12], 12, 120, &[0xC0, 12]).done();
    junk.truncate(40);
    for i in 0..3000u32 {
        let d = match i % 4 {
            0 => (0..(i % 1400)).map(|j| (j * 37 + i) as u8).collect::<Vec<u8>>(),
            1 => junk.clone(),
            2 => vec![0u8; 9500],
            _ => printer(&format!("Junk {i}"), &format!("h{i}.local"), (i % 250) as u8),
        };
        let _ = lan.send_to(&d, to);
        if i % 200 == 0 {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    // Still answering prelumd, and the list is capped.
    frame::write(&mut c, &Req::Query).unwrap();
    let s = services_until(&mut c, |s| s.len() >= prelum_dnssd::MAX_SERVICES, 20);
    assert_eq!(s.len(), prelum_dnssd::MAX_SERVICES);
    assert!(s.iter().any(|x| x.instance == "Office"), "what was there first stays");
    assert!(t0.elapsed() < Duration::from_secs(20));
    // prelumd hangs up: the worker ends.
    drop(c);
    let st = h.join().unwrap();
    // The kernel drops what the socket can't hold; what came was read.
    assert!(st.refused >= 500, "{st:?}");
    assert!(st.parsed >= prelum_dnssd::MAX_SERVICES as u64, "{st:?}");
}

#[test]
fn asks_again_for_what_an_answer_left_out() {
    let lan = UdpSocket::bind("127.0.0.1:0").unwrap();
    lan.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let (mut c, to, _h) = start(&lan);
    let mut q = [0u8; 1500];
    lan.recv(&mut q).unwrap();
    // Only the PTR: the worker must ask for the SRV and TXT.
    lan.send_to(&Resp::new().rr(&name("_ipps._tcp.local"), 12, 120, &name("Lone._ipps._tcp.local")).done(), to).unwrap();
    let want = name("Lone._ipps._tcp.local");
    let t0 = Instant::now();
    loop {
        assert!(t0.elapsed() < Duration::from_secs(5), "no follow-up question");
        let n = lan.recv(&mut q).unwrap();
        if q[12..n].windows(want.len()).any(|w| w == &want[..]) {
            break;
        }
    }
    lan.send_to(&printer("Lone", "lone.local", 9), to).unwrap();
    let s = services_until(&mut c, |s| s.iter().any(|x| x.instance == "Lone"), 5);
    assert_eq!(s.len(), 1);
}
