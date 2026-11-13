use super::*;
use aura_proto::{bss_flags, Cipher, List, RegRule, Width};
use frame::{BROADCAST, RFC1042};
use std::vec::Vec;

const OWN: Mac = [0x02, 0, 0, 0, 0, 0x01];
const AP: Mac = [0x02, 0xaa, 0, 0, 0, 0x02];
const OTHER: Mac = [0x02, 0xbb, 0, 0, 0, 0x03];

#[derive(Default)]
struct Mock {
    log: Vec<&'static str>,
    keys_set: usize,
}

impl Radio for Mock {
    fn own_mac(&self) -> Mac {
        OWN
    }
    fn scan(&mut self, _: &ScanReq<'_>, _: &Regulatory) -> Result<(), i32> {
        self.log.push("scan");
        Ok(())
    }
    fn set_bss(&mut self, _: &Bss) -> Result<(), i32> {
        self.log.push("set_bss");
        Ok(())
    }
    fn clear_bss(&mut self) {
        self.log.push("clear_bss");
    }
    fn tx_mgmt(&mut self, _: u16, _: &[u8]) -> Result<(), i32> {
        self.log.push("tx_mgmt");
        Ok(())
    }
    fn set_key(&mut self, _: &Key) -> Result<(), i32> {
        self.keys_set += 1;
        Ok(())
    }
    fn del_key(&mut self, _: KeyKind, _: u8, _: &Mac) -> Result<(), i32> {
        Ok(())
    }
    fn set_power_save(&mut self, _: PowerSave) -> Result<(), i32> {
        Ok(())
    }
    fn set_mac(&mut self, _: &Mac) -> Result<(), i32> {
        self.log.push("set_mac");
        Ok(())
    }
}

fn bss(freq: u16) -> Bss {
    Bss {
        bssid: AP,
        freq,
        width: Width::W20,
        center1: 0,
        center2: 0,
        aid: 1,
        beacon_int: 100,
        dtim_period: 1,
        flags: bss_flags::QOS,
        basic_rates: 0,
        rates: 0x0ff0,
        ht_mcs: [0; 10],
        vht_rx_mcs: 0,
        vht_tx_mcs: 0,
        he_mcs: [0; 6],
        ampdu_factor: 0,
        ampdu_density: 0,
    }
}

fn joined() -> Wlan<Mock> {
    let mut w = Wlan::new(Mock::default());
    assert_eq!(w.ctl(&Msg::SetBss(bss(2412))), 0);
    w
}

fn ptk(byte: u8) -> Key {
    let mut key = [0u8; 32];
    key[..16].copy_from_slice(&[byte; 16]);
    Key { kind: KeyKind::Pairwise, idx: 0, cipher: Cipher::Ccmp128, addr: AP, rsc: [0; 6], key }
}

fn gtk(rsc: u64) -> Key {
    let mut key = [0u8; 32];
    key[..16].copy_from_slice(&[7; 16]);
    let mut r = [0u8; 6];
    r.copy_from_slice(&rsc.to_le_bytes()[..6]);
    Key { kind: KeyKind::Group, idx: 1, cipher: Cipher::Ccmp128, addr: BROADCAST, rsc: r, key }
}

/// A QoS data frame from the AP. `fc1` extra flags, TID, A-MSDU bit.
fn data(da: Mac, sa: Mac, tid: u8, amsdu: bool, protected: bool, body: &[u8]) -> Vec<u8> {
    let mut f = std::vec![0x88, 0x02 | if protected { 0x40 } else { 0 }, 0, 0];
    f.extend_from_slice(&da);
    f.extend_from_slice(&AP);
    f.extend_from_slice(&sa);
    f.extend_from_slice(&[0x10, 0x00]);
    f.push(tid | if amsdu { 0x80 } else { 0 });
    f.push(0);
    f.extend_from_slice(body);
    f
}

fn snap(et: u16, payload: &[u8]) -> Vec<u8> {
    let mut v = RFC1042.to_vec();
    v.extend_from_slice(&et.to_be_bytes());
    v.extend_from_slice(payload);
    v
}

