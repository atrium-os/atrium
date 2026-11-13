use super::*;
use std::vec::Vec;

const AP: Mac = [0x02, 0x11, 0x22, 0x33, 0x44, 0x55];

fn auth_frame() -> Vec<u8> {
    // Authentication (subtype 11): FC, duration, DA, SA, BSSID, seq, body.
    let mut f = std::vec![0xb0, 0x00, 0, 0];
    f.extend_from_slice(&AP);
    f.extend_from_slice(&[0x02, 1, 2, 3, 4, 5]);
    f.extend_from_slice(&AP);
    f.extend_from_slice(&[0, 0]);
    f.extend_from_slice(&[3, 0, 1, 0, 0, 0]); // SAE, seq 1, status 0
    f
}

fn beacon_frame() -> Vec<u8> {
    let mut f = auth_frame();
    f[0] = 0x80;
    f
}

fn key() -> Key {
    let mut k = Key {
        kind: KeyKind::Pairwise,
        idx: 0,
        cipher: Cipher::Ccmp128,
        addr: AP,
        rsc: [0; 6],
        key: [0; 32],
    };
    k.key[..16].copy_from_slice(&[0xaa; 16]);
    k
}

fn bss() -> Bss {
    Bss {
        bssid: AP,
        freq: 5180,
        width: Width::W80,
        center1: 5210,
        center2: 0,
        aid: 1,
        beacon_int: 100,
        dtim_period: 3,
        flags: bss_flags::QOS | bss_flags::HT | bss_flags::VHT | bss_flags::HE | bss_flags::PMF,
        basic_rates: 0b0001_0101_0000,
        rates: 0x0ff0,
        ht_mcs: [0xff, 0xff, 0, 0, 0, 0, 0, 0, 0, 0],
        vht_rx_mcs: 0xfffa,
        vht_tx_mcs: 0xfffa,
        he_mcs: [0xfffa, 0xfffa, 0xffff, 0xffff, 0xffff, 0xffff],
        ampdu_factor: 7,
        ampdu_density: 0,
    }
}

fn dir_of(m: &Msg<'_>) -> Direction {
    [
        Direction::MlmeToKernel,
        Direction::KeysToKernel,
        Direction::KernelToMlme,
        Direction::KernelToKeys,
    ]
    .into_iter()
    .find(|&d| permitted(m.kind(), d))
    .unwrap()
}

/// Every message kind, valid.
fn samples<'a>(bufs: &'a mut [[u8; 512]; 4], auth: &'a [u8], beacon: &'a [u8]) -> Vec<Msg<'a>> {
    let [b0, b1, b2, b3] = bufs;
    let ssids = List::build(&[Ssid::new(b"hidden-net").unwrap()], b0).unwrap();
    let freqs = List::build(&[Freq(2412), Freq(5180), Freq(5955)], b1).unwrap();
    let rules = List::build(
        &[RegRule {
            start_khz: 2_402_000,
            end_khz: 2_482_000,
            max_bw_khz: 40_000,
            max_eirp_mbm: 2000,
            flags: 0,
        }],
        b2,
    )
    .unwrap();
    let chans = List::build(
        &[ChanInfo { freq: 2412, flags: 0, max_power_mbm: 2000 }, ChanInfo {
            freq: 5260,
            flags: chan_flags::DFS | chan_flags::NO_IR,
            max_power_mbm: 2300,
        }],
        b3,
    )
    .unwrap();
    std::vec![
        Msg::GetCaps,
        Msg::Scan(ScanReq {
            flags: scan_flags::RANDOM_MAC,
            mac: [0x06, 9, 9, 9, 9, 9],
            dwell_ms: 30,
            ssids,
            freqs
        }),
        Msg::SetBss(bss()),
        Msg::ClearBss,
        Msg::TxMgmt { freq: 5180, frame: auth },
        Msg::SetPowerSave(PowerSave::On),
        Msg::SetRegulatory { alpha2: *b"IN", rules },
        Msg::SetMac([0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0xee]),
        Msg::Disconnect { reason: 3 },
        Msg::SetKey(key()),
        Msg::DelKey { kind: KeyKind::Group, idx: 1, addr: [0xff; 6] },
        Msg::PortAuthorized(true),
        Msg::TxEapol { dst: AP, body: &[2, 3, 0, 0x5f, 2, 1, 0x0a] },
        Msg::Ack { status: -22 },
        Msg::Caps(Caps {
            mac: [0x02, 0, 0, 0, 0, 1],
            bands: bands::ALL,
            ciphers: Cipher::Ccmp128.bit() | Cipher::Gcmp256.bit(),
            flags: caps_flags::PMF | caps_flags::HW_SCAN,
            max_scan_ssids: 4,
            channels: chans
        }),
        Msg::MgmtRx { freq: 2412, rssi: -61, flags: rx_flags::SCAN, tsf_us: 123_456, frame: beacon },
        Msg::EapolRx { src: AP, body: &[2, 3, 0, 0x5f, 2] },
        Msg::ScanDone { status: 0 },
        Msg::Disassociated { reason: 7, local: false },
        Msg::BeaconLoss,
        Msg::RfKill { blocked: true },
        Msg::FirmwareError { code: 0xdead },
        Msg::Stats(Stats {
            tx_packets: 1,
            rx_packets: 2,
            tx_failed: 3,
            rx_dropped: 4,
            beacon_rssi: -50,
            tx_rate_kbps: 1_200_000
        }),
    ]
}

