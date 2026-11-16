//! DHCPv6 client tests: the exchanges, timers, and what a hostile server
//! cannot make it do.

use crate::dhcp6::*;
use crate::ra;
use std::net::Ipv6Addr;

const MAC: [u8; 6] = [0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0xee];
const SID: &[u8] = &[0, 1, 0, 1, 1, 2, 3, 4, 2, 0, 0, 0, 0, 1];
const ADDR: Ipv6Addr = Ipv6Addr::new(0xfd77, 0, 0, 0x88, 0, 0, 1, 0x50);
const DNS: Ipv6Addr = Ipv6Addr::new(0xfd77, 0, 0, 0x88, 0, 0, 0, 0x153);
const LL: Ipv6Addr = Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);

fn rng(seed: u32) -> Box<dyn FnMut() -> u32 + Send> {
    let mut s = seed;
    Box::new(move || {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        s
    })
}

/// (type, xid, IAID if any) of what the client sent, checked for form.
fn sent(out: &[Out6]) -> Vec<(u8, [u8; 3], Option<u32>, Vec<u8>)> {
    out.iter()
        .filter_map(|o| match o {
            Out6::Send(p) => {
                assert!(p.len() <= MAX_SEND);
                let opts = options(&p[4..]).expect("well-formed");
                let cid = opts.iter().find(|(c, _)| *c == OPT_CLIENTID).unwrap().1;
                assert_eq!(cid, &duid_ll(&MAC)[..]);
                assert!(opts.iter().any(|(c, _)| *c == OPT_ELAPSED));
                let ia = opts.iter().find(|(c, _)| *c == OPT_IA_NA).map(|(_, b)| u32::from_be_bytes(b[..4].try_into().unwrap()));
                let sid = opts.iter().find(|(c, _)| *c == OPT_SERVERID).map(|(_, b)| b.to_vec()).unwrap_or_default();
                Some((p[0], [p[1], p[2], p[3]], ia, sid))
            }
            _ => None,
        })
        .collect()
}

fn cid() -> Vec<u8> {
    duid_ll(&MAC).to_vec()
}

fn bound(c: &mut Client6) -> u32 {
    let s = sent(&c.ra(true, true, 0));
    assert_eq!(s[0].0, SOLICIT);
    let iaid = s[0].2.unwrap();
    let adv = server_msg_for_test(ADVERTISE, s[0].1, &cid(), SID, Some((iaid, 0, 0, &[(ADDR, 3600, 7200)])), &[]);
    let s = sent(&c.rx(&adv, 100));
    assert_eq!(s[0].0, REQUEST);
    assert_eq!(s[0].3, SID);
    let rep = server_msg_for_test(REPLY, s[0].1, &cid(), SID, Some((iaid, 1800, 2880, &[(ADDR, 3600, 7200)])), &[DNS]);
    let o = c.rx(&rep, 200);
    assert_eq!(o, vec![Out6::Bound(IaAddr { addr: ADDR, pref: 3600, valid: 7200 })]);
    assert_eq!(c.dns(), &[DNS]);
    iaid
}

#[test]
fn solicit_advertise_request_reply() {
    let mut c = Client6::new(MAC, rng(1));
    bound(&mut c);
    assert_eq!(c.state(), "bound");
    assert_eq!(c.deadline(), Some(200 + 1800 * 1000));
}

#[test]
fn renew_rebind_expire() {
    let mut c = Client6::new(MAC, rng(2));
    bound(&mut c);
    let t1 = 200 + 1800 * 1000;
    let s = sent(&c.tick(t1));
    assert_eq!(s[0].0, RENEW);
    assert_eq!(s[0].3, SID);
    // No answer: renewals until T2, then Rebind without a server id.
    let mut now = t1;
    let mut kinds = Vec::new();
    while now < 200 + 7200 * 1000 {
        now = c.deadline().unwrap();
        for (k, ..) in sent(&c.tick(now)) {
            kinds.push(k);
        }
        if c.addr().is_none() {
            break;
        }
    }
    assert!(kinds.contains(&RENEW) && kinds.contains(&REBIND));
    assert!(kinds.iter().skip_while(|k| **k == RENEW).all(|k| *k != RENEW));
    assert_eq!(now, 200 + 7200 * 1000);
    assert!(c.addr().is_none());
    assert_eq!(c.state(), "soliciting");
}