fn sub(da: Mac, sa: Mac, msdu: &[u8], last: bool) -> Vec<u8> {
    let mut v = da.to_vec();
    v.extend_from_slice(&sa);
    v.extend_from_slice(&(msdu.len() as u16).to_be_bytes());
    v.extend_from_slice(msdu);
    if !last {
        while v.len() % 4 != 0 {
            v.push(0);
        }
    }
    v
}

const ENC: RxInfo = RxInfo { decrypted: true, pn: 0, key_idx: 0 };

fn rx(w: &mut Wlan<Mock>, f: &[u8], info: RxInfo) -> Result<Vec<(Dest, u16, Vec<u8>)>, Drop> {
    w.rx_data(f, &info).map(|r| r.iter().map(|m| (m.dest, m.ethertype, m.payload.to_vec())).collect())
}

#[test]
fn nothing_before_association() {
    let mut w = Wlan::new(Mock::default());
    let f = data(OWN, AP, 0, false, false, &snap(0x0800, b"ip"));
    assert_eq!(rx(&mut w, &f, RxInfo::default()), Err(Drop::NotAssociated));
    assert_eq!(w.tx_data(TxFrom::Stack, &AP, &OWN, 0x0800, 0), Err(Drop::NotAssociated));
}

#[test]
fn controlled_port() {
    let mut w = joined();
    let ip = data(OWN, OTHER, 0, false, false, &snap(0x0800, b"ip"));
    let eapol = data(OWN, AP, 0, false, false, &snap(0x888e, b"\x02\x03\x00\x5f"));
    // Closed: data dropped, EAPOL to aura-keys.
    assert_eq!(rx(&mut w, &ip, RxInfo::default()), Err(Drop::PortClosed));
    assert_eq!(rx(&mut w, &eapol, RxInfo::default()).unwrap(), [(Dest::Keys, 0x888e, b"\x02\x03\x00\x5f".to_vec())]);
    // Open network: aura-keys opens the port.
    assert_eq!(w.keys(&Msg::PortAuthorized(true)), 0);
    assert_eq!(rx(&mut w, &ip, RxInfo::default()).unwrap(), [(Dest::Stack, 0x0800, b"ip".to_vec())]);
    // EAPOL never reaches the stack, even with the port open.
    assert_eq!(rx(&mut w, &eapol, RxInfo::default()).unwrap()[0].0, Dest::Keys);
}

#[test]
fn eapol_only_from_the_ap_to_us() {
    let mut w = joined();
    let spoof = data(OWN, OTHER, 0, false, false, &snap(0x888e, b"\x02\x03\x00\x5f"));
    assert_eq!(rx(&mut w, &spoof, RxInfo::default()), Err(Drop::BadEapol));
    let bcast = data(BROADCAST, AP, 0, false, false, &snap(0x888e, b"\x02\x03\x00\x5f"));
    assert_eq!(rx(&mut w, &bcast, RxInfo::default()), Err(Drop::BadEapol));
}

#[test]
fn keys_replay_and_cleartext() {
    let mut w = joined();
    assert_eq!(w.keys(&Msg::SetKey(ptk(1))), 0);
    assert_eq!(w.keys(&Msg::PortAuthorized(true)), 0);
    let clear = data(OWN, OTHER, 0, false, false, &snap(0x0800, b"ip"));
    assert_eq!(rx(&mut w, &clear, RxInfo::default()), Err(Drop::Unprotected));
    // Cleartext EAPOL is refused too once a key exists.
    let eapol = data(OWN, AP, 0, false, false, &snap(0x888e, b"\x02\x03\x00\x5f"));
    assert_eq!(rx(&mut w, &eapol, RxInfo::default()), Err(Drop::Unprotected));
    let prot = data(OWN, OTHER, 3, false, true, &snap(0x0800, b"ip"));
    assert_eq!(rx(&mut w, &prot, RxInfo { decrypted: false, ..ENC }), Err(Drop::NotDecrypted));
    assert!(rx(&mut w, &prot, RxInfo { pn: 1, ..ENC }).is_ok());
    assert_eq!(rx(&mut w, &prot, RxInfo { pn: 1, ..ENC }), Err(Drop::Replay));
    assert!(rx(&mut w, &prot, RxInfo { pn: 2, ..ENC }).is_ok());
    // Each TID has its own counter.
    let tid5 = data(OWN, OTHER, 5, false, true, &snap(0x0800, b"ip"));
    assert!(rx(&mut w, &tid5, RxInfo { pn: 1, ..ENC }).is_ok());
    // Wrong key index.
    assert_eq!(rx(&mut w, &prot, RxInfo { pn: 9, key_idx: 1, ..ENC }), Err(Drop::NotDecrypted));
}

