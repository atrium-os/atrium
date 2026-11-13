use crate::ie::{self, Pmf, Security};
use crate::mgmt;
use crate::sta::{Out, Req, Station};
use aura_ipc::Line;
use aura_proto::rx_flags;
use aura_rsn::crypto::Akm;
use aura_rsn::rsne::{self, Rsne};

const AP: [u8; 6] = [0x02, 0, 0, 0, 0, 0];
const OWN: [u8; 6] = [0x02, 0xa5, 0, 0, 0, 1];

fn beacon(ssid: &[u8], rsne: Option<Vec<u8>>, privacy: bool, ch: u8) -> Vec<u8> {
    let mut f = vec![0x80, 0, 0, 0];
    f.extend_from_slice(&[0xff; 6]);
    f.extend_from_slice(&AP);
    f.extend_from_slice(&AP);
    f.extend_from_slice(&[0, 0]);
    f.extend_from_slice(&[0; 8]);
    f.extend_from_slice(&100u16.to_le_bytes());
    let cap: u16 = 0x0401 | if privacy { 0x10 } else { 0 };
    f.extend_from_slice(&cap.to_le_bytes());
    f.push(0);
    f.push(ssid.len() as u8);
    f.extend_from_slice(ssid);
    f.extend_from_slice(&[1, 8, 0x82, 0x84, 0x8b, 0x96, 0x0c, 0x12, 0x18, 0x24, 3, 1, ch, 5, 4, 0, 2, 0, 0]);
    if let Some(r) = rsne {
        f.extend_from_slice(&r);
    }
    f.extend_from_slice(&[50, 4, 0x30, 0x48, 0x60, 0x6c]);
    f.extend_from_slice(&[221, 24, 0x00, 0x50, 0xf2, 2, 1, 1, 0, 0, 3, 0xa4, 0, 0, 0x27, 0xa4, 0, 0, 0x42, 0x43, 0x5e, 0, 0x62, 0x32, 0x2f, 0]);
    f
}

fn wpa2_pmf() -> Vec<u8> {
    let mut r = Rsne::build(rsne::CIPHER_CCMP128, rsne::CIPHER_CCMP128, Akm::Psk.suite(), rsne::CAP_MFPC | rsne::CAP_MFPR, None);
    // Two AKMs as hostapd advertises "WPA-PSK WPA-PSK-SHA256".
    let body = [&r[2..14], &[2, 0, 0x00, 0x0f, 0xac, 2, 0x00, 0x0f, 0xac, 6], &r[20..]].concat();
    r = vec![48, body.len() as u8];
    r.extend_from_slice(&body);
    r
}

#[test]
fn beacon_parsing_and_security() {
    let b = ie::parse_bss(&beacon(b"aura-test", Some(wpa2_pmf()), true, 6), 2437, -40).unwrap();
    assert_eq!(b.ssid, b"aura-test");
    assert_eq!(b.dtim, 2);
    assert!(b.wmm);
    assert_eq!(b.security, Security::Psk { akm: Akm::PskSha256, pmf: Pmf::Required });
    assert_eq!(ie::rate_bits(&b.rates, true), 0b1111);
    assert_eq!(ie::rate_bits(&b.rates, false), 0xfff);
    // Heard on the wrong channel: dropped.
    assert!(ie::parse_bss(&beacon(b"x", None, false, 6), 2412, -40).is_none());
    // WEP: refused.
    let w = ie::parse_bss(&beacon(b"x", None, true, 6), 2437, -40).unwrap();
    assert!(matches!(w.security, Security::Refused("wep")));
    // TKIP group: refused.
    let t = Rsne::build(rsne::CIPHER_TKIP, rsne::CIPHER_CCMP128, Akm::Psk.suite(), 0, None);
    let t = ie::parse_bss(&beacon(b"x", Some(t), true, 6), 2437, -40).unwrap();
    assert!(matches!(t.security, Security::Refused("tkip")));
    // Mutations never panic.
    let base = beacon(b"aura-test", Some(wpa2_pmf()), true, 6);
    let mut x: u64 = 0x2545F4914F6CDD1D;
    for i in 0..100_000 {
        let mut f = base.clone();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let n = f.len();
        f[(x as usize) % n] = (x >> 40) as u8;
        if i % 5 == 0 {
            f.truncate((x >> 20) as usize % (f.len() + 1));
        }
        if f.len() >= 24 {
            let _ = ie::parse_bss(&f, 2437, -40);
            let _ = mgmt::parse_assoc_resp(&f);
            let _ = ie::has_wmm(&f[24..]);
        }
    }
}

