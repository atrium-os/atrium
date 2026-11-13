//! aura-air: the FreeBSD half of Aura's simulated air (docs/spec/wifi.md
//! §6.2). Test only.
//!
//! aura_sim.ko hands every request the kernel core accepted and every data
//! frame to transmit to `/dev/aura/sim0`; this program does what radio
//! firmware does with them and exchanges the resulting 802.11 frames with
//! `air-hwsim.py` (beside hostapd on mac80211_hwsim, in a Linux VM) over UDP:
//!
//! - tunes the "radio" (frames are delivered only on the current channel);
//! - scans: hops the channels, sends probe requests where active scanning
//!   is allowed, passes beacons and probe responses up flagged SCAN, then
//!   reports ScanDone;
//! - does CCMP-128 in both directions with the keys `aura-keys` installed,
//!   for data and for unicast robust management frames, and checks BIP on
//!   group-addressed robust management frames, setting the same rx flags a
//!   chip would. Data-frame replay checks stay in the kernel core (the PN is
//!   passed up); management-frame replay is checked here, as firmware does.
//!
//! usage: aura-air [-v] [PORT]     (default 7701; the peer is whoever sends)

mod crypt;

use aura_proto::{decode, encode, scan_flags, Direction, KeyKind, Mac, Msg, HEADER_LEN, MAX_BODY};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::net::{SocketAddr, UdpSocket};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::time::{Duration, Instant};

const SIM_REQ: u32 = 1;
const SIM_TX: u32 = 2;
const SIM_RX_MGMT: u32 = 3;
const SIM_RX_DATA: u32 = 4;
const SIM_EVENT: u32 = 5;
const SIM_MAX: usize = 8 + 16 + 11454;

const RX_DECRYPTED: u8 = 1;
const RX_MIC_OK: u8 = 2;
const RX_SCAN: u8 = 4;

const MAGIC: &[u8; 4] = b"AAIR";
/// aura_sim.c's channel list.
const CHANNELS: [u16; 15] = [2412, 2417, 2422, 2427, 2432, 2437, 2442, 2447, 2452, 2457, 2462, 5180, 5200, 5220, 5240];
const OWN_MAC: Mac = [0x02, 0xa5, 0, 0, 0, 0x01];

struct Ptk {
    tk: [u8; 16],
    tx_pn: u64,
    rx_mgmt_pn: Option<u64>,
}

#[derive(Default)]
struct Keys {
    ptk: Option<Ptk>,
    gtk: [Option<[u8; 16]>; 4],
    igtk: [Option<([u8; 16], Option<u64>)>; 2],
}

struct Scan {
    chans: Vec<u16>,
    i: usize,
    until: Instant,
    mac: Mac,
    ssids: Vec<Vec<u8>>,
    passive: bool,
    dwell: Duration,
}

#[derive(Default, Debug)]
struct Stats {
    air_rx: u64,
    air_tx: u64,
    off_channel: u64,
    not_for_us: u64,
    decrypt_fail: u64,
    mgmt_replay: u64,
    no_key: u64,
    to_kernel: u64,
}

struct Air {
    sim: File,
    udp: UdpSocket,
    peer: Option<SocketAddr>,
    own: Mac,
    freq: u16,
    bss: Option<(Mac, u16)>,
    keys: Keys,
    scan: Option<Scan>,
    seq: u16,
    start: Instant,
    verbose: bool,
    st: Stats,
}

fn group(a: &[u8]) -> bool {
    a[0] & 1 != 0
}

/// Channels where the world regulatory rules allow initiating radiation
/// (aura-wlan-core reg.rs): everything else is scanned passively.
fn may_probe(f: u16) -> bool {
    (2412..=2452).contains(&f)
}

fn rates_ies(freq: u16) -> Vec<u8> {
    if freq < 3000 {
        vec![1, 8, 0x82, 0x84, 0x8b, 0x96, 0x0c, 0x12, 0x18, 0x24, 50, 4, 0x30, 0x48, 0x60, 0x6c]
    } else {
        vec![1, 8, 0x8c, 0x12, 0x98, 0x24, 0xb0, 0x48, 0x60, 0x6c]
    }
}

impl Air {
    fn log(&self, s: &str) {
        if self.verbose {
            eprintln!("air {:>8.3} {s}", self.start.elapsed().as_secs_f64());
        }
    }

    fn to_sim(&mut self, typ: u32, a: &[u8], b: &[u8]) {
        let mut m = Vec::with_capacity(8 + a.len() + b.len());
        m.extend_from_slice(&typ.to_le_bytes());
        m.extend_from_slice(&((a.len() + b.len()) as u32).to_le_bytes());
        m.extend_from_slice(a);
        m.extend_from_slice(b);
        if let Err(e) = self.sim.write(&m) {
            eprintln!("aura-air: sim write: {e}");
        } else {
            self.st.to_kernel += 1;
        }
    }

