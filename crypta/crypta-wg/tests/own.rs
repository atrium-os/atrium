//! Two crypta-wg devices talking: the handshake, transport both ways,
//! replay, cookies under load, retries, rekeying and keepalives.

mod common;
use common::*;
use std::sync::Arc;
use std::time::Duration;

use crypta_wg::tunnel::Event;
use crypta_wg::{Device, LocalKey, Peer, StaticKey};

const A_ADDR: &[u8] = &[192, 0, 2, 1, 0x51, 0x80];
const B_ADDR: &[u8] = &[192, 0, 2, 2, 0xca, 0x6c];

fn pair(psk: Option<[u8; 32]>) -> (Device, Device) {
    let ka = Arc::new(LocalKey::generate());
    let kb = Arc::new(LocalKey::generate());
    let (pa, pb) = (ka.public(), kb.public());
    let mut a = Device::new(ka);
    let mut b = Device::new(kb);
    a.add_peer(Peer { public: pb, psk, persistent_keepalive: None });
    b.add_peer(Peer { public: pa, psk, persistent_keepalive: None });
    (a, b)
}

fn t(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

/// A sends `p` at `now`, everything is delivered both ways until quiet;
/// what B's tun got.
fn exchange(a: &mut Device, b: &mut Device, first: Vec<Vec<u8>>, now: Duration) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let (mut to_b, mut to_a) = (first, Vec::new());
    let (mut at_a, mut at_b) = (Vec::new(), Vec::new());
    for _ in 0..10 {
        for d in to_b.drain(..) {
            let (_, acts) = b.decapsulate(A_ADDR, &d, now);
            to_a.extend(sends(&acts));
            at_b.extend(delivers(&acts));
        }
        for d in to_a.drain(..) {
            let (_, acts) = a.decapsulate(B_ADDR, &d, now);
            to_b.extend(sends(&acts));
            at_a.extend(delivers(&acts));
        }
        if to_b.is_empty() {
            break;
        }
    }
    (at_a, at_b)
}

#[test]
fn the_first_packet_waits_for_the_handshake_then_goes() {
    let (mut a, mut b) = pair(Some([7; 32]));
    let p1 = ipv4(60, 1);
    let acts = a.peer(0).encapsulate(&p1, t(0));
    assert_eq!(events(&acts), vec![Event::HandshakeSent]);
    let (_, at_b) = exchange(&mut a, &mut b, sends(&acts), t(10));
    assert_eq!(at_b, vec![p1], "queued packet delivered once the keys exist");
    assert!(a.peer(0).is_up(t(10)));
    // B now sends with the confirmed keys, and A gets it.
    let p2 = ipv4(1400, 2);
    let back = sends(&b.peer(0).encapsulate(&p2, t(20)));
    assert_eq!(back.len(), 1, "B sends at once, no new handshake");
    let (_, at_a) = exchange(&mut b, &mut a, back, t(20));
    assert_eq!(at_a, vec![p2]);
}

#[test]
fn a_replayed_or_tampered_packet_is_dropped() {
    let (mut a, mut b) = pair(None);
    let acts = a.peer(0).encapsulate(&ipv4(40, 1), t(0));
    exchange(&mut a, &mut b, sends(&acts), t(1));
    let d = sends(&a.peer(0).encapsulate(&ipv4(40, 3), t(2))).remove(0);
    let (_, ok) = b.decapsulate(A_ADDR, &d, t(3));
    assert_eq!(delivers(&ok).len(), 1);
    let (_, again) = b.decapsulate(A_ADDR, &d, t(4));
    assert!(delivers(&again).is_empty());
    assert_eq!(events(&again), vec![Event::Dropped("an old or repeated packet")]);
    let mut bad = sends(&a.peer(0).encapsulate(&ipv4(40, 4), t(5))).remove(0);
    let n = bad.len();
    bad[n - 1] ^= 1;
    let (_, acts) = b.decapsulate(A_ADDR, &bad, t(6));
    assert_eq!(events(&acts), vec![Event::Dropped("a packet that does not open")]);
    // The tampered one did not use up its counter's slot... but its true
    // twin was never sent, so nothing else to check: the next one goes.
    let d = sends(&a.peer(0).encapsulate(&ipv4(40, 5), t(7))).remove(0);
    assert_eq!(delivers(&b.decapsulate(A_ADDR, &d, t(8)).1).len(), 1);
}

#[test]
fn an_initiation_replayed_is_dropped_and_a_stranger_is_refused() {
    let (mut a, mut b) = pair(None);
    let init = sends(&a.peer(0).encapsulate(&ipv4(40, 1), t(0))).remove(0);
    let (_, first) = b.decapsulate(A_ADDR, &init, t(1));
    assert_eq!(sends(&first).len(), 1);
    let (_, again) = b.decapsulate(A_ADDR, &init, t(500));
    assert_eq!(events(&again), vec![Event::Dropped("an initiation replayed or out of order")]);
    // Someone B doesn't know, who knows B's public key.
    let stranger = Arc::new(LocalKey::generate());
    let mut s = Device::new(stranger);
    s.add_peer(Peer { public: b.public(), psk: None, persistent_keepalive: None });
    let init = sends(&s.peer(0).encapsulate(&ipv4(40, 1), t(0))).remove(0);
    let (who, acts) = b.decapsulate(A_ADDR, &init, t(600));
    assert_eq!(who, None);
    assert_eq!(events(&acts), vec![Event::Dropped("an initiation from an unknown key")]);
    // Without B's public key there is no valid mac1: dropped before any DH.
    let mut junk = init.clone();
    junk[20] ^= 1;
    assert_eq!(events(&b.decapsulate(A_ADDR, &junk, t(700)).1), vec![Event::Dropped("a handshake without a valid mac1")]);
}