#[test]
fn krack_reinstall_keeps_counters() {
    let mut w = joined();
    assert_eq!(w.keys(&Msg::SetKey(ptk(1))), 0);
    w.keys(&Msg::PortAuthorized(true));
    let prot = data(OWN, OTHER, 0, false, true, &snap(0x0800, b"ip"));
    assert!(rx(&mut w, &prot, RxInfo { pn: 10, ..ENC }).is_ok());
    // Retransmitted message 3 makes aura-keys install the same key again.
    assert_eq!(w.keys(&Msg::SetKey(ptk(1))), 0);
    assert_eq!(w.radio.keys_set, 1, "identical key must not be reprogrammed");
    assert_eq!(rx(&mut w, &prot, RxInfo { pn: 10, ..ENC }), Err(Drop::Replay));
    // A new key (rekey) starts fresh counters.
    assert_eq!(w.keys(&Msg::SetKey(ptk(2))), 0);
    assert_eq!(w.radio.keys_set, 2);
    assert!(rx(&mut w, &prot, RxInfo { pn: 1, ..ENC }).is_ok());
}

#[test]
fn group_key_starts_at_rsc() {
    let mut w = joined();
    w.keys(&Msg::SetKey(ptk(1)));
    w.keys(&Msg::SetKey(gtk(500)));
    w.keys(&Msg::PortAuthorized(true));
    let f = data(BROADCAST, OTHER, 0, false, true, &snap(0x0806, b"arp"));
    assert_eq!(rx(&mut w, &f, RxInfo { pn: 500, key_idx: 1, ..ENC }), Err(Drop::Replay));
    assert!(rx(&mut w, &f, RxInfo { pn: 501, key_idx: 1, ..ENC }).is_ok());
    // Group key index from the wrong family.
    let mut bad = gtk(0);
    bad.idx = 4;
    assert_eq!(w.keys(&Msg::SetKey(bad)), 0, "proto validates ranges; table ignores");
}

#[test]
fn amsdu_rules() {
    let mut w = joined();
    w.keys(&Msg::PortAuthorized(true));
    let a = snap(0x0800, b"one");
    let b = snap(0x86dd, b"two!");
    let mut body = sub(OWN, OTHER, &a, false);
    body.extend(sub(BROADCAST, OTHER, &b, true));
    let f = data(OWN, OTHER, 0, true, false, &body);
    let got = rx(&mut w, &f, RxInfo::default()).unwrap();
    assert_eq!(got, [(Dest::Stack, 0x0800, b"one".to_vec()), (Dest::Stack, 0x86dd, b"two!".to_vec())]);

    // FragAttacks: a normal frame reinterpreted as an A-MSDU starts with the
    // RFC 1042 header where the first DA would be.
    let mut inj = RFC1042.to_vec();
    inj.extend_from_slice(&[0x08, 0x00]);
    inj.extend(sub(OWN, OTHER, &a, true));
    let f = data(OWN, OTHER, 0, true, false, &inj);
    assert_eq!(rx(&mut w, &f, RxInfo::default()), Err(Drop::AmsduInjection));

    // A subframe for another station drops the whole A-MSDU.
    let mut body = sub(OWN, OTHER, &a, false);
    body.extend(sub(OTHER, AP, &b, true));
    let f = data(OWN, OTHER, 0, true, false, &body);
    assert_eq!(rx(&mut w, &f, RxInfo::default()), Err(Drop::AmsduInjection));

    // Non-zero padding.
    let mut body = sub(OWN, OTHER, &a, false);
    let l = body.len();
    body[l - 1] = 1;
    body.extend(sub(OWN, OTHER, &b, true));
    let f = data(OWN, OTHER, 0, true, false, &body);
    assert_eq!(rx(&mut w, &f, RxInfo::default()), Err(Drop::AmsduInjection));

    // Reflection of our own frames.
    let f = data(OWN, OTHER, 0, true, false, &sub(OWN, OWN, &a, true));
    assert_eq!(rx(&mut w, &f, RxInfo::default()), Err(Drop::FromSelf));

    // One bad MSDU (unknown encapsulation) drops all.
    let mut body = sub(OWN, OTHER, &a, false);
    body.extend(sub(OWN, OTHER, b"\x00\x01\x02\x03\x04\x05\x06\x07", true));
    let f = data(OWN, OTHER, 0, true, false, &body);
    assert_eq!(rx(&mut w, &f, RxInfo::default()), Err(Drop::UnknownEncap));
}