    fn event(&mut self, m: &Msg<'_>) {
        let mut buf = vec![0u8; HEADER_LEN + MAX_BODY];
        if let Ok(n) = encode(m, 0, &mut buf) {
            self.to_sim(SIM_EVENT, &[], &buf[..n].to_vec());
        }
    }

    fn air_tx(&mut self, freq: u16, frame: &[u8]) {
        let Some(peer) = self.peer else { return };
        let mut d = Vec::with_capacity(8 + frame.len());
        d.extend_from_slice(MAGIC);
        d.extend_from_slice(&freq.to_le_bytes());
        d.push((-40i8) as u8);
        d.push(0);
        d.extend_from_slice(frame);
        if self.udp.send_to(&d, peer).is_ok() {
            self.st.air_tx += 1;
        }
    }

    fn next_seq(&mut self) -> [u8; 2] {
        self.seq = self.seq.wrapping_add(1) & 0x0fff;
        (self.seq << 4).to_le_bytes()
    }

    fn probe(&mut self) {
        let Some(s) = &self.scan else { return };
        let f = s.chans[s.i];
        if s.passive || !may_probe(f) {
            return;
        }
        let mac = s.mac;
        let ssids = if s.ssids.is_empty() { vec![Vec::new()] } else { s.ssids.clone() };
        for ssid in ssids {
            let mut p = vec![0x40, 0, 0, 0];
            p.extend_from_slice(&[0xff; 6]);
            p.extend_from_slice(&mac);
            p.extend_from_slice(&[0xff; 6]);
            let sq = self.next_seq();
            p.extend_from_slice(&sq);
            p.push(0);
            p.push(ssid.len() as u8);
            p.extend_from_slice(&ssid);
            p.extend_from_slice(&rates_ies(f));
            self.air_tx(f, &p);
        }
    }

    fn scan_step(&mut self) {
        let done = {
            let Some(s) = &mut self.scan else { return };
            if Instant::now() < s.until {
                return;
            }
            s.i += 1;
            if s.i < s.chans.len() {
                s.until = Instant::now() + s.dwell;
                self.freq = s.chans[s.i];
                false
            } else {
                true
            }
        };
        if done {
            self.scan = None;
            if let Some((_, f)) = self.bss {
                self.freq = f;
            }
            self.log("scan done");
            self.event(&Msg::ScanDone { status: 0 });
        } else {
            self.probe();
        }
    }

    fn request(&mut self, body: &[u8]) {
        let m = match decode(body, Direction::MlmeToKernel).or_else(|_| decode(body, Direction::KeysToKernel)) {
            Ok((_, m)) => m,
            Err(e) => {
                eprintln!("aura-air: undecodable request: {e:?}");
                return;
            }
        };
        match &m {
            Msg::Scan(s) => {
                let mut chans: Vec<u16> = s.freqs.iter().map(|f| f.0).collect();
                if chans.is_empty() {
                    chans = CHANNELS.to_vec();
                }
                chans.retain(|f| CHANNELS.contains(f));
                let passive = s.flags & scan_flags::PASSIVE != 0;
                let dwell = Duration::from_millis(if s.dwell_ms != 0 { s.dwell_ms as u64 } else { 150 });
                let mac = if s.flags & scan_flags::RANDOM_MAC != 0 { s.mac } else { self.own };
                let ssids = s.ssids.iter().map(|x| x.as_bytes().to_vec()).collect();
                self.log(&format!("scan {} channels from {mac:02x?}", chans.len()));
                if chans.is_empty() {
                    self.event(&Msg::ScanDone { status: 0 });
                    return;
                }
                self.freq = chans[0];
                self.scan = Some(Scan { chans, i: 0, until: Instant::now() + dwell, mac, ssids, passive, dwell });
                self.probe();
            }
            Msg::SetBss(b) => {
                self.log(&format!("set bss {:02x?} on {}", b.bssid, b.freq));
                self.bss = Some((b.bssid, b.freq));
                self.freq = b.freq;
            }
            Msg::ClearBss => {
                self.log("clear bss");
                self.bss = None;
                self.keys = Keys::default();
            }
            Msg::TxMgmt { freq, frame } => {
                if self.scan.is_none() {
                    self.freq = *freq;
                }
                let mut f = frame.to_vec();
                if f[1] & 0x40 != 0 {
                    let Some(p) = &mut self.keys.ptk else {
                        self.st.no_key += 1;
                        return;
                    };
                    p.tx_pn += 1;
                    let mut plain = f.clone();
                    plain[1] &= !0x40;
                    match crypt::ccmp_encrypt(&p.tk, &plain, p.tx_pn, 0) {
                        Some(e) => f = e,
                        None => return,
                    }
                }
                self.log(&format!("tx mgmt subtype {} on {freq}", frame[0] >> 4));
                self.air_tx(*freq, &f);
            }
            Msg::SetKey(k) => {
                let mut key = [0u8; 16];
                if k.material().len() != 16 {
                    eprintln!("aura-air: only 128-bit ciphers are simulated");
                    return;
                }
                key.copy_from_slice(k.material());
                self.log(&format!("set key {:?} idx {}", k.kind, k.idx));
                match k.kind {
                    KeyKind::Pairwise => self.keys.ptk = Some(Ptk { tk: key, tx_pn: 0, rx_mgmt_pn: None }),
                    KeyKind::Group => self.keys.gtk[k.idx as usize] = Some(key),
                    KeyKind::Igtk => {
                        let mut r = [0u8; 8];
                        r[..6].copy_from_slice(&k.rsc);
                        let ipn = u64::from_le_bytes(r);
                        self.keys.igtk[(k.idx - 4) as usize] = Some((key, (ipn > 0).then(|| ipn - 1)));
                    }
                    KeyKind::Bigtk => {}
                }
            }
            Msg::DelKey { kind, idx, .. } => match kind {
                KeyKind::Pairwise => self.keys.ptk = None,
                KeyKind::Group => self.keys.gtk[*idx as usize] = None,
                KeyKind::Igtk => self.keys.igtk[(*idx - 4) as usize] = None,
                KeyKind::Bigtk => {}
            },
            Msg::SetMac(m) => self.own = *m,
            _ => {}
        }
    }

