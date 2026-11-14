use crate::client::{prefix_of, Client, Dest, Out};
use crate::dhcp::{self, Kind, Reply};
use crate::frame;
use net_proto::{check_v4, HostView, LeaseV4, Route};
use std::net::Ipv4Addr;

const MAC: [u8; 6] = [0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0xee];
const SRV: Ipv4Addr = Ipv4Addr::new(10, 77, 0, 1);
const YOU: Ipv4Addr = Ipv4Addr::new(10, 77, 0, 118);

fn rng(seed: u32) -> Box<dyn FnMut() -> u32 + Send> {
    let mut s = seed;
    Box::new(move || {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        s
    })
}

fn sent(out: &[Out]) -> Vec<(Dest, crate::dhcp::Msg)> {
    out.iter()
        .filter_map(|o| match o {
            Out::Send { to, payload } => Some((*to, parse_req(payload))),
            _ => None,
        })
        .collect()
}

/// Parse what the client sent (the server's view).
fn parse_req(b: &[u8]) -> crate::dhcp::Msg {
    assert_eq!(b[0], 1);
    let mut m = crate::dhcp::Msg {
        kind: Kind::Discover,
        xid: u32::from_be_bytes(b[4..8].try_into().unwrap()),
        secs: 0,
        chaddr: b[28..34].try_into().unwrap(),
        ciaddr: Ipv4Addr::new(b[12], b[13], b[14], b[15]),
        requested: None,
        server_id: None,
    };
    let mut i = 240;
    let mut saw_cid = false;
    while b[i] != 255 {
        if b[i] == 0 {
            i += 1;
            continue;
        }
        let (c, l) = (b[i], b[i + 1] as usize);
        let v = &b[i + 2..i + 2 + l];
        match c {
            53 => m.kind = match v[0] { 1 => Kind::Discover, 3 => Kind::Request, 7 => Kind::Release, k => panic!("kind {k}") },
            50 => m.requested = Some(Ipv4Addr::new(v[0], v[1], v[2], v[3])),
            54 => m.server_id = Some(Ipv4Addr::new(v[0], v[1], v[2], v[3])),
            61 => {
                assert_eq!(v, [&[1u8][..], &MAC[..]].concat());
                saw_cid = true;
            }
            12 | 60 | 81 => panic!("RFC 7844: option {c} must not be sent"),
            _ => {}
        }
        i += 2 + l;
    }
    assert!(saw_cid);
    m
}

fn reply(kind: Kind, xid: u32) -> Reply {
    Reply {
        kind,
        xid,
        chaddr: MAC,
        yiaddr: YOU,
        server_id: Some(SRV),
        subnet: Some(Ipv4Addr::new(255, 255, 255, 0)),
        routers: vec![SRV],
        dns: vec![SRV],
        mtu: None,
        lease: Some(600),
        t1: None,
        t2: None,
        classless: None,
        metered: false,
    }
}

fn bound(c: &mut Client) -> u32 {
    let out = c.start(0);
    let (to, d) = sent(&out).remove(0);
    assert_eq!((to, d.kind), (Dest::Broadcast, Kind::Discover));
    let out = c.rx(&dhcp::build_reply(&reply(Kind::Offer, d.xid)), 100);
    let (_, r) = sent(&out).remove(0);
    assert_eq!((r.kind, r.requested, r.server_id), (Kind::Request, Some(YOU), Some(SRV)));
    let out = c.rx(&dhcp::build_reply(&reply(Kind::Ack, r.xid)), 200);
    let Some(Out::Bound(l)) = out.into_iter().next() else { panic!("not bound") };
    assert_eq!((l.addr, l.prefix, l.router, l.lease_s), (YOU, 24, Some(SRV), 600));
    assert_eq!(check_v4(&l, &HostView::default()), Ok(()));
    d.xid
}

#[test]
fn discover_offer_request_ack() {
    let mut c = Client::new(MAC, rng(7));
    bound(&mut c);
    assert_eq!(c.state(), "bound");
}