#[test]
fn header_checks() {
    let mut w = joined();
    w.keys(&Msg::PortAuthorized(true));
    let good = data(OWN, OTHER, 0, false, false, &snap(0x0800, b"ip"));
    let mut f = good.clone();
    f[1] = 0x01; // ToDS: a frame a station would send
    assert_eq!(rx(&mut w, &f, RxInfo::default()), Err(Drop::WrongDirection));
    let mut f = good.clone();
    f[1] = 0x03; // four-address
    assert_eq!(rx(&mut w, &f, RxInfo::default()), Err(Drop::WrongDirection));
    let mut f = good.clone();
    f[10..16].copy_from_slice(&OTHER);
    assert_eq!(rx(&mut w, &f, RxInfo::default()), Err(Drop::NotFromBss));
    let mut f = good.clone();
    f[4..10].copy_from_slice(&OTHER);
    assert_eq!(rx(&mut w, &f, RxInfo::default()), Err(Drop::NotForUs));
    let mut f = good.clone();
    f[1] |= 0x04;
    assert_eq!(rx(&mut w, &f, RxInfo::default()), Err(Drop::Fragment));
    let mut f = good.clone();
    f[22] |= 1;
    assert_eq!(rx(&mut w, &f, RxInfo::default()), Err(Drop::Fragment));
    let mut f = good.clone();
    f[0] = 0xc8; // QoS Null
    assert_eq!(rx(&mut w, &f, RxInfo::default()), Err(Drop::NoBody));
    let mut f = good.clone();
    f[0] = 0x80; // beacon
    assert_eq!(rx(&mut w, &f, RxInfo::default()), Err(Drop::NotData));
    // Our own address as the source of a non-A-MSDU frame.
    let f = data(OWN, OWN, 0, false, false, &snap(0x0800, b"ip"));
    assert_eq!(rx(&mut w, &f, RxInfo::default()), Err(Drop::FromSelf));
    // A plain (non-QoS) data frame also works.
    let mut f = std::vec![0x08, 0x02, 0, 0];
    f.extend_from_slice(&OWN);
    f.extend_from_slice(&AP);
    f.extend_from_slice(&OTHER);
    f.extend_from_slice(&[0, 0]);
    f.extend(snap(0x0800, b"x"));
    assert_eq!(rx(&mut w, &f, RxInfo::default()).unwrap(), [(Dest::Stack, 0x0800, b"x".to_vec())]);
}