    fn data_tx(&mut self, body: &[u8]) {
        if body.len() < 8 + 24 {
            return;
        }
        let encrypt = body[1] != 0;
        let f = &body[8..];
        let out = if encrypt {
            let Some(p) = &mut self.keys.ptk else {
                self.st.no_key += 1;
                return;
            };
            p.tx_pn += 1;
            let mut plain = f.to_vec();
            plain[1] &= !0x40;
            match crypt::ccmp_encrypt(&p.tk, &plain, p.tx_pn, 0) {
                Some(e) => e,
                None => return,
            }
        } else {
            f.to_vec()
        };
        let freq = self.freq;
        self.air_tx(freq, &out);
    }

    fn air_rx(&mut self, d: &[u8]) {
        if d.len() < 8 + 24 || &d[..4] != MAGIC {
            return;
        }
        self.st.air_rx += 1;
        let freq = u16::from_le_bytes([d[4], d[5]]);
        let rssi = d[6] as i8;
        let f = &d[8..];
        if freq != self.freq {
            self.st.off_channel += 1;
            return;
        }
        let (a1, a2) = (&f[4..10], &f[10..16]);
        let from_bss = self.bss.is_some_and(|(b, _)| b[..] == *a2);
        match f[0] & 0x0c {
            0x00 => {
                let sub = f[0] >> 4;
                let mut flags = 0u8;
                let mut frame = f.to_vec();
                if let Some(s) = &self.scan {
                    if sub == 8 || (sub == 5 && *a1 == s.mac) {
                        flags |= RX_SCAN;
                    }
                }
                let for_us = *a1 == self.own || (group(a1) && from_bss);
                if flags & RX_SCAN == 0 && !for_us {
                    self.st.not_for_us += 1;
                    return;
                }
                if f[1] & 0x40 != 0 {
                    if group(a1) {
                        self.st.decrypt_fail += 1;
                        return;
                    }
                    let Some(p) = &mut self.keys.ptk else {
                        self.st.no_key += 1;
                        return;
                    };
                    let Some((plain, _, pn)) = crypt::ccmp_decrypt(&p.tk, f) else {
                        self.st.decrypt_fail += 1;
                        return;
                    };
                    if p.rx_mgmt_pn.is_some_and(|last| pn <= last) {
                        self.st.mgmt_replay += 1;
                        return;
                    }
                    p.rx_mgmt_pn = Some(pn);
                    frame = plain;
                    flags |= RX_DECRYPTED | RX_MIC_OK;
                } else if group(a1) && matches!(sub, 10 | 12 | 13) && f.len() >= 42 && f[f.len() - 18] == 76 {
                    let kid = u16::from_le_bytes([f[f.len() - 16], f[f.len() - 15]]);
                    if (4..=5).contains(&kid) {
                        if let Some((k, last)) = &mut self.keys.igtk[(kid - 4) as usize] {
                            if let Some((_, ipn)) = crypt::bip_check(k, f) {
                                if last.is_some_and(|l| ipn <= l) {
                                    self.st.mgmt_replay += 1;
                                    return;
                                }
                                *last = Some(ipn);
                                flags |= RX_MIC_OK;
                            }
                        }
                    }
                }
                if sub != 8 {
                    self.log(&format!("rx mgmt subtype {sub} flags {flags:#x}"));
                }
                let tsf = self.start.elapsed().as_micros() as u64;
                let mut h = Vec::with_capacity(16);
                h.extend_from_slice(&freq.to_le_bytes());
                h.push(rssi as u8);
                h.push(flags);
                h.extend_from_slice(&0u32.to_le_bytes());
                h.extend_from_slice(&tsf.to_le_bytes());
                self.to_sim(SIM_RX_MGMT, &h, &frame);
            }
            0x08 => {
                if f[0] & 0x40 != 0 {
                    return; // no-data subtypes (null, QoS null)
                }
                if *a1 != self.own && !group(a1) {
                    self.st.not_for_us += 1;
                    return;
                }
                let mut h = [0u8; 16];
                let frame = if f[1] & 0x40 != 0 {
                    let Some((kid, _)) = crypt::ccmp_hdr(f) else {
                        self.st.decrypt_fail += 1;
                        return;
                    };
                    let key = if group(a1) {
                        self.keys.gtk[kid as usize]
                    } else {
                        self.keys.ptk.as_ref().map(|p| p.tk)
                    };
                    let Some(key) = key else {
                        self.st.no_key += 1;
                        return;
                    };
                    let Some((plain, kid, pn)) = crypt::ccmp_decrypt(&key, f) else {
                        self.st.decrypt_fail += 1;
                        return;
                    };
                    h[0] = 1;
                    h[1] = kid;
                    h[8..16].copy_from_slice(&pn.to_le_bytes());
                    plain
                } else {
                    f.to_vec()
                };
                self.to_sim(SIM_RX_DATA, &h, &frame);
            }
            _ => {}
        }
    }