#[test]
fn renew_then_rebind_then_expire() {
    let mut c = Client::new(MAC, rng(7));
    bound(&mut c);
    // T1 = 300 s after the ACK at 200 ms.
    assert!(c.tick(299_000).is_empty());
    let s = sent(&c.tick(300_200));
    assert_eq!(s.len(), 1);
    assert_eq!((s[0].0, s[0].1.kind, s[0].1.ciaddr, s[0].1.requested), (Dest::Server(SRV), Kind::Request, YOU, None));
    assert_eq!(c.state(), "renewing");
    // Unanswered until T2 (525 s): rebinding is broadcast.
    let mut t = 300_200;
    let mut rebind = false;
    while t < 600_200 {
        t = c.deadline().unwrap();
        let out = c.tick(t);
        if out.iter().any(|o| matches!(o, Out::Lost(_))) {
            assert!(rebind, "expired before rebinding");
            assert_eq!(t, 600_200);
            assert_eq!(c.state(), "selecting");
            assert_eq!(sent(&out)[0].1.kind, Kind::Discover);
            return;
        }
        if let Some((Dest::Broadcast, m)) = sent(&out).first() {
            assert_eq!(m.ciaddr, YOU);
            rebind = true;
        }
    }
    panic!("lease never expired");
}

#[test]
fn renew_ack_extends() {
    let mut c = Client::new(MAC, rng(9));
    bound(&mut c);
    let s = sent(&c.tick(300_200));
    let out = c.rx(&dhcp::build_reply(&reply(Kind::Ack, s[0].1.xid)), 300_300);
    assert!(matches!(out[0], Out::Bound(_)));
    assert_eq!(c.state(), "bound");
    assert_eq!(c.deadline(), Some(300_300 + 300_000));
}

#[test]
fn nak_during_renew_drops_address() {
    let mut c = Client::new(MAC, rng(3));
    bound(&mut c);
    let s = sent(&c.tick(300_200));
    let out = c.rx(&dhcp::build_reply(&reply(Kind::Nak, s[0].1.xid)), 300_300);
    assert_eq!(out[0], Out::Lost("nak"));
    assert_eq!(sent(&out)[0].1.kind, Kind::Discover);
}

#[test]
fn roam_confirms_with_init_reboot() {
    let mut c = Client::new(MAC, rng(5));
    bound(&mut c);
    c.stop();
    let s = sent(&c.start(10_000));
    assert_eq!((s[0].0, s[0].1.kind, s[0].1.requested, s[0].1.server_id, s[0].1.ciaddr), (Dest::Broadcast, Kind::Request, Some(YOU), None, Ipv4Addr::UNSPECIFIED));
    assert_eq!(c.state(), "rebooting");
    let out = c.rx(&dhcp::build_reply(&reply(Kind::Ack, s[0].1.xid)), 10_100);
    assert!(matches!(out[0], Out::Bound(_)));
    // Unanswered reboot gives up after two tries.
    c.stop();
    let mut out = c.start(20_000);
    for _ in 0..3 {
        if out.iter().any(|o| matches!(o, Out::Lost(_))) {
            break;
        }
        out = c.tick(c.deadline().unwrap());
    }
    assert!(out.contains(&Out::Lost("reboot-unanswered")));
    assert_eq!(c.state(), "selecting");
}

#[test]
fn ignores_other_xid_mac_and_server() {
    let mut c = Client::new(MAC, rng(11));
    let d = sent(&c.start(0)).remove(0).1;
    assert!(c.rx(&dhcp::build_reply(&reply(Kind::Offer, d.xid ^ 1)), 10).is_empty());
    let mut r = reply(Kind::Offer, d.xid);
    r.chaddr[5] ^= 1;
    assert!(c.rx(&dhcp::build_reply(&r), 10).is_empty());
    let r = c.rx(&dhcp::build_reply(&reply(Kind::Offer, d.xid)), 10);
    let x = sent(&r)[0].1.xid;
    // An ACK from a server we did not request from is ignored.
    let mut a = reply(Kind::Ack, x);
    a.server_id = Some(Ipv4Addr::new(10, 77, 0, 66));
    assert!(c.rx(&dhcp::build_reply(&a), 20).is_empty());
    // A second OFFER while requesting is ignored too.
    assert!(c.rx(&dhcp::build_reply(&reply(Kind::Offer, x)), 20).is_empty());
}