#[test]
fn transmit_rules() {
    let mut w = joined();
    assert_eq!(w.tx_data(TxFrom::Stack, &OTHER, &OWN, 0x0800, 0), Err(Drop::PortClosed));
    assert_eq!(w.tx_data(TxFrom::Stack, &AP, &OWN, 0x888e, 0), Err(Drop::BadEapol));
    assert_eq!(w.tx_data(TxFrom::Keys, &AP, &OWN, 0x0800, 0), Err(Drop::BadEapol));
    assert_eq!(w.tx_data(TxFrom::Keys, &OTHER, &OWN, 0x888e, 0), Err(Drop::BadEapol));
    assert_eq!(w.tx_data(TxFrom::Keys, &AP, &OTHER, 0x888e, 0), Err(Drop::FromSelf));
    let h = w.tx_data(TxFrom::Keys, &AP, &OWN, 0x888e, 0).unwrap();
    assert!(!h.encrypt);
    assert_eq!(h.tid, 7);
    let mut want = std::vec![0x88, 0x01, 0, 0];
    want.extend_from_slice(&AP);
    want.extend_from_slice(&OWN);
    want.extend_from_slice(&AP);
    want.extend_from_slice(&[0, 0, 7, 0]);
    want.extend(snap(0x888e, b""));
    assert_eq!(h.as_bytes(), &want[..]);

    w.keys(&Msg::SetKey(ptk(1)));
    w.keys(&Msg::PortAuthorized(true));
    let h = w.tx_data(TxFrom::Stack, &OTHER, &OWN, 0x0800, 5).unwrap();
    assert!(h.encrypt);
    assert_eq!(h.bytes[1], 0x41);
    assert_eq!(&h.bytes[16..22], &OTHER);
    assert_eq!(h.tid, 5);
    // AARP goes in bridge-tunnel encapsulation.
    let h = w.tx_data(TxFrom::Stack, &OTHER, &OWN, 0x80f3, 0).unwrap();
    assert_eq!(&h.bytes[26..32], &frame::BRIDGE_TUNNEL);
}

fn mgmt(sa: Mac) -> Vec<u8> {
    let mut f = std::vec![0xb0, 0, 0, 0];
    f.extend_from_slice(&AP);
    f.extend_from_slice(&sa);
    f.extend_from_slice(&AP);
    f.extend_from_slice(&[0, 0, 0, 0, 1, 0, 0, 0]);
    f
}

#[test]
fn control_requests() {
    use errno::*;
    let mut w = Wlan::new(Mock::default());
    // Spoofed source address.
    assert_eq!(w.ctl(&Msg::TxMgmt { freq: 2412, frame: &mgmt(OTHER) }), EPERM);
    // World rules: 2412 may initiate, 5180 may not until a country is set.
    assert_eq!(w.ctl(&Msg::TxMgmt { freq: 2412, frame: &mgmt(OWN) }), 0);
    assert_eq!(w.ctl(&Msg::TxMgmt { freq: 5180, frame: &mgmt(OWN) }), EPERM);
    // Scan outside the rules, then a valid one, then busy.
    let mut b0 = [0u8; 0];
    let ssids = List::build(&[], &mut b0).unwrap();
    let mut b1 = [0u8; 2];
    let freqs = List::build(&[aura_proto::Freq(2484)], &mut b1).unwrap();
    let scan = |freqs| Msg::Scan(ScanReq { flags: 0, mac: [0; 6], dwell_ms: 0, ssids, freqs });
    assert_eq!(w.ctl(&scan(freqs)), EINVAL);
    let mut b2 = [0u8; 0];
    let all = List::build(&[], &mut b2).unwrap();
    assert_eq!(w.ctl(&scan(all)), 0);
    assert_eq!(w.ctl(&scan(all)), EBUSY);
    assert_eq!(w.ctl(&Msg::SetMac(OTHER)), EBUSY);
    w.scan_done();
    assert_eq!(w.ctl(&Msg::SetMac(OTHER)), 0);
    // Keys channel needs a BSS.
    assert_eq!(w.keys(&Msg::SetKey(ptk(1))), ENOTCONN);
    assert_eq!(w.ctl(&Msg::Disconnect { reason: 3 }), ENOTCONN);
    // Join 5180 is allowed (an AP heard there), twice is refused.
    assert_eq!(w.ctl(&Msg::SetBss(bss(5180))), 0);
    assert_eq!(w.ctl(&Msg::SetBss(bss(5180))), EALREADY);
    // On the BSS channel we may transmit even though it is NO_IR.
    assert_eq!(w.ctl(&Msg::TxMgmt { freq: 5180, frame: &mgmt(OWN) }), 0);
    assert_eq!(w.ctl(&Msg::SetMac(OTHER)), EBUSY);
    // Key address rules.
    let mut k = ptk(1);
    k.addr = OTHER;
    assert_eq!(w.keys(&Msg::SetKey(k)), EINVAL);
    let mut g = gtk(0);
    g.addr = AP;
    assert_eq!(w.keys(&Msg::SetKey(g)), EINVAL);
    assert_eq!(w.keys(&Msg::SetKey(ptk(1))), 0);
    w.keys(&Msg::PortAuthorized(true));
    // A country without 5 GHz takes the BSS away, with its keys and port.
    let mut rb = [0u8; 20];
    let rules = List::build(
        &[RegRule { start_khz: 2_402_000, end_khz: 2_482_000, max_bw_khz: 40_000, max_eirp_mbm: 2000, flags: 0 }],
        &mut rb,
    )
    .unwrap();
    assert_eq!(w.ctl(&Msg::SetRegulatory { alpha2: *b"XX", rules }), 0);
    assert!(w.bss().is_none() && !w.port_open());
    assert_eq!(w.radio.log.last(), Some(&"clear_bss"));
    // Wrong channel kinds.
    assert_eq!(w.ctl(&Msg::PortAuthorized(true)), EINVAL);
    w.ctl(&Msg::SetBss(bss(2412)));
    assert_eq!(w.keys(&Msg::Scan(ScanReq { flags: 0, mac: [0; 6], dwell_ms: 0, ssids, freqs: all })), EINVAL);
    // After a disassociation keys are gone: the same key installs again.
    w.keys(&Msg::SetKey(ptk(1)));
    let before = w.radio.keys_set;
    w.disassociated();
    w.ctl(&Msg::SetBss(bss(2412)));
    w.keys(&Msg::SetKey(ptk(1)));
    assert_eq!(w.radio.keys_set, before + 1);
}