#[test]
fn under_load_the_responder_asks_for_a_cookie_first() {
    let (mut a, mut b) = pair(None);
    b.force_load = true;
    let init = sends(&a.peer(0).encapsulate(&ipv4(40, 1), t(0))).remove(0);
    let (_, acts) = b.decapsulate(A_ADDR, &init, t(1));
    let reply = sends(&acts).remove(0);
    assert_eq!(reply.len(), 64, "a cookie reply, not a response");
    let (_, acts) = a.decapsulate(B_ADDR, &reply, t(2));
    assert_eq!(events(&acts), vec![Event::CookieReceived]);
    // A retries after REKEY_TIMEOUT, now with mac2, and gets through.
    let retry = sends(&a.peer(0).tick(t(5_400)));
    assert_eq!(retry.len(), 1);
    let (_, at_b) = exchange(&mut a, &mut b, retry, t(5_401));
    assert_eq!(at_b, vec![ipv4(40, 1)]);
}

#[test]
fn retries_every_five_seconds_then_gives_up_at_ninety() {
    let (mut a, _b) = pair(None);
    sends(&a.peer(0).encapsulate(&ipv4(40, 1), t(0)));
    let mut sent = 1;
    let mut gave_up = None;
    for ms in (100..100_000).step_by(100) {
        let acts = a.peer(0).tick(t(ms));
        sent += sends(&acts).len();
        if events(&acts).contains(&Event::HandshakeGaveUp) {
            gave_up = Some(ms);
            break;
        }
    }
    let g = gave_up.expect("gave up");
    assert!((90_000..96_000).contains(&g), "gave up at {g} ms");
    assert!((16..=18).contains(&sent), "{sent} initiations");
    // Nothing is waiting any more: a new packet starts a new round.
    assert_eq!(events(&a.peer(0).encapsulate(&ipv4(40, 2), t(g + 1))), vec![Event::HandshakeSent]);
}

#[test]
fn the_initiator_rekeys_after_two_minutes_without_a_gap() {
    let (mut a, mut b) = pair(None);
    let acts = a.peer(0).encapsulate(&ipv4(40, 1), t(0));
    exchange(&mut a, &mut b, sends(&acts), t(1));
    let acts = a.peer(0).encapsulate(&ipv4(40, 2), t(121_000));
    let out = sends(&acts);
    assert_eq!(out.len(), 2, "the packet with the old keys, and an initiation");
    assert!(events(&acts).contains(&Event::HandshakeSent));
    let (_, at_b) = exchange(&mut a, &mut b, out, t(121_001));
    assert!(at_b.contains(&ipv4(40, 2)));
    // B gets A's next packet on the new keys, and answers on them.
    let d = sends(&a.peer(0).encapsulate(&ipv4(40, 3), t(121_002)));
    let (_, at_b) = exchange(&mut a, &mut b, d, t(121_003));
    assert_eq!(at_b, vec![ipv4(40, 3)]);
    let back = sends(&b.peer(0).encapsulate(&ipv4(40, 4), t(121_004)));
    assert_eq!(back.len(), 1);
    assert_eq!(delivers(&a.decapsulate(B_ADDR, &back[0], t(121_005)).1), vec![ipv4(40, 4)]);
    // Old keys are refused after REJECT_AFTER_TIME.
    assert!(!a.peer(0).is_up(t(121_005 + 180_000)));
}

#[test]
fn data_in_with_nothing_out_gets_a_keepalive_after_ten_seconds() {
    let (mut a, mut b) = pair(None);
    let acts = a.peer(0).encapsulate(&ipv4(40, 1), t(0));
    exchange(&mut a, &mut b, sends(&acts), t(1));
    assert!(sends(&b.peer(0).tick(t(9_000))).is_empty());
    let ka = sends(&b.peer(0).tick(t(10_100)));
    assert_eq!(ka.len(), 1);
    assert_eq!(ka[0].len(), 32, "a keepalive: header and tag");
    let (_, acts) = a.decapsulate(B_ADDR, &ka[0], t(10_200));
    assert!(delivers(&acts).is_empty() && events(&acts).is_empty());
    // A sent data and heard nothing for 15 s: a new handshake.
    let (mut a, mut b) = pair(None);
    let acts = a.peer(0).encapsulate(&ipv4(40, 1), t(0));
    exchange(&mut a, &mut b, sends(&acts), t(1));
    sends(&a.peer(0).encapsulate(&ipv4(40, 2), t(100)));
    assert!(sends(&a.peer(0).tick(t(14_000))).is_empty());
    assert!(events(&a.peer(0).tick(t(15_200))).contains(&Event::HandshakeSent));
}

#[test]
fn a_wrong_preshared_key_never_completes() {
    let ka = Arc::new(LocalKey::generate());
    let kb = Arc::new(LocalKey::generate());
    let (pa, pb) = (ka.public(), kb.public());
    let mut a = Device::new(ka);
    let mut b = Device::new(kb);
    a.add_peer(Peer { public: pb, psk: Some([1; 32]), persistent_keepalive: None });
    b.add_peer(Peer { public: pa, psk: Some([2; 32]), persistent_keepalive: None });
    let init = sends(&a.peer(0).encapsulate(&ipv4(40, 1), t(0))).remove(0);
    let resp = sends(&b.decapsulate(A_ADDR, &init, t(1)).1).remove(0);
    assert_eq!(events(&a.decapsulate(B_ADDR, &resp, t(2)).1), vec![Event::Dropped("a response that does not open")]);
    assert!(!a.peer(0).is_up(t(3)));
}