#[test]
fn retransmit_backoff() {
    let mut c = Client::new(MAC, rng(13));
    c.start(0);
    let mut last = 0;
    let mut gaps = vec![];
    for _ in 0..7 {
        let d = c.deadline().unwrap();
        gaps.push(d - last);
        last = d;
        assert_eq!(sent(&c.tick(d)).len(), 1);
    }
    for (g, base) in gaps.iter().zip([4000u64, 8000, 16000, 32000, 64000, 64000, 64000]) {
        assert!(*g >= base - 1000 && *g <= base + 1000, "{gaps:?}");
    }
}

#[test]
fn classless_routes_replace_router() {
    // 10.1.0.0/16 via 10.77.0.2, default via 10.77.0.1, 192.168.5.0/24 on-link.
    let v = vec![16, 10, 1, 10, 77, 0, 2, 0, 10, 77, 0, 1, 24, 192, 168, 5, 0, 0, 0, 0];
    assert_eq!(
        dhcp::classless(&v).unwrap(),
        vec![
            (Ipv4Addr::new(10, 1, 0, 0), 16, Ipv4Addr::new(10, 77, 0, 2)),
            (Ipv4Addr::new(0, 0, 0, 0), 0, SRV),
            (Ipv4Addr::new(192, 168, 5, 0), 24, Ipv4Addr::UNSPECIFIED)
        ]
    );
    let mut c = Client::new(MAC, rng(17));
    let d = sent(&c.start(0)).remove(0).1;
    let x = sent(&c.rx(&dhcp::build_reply(&reply(Kind::Offer, d.xid)), 1))[0].1.xid;
    let mut a = reply(Kind::Ack, x);
    a.routers = vec![Ipv4Addr::new(10, 77, 0, 99)];
    a.classless = Some(v);
    let Out::Bound(l) = c.rx(&dhcp::build_reply(&a), 2).remove(0) else { panic!() };
    assert_eq!(l.router, Some(SRV));
    assert_eq!(l.routes, vec![
        Route { dst: Ipv4Addr::new(10, 1, 0, 0), prefix: 16, gw: Ipv4Addr::new(10, 77, 0, 2) },
        Route { dst: Ipv4Addr::new(192, 168, 5, 0), prefix: 24, gw: Ipv4Addr::UNSPECIFIED },
    ]);
    assert!(dhcp::classless(&[33, 0, 0, 0, 0]).is_none());
    assert!(dhcp::classless(&[24, 10, 0]).is_none());
}

#[test]
fn netmask() {
    assert_eq!(prefix_of(Ipv4Addr::new(255, 255, 255, 0)), Some(24));
    assert_eq!(prefix_of(Ipv4Addr::new(255, 255, 255, 255)), Some(32));
    assert_eq!(prefix_of(Ipv4Addr::new(0, 0, 0, 0)), Some(0));
    assert_eq!(prefix_of(Ipv4Addr::new(255, 0, 255, 0)), None);
}

#[test]
fn malformed_replies() {
    let good = dhcp::build_reply(&reply(Kind::Ack, 1));
    assert!(dhcp::parse(&good).is_some());
    // Truncated option, repeated option, overload, missing END, BOOTREQUEST.
    let mut t = good.clone();
    t.truncate(t.len() - 3);
    assert!(dhcp::parse(&t).is_none());
    let mut r = good.clone();
    r.pop();
    r.extend_from_slice(&[54, 4, 1, 2, 3, 4, 255]);
    assert!(dhcp::parse(&r).is_none());
    let mut o = good.clone();
    o.pop();
    o.extend_from_slice(&[52, 1, 3, 255]);
    assert!(dhcp::parse(&o).is_none());
    let mut e = good.clone();
    e.pop();
    assert!(dhcp::parse(&e).is_none());
    let mut q = good.clone();
    q[0] = 1;
    assert!(dhcp::parse(&q).is_none());
    // Bad lengths for fixed-size options.
    for (c, l) in [(1u8, 3u8), (51, 2), (26, 4), (3, 6), (53, 2)] {
        let mut b = good.clone();
        b.pop();
        b.push(c);
        b.push(l);
        b.extend(std::iter::repeat_n(1u8, l as usize));
        b.push(255);
        if c == 53 || c == 1 || c == 51 || c == 3 {
            // also repeated; either way refused
        }
        assert!(dhcp::parse(&b).is_none(), "option {c} len {l}");
    }
}