    fn drain_sim(&mut self) {
        let mut buf = vec![0u8; SIM_MAX];
        loop {
            match self.sim.read(&mut buf) {
                Ok(n) if n >= 8 => {
                    let typ = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                    let body = buf[8..n].to_vec();
                    match typ {
                        SIM_REQ => self.request(&body),
                        SIM_TX => self.data_tx(&body),
                        _ => {}
                    }
                }
                Ok(_) => return,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return,
                Err(e) => {
                    eprintln!("aura-air: sim read: {e}");
                    std::process::exit(1);
                }
            }
        }
    }

    fn drain_udp(&mut self) {
        let mut buf = vec![0u8; 16384];
        loop {
            match self.udp.recv_from(&mut buf) {
                Ok((n, from)) => {
                    if self.peer != Some(from) {
                        eprintln!("aura-air: peer {from}");
                        self.peer = Some(from);
                    }
                    let d = buf[..n].to_vec();
                    self.air_rx(&d);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return,
                Err(e) => {
                    eprintln!("aura-air: udp: {e}");
                    return;
                }
            }
        }
    }
}

fn main() {
    let mut verbose = false;
    let mut port = 7701u16;
    for a in std::env::args().skip(1) {
        if a == "-v" {
            verbose = true;
        } else if let Ok(p) = a.parse() {
            port = p;
        } else {
            eprintln!("usage: aura-air [-v] [PORT]");
            std::process::exit(2);
        }
    }
    let sim = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open("/dev/aura/sim0")
        .unwrap_or_else(|e| {
            eprintln!("aura-air: /dev/aura/sim0: {e}");
            std::process::exit(1)
        });
    let udp = UdpSocket::bind(("0.0.0.0", port)).unwrap_or_else(|e| {
        eprintln!("aura-air: bind {port}: {e}");
        std::process::exit(1)
    });
    udp.set_nonblocking(true).ok();
    let mut air = Air {
        sim,
        udp,
        peer: None,
        own: OWN_MAC,
        freq: 2412,
        bss: None,
        keys: Keys::default(),
        scan: None,
        seq: 0x123,
        start: Instant::now(),
        verbose,
        st: Stats::default(),
    };
    eprintln!("aura-air: up, udp {port}");
    let mut last = Instant::now();
    loop {
        let mut fds = [
            libc::pollfd { fd: air.sim.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: air.udp.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        ];
        let timeout = match &air.scan {
            Some(s) => s.until.saturating_duration_since(Instant::now()).as_millis().min(1000) as i32,
            None => 1000,
        };
        // SAFETY: two valid pollfds owned by this frame.
        unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout.max(1)) };
        air.drain_sim();
        air.drain_udp();
        air.scan_step();
        if verbose && last.elapsed() > Duration::from_secs(10) {
            last = Instant::now();
            eprintln!("aura-air: {:?}", air.st);
        }
    }
}
