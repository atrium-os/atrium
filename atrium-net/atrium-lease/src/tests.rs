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