fn kernel(o: &[Out]) -> Vec<&Req> {
    o.iter().filter_map(|x| if let Out::Kernel(r) = x { Some(r) } else { None }).collect()
}

fn ctl_states(o: &[Out]) -> Vec<String> {
    o.iter().filter_map(|x| if let Out::Ctl(l) = x { l.get("state").map(String::from) } else { None }).collect()
}

fn reply(sub: u8, body: &[u8], protected: bool) -> Vec<u8> {
    let mut f = vec![sub << 4, if protected { 0x40 } else { 0 }, 0, 0];
    f.extend_from_slice(&OWN);
    f.extend_from_slice(&AP);
    f.extend_from_slice(&AP);
    f.extend_from_slice(&[0, 0]);
    f.extend_from_slice(body);
    f
}

fn joined(pmf_rsne: bool) -> (Station, Vec<Out>) {
    let mut s = Station::new(OWN, Box::new(|b: &mut [u8]| b.fill(7)));
    let o = s.cmd(&Line::new("join").with_hex("ssid", b"aura-test").with("security", "psk"), 0);
    assert!(matches!(kernel(&o)[0], Req::Scan { mac } if mac[0] & 3 == 2));
    let r = if pmf_rsne { wpa2_pmf() } else { Rsne::build(rsne::CIPHER_CCMP128, rsne::CIPHER_CCMP128, Akm::Psk.suite(), 0, None) };
    s.mgmt_rx(2437, -40, rx_flags::SCAN, &beacon(b"aura-test", Some(r), true, 6), 1);
    // An open evil twin with the same SSID is never chosen.
    let mut twin = beacon(b"aura-test", None, false, 6);
    twin[10..16].copy_from_slice(&[2, 9, 9, 9, 9, 9]);
    twin[16..22].copy_from_slice(&[2, 9, 9, 9, 9, 9]);
    s.mgmt_rx(2437, -20, rx_flags::SCAN, &twin, 1);
    let o = s.scan_done(0, 2);
    let k = kernel(&o);
    assert!(matches!(k[0], Req::TxMgmt { freq: 2437, frame } if mgmt::subtype(frame) == mgmt::AUTH && mgmt::addr1(frame) == AP));
    let o = s.mgmt_rx(2437, -40, 0, &reply(mgmt::AUTH, &[0, 0, 2, 0, 0, 0], false), 3);
    let k = kernel(&o);
    let Req::TxMgmt { frame, .. } = k[0] else { panic!() };
    assert_eq!(mgmt::subtype(frame), mgmt::ASSOC_REQ);
    // Association response with a WMM parameter element.
    let mut ar = vec![0x11, 0x04, 0, 0, 0x05, 0xc0];
    ar.extend_from_slice(&[221, 24, 0x00, 0x50, 0xf2, 2, 1, 1, 0, 0, 3, 0xa4, 0, 0, 0x27, 0xa4, 0, 0, 0x42, 0x43, 0x5e, 0, 0x62, 0x32, 0x2f, 0]);
    let o = s.mgmt_rx(2437, -40, 0, &reply(mgmt::ASSOC_RESP, &ar, false), 4);
    (s, o)
}