#[test]
fn frames() {
    let p = dhcp::build_reply(&reply(Kind::Offer, 9));
    let f = frame::frame_for_test(SRV, Ipv4Addr::BROADCAST, &p);
    let (src, got) = frame::dhcp_from_frame(&f).unwrap();
    assert_eq!((src, got), (SRV, &p[..]));
    let mut bad = f.clone();
    bad[14 + 10] ^= 1; // IP checksum
    assert!(frame::dhcp_from_frame(&bad).is_none());
    let mut frag = f.clone();
    frag[14 + 6] = 0x20; // MF
    assert!(frame::dhcp_from_frame(&frag).is_none());
    let mut port = f.clone();
    port[14 + 20 + 1] = 68; // source port 68
    assert!(frame::dhcp_from_frame(&port).is_none());
    assert!(frame::dhcp_from_frame(&f[..40]).is_none());
}

/// A hostile server: mutate valid replies at random. The client must never
/// panic, and anything it proposes either passes atrium-net's checks or is
/// refused by them (never applied unchecked: that is atrium-net's job; here
/// we check the lease is internally consistent).
#[test]
fn hostile_server_fuzz() {
    let mut r = rng(0x5eed);
    for round in 0..20_000u32 {
        let mut c = Client::new(MAC, rng(round | 1));
        let d = sent(&c.start(0)).remove(0).1;
        let mut base = reply(if round % 3 == 0 { Kind::Offer } else { Kind::Ack }, d.xid);
        if round % 5 == 0 {
            base.classless = Some((0..(r() % 24)).map(|_| r() as u8).collect());
        }
        let mut b = dhcp::build_reply(&base);
        for _ in 0..(1 + r() % 6) {
            let i = 236 + (r() as usize % (b.len() - 236));
            match r() % 4 {
                0 => b[i] = r() as u8,
                1 => { b.insert(i, r() as u8); }
                2 => { if b.len() > 241 { b.remove(i); } }
                _ => b.truncate(i.max(240)),
            }
        }
        let mut f = frame::frame_for_test(SRV, YOU, &b);
        if round % 7 == 0 {
            let i = r() as usize % f.len();
            f[i] = r() as u8;
        }
        if let Some((_, p)) = frame::dhcp_from_frame(&f) {
            for o in c.rx(p, 10) {
                if let Out::Bound(l) = o {
                    assert!(l.prefix <= 32 && l.dns.len() <= 3);
                    let _ = check_v4(&l, &HostView::default());
                }
            }
        }
    }
}

#[test]
fn android_hotspot_is_metered() {
    let mut c = Client::new(MAC, rng(11));
    let out = c.start(0);
    let (_, d) = sent(&out).remove(0);
    let mut r = reply(Kind::Offer, d.xid);
    r.metered = true;
    let out = c.rx(&dhcp::build_reply(&r), 100);
    let (_, req) = sent(&out).remove(0);
    let mut a = reply(Kind::Ack, req.xid);
    a.metered = true;
    let out = c.rx(&dhcp::build_reply(&a), 200);
    let l = out.iter().find_map(|o| match o { Out::Bound(l) => Some(l.clone()), _ => None }).expect("bound");
    assert!(l.metered);
    assert_eq!(LeaseV4::from_line(&l.to_line()).unwrap().metered, true);
    // An ordinary router's vendor option is not a metered marker.
    let mut v = dhcp::build_reply(&reply(Kind::Ack, 1));
    let end = v.len() - 1;
    v.splice(end..end, [43u8, 5, b'h', b'e', b'l', b'l', b'o']);
    assert!(!dhcp::parse(&v).unwrap().metered);
}