#[test]
fn world_regulatory() {
    let r = Regulatory::default();
    assert!(r.may_initiate(2412) && r.may_initiate(2462));
    assert!(r.allowed(2467) && !r.may_initiate(2467));
    assert!(r.allowed(5180) && !r.may_initiate(5180));
    assert!(r.allowed(5955) && !r.may_initiate(5955));
    assert!(!r.allowed(2484));
}

/// No input panics the receive path.
#[test]
fn rx_mutations_never_panic() {
    let mut w = joined();
    w.keys(&Msg::SetKey(ptk(1)));
    w.keys(&Msg::SetKey(gtk(0)));
    w.keys(&Msg::PortAuthorized(true));
    let a = snap(0x0800, b"one");
    let mut am = sub(OWN, OTHER, &a, false);
    am.extend(sub(BROADCAST, OTHER, &a, true));
    let seeds = [
        data(OWN, OTHER, 0, false, true, &a),
        data(OWN, OTHER, 0, true, true, &am),
        data(OWN, AP, 0, false, true, &snap(0x888e, b"\x02\x03\x00\x5f")),
        data(BROADCAST, OTHER, 0, false, true, &a),
    ];
    let mut x: u64 = 0x1234_5678_9abc_def1;
    let mut rnd = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut ok = 0;
    for i in 0..200_000u64 {
        let mut f = seeds[(rnd() as usize) % seeds.len()].clone();
        for _ in 0..1 + rnd() % 4 {
            let j = (rnd() as usize) % f.len();
            match rnd() % 4 {
                0 => f[j] ^= 1 << (rnd() % 8),
                1 => f[j] = rnd() as u8,
                2 => f.truncate(j),
                _ => f.push(rnd() as u8),
            }
            if f.is_empty() {
                f.push(0);
            }
        }
        let info = RxInfo { decrypted: rnd() % 2 == 0, pn: i + 1, key_idx: (rnd() % 3) as u8 };
        if let Ok(r) = w.rx_data(&f, &info) {
            ok += r.iter().count();
        }
    }
    assert!(ok > 100, "too few mutated frames accepted: {ok}");
}