#[test]
fn every_kind_round_trips() {
    let (auth, beacon) = (auth_frame(), beacon_frame());
    let mut bufs = [[0u8; 512]; 4];
    let msgs = samples(&mut bufs, &auth, &beacon);
    assert_eq!(msgs.len(), 23);
    for (i, m) in msgs.iter().enumerate() {
        let mut out = [0u8; MAX_BODY + HEADER_LEN];
        let n = encode(m, i as u32 + 7, &mut out).unwrap_or_else(|e| panic!("{m:?}: {e:?}"));
        let (h, back) = decode(&out[..n], dir_of(m)).unwrap();
        assert_eq!(h.seq, i as u32 + 7);
        assert_eq!(&back, m);
    }
}

#[test]
fn directions_are_enforced() {
    let (auth, beacon) = (auth_frame(), beacon_frame());
    let mut bufs = [[0u8; 512]; 4];
    for m in samples(&mut bufs, &auth, &beacon) {
        let mut out = [0u8; 4096];
        let n = encode(&m, 1, &mut out).unwrap();
        for d in [
            Direction::MlmeToKernel,
            Direction::KeysToKernel,
            Direction::KernelToMlme,
            Direction::KernelToKeys,
        ] {
            let r = decode(&out[..n], d);
            if permitted(m.kind(), d) {
                assert!(r.is_ok());
            } else {
                assert_eq!(r.unwrap_err(), Error::NotPermitted, "{m:?} {d:?}");
            }
        }
    }
    // The split of wifi.md §8.3.
    assert!(!permitted(kind::SET_KEY, Direction::MlmeToKernel));
    assert!(!permitted(kind::TX_EAPOL, Direction::MlmeToKernel));
    assert!(!permitted(kind::EAPOL_RX, Direction::KernelToMlme));
    assert!(!permitted(kind::TX_MGMT, Direction::KeysToKernel));
    assert!(!permitted(kind::MGMT_RX, Direction::KernelToKeys));
}

#[test]
fn every_truncation_fails() {
    let (auth, beacon) = (auth_frame(), beacon_frame());
    let mut bufs = [[0u8; 512]; 4];
    for m in samples(&mut bufs, &auth, &beacon) {
        let mut out = [0u8; 4096];
        let n = encode(&m, 1, &mut out).unwrap();
        for cut in 0..n {
            assert!(decode(&out[..cut], dir_of(&m)).is_err(), "{m:?} cut at {cut}");
        }
        // Extra byte with an unchanged header is refused too.
        assert_eq!(decode(&out[..n + 1], dir_of(&m)).unwrap_err(), Error::BadLength);
    }
}

fn enc(m: &Msg<'_>) -> Result<usize, Error> {
    encode(m, 0, &mut [0u8; 4096])
}

