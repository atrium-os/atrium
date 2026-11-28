use super::*;

fn name(s: &str) -> Name {
    s.split('.').map(|l| l.as_bytes().to_vec()).collect()
}

fn dq(id: u16, n: &str, t: u16) -> Vec<u8> {
    let mut q = vec![(id >> 8) as u8, id as u8, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0];
    put_name(&mut q, &name(n));
    q.extend_from_slice(&t.to_be_bytes());
    q.extend_from_slice(&[0, 1]);
    q
}

fn a_rr(n: &[u8], ttl: u32, a: [u8; 4]) -> Vec<u8> {
    let mut r = n.to_vec();
    r.extend_from_slice(&[0, 1, 0x80, 1]);
    r.extend_from_slice(&ttl.to_be_bytes());
    r.extend_from_slice(&[0, 4]);
    r.extend_from_slice(&a);
    r
}

fn wire(n: &str) -> Vec<u8> {
    let mut v = Vec::new();
    put_name(&mut v, &name(n));
    v
}

fn resp(rrs: &[Vec<u8>]) -> Vec<u8> {
    let mut m = vec![0, 0, 0x84, 0, 0, 0, 0, rrs.len() as u8, 0, 0, 0, 0];
    for r in rrs {
        m.extend_from_slice(r);
    }
    m
}

#[allow(non_snake_case)]
fn NET() -> Net {
    Net { addr: Ipv4Addr::new(10, 88, 0, 50), prefix: 24, v6: vec!["fd77:0:0:88::".parse().unwrap()] }
}

fn v4(a: [u8; 4]) -> IpAddr {
    IpAddr::V4(Ipv4Addr::from(a))
}

fn aaaa_rr(n: &[u8], ttl: u32, a: &str) -> Vec<u8> {
    let mut r = n.to_vec();
    r.extend_from_slice(&[0, 28, 0x80, 1]);
    r.extend_from_slice(&ttl.to_be_bytes());
    r.extend_from_slice(&[0, 16]);
    r.extend_from_slice(&a.parse::<Ipv6Addr>().unwrap().octets());
    r
}

#[test]
fn local_names() {
    for ok in ["nas.local", "Gate-Box.local", "a.b.c.d.local", "my_printer.local"] {
        let n: Name = name(ok).into_iter().map(|l| l.to_ascii_lowercase()).collect();
        assert!(is_local_host(&n), "{ok}");
    }
    for bad in ["local", "nas.lan", "a.b.c.d.e.local", "-x.local", "a b.local", "x..local"] {
        assert!(!is_local_host(&name(bad)), "{bad}");
    }
    assert!(!is_local_host(&vec![vec![b'x'; 64], b"local".to_vec()]));
}

#[test]
fn queries_parse_and_lower_case() {
    let q = parse_query(&dq(7, "NAS.Local", TYPE_A)).unwrap();
    assert_eq!((q.id, q.qtype, q.rd, q.name), (7, TYPE_A, true, name("nas.local")));
    let mut r = dq(7, "nas.local", 1);
    r[2] |= 0x80;
    assert!(parse_query(&r).is_none(), "a response is no query");
    assert!(parse_query(&dq(7, "nas.local", 1)[..15]).is_none());
}

