//! R0 cross-check: send this crate's query to an mDNS responder and print the
//! services the browser makes of real answers. Test-only.
//!
//! Usage: `probe [addr:port]` (default 224.0.0.251:5353). Pointed at
//! 127.0.0.1:5353 it uses RFC 6762 §6.7 legacy unicast, which Avahi answers,
//! so a container without multicast can still check the parser against a real
//! responder (`scripts/prelum-r0-oracle.sh` does).

use prelum_dnssd::{Browser, Packet, IPP, IPPS};
use std::net::UdpSocket;
use std::time::{Duration, Instant};

fn main() {
    let to = std::env::args().nth(1).unwrap_or_else(|| "224.0.0.251:5353".into());
    let sock = UdpSocket::bind("0.0.0.0:0").expect("bind");
    sock.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
    let mut b = Browser::new(&[IPP, IPPS]);
    let start = Instant::now();
    let mut buf = vec![0u8; prelum_dnssd::MAX_PACKET];
    let (mut ok, mut bad) = (0, 0);
    // Legacy unicast answers PTR questions only with what the responder adds;
    // ask twice in case the first answer was cut.
    for _ in 0..2 {
        sock.send_to(&b.query(), &to).expect("send");
        while start.elapsed() < Duration::from_secs(2) {
            match sock.recv_from(&mut buf) {
                Ok((n, _)) => match Packet::parse(&buf[..n]) {
                    Ok(p) => {
                        ok += 1;
                        b.feed(&p, start.elapsed().as_secs());
                    }
                    Err(e) => {
                        bad += 1;
                        eprintln!("unparsed answer: {e:?}");
                    }
                },
                Err(_) => break,
            }
        }
    }
    let services = b.services(start.elapsed().as_secs());
    println!("{ok} answers parsed, {bad} refused, {} services", services.len());
    for s in &services {
        println!(
            "service {:?} type={} host={} port={} addrs={:?} ty={:?} pdl={:?} urf={:?} uuid={:?} rp={:?}",
            s.instance, s.service_type, s.host, s.port, s.addrs, s.txt.ty, s.txt.pdl, s.txt.urf, s.txt.uuid, s.txt.rp
        );
    }
    if services.is_empty() || bad > 0 {
        std::process::exit(1);
    }
}