#[test]
fn renew_reply_extends_and_rebind_takes_other_server() {
    let mut c = Client6::new(MAC, rng(3));
    let iaid = bound(&mut c);
    let t1 = 200 + 1800 * 1000;
    let s = sent(&c.tick(t1));
    let rep = server_msg_for_test(REPLY, s[0].1, &cid(), SID, Some((iaid, 1800, 2880, &[(ADDR, 3600, 7200)])), &[DNS]);
    assert!(matches!(c.rx(&rep, t1 + 5)[..], [Out6::Bound(_)]));
    assert_eq!(c.deadline(), Some(t1 + 5 + 1800 * 1000));
    // At T2 any server may answer the Rebind.
    let t2 = t1 + 5 + 2880 * 1000;
    let _ = c.tick(t1 + 5 + 1800 * 1000);
    let s = sent(&c.tick(t2));
    assert_eq!(s[0].0, REBIND);
    assert!(s[0].3.is_empty());
    let other: &[u8] = &[0, 3, 0, 1, 2, 9, 9, 9, 9, 9];
    let rep = server_msg_for_test(REPLY, s[0].1, &cid(), other, Some((iaid, 0, 0, &[(ADDR, 1000, 2000)])), &[]);
    assert!(matches!(c.rx(&rep, t2 + 1)[..], [Out6::Bound(_)]));
    // T1/T2 0: the client picks 0.5 and 0.8 of the preferred lifetime.
    assert_eq!(c.deadline(), Some(t2 + 1 + 500 * 1000));
}

#[test]
fn information_request_for_dns() {
    let mut c = Client6::new(MAC, rng(4));
    assert!(c.ra(false, false, 0).is_empty());
    let s = sent(&c.ra(false, true, 0));
    assert_eq!((s[0].0, s[0].2), (INFO_REQUEST, None));
    let mut rep = server_msg_for_test(REPLY, s[0].1, &cid(), SID, None, &[DNS, DNS, LL]);
    rep.extend_from_slice(&[0, 32, 0, 4, 0, 0, 0, 5]); // refresh 5 s: raised to 600
    assert!(c.rx(&rep, 10).is_empty());
    assert_eq!(c.dns(), &[DNS, LL]);
    assert_eq!(c.state(), "informed");
    assert_eq!(c.deadline(), Some(10 + 600_000));
    assert!(c.ra(false, true, 20).is_empty());
    assert_eq!(sent(&c.tick(10 + 600_000))[0].0, INFO_REQUEST);
    // An M flag later: addresses too.
    assert_eq!(sent(&c.ra(true, true, 700_000))[0].0, SOLICIT);
}

#[test]
fn retransmit_backoff_caps() {
    let mut c = Client6::new(MAC, rng(5));
    let _ = c.ra(true, false, 0);
    let mut last = 0;
    let mut gaps = Vec::new();
    for _ in 0..20 {
        let d = c.deadline().unwrap();
        gaps.push(d - last);
        last = d;
        assert_eq!(sent(&c.tick(d))[0].0, SOLICIT);
    }
    assert!((900..=1100).contains(&gaps[0]), "{gaps:?}");
    assert!(gaps.iter().all(|&g| g <= 120_000), "{gaps:?}");
    assert!(gaps[19] >= 100_000);
}

#[test]
fn ignores_wrong_xid_client_server() {
    let mut c = Client6::new(MAC, rng(6));
    let s = sent(&c.ra(true, false, 0));
    let iaid = s[0].2.unwrap();
    let ia = Some((iaid, 0, 0, &[(ADDR, 3600, 7200)][..]));
    let bad_xid = [s[0].1[0] ^ 1, s[0].1[1], s[0].1[2]];
    assert!(c.rx(&server_msg_for_test(ADVERTISE, bad_xid, &cid(), SID, ia, &[]), 1).is_empty());
    assert!(c.rx(&server_msg_for_test(ADVERTISE, s[0].1, &[0, 3, 0, 1, 9, 9, 9, 9, 9, 9], SID, ia, &[]), 1).is_empty());
    // A Reply while soliciting is not an answer to anything.
    assert!(c.rx(&server_msg_for_test(REPLY, s[0].1, &cid(), SID, ia, &[]), 1).is_empty());
    // Other IAIDs, link-local, multicast and unspecified addresses are no offer.
    for a in [LL, Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 1), Ipv6Addr::UNSPECIFIED, Ipv6Addr::LOCALHOST] {
        assert!(c.rx(&server_msg_for_test(ADVERTISE, s[0].1, &cid(), SID, Some((iaid, 0, 0, &[(a, 1, 2)])), &[]), 1).is_empty());
    }
    assert!(c.rx(&server_msg_for_test(ADVERTISE, s[0].1, &cid(), SID, Some((iaid ^ 1, 0, 0, &[(ADDR, 1, 2)])), &[]), 1).is_empty());
    // Preferred beyond valid, or valid 0.
    assert!(c.rx(&server_msg_for_test(ADVERTISE, s[0].1, &cid(), SID, Some((iaid, 0, 0, &[(ADDR, 9, 2)])), &[]), 1).is_empty());
    assert!(c.rx(&server_msg_for_test(ADVERTISE, s[0].1, &cid(), SID, Some((iaid, 0, 0, &[(ADDR, 0, 0)])), &[]), 1).is_empty());
    assert_eq!(c.state(), "soliciting");
    // The Request goes to the server that offered; another server's Reply is ignored.
    let s2 = sent(&c.rx(&server_msg_for_test(ADVERTISE, s[0].1, &cid(), SID, ia, &[]), 2));
    assert_eq!(s2[0].0, REQUEST);
    assert!(c.rx(&server_msg_for_test(REPLY, s2[0].1, &cid(), &[0, 3, 0, 1, 7, 7, 7, 7, 7, 7], ia, &[]), 3).is_empty());
    assert_eq!(c.state(), "requesting");
}