mod ra_tests {
    use crate::ra::{ra_for_test, ra_from_frame, rdnss_opt, Rdnss, MAX_LIFE_S};
    use net_proto::check_dns6;
    use std::net::Ipv6Addr;

    const R: Ipv6Addr = Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);
    const D1: Ipv6Addr = Ipv6Addr::new(0xfd77, 0, 0, 0, 0, 0, 0, 0x53);
    const D2: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0x53);

    #[test]
    fn rdnss_read_and_expire() {
        let f = ra_for_test(R, 1800, &rdnss_opt(600, &[D1, D2]));
        let ra = ra_from_frame(&f).expect("valid RA");
        assert_eq!(ra.router, R);
        assert_eq!(ra.dns, vec![(D1, 600), (D2, 600)]);
        let mut r = Rdnss::default();
        assert_eq!(r.rx(&ra, 0), Some(vec![D1, D2]));
        // The same again changes nothing.
        assert_eq!(r.rx(&ra, 1000), None);
        assert_eq!(r.deadline(), Some(601_000));
        assert_eq!(r.tick(601_000), Some(vec![]));
        // Lifetime 0 withdraws.
        r.rx(&ra, 0);
        let gone = ra_from_frame(&ra_for_test(R, 1800, &rdnss_opt(0, &[D1, D2]))).unwrap();
        assert_eq!(r.rx(&gone, 10), Some(vec![]));
        // Infinite lifetimes are capped.
        let inf = ra_from_frame(&ra_for_test(R, 1800, &rdnss_opt(u32::MAX, &[D1]))).unwrap();
        assert_eq!(inf.dns, vec![(D1, MAX_LIFE_S)]);
        // A rogue router's loopback server is dropped; the good ones stay usable.
        let mut r = Rdnss::default();
        r.rx(&ra, 0);
        let rogue = ra_from_frame(&ra_for_test(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0x66), 0, &rdnss_opt(600, &[Ipv6Addr::LOCALHOST]))).unwrap();
        assert_eq!(r.rx(&rogue, 5), None);
        assert!(check_dns6(&r.tick(6).unwrap_or(vec![D1, D2])).is_ok());
    }

    #[test]
    fn ra_validation() {
        let good = ra_for_test(R, 1800, &rdnss_opt(600, &[D1]));
        assert!(ra_from_frame(&good).is_some());
        // Hop limit below 255: forwarded from off-link.
        let mut f = good.clone();
        f[21] = 64;
        assert!(ra_from_frame(&f).is_none());
        // Source not link-local.
        assert!(ra_from_frame(&ra_for_test(D1, 1800, &rdnss_opt(600, &[D1]))).is_none());
        // Bad checksum.
        let mut f = good.clone();
        let n = f.len();
        f[n - 1] ^= 1;
        assert!(ra_from_frame(&f).is_none());
        // Zero-length option (would loop forever).
        assert!(ra_from_frame(&ra_for_test(R, 1800, &[25, 0, 0, 0, 0, 0, 0, 0])).is_none());
        // RDNSS with an even length.
        let mut o = rdnss_opt(600, &[D1]);
        o[1] = 2;
        o.truncate(16);
        assert!(ra_from_frame(&ra_for_test(R, 1800, &o)).is_none());
        // Option running past the end.
        let mut o = rdnss_opt(600, &[D1]);
        o[1] = 5;
        assert!(ra_from_frame(&ra_for_test(R, 1800, &o)).is_none());
        // Truncated frames never panic.
        for n in 0..good.len() {
            let _ = ra_from_frame(&good[..n]);
        }
    }

    #[test]
    fn dns6_checks() {
        assert!(check_dns6(&[D1, D2, R]).is_ok());
        for bad in ["::1", "::", "ff02::1", "::ffff:10.0.0.1", "2002:a00:1::1", "2001:0:1::1", "fec0::1", "::10.0.0.1"] {
            assert!(check_dns6(&[bad.parse().unwrap()]).is_err(), "{bad}");
        }
        assert!(check_dns6(&[D1, D1]).is_err());
        assert!(check_dns6(&[D1, D2, R, Ipv6Addr::new(0xfd77, 0, 0, 0, 0, 0, 0, 1)]).is_err());
    }

    #[test]
    fn ra_fuzz() {
        let mut seed = 0x1234_5678u32;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        let base = ra_for_test(R, 1800, &[rdnss_opt(600, &[D1, D2]), vec![5, 1, 0, 0, 0, 0, 5, 220]].concat());
        let mut r = Rdnss::default();
        for i in 0..20_000u64 {
            let mut f = base.clone();
            for _ in 0..(next() % 6) {
                let k = 54 + (next() as usize % (f.len() - 54));
                f[k] = next() as u8;
            }
            if next() % 4 == 0 {
                f.truncate(54 + next() as usize % (f.len() - 54));
            }
            // Mutated frames fail the checksum; fix it up so the option
            // parser sees them too.
            let m = f[54..].to_vec();
            let refixed = ra_for_test(R, u16::from_be_bytes([m.get(6).copied().unwrap_or(0), m.get(7).copied().unwrap_or(0)]), m.get(16..).unwrap_or(&[]));
            for g in [&f, &refixed] {
                if let Some(ra) = ra_from_frame(g) {
                    assert!(ra.dns.len() <= 12);
                    if let Some(v) = r.rx(&ra, i) {
                        assert!(v.len() <= 3);
                    }
                }
            }
        }
    }
}