#[test]
fn join_flow_and_pmf() {
    let (mut s, o) = joined(true);
    let k = kernel(&o);
    let Req::SetBss(b) = k[0] else { panic!("{k:?}") };
    assert_eq!(b.aid, 5);
    assert_ne!(b.flags & aura_proto::bss_flags::QOS, 0);
    assert_ne!(b.flags & aura_proto::bss_flags::PMF, 0);
    let assoc = o.iter().find_map(|x| if let Out::Keys(l) = x { Some(l) } else { None }).unwrap();
    assert_eq!(assoc.verb, "assoc");
    assert_eq!(assoc.get("akm"), Some("6"));
    assert_eq!(assoc.hex("ap_rsne").unwrap(), wpa2_pmf());
    let own = Rsne::parse(&assoc.hex("own_rsne").unwrap()[2..]).unwrap();
    assert!(own.mfpc());
    assert_eq!(own.akms, vec![Akm::PskSha256.suite()]);
    let o = s.keys_msg(&Line::new("authorized"));
    assert_eq!(ctl_states(&o), vec!["authorized"]);

    // Unprotected deauth: ignored, SA Query sent (protected).
    let o = s.mgmt_rx(2437, -40, 0, &reply(mgmt::DEAUTH, &[7, 0], false), 10);
    assert!(ctl_states(&o).is_empty());
    let k = kernel(&o);
    let Req::TxMgmt { frame, .. } = k[0] else { panic!() };
    assert!(mgmt::protected(frame));
    assert_eq!(mgmt::parse_sa_query(frame), Some((mgmt::SA_QUERY_REQ, [7, 7])));
    // Protected answer: association kept.
    let o = s.mgmt_rx(2437, -40, rx_flags::DECRYPTED | rx_flags::MIC_OK, &reply(mgmt::ACTION, &[8, 1, 7, 7], true), 11);
    assert!(ctl_states(&o).is_empty());
    assert!(s.tick(5000).is_empty());
    assert_eq!(s.state_name(), "authorized");
    // AP's SA Query request: answered.
    let o = s.mgmt_rx(2437, -40, rx_flags::DECRYPTED | rx_flags::MIC_OK, &reply(mgmt::ACTION, &[8, 0, 1, 2], true), 12);
    let k = kernel(&o);
    assert!(matches!(k[0], Req::TxMgmt { frame, .. } if mgmt::parse_sa_query(frame) == Some((1, [1, 2]))));
    // Unprotected deauth with no answer: gone after the SA Query timeout.
    s.mgmt_rx(2437, -40, 0, &reply(mgmt::DEAUTH, &[7, 0], false), 20);
    let o = s.tick(2000);
    assert_eq!(ctl_states(&o), vec!["disconnected"]);
    assert!(kernel(&o).iter().any(|r| matches!(r, Req::Disconnect { .. })));
}

#[test]
fn deauth_without_pmf_and_timeouts() {
    let (mut s, _) = joined(false);
    s.keys_msg(&Line::new("authorized"));
    let o = s.mgmt_rx(2437, -40, 0, &reply(mgmt::DEAUTH, &[7, 0], false), 10);
    assert_eq!(ctl_states(&o), vec!["disconnected"]);
    // A deauth from someone else is ignored.
    let (mut s, _) = joined(false);
    let mut f = reply(mgmt::DEAUTH, &[7, 0], false);
    f[10] = 0x0a;
    assert!(s.mgmt_rx(2437, -40, 0, &f, 10).is_empty());
    // Handshake timeout.
    let o = s.tick(60_000);
    assert_eq!(ctl_states(&o), vec!["disconnected"]);
    // Not found.
    let mut s = Station::new(OWN, Box::new(|b: &mut [u8]| b.fill(1)));
    s.cmd(&Line::new("join").with_hex("ssid", b"nope").with("security", "psk"), 0);
    let o = s.scan_done(0, 1);
    assert_eq!(ctl_states(&o), vec!["failed"]);
}