#[test]
fn huge_lifetimes_capped() {
    let mut c = Client6::new(MAC, rng(7));
    let s = sent(&c.ra(true, false, 0));
    let iaid = s[0].2.unwrap();
    let ia = Some((iaid, u32::MAX, u32::MAX, &[(ADDR, u32::MAX, u32::MAX)][..]));
    let s = sent(&c.rx(&server_msg_for_test(ADVERTISE, s[0].1, &cid(), SID, ia, &[]), 1));
    let o = c.rx(&server_msg_for_test(REPLY, s[0].1, &cid(), SID, ia, &[]), 2);
    assert_eq!(o, vec![Out6::Bound(IaAddr { addr: ADDR, pref: MAX_LIFE_S, valid: MAX_LIFE_S })]);
}

#[test]
fn noaddrs_reply_drops_and_backs_off() {
    let mut c = Client6::new(MAC, rng(8));
    let iaid = bound(&mut c);
    let t1 = 200 + 1800 * 1000;
    let s = sent(&c.tick(t1));
    let mut rep = server_msg_for_test(REPLY, s[0].1, &cid(), SID, None, &[]);
    let mut ia = iaid.to_be_bytes().to_vec();
    ia.extend_from_slice(&[0; 8]);
    ia.extend_from_slice(&[0, 13, 0, 2, 0, 2]); // NoAddrsAvail
    rep.extend_from_slice(&[0, 3, 0, ia.len() as u8]);
    rep.extend_from_slice(&ia);
    assert_eq!(c.rx(&rep, t1 + 1), vec![Out6::Lost("refused")]);
    assert_eq!(c.deadline(), Some(t1 + 1 + 60_000));
    assert!(c.dns().is_empty());
}

#[test]
fn stop_forgets() {
    let mut c = Client6::new(MAC, rng(9));
    bound(&mut c);
    assert_eq!(c.stop(), vec![Out6::Lost("link")]);
    assert!(c.dns().is_empty() && c.deadline().is_none());
    assert!(c.stop().is_empty());
}

#[test]
fn frames() {
    let p = server_msg_for_test(REPLY, [1, 2, 3], &cid(), SID, None, &[DNS]);
    let me = Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0xaa, 0xbb, 0xcc);
    let f = frame_for_test(LL, me, &p);
    assert_eq!(from_frame(&f), Some(&p[..]));
    // Bad checksum, zero checksum, not to a link-local address, cut short.
    let mut g = f.clone();
    *g.last_mut().unwrap() ^= 1;
    assert!(from_frame(&g).is_none());
    let mut g = f.clone();
    g[14 + 40 + 6..14 + 40 + 8].copy_from_slice(&[0, 0]);
    assert!(from_frame(&g).is_none());
    assert!(from_frame(&frame_for_test(LL, ADDR, &p)).is_none());
    assert!(from_frame(&f[..f.len() - 1]).is_none());
    // An RA is not DHCPv6, and the RA reader takes the flags.
    let r = ra::ra_flags_for_test(LL, 1800, 0xc0, &[]);
    assert!(from_frame(&r).is_none());
    let r = ra::ra_from_frame(&r).unwrap();
    assert!(r.managed && r.other);
}

/// Random and mutated server messages and frames never panic, and nothing
/// the client binds is outside what good_addr allows.
#[test]
fn fuzz() {
    let mut r = rng(0x5eed);
    let mut c = Client6::new(MAC, rng(10));
    let mut now = 0;
    for round in 0..200_000u32 {
        now += (r() % 5000) as u64;
        let mut xid = [0u8; 3];
        for o in c.ra(r() % 3 == 0, r() % 2 == 0, now).into_iter().chain(c.tick(now)) {
            if let Out6::Send(p) = o {
                xid = [p[1], p[2], p[3]];
            }
        }
        let n = (r() % 6) as usize;
        let addrs: Vec<(Ipv6Addr, u32, u32)> = (0..n).map(|_| (Ipv6Addr::from((r() as u128) << 96 | r() as u128), r(), r())).collect();
        let typ = if r() % 2 == 0 { ADVERTISE } else { REPLY };
        let mut m = server_msg_for_test(typ, xid, &cid(), SID, Some((r(), r(), r(), &addrs)), &[DNS]);
        if round % 3 == 0 {
            // Mutate: flip, cut, or append garbage.
            match r() % 3 {
                0 => {
                    let i = r() as usize % m.len();
                    m[i] ^= r() as u8;
                }
                1 => m.truncate(r() as usize % m.len()),
                _ => m.extend((0..r() % 40).map(|_| r() as u8)),
            }
        }
        for o in c.rx(&m, now) {
            if let Out6::Bound(a) = o {
                assert!(good_addr(a.addr) && a.valid <= MAX_LIFE_S && a.pref <= a.valid);
            }
        }
        let f: Vec<u8> = (0..r() % 200).map(|_| r() as u8).collect();
        let _ = from_frame(&f);
        let _ = parse(&f);
    }
}