/// The gate's fake LAN (atrium-net/sim/lan.py) RA, as it puts it on the wire.
#[test]
fn lan_py_ra_parses() {
    let f: Vec<u8> = (0..158).map(|i| u8::from_str_radix(&"333300000001024e000000fe86dd6000000000683afffe800000000000000000000000000001ff020000000000000000000000000001860041ed400007080000000000000000030440c0000151800000384000000000fd77000000000088000000000000000005010000000005dc0101024e000000fe1905000000000258fd770000000000880000000000000053fe800000000000000000000000000001"[2 * i..2 * i + 2], 16).unwrap()).collect();
    let ra = crate::ra::ra_from_frame(&f).expect("lan.py RA");
    let want: Vec<std::net::Ipv6Addr> = vec!["fd77:0:0:88::53".parse().unwrap(), "fe80::1".parse().unwrap()];
    assert_eq!(ra.dns.iter().map(|d| d.0).collect::<Vec<_>>(), want);
    assert_eq!(ra.router_life, 1800);
}

// ---- connectivity probe (probe.rs) -------------------------------------------

fn dns_reply(q: &[u8], rcode: u8, answers: &[(u16, &[u8])]) -> Vec<u8> {
    let mut r = q[..2].to_vec();
    r.extend_from_slice(&[0x81, 0x80 | rcode, 0, 1, 0, answers.len() as u8, 0, 0, 0, 0]);
    r.extend_from_slice(&q[12..]);
    for (typ, data) in answers {
        r.extend_from_slice(&[0xc0, 0x0c]);
        r.extend_from_slice(&typ.to_be_bytes());
        r.extend_from_slice(&[0, 1, 0, 0, 0, 60]);
        r.extend_from_slice(&(data.len() as u16).to_be_bytes());
        r.extend_from_slice(data);
    }
    r
}

