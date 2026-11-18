//! prelum-browse (printing.md §7.1): asks the LAN for `_ipp._tcp` and
//! `_ipps._tcp` and tells prelumd what answered.
//!
//! [`run`] takes the worker's socket to prelumd and the two mDNS sockets
//! (one to read, one to send on; see `prelum_proto::browse`). It never
//! answers a query and announces nothing. What it reads is parsed by
//! `prelum_dnssd` (bounded, fuzzed) into a cache of at most
//! `prelum_dnssd::MAX_SERVICES` services that forget themselves with their
//! TTL; a flood of answers costs a bounded parse each and can, at worst,
//! fill the cache.

#![forbid(unsafe_code)]

use std::io::{self, Write};
use std::net::UdpSocket;
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use prelum_dnssd::{query, query_for, Browser, Packet, Service, IPP, IPPS, MAX_PACKET};
use prelum_proto::browse::{Ev, Req};
use prelum_proto::frame;

pub struct Opts {
    /// The longest gap between two rounds of questions. Queries go out at
    /// 0, 1, 3, 7 … seconds, doubling up to this (RFC 6762 §5.2).
    pub max_interval: Duration,
    /// The shortest gap between two follow-up datagrams (SRV, TXT, A for
    /// an instance that came without them).
    pub follow_up: Duration,
    /// How often, at most, prelumd hears that the list changed.
    pub report: Duration,
}

impl Default for Opts {
    fn default() -> Opts {
        Opts { max_interval: Duration::from_secs(20), follow_up: Duration::from_secs(1), report: Duration::from_millis(250) }
    }
}

/// Counters, for tests and the log.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub datagrams: u64,
    pub parsed: u64,
    pub refused: u64,
    pub queries: u64,
    pub reports: u64,
}

enum In {
    Req(Req),
    Gone,
}

/// Runs until prelumd closes the socket. `recv` is read with `recv`, and
/// questions go out with `send` on `send` (connected): neither needs an
/// address, so this works in capability mode.
pub fn run(ctl: UnixStream, recv: UdpSocket, send: UdpSocket, o: &Opts) -> io::Result<Stats> {
    let mut out = ctl.try_clone()?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut r = ctl;
        loop {
            match frame::read::<Req>(&mut r) {
                Ok(Some(q)) => {
                    if tx.send(In::Req(q)).is_err() {
                        return;
                    }
                }
                _ => {
                    let _ = tx.send(In::Gone);
                    return;
                }
            }
        }
    });
    recv.set_read_timeout(Some(Duration::from_millis(100)))?;
    let start = Instant::now();
    let secs = || start.elapsed().as_secs();
    let mut b = Browser::new(&[IPP, IPPS]);
    let mut st = Stats::default();
    let mut buf = vec![0u8; MAX_PACKET + 1];
    let mut interval = Duration::from_secs(1);
    let mut next_query = start;
    let mut next_follow = start;
    let mut next_report = start;
    let mut last: Option<Vec<Service>> = None;
    let mut dirty = true;
    let mut next_expiry = start;
    loop {
        loop {
            match rx.try_recv() {
                Ok(In::Req(Req::Query)) => {
                    interval = Duration::from_secs(1);
                    next_query = Instant::now();
                }
                Ok(In::Gone) | Err(mpsc::TryRecvError::Disconnected) => return Ok(st),
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
        let now = Instant::now();
        if now >= next_query {
            // One question per datagram: a responder answering many
            // printers in one go may cut a combined answer (Avahi does, for
            // legacy unicast). A failed send (no route for a moment) is
            // tried next round.
            for t in [IPP, IPPS] {
                if send.send(&query(&[t])).is_ok() {
                    st.queries += 1;
                }
            }
            next_query = now + interval;
            interval = (interval * 2).min(o.max_interval);
        }
        if now >= next_follow {
            let w = b.wanting(secs());
            if !w.is_empty() && send.send(&query_for(&w)).is_ok() {
                st.queries += 1;
            }
            next_follow = now + o.follow_up;
        }
        // Read whatever came, for at most a tenth of a second.
        let until = Instant::now() + Duration::from_millis(100);
        while Instant::now() < until {
            match recv.recv(&mut buf) {
                Ok(n) => {
                    st.datagrams += 1;
                    match Packet::parse(&buf[..n]) {
                        Ok(p) => {
                            st.parsed += 1;
                            b.feed(&p, secs());
                            dirty = true;
                        }
                        Err(_) => st.refused += 1,
                    }
                }
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted) => break,
                // ICMP port unreachable for a question (legacy unicast with
                // nobody there): nothing to read; keep going.
                Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => break,
                Err(e) => {
                    let _ = frame::write(&mut out, &Ev::Failed(format!("mDNS socket: {e}")));
                    return Err(e);
                }
            }
        }
        // Entries also leave by expiring: look at least once a second.
        if Instant::now() >= next_expiry {
            dirty = true;
            next_expiry = Instant::now() + Duration::from_secs(1);
        }
        if dirty && Instant::now() >= next_report {
            let s = b.services(secs());
            if last.as_ref() != Some(&s) {
                frame::write(&mut out, &Ev::Services(s.clone()))?;
                out.flush()?;
                st.reports += 1;
                last = Some(s);
            }
            dirty = false;
            next_report = Instant::now() + o.report;
        }
    }
}