#[test]
fn mdns_question_one_shot() {
    let q = mdns_query(&name("nas.local"), TYPE_A);
    assert_eq!(&q[..12], &[0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
    assert_eq!(&q[q.len() - 4..], &[0, 1, 0, 1]);
}

#[test]
fn net_rules() {
    assert!(NET().allows(Ipv4Addr::new(10, 88, 0, 77)));
    assert!(NET().allows(Ipv4Addr::new(169, 254, 3, 4)));
    for bad in [[10, 88, 1, 77], [10, 88, 0, 0], [10, 88, 0, 255], [10, 88, 0, 50], [127, 0, 0, 1], [8, 8, 8, 8], [224, 0, 0, 251], [0, 0, 0, 0]] {
        assert!(!NET().allows(Ipv4Addr::from(bad)), "{bad:?}");
    }
}

#[test]
fn answers_checked() {
    let want = name("nas.local");
    let n = wire("nas.local");
    let m = resp(&[
        a_rr(&n, 4500, [10, 88, 0, 77]),
        a_rr(&n, 120, [10, 88, 0, 77]),       // duplicate
        a_rr(&n, 120, [8, 8, 8, 8]),          // off the link
        a_rr(&n, 0, [10, 88, 0, 78]),         // goodbye
        a_rr(&wire("other.local"), 120, [10, 88, 0, 79]),
        a_rr(&n, 30, [169, 254, 9, 9]),
    ]);
    assert_eq!(answers(&m, &want, TYPE_A, &NET()), vec![(v4([10, 88, 0, 77]), 120), (v4([169, 254, 9, 9]), 30)]);
    // Upper case on the wire still matches; a query (QR clear) never does.
    assert_eq!(answers(&resp(&[a_rr(&wire("NAS.local"), 60, [10, 88, 0, 77])]), &want, TYPE_A, &NET()).len(), 1);
    let mut q = m.clone();
    q[2] = 0;
    assert!(answers(&q, &want, TYPE_A, &NET()).is_empty());
    // Many addresses: four kept.
    let many: Vec<Vec<u8>> = (1..20).map(|k| a_rr(&n, 60, [10, 88, 0, k])).collect();
    assert_eq!(answers(&resp(&many), &want, TYPE_A, &NET()).len(), MAX_ADDRS);
    // Compressed names, pointing backwards only.
    let mut m = resp(&[]);
    m[7] = 2;
    m.extend(a_rr(&n, 60, [10, 88, 0, 70])); // name at 12
    m.extend(a_rr(&[0xc0, 12], 60, [10, 88, 0, 71]));
    assert_eq!(answers(&m, &want, TYPE_A, &NET()).len(), 2);
}

#[test]
fn hostile_messages_are_bounded() {
    let want = name("nas.local");
    // A pointer to itself, a forward pointer, a label past the end, a
    // record longer than the message, huge counts.
    let mut loop1 = resp(&[]);
    loop1[7] = 1;
    loop1.extend_from_slice(&[0xc0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 10, 88, 0, 7]);
    let mut fwd = resp(&[]);
    fwd[7] = 1;
    fwd.extend_from_slice(&[0xc0, 40, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 10, 88, 0, 7]);
    let mut past = resp(&[]);
    past[7] = 1;
    past.extend_from_slice(&[63, b'x', b'y']);
    let mut long = resp(&[]);
    long[7] = 1;
    long.extend(wire("nas.local"));
    long.extend_from_slice(&[0, 1, 0, 1, 0, 0, 0, 60, 0xff, 0xff, 10, 88]);
    let mut counts = resp(&[a_rr(&wire("nas.local"), 60, [10, 88, 0, 9])]);
    counts[4..12].copy_from_slice(&[0xff; 8]);
    for m in [loop1, fwd, past, long, counts] {
        assert!(answers(&m, &want, TYPE_A, &NET()).is_empty());
    }
    // Labels adding up past 255 bytes.
    let mut big = resp(&[]);
    big[7] = 1;
    for _ in 0..5 {
        big.push(63);
        big.extend_from_slice(&[b'a'; 63]);
    }
    big.push(0);
    assert!(read_name(&big, 12).is_none());
    // Random bytes: never a panic.
    let mut x: u64 = 0x9e3779b97f4a7c15;
    for _ in 0..20_000 {
        let len = (x % 300) as usize;
        let mut m = Vec::with_capacity(len);
        for _ in 0..len {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            m.push(x as u8);
        }
        if m.len() > 3 {
            m[2] |= 0x80;
            m[3] &= 0xf0;
        }
        let _ = answers(&m, &want, TYPE_A, &NET());
        let _ = parse_query(&m);
    }
}

#[test]
fn resolver_flow() {
    let mut r: Resolver<u32> = Resolver::new(Net { v6: vec![], ..NET() });
    // Not ours: REFUSED at once. AAAA on a v4-only network: no data, no
    // question on the link.
    let out = r.query(1, &dq(1, "example.com", TYPE_A), 0);
    assert!(matches!(&out[..], [Out::Reply(1, m)] if m[3] & 0xf == rcode::REFUSED as u8));
    let out = r.query(1, &dq(2, "nas.local", TYPE_AAAA), 0);
    assert!(matches!(&out[..], [Out::Reply(1, m)] if m[3] & 0xf == 0 && m[7] == 0));
    // Two askers, one question on the link.
    let out = r.query(1, &dq(3, "nas.local", TYPE_A), 0);
    assert!(matches!(&out[..], [Out::Ask(_)]));
    assert!(r.query(2, &dq(4, "NAS.local", TYPE_A), 10).is_empty());
    r.mdns(&resp(&[a_rr(&wire("nas.local"), 4500, [10, 88, 0, 77])]));
    assert!(r.tick(500).is_empty());
    let out = r.tick(1000);
    assert_eq!(out.len(), 2);
    for o in &out {
        let Out::Reply(_, m) = o else { panic!() };
        assert_eq!((m[3] & 0xf, m[7]), (0, 1));
        assert_eq!(&m[m.len() - 4..], &[10, 88, 0, 77]);
        assert_eq!(&m[m.len() - 10..m.len() - 6], &120u32.to_be_bytes());
    }
    // Nobody answers: NXDOMAIN, and not asked again for a while.
    assert!(matches!(&r.query(1, &dq(5, "gone.local", TYPE_A), 2000)[..], [Out::Ask(_)]));
    let out = r.tick(3000);
    assert!(matches!(&out[..], [Out::Reply(1, m)] if m[3] & 0xf == rcode::NXDOMAIN as u8));
    assert!(matches!(&r.query(1, &dq(6, "gone.local", TYPE_A), 4000)[..], [Out::Reply(1, _)]));
    assert!(matches!(&r.query(1, &dq(7, "gone.local", TYPE_A), 3000 + NEGATIVE_MS)[..], [Out::Ask(_)]));
    // At most MAX_PENDING questions on the link.
    let mut r: Resolver<u32> = Resolver::new(NET());
    for k in 0..MAX_PENDING {
        assert!(matches!(&r.query(1, &dq(k as u16, &format!("h{k}.local"), TYPE_A), 0)[..], [Out::Ask(_)]));
    }
    let out = r.query(1, &dq(99, "more.local", TYPE_A), 0);
    assert!(matches!(&out[..], [Out::Reply(1, m)] if m[3] & 0xf == rcode::SERVFAIL as u8));
}

#[test]
fn ipv6_answers() {
    let net = NET();
    let ok = |s: &str| net.allows_ip(IpAddr::V6(s.parse().unwrap()));
    assert!(ok("fd77:0:0:88::77"));
    for bad in ["fd77:0:0:88::", "fd77:0:0:89::77", "::1", "2001:db8::1", "fe80::1", "::"] {
        assert!(!ok(bad), "{bad}");
    }
    let want = name("nas.local");
    let n = wire("nas.local");
    let m = resp(&[
        aaaa_rr(&n, 600, "fd77:0:0:88::77"),
        aaaa_rr(&n, 60, "::1"),
        aaaa_rr(&n, 60, "fe80::1"),
        aaaa_rr(&n, 60, "2001:db8::1"),
        a_rr(&n, 60, [10, 88, 0, 77]),
    ]);
    assert_eq!(answers(&m, &want, TYPE_AAAA, &net), vec![(IpAddr::V6("fd77:0:0:88::77".parse().unwrap()), 120)]);
    assert_eq!(answers(&m, &want, TYPE_A, &net), vec![(v4([10, 88, 0, 77]), 60)]);
    // A wrong length is no address.
    let mut bad = aaaa_rr(&n, 60, "fd77:0:0:88::78");
    let l = bad.len();
    bad[l - 17] = 4;
    bad.truncate(l - 12);
    assert!(answers(&resp(&[bad]), &want, TYPE_AAAA, &net).is_empty());
}

#[test]
fn ipv6_resolver() {
    let mut r: Resolver<u32> = Resolver::new(NET());
    assert!(matches!(&r.query(1, &dq(1, "nas.local", TYPE_AAAA), 0)[..], [Out::Ask(q)] if q[q.len() - 4..] == [0, 28, 0, 1]));
    assert!(matches!(&r.query(1, &dq(2, "nas.local", TYPE_A), 0)[..], [Out::Ask(_)]));
    r.mdns(&resp(&[aaaa_rr(&wire("nas.local"), 60, "fd77:0:0:88::77")]));
    let out = r.tick(1000);
    assert_eq!(out.len(), 2);
    for o in &out {
        let Out::Reply(_, m) = o else { panic!() };
        match u16::from_be_bytes([m[0], m[1]]) {
            1 => assert_eq!((m[3] & 0xf, m[7], &m[m.len() - 2..]), (0, 1, &[0u8, 0x77][..])),
            2 => assert_eq!(m[3] & 0xf, rcode::NXDOMAIN as u8),
            x => panic!("{x}"),
        }
    }
    // An AAAA question nobody answers: no data, not NXDOMAIN.
    assert!(matches!(&r.query(1, &dq(3, "v4only.local", TYPE_AAAA), 2000)[..], [Out::Ask(_)]));
    assert!(matches!(&r.tick(3000)[..], [Out::Reply(1, m)] if m[3] & 0xf == 0 && m[7] == 0));
    // No IPv6 on this network: AAAA is answered at once, nothing asked.
    let mut r: Resolver<u32> = Resolver::new(Net { v6: vec![], ..NET() });
    assert!(matches!(&r.query(1, &dq(4, "nas.local", TYPE_AAAA), 0)[..], [Out::Reply(1, m)] if m[3] & 0xf == 0 && m[7] == 0));
}

#[test]
fn tunnel_link_needs_a_complete_answer_for_its_question() {
    let q = parse_query(&dq(17, "public.example", TYPE_A)).unwrap();
    let ips = [v4([203, 0, 113, 9]), v4([10, 99, 0, 6])];
    let mut msg = response(&q, 0, &[(ips[0], 60), (ips[1], 60)]);
    assert_eq!(link_addresses(&msg, 17, "public.example"), Some((TYPE_A, ips.to_vec())));
    assert_eq!(link_addresses(&msg, 17, "other.example"), None);
    assert_eq!(link_addresses(&msg, 18, "public.example"), None);
    msg[2] |= 2;
    assert_eq!(link_addresses(&msg, 17, "public.example"), None);
    msg[2] &= !2;
    msg.pop();
    assert_eq!(link_addresses(&msg, 17, "public.example"), None);
    let q = parse_query(&dq(17, "public.example", TYPE_AAAA)).unwrap();
    assert_eq!(link_addresses(&response(&q, 0, &[]), 17, "public.example"), Some((TYPE_AAAA, vec![])));
}