#[test]
fn probe_dns() {
    use crate::probe::{dns_answer, dns_query, DnsErr};
    let q = dns_query(0x1234, "probe.atrium.test");
    assert_eq!(&q[..2], &[0x12, 0x34]);
    // A CNAME first, then the A record.
    let r = dns_reply(&q, 0, &[(5, &[0xc0, 0x0c]), (1, &[10, 88, 0, 80])]);
    assert_eq!(dns_answer(&q, &r), Ok(Ipv4Addr::new(10, 88, 0, 80)));
    assert_eq!(dns_answer(&q, &dns_reply(&q, 3, &[])), Err(DnsErr::Rcode(3)));
    assert_eq!(dns_answer(&q, &dns_reply(&q, 0, &[(28, &[0; 16])])), Err(DnsErr::NoAnswer));
    // Another id, or another question: not ours.
    let mut other = r.clone();
    other[1] ^= 1;
    assert_eq!(dns_answer(&q, &other), Err(DnsErr::NotOurs));
    let q2 = dns_query(0x1234, "evil.atrium.test");
    assert_eq!(dns_answer(&q2, &r), Err(DnsErr::NotOurs));
    // Upper case in the reply's question is fine (0x20 encoding).
    let mut up = r.clone();
    up[13] = b'P';
    assert!(dns_answer(&q, &up).is_ok());
    // A compression loop and lengths past the end are errors, not hangs.
    let mut lp = dns_reply(&q, 0, &[]);
    lp[7] = 1;
    lp.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 40, 1]);
    assert_eq!(dns_answer(&q, &lp), Err(DnsErr::Malformed));
    let mut rnd = rng(9);
    for _ in 0..20_000 {
        let mut m = r.clone();
        let n = (rnd() % 6) as usize;
        for _ in 0..n {
            let i = rnd() as usize % m.len();
            m[i] = rnd() as u8;
        }
        m.truncate(rnd() as usize % (m.len() + 1));
        let _ = dns_answer(&q, &m);
    }
}

#[test]
fn probe_http() {
    use crate::probe::{http_reply, valid_name, valid_path};
    let ok = http_reply(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n", None).unwrap();
    assert_eq!((ok.status, ok.location, ok.matched), (204, None, false));
    let cap = http_reply(b"HTTP/1.0 302 Found\r\nLOCATION: http://10.77.0.1/login?x=1\r\n\r\n", None).unwrap();
    assert_eq!((cap.status, cap.location.as_deref()), (302, Some("http://10.77.0.1/login?x=1")));
    let body = http_reply(b"HTTP/1.1 200 OK\r\n\r\n<html>success</html>", Some(b"success")).unwrap();
    assert!(body.matched);
    assert!(!http_reply(b"HTTP/1.1 200 OK\r\n\r\n<html>sign in</html>", Some(b"success")).unwrap().matched);
    // A Location with control bytes or too long is dropped, not passed on.
    assert_eq!(http_reply(b"HTTP/1.1 302 x\r\nLocation: http://a/\x1b[2J\r\n\r\n", None).unwrap().location, None);
    let long = format!("HTTP/1.1 302 x\r\nLocation: http://a/{}\r\n\r\n", "a".repeat(300));
    assert_eq!(http_reply(long.as_bytes(), None).unwrap().location, None);
    for bad in [&b""[..], b"SSH-2.0-x\r\n", b"HTTP/2 200\r\n\r\n", b"HTTP/1.1 2x4 x\r\n", b"HTTP/1.1 2040\r\n"] {
        assert_eq!(http_reply(bad, None), None, "{:?}", String::from_utf8_lossy(bad));
    }
    assert!(valid_name("connectivitycheck.gstatic.com") && !valid_name("a..b") && !valid_name("a b") && !valid_name("-a"));
    assert!(valid_path("/generate_204") && !valid_path("x") && !valid_path("/a b") && !valid_path("/\r\nHost: x"));
    let mut rnd = rng(11);
    let base = b"HTTP/1.1 302 Found\r\nLocation: http://10.77.0.1/login\r\nX: y\r\n\r\nbody".to_vec();
    for _ in 0..20_000 {
        let mut m = base.clone();
        for _ in 0..(rnd() % 6) {
            let i = rnd() as usize % m.len();
            m[i] = rnd() as u8;
        }
        if let Some(h) = http_reply(&m, Some(b"success")) {
            assert!(h.location.as_ref().is_none_or(|l| l.len() <= 256 && l.bytes().all(|c| (0x21..0x7f).contains(&c))));
        }
    }
}