#[test]
fn bad_values_are_refused() {
    let mut beacon = beacon_frame();
    // A station never sends beacons.
    assert_eq!(enc(&Msg::TxMgmt { freq: 2412, frame: &beacon }), Err(Error::BadValue));
    // A data frame is not a management frame.
    beacon[0] = 0x08;
    assert_eq!(
        enc(&Msg::MgmtRx { freq: 2412, rssi: 0, flags: 0, tsf_us: 0, frame: &beacon }),
        Err(Error::BadValue)
    );
    // Short management frame.
    assert_eq!(
        enc(&Msg::MgmtRx { freq: 2412, rssi: 0, flags: 0, tsf_us: 0, frame: &[0x80; 23] }),
        Err(Error::BadValue)
    );
    // Channel outside every band.
    let auth = auth_frame();
    assert_eq!(enc(&Msg::TxMgmt { freq: 3000, frame: &auth }), Err(Error::BadValue));
    // Key index outside the pairwise range; integrity cipher on a pairwise key;
    // bytes past the key length.
    let mut k = key();
    k.idx = 2;
    assert_eq!(enc(&Msg::SetKey(k)), Err(Error::BadValue));
    let mut k = key();
    k.cipher = Cipher::BipCmac128;
    assert_eq!(enc(&Msg::SetKey(k)), Err(Error::BadValue));
    let mut k = key();
    k.key[20] = 1;
    assert_eq!(enc(&Msg::SetKey(k)), Err(Error::BadValue));
    // Multicast BSSID, AID 0, basic rate not in the rate set.
    let mut b = bss();
    b.bssid[0] |= 1;
    assert_eq!(enc(&Msg::SetBss(b)), Err(Error::BadValue));
    let mut b = bss();
    b.aid = 0;
    assert_eq!(enc(&Msg::SetBss(b)), Err(Error::BadValue));
    let mut b = bss();
    b.rates = 0x000f;
    assert_eq!(enc(&Msg::SetBss(b)), Err(Error::BadValue));
    // Passive scan with directed SSIDs.
    let mut buf = [0u8; 64];
    let ssids = List::build(&[Ssid::new(b"x").unwrap()], &mut buf).unwrap();
    let mut fb = [0u8; 0];
    let freqs = List::build(&[], &mut fb).unwrap();
    assert_eq!(
        enc(&Msg::Scan(ScanReq { flags: scan_flags::PASSIVE, mac: [0; 6], dwell_ms: 0, ssids, freqs })),
        Err(Error::BadValue)
    );
    // Lowercase country.
    let mut rb = [0u8; 0];
    let rules = List::build(&[], &mut rb).unwrap();
    assert_eq!(enc(&Msg::SetRegulatory { alpha2: *b"in", rules }), Err(Error::BadValue));
    // Multicast own address.
    assert_eq!(enc(&Msg::SetMac([1, 0, 0, 0, 0, 0])), Err(Error::BadValue));
    // EAPOL too short.
    assert_eq!(enc(&Msg::EapolRx { src: AP, body: &[2, 3] }), Err(Error::BadValue));
}

#[test]
fn header_checks() {
    let mut out = [0u8; 64];
    let n = encode(&Msg::GetCaps, 1, &mut out).unwrap();
    assert_eq!(n, HEADER_LEN);
    let mut bad = out;
    bad[0] ^= 1;
    assert_eq!(decode(&bad[..n], Direction::MlmeToKernel).unwrap_err(), Error::BadMagic);
    let mut bad = out;
    bad[4] = 2;
    assert_eq!(decode(&bad[..n], Direction::MlmeToKernel).unwrap_err(), Error::BadVersion(2));
    let mut bad = out;
    bad[6] = 0x77;
    assert_eq!(decode(&bad[..n], Direction::MlmeToKernel).unwrap_err(), Error::UnknownKind(0x77));
    let mut bad = out;
    bad[12..16].copy_from_slice(&(MAX_BODY as u32 + 1).to_le_bytes());
    assert_eq!(decode(&bad[..n], Direction::MlmeToKernel).unwrap_err(), Error::BadLength);
}

#[test]
fn key_debug_hides_material() {
    let s = std::format!("{:?}", key());
    assert!(!s.contains("170") && !s.contains("aa"), "{s}");
}

/// Mutation fuzzing without dependencies: no panic on any input, and any
/// input that decodes re-encodes to the same bytes (one encoding per message).
#[test]
fn mutations_never_panic_and_encoding_is_canonical() {
    let (auth, beacon) = (auth_frame(), beacon_frame());
    let mut bufs = [[0u8; 512]; 4];
    let msgs = samples(&mut bufs, &auth, &beacon);
    let seeds: Vec<Vec<u8>> = msgs
        .iter()
        .map(|m| {
            let mut out = [0u8; 4096];
            let n = encode(m, 5, &mut out).unwrap();
            out[..n].to_vec()
        })
        .collect();
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut rnd = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let dirs = [
        Direction::MlmeToKernel,
        Direction::KeysToKernel,
        Direction::KernelToMlme,
        Direction::KernelToKeys,
    ];
    let mut decoded = 0;
    for _ in 0..300_000 {
        let mut b = seeds[(rnd() as usize) % seeds.len()].clone();
        for _ in 0..1 + rnd() % 4 {
            let i = (rnd() as usize) % b.len();
            match rnd() % 4 {
                0 => b[i] ^= 1 << (rnd() % 8),
                1 => b[i] = rnd() as u8,
                2 => b.truncate(i.max(1)),
                _ => b.push(rnd() as u8),
            }
        }
        // Keep the header length consistent half the time, to reach bodies.
        if rnd() % 2 == 0 && b.len() >= HEADER_LEN {
            let l = (b.len() - HEADER_LEN) as u32;
            b[12..16].copy_from_slice(&l.to_le_bytes());
        }
        for d in dirs {
            if let Ok((h, m)) = decode(&b, d) {
                decoded += 1;
                let mut out = [0u8; MAX_BODY + HEADER_LEN];
                let n = encode(&m, h.seq, &mut out).unwrap();
                assert_eq!(&out[..n], &b[..], "{m:?}");
            }
        }
    }
    assert!(decoded > 1000, "mutations reached too few valid bodies: {decoded}");
}
