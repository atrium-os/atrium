//! W0 gate for Aura's kernel core (docs/spec/wifi.md §7): aura_wlan.ko with
//! the simulated radio aura_sim.ko, driven through the three nodes.
//!
//! Run as root in the VM by scripts/aura-vm-check.sh, which loads the
//! modules, sets wlan0's address and a static ARP entry for the "far" host,
//! and reads the drop counters afterwards. Prints one line per check and
//! exits non-zero on the first failure.

use aura_proto::{decode, encode, Bss, Direction, Mac, Msg, Width, HEADER_LEN, MAX_BODY};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::process::{exit, Command};
use std::time::{Duration, Instant};

const OWN: Mac = [0x02, 0xa5, 0, 0, 0, 0x01];
const AP: Mac = [0x02, 0xaa, 0, 0, 0, 0x02];
const FAR: Mac = [0x02, 0xbb, 0, 0, 0, 0x03];
const FAR_IP: [u8; 4] = [10, 77, 0, 1];
const OWN_IP: [u8; 4] = [10, 77, 0, 2];

// aura_sim.h
const SIM_REQ: u32 = 1;
const SIM_TX: u32 = 2;
const SIM_RX_MGMT: u32 = 3;
const SIM_RX_DATA: u32 = 4;
const SIM_EVENT: u32 = 5;

static mut PASSED: u32 = 0;

fn ok(what: &str) {
    println!("ok   {what}");
    unsafe { PASSED += 1 };
}

fn fail(what: &str) -> ! {
    println!("FAIL {what}");
    exit(1);
}

fn check(c: bool, what: &str) {
    if c {
        ok(what)
    } else {
        fail(what)
    }
}

fn open(path: &str) -> std::io::Result<File> {
    OpenOptions::new().read(true).write(true).custom_flags(libc::O_NONBLOCK).open(path)
}

/// One message from a node, waiting up to `ms`.
fn read_msg(f: &mut File, ms: u64) -> Option<Vec<u8>> {
    let end = Instant::now() + Duration::from_millis(ms);
    let mut buf = vec![0u8; HEADER_LEN + MAX_BODY + 16 + 11454];
    loop {
        match f.read(&mut buf) {
            Ok(n) => return Some(buf[..n].to_vec()),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() > end {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => fail(&format!("read: {e}")),
        }
    }
}

fn send(f: &mut File, m: &Msg<'_>, seq: u32) -> std::io::Result<usize> {
    let mut buf = vec![0u8; HEADER_LEN + MAX_BODY];
    let n = encode(m, seq, &mut buf).unwrap_or_else(|e| fail(&format!("encode {m:?}: {e:?}")));
    f.write(&buf[..n])
}

/// Sends a request and returns the Ack status.
fn request(f: &mut File, m: &Msg<'_>, seq: u32, dir: Direction) -> i32 {
    send(f, m, seq).unwrap_or_else(|e| fail(&format!("write {m:?}: {e}")));
    let r = read_msg(f, 2000).unwrap_or_else(|| fail(&format!("no reply to {m:?}")));
    match decode(&r, dir) {
        Ok((h, Msg::Ack { status })) if h.seq == seq => status,
        other => fail(&format!("bad reply to {m:?}: {other:?}")),
    }
}

fn sim_write(f: &mut File, ty: u32, meta: &[u8], body: &[u8]) {
    let mut b = Vec::new();
    b.extend_from_slice(&ty.to_le_bytes());
    b.extend_from_slice(&((meta.len() + body.len()) as u32).to_le_bytes());
    b.extend_from_slice(meta);
    b.extend_from_slice(body);
    f.write_all(&b).unwrap_or_else(|e| fail(&format!("sim write: {e}")));
}

/// Next sim message of type `ty`, skipping others; payload.
fn sim_expect(f: &mut File, ty: u32, ms: u64) -> Option<Vec<u8>> {
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        let m = read_msg(f, 100)?;
        let t = u32::from_le_bytes(m[0..4].try_into().unwrap());
        if t == ty {
            return Some(m[8..].to_vec());
        }
    }
    None
}

fn rx_data(sim: &mut File, frame: &[u8]) {
    let meta = [0u8; 16]; // not decrypted, key 0, pn 0
    sim_write(sim, SIM_RX_DATA, &meta, frame);
}

/// A QoS data frame from the AP to `da`, source `sa`, LLC/SNAP + payload.
fn from_ap(da: Mac, sa: Mac, ethertype: u16, payload: &[u8]) -> Vec<u8> {
    let mut f = vec![0x88, 0x02, 0, 0];
    f.extend_from_slice(&da);
    f.extend_from_slice(&AP);
    f.extend_from_slice(&sa);
    f.extend_from_slice(&[0, 0, 0, 0]);
    f.extend_from_slice(&[0xaa, 0xaa, 0x03, 0, 0, 0]);
    f.extend_from_slice(&ethertype.to_be_bytes());
    f.extend_from_slice(payload);
    f
}

fn csum(b: &[u8]) -> u16 {
    let mut s: u32 = 0;
    for c in b.chunks(2) {
        s += u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]) as u32;
    }
    while s >> 16 != 0 {
        s = (s & 0xffff) + (s >> 16);
    }
    !(s as u16)
}

fn icmp_echo(id: u16) -> Vec<u8> {
    let mut icmp = vec![8, 0, 0, 0];
    icmp.extend_from_slice(&id.to_be_bytes());
    icmp.extend_from_slice(&1u16.to_be_bytes());
    icmp.extend_from_slice(b"aura-w0-ping-payload");
    let c = csum(&icmp);
    icmp[2..4].copy_from_slice(&c.to_be_bytes());
    let total = 20 + icmp.len();
    let mut ip = vec![0x45, 0, 0, 0, 0, 1, 0, 0, 64, 1, 0, 0];
    ip[2..4].copy_from_slice(&(total as u16).to_be_bytes());
    ip.extend_from_slice(&FAR_IP);
    ip.extend_from_slice(&OWN_IP);
    let c = csum(&ip);
    ip[10..12].copy_from_slice(&c.to_be_bytes());
    ip.extend_from_slice(&icmp);
    ip
}

fn sysctl_drops() -> Vec<u64> {
    let out = Command::new("sysctl").args(["-x", "-n", "net.aura.wlan0.drops"]).output();
    // -x prints the opaque bytes as hex; parse 8-byte little-endian words.
    let s = out.map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
    let dump = s.split("0x").nth(1).unwrap_or("");
    let hex: String = dump.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
    let bytes: Vec<u8> = (0..hex.len() / 2).map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap_or(0)).collect();
    bytes.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect()
}

fn ifconfig_status() -> String {
    let out = Command::new("ifconfig").arg("wlan0").output().map(|o| o.stdout).unwrap_or_default();
    String::from_utf8_lossy(&out).to_string()
}

fn main() {
    let mut sim = open("/dev/aura/sim0").unwrap_or_else(|e| fail(&format!("open sim0: {e}")));
    let mut ctl = open("/dev/aura/ctl0").unwrap_or_else(|e| fail(&format!("open ctl0: {e}")));
    let mut key = open("/dev/aura/key0").unwrap_or_else(|e| fail(&format!("open key0: {e}")));
    let busy = open("/dev/aura/ctl0").err().and_then(|e| e.raw_os_error());
    check(busy == Some(libc::EBUSY), "second open of ctl0 is EBUSY");

    // Caps.
    send(&mut ctl, &Msg::GetCaps, 1).unwrap();
    let r = read_msg(&mut ctl, 2000).unwrap_or_else(|| fail("no Caps"));
    match decode(&r, Direction::KernelToMlme) {
        Ok((h, Msg::Caps(c))) => check(h.seq == 1 && c.mac == OWN && c.channels.len() == 15, "GetCaps answers the sim's caps"),
        other => fail(&format!("GetCaps: {other:?}")),
    }

    // A key-channel message on ctl0 is refused at decode: write fails.
    let mut k = [0u8; 32];
    k[..16].fill(1);
    let ptk = aura_proto::Key { kind: aura_proto::KeyKind::Pairwise, idx: 0, cipher: aura_proto::Cipher::Ccmp128, addr: AP, rsc: [0; 6], key: k };
    let e = send(&mut ctl, &Msg::SetKey(ptk.clone()), 2).err().and_then(|e| e.raw_os_error());
    check(e == Some(libc::EINVAL), "SetKey on ctl0 is refused (keys only on key0)");
    check(read_msg(&mut ctl, 100).is_none(), "and nothing reaches the radio or the reply queue");

    // Join.
    let bss = Bss {
        bssid: AP, freq: 2412, width: Width::W20, center1: 0, center2: 0, aid: 1, beacon_int: 100,
        dtim_period: 1, flags: aura_proto::bss_flags::QOS, basic_rates: 0, rates: 0x0ff0, ht_mcs: [0; 10],
        vht_rx_mcs: 0, vht_tx_mcs: 0, he_mcs: [0; 6], ampdu_factor: 0, ampdu_density: 0,
    };
    check(request(&mut ctl, &Msg::SetBss(bss), 3, Direction::KernelToMlme) == 0, "SetBss acked");
    let req = sim_expect(&mut sim, SIM_REQ, 1000).unwrap_or_else(|| fail("radio got no SetBss"));
    check(matches!(decode(&req, Direction::MlmeToKernel), Ok((_, Msg::SetBss(b))) if b == bss), "radio got the same SetBss");

    // EAPOL out through key0.
    let body: Vec<u8> = (0..99u8).collect();
    let mut eapol = vec![2, 3, 0, 95];
    eapol.extend_from_slice(&body[4..]);
    check(request(&mut key, &Msg::TxEapol { dst: AP, body: &eapol }, 4, Direction::KernelToKeys) == 0, "TxEapol acked");
    let tx = sim_expect(&mut sim, SIM_TX, 1000).unwrap_or_else(|| fail("no EAPOL frame on the air"));
    let f = &tx[8..];
    let mut want = vec![0x88, 0x01, 0, 0];
    want.extend_from_slice(&AP);
    want.extend_from_slice(&OWN);
    want.extend_from_slice(&AP);
    want.extend_from_slice(&[0, 0, 7, 0, 0xaa, 0xaa, 3, 0, 0, 0, 0x88, 0x8e]);
    want.extend_from_slice(&eapol);
    check(f == &want[..] && tx[0] == 7 && tx[1] == 0, "EAPOL frame: ToDS, AP/us/AP, TID 7, SNAP 888e, unencrypted");

    // EAPOL in: to key0 only.
    rx_data(&mut sim, &from_ap(OWN, AP, 0x888e, &eapol));
    let m = read_msg(&mut key, 1000).unwrap_or_else(|| fail("no EapolRx on key0"));
    check(matches!(decode(&m, Direction::KernelToKeys), Ok((_, Msg::EapolRx { src, body })) if src == AP && body == &eapol[..]), "EAPOL from the AP reaches key0");
    check(read_msg(&mut ctl, 100).is_none(), "and not ctl0");

    // Data before the port opens is dropped.
    let d0 = sysctl_drops();
    rx_data(&mut sim, &from_ap(OWN, FAR, 0x0800, &icmp_echo(1)));
    std::thread::sleep(Duration::from_millis(300));
    let d1 = sysctl_drops();
    // Drop::PortClosed is variant 14 -> counter index 15.
    check(d1.get(15) > d0.get(15), "data with the port closed is dropped (PortClosed)");
    check(sim_expect(&mut sim, SIM_TX, 300).is_none(), "and nothing answers it");
    check(ifconfig_status().contains("no carrier"), "wlan0 shows no carrier with the port closed");

    // Open the port: link up, ping answered.
    check(request(&mut key, &Msg::PortAuthorized(true), 5, Direction::KernelToKeys) == 0, "PortAuthorized acked");
    std::thread::sleep(Duration::from_millis(300));
    check(ifconfig_status().contains("status: active"), "wlan0 link active after PortAuthorized");
    rx_data(&mut sim, &from_ap(OWN, FAR, 0x0800, &icmp_echo(2)));
    let tx = sim_expect(&mut sim, SIM_TX, 2000).unwrap_or_else(|| fail("no echo reply"));
    let f = &tx[8..];
    let ok_hdr = f[0] == 0x88 && f[1] == 0x01 && f[4..10] == AP && f[10..16] == OWN && f[16..22] == FAR;
    let ip = &f[34..];
    check(ok_hdr && f[32..34] == [0x08, 0x00] && ip[9] == 1 && ip[12..16] == OWN_IP && ip[16..20] == FAR_IP && ip[20] == 0, "ICMP echo answered through the sim, 802.11 header to the AP for FAR");

    // Management frames.
    let mut spoof = vec![0xb0, 0, 0, 0];
    spoof.extend_from_slice(&AP);
    spoof.extend_from_slice(&FAR);
    spoof.extend_from_slice(&AP);
    spoof.extend_from_slice(&[0, 0, 0, 0, 1, 0, 0, 0]);
    check(request(&mut ctl, &Msg::TxMgmt { freq: 2412, frame: &spoof }, 6, Direction::KernelToMlme) == -libc::EPERM, "TxMgmt with another station's address: EPERM");
    let mut beacon = spoof.clone();
    beacon[0] = 0x80;
    beacon[10..16].copy_from_slice(&OWN);
    let mut raw = vec![0u8; HEADER_LEN + MAX_BODY];
    // Hand-encode a beacon TxMgmt (encode() refuses it) to check the kernel does too.
    let n = {
        let good = Msg::TxMgmt { freq: 2412, frame: &spoof };
        encode(&good, 7, &mut raw).unwrap()
    };
    raw[HEADER_LEN + 2] = 0x80;
    let e = ctl.write(&raw[..n]).err().and_then(|e| e.raw_os_error());
    check(e == Some(libc::EINVAL), "a beacon in TxMgmt is refused (station frames only)");
    let mut mgmt = Vec::new();
    mgmt.extend_from_slice(&2412u16.to_le_bytes());
    mgmt.push((-40i8) as u8);
    mgmt.push(0);
    mgmt.extend_from_slice(&[0; 4]);
    mgmt.extend_from_slice(&5u64.to_le_bytes());
    sim_write(&mut sim, SIM_RX_MGMT, &mgmt, &beacon);
    let m = read_msg(&mut ctl, 1000).unwrap_or_else(|| fail("no MgmtRx"));
    check(matches!(decode(&m, Direction::KernelToMlme), Ok((_, Msg::MgmtRx { frame, rssi: -40, .. })) if frame == &beacon[..]), "received beacon reaches ctl0");
    check(read_msg(&mut key, 100).is_none(), "and not key0");

    // Events.
    let mut ev = vec![0u8; 64];
    let n = encode(&Msg::ScanDone { status: 0 }, 0, &mut ev).unwrap();
    sim_write(&mut sim, SIM_EVENT, &ev[..n], &[]);
    let m = read_msg(&mut ctl, 1000).unwrap_or_else(|| fail("no ScanDone"));
    check(matches!(decode(&m, Direction::KernelToMlme), Ok((_, Msg::ScanDone { status: 0 }))), "ScanDone event reaches ctl0");

    // Closing key0 leaves the BSS.
    drop(key);
    let req = sim_expect(&mut sim, SIM_REQ, 1000).unwrap_or_else(|| fail("no ClearBss after key0 closed"));
    check(matches!(decode(&req, Direction::MlmeToKernel), Ok((_, Msg::ClearBss))), "closing key0 clears the BSS in the radio");
    std::thread::sleep(Duration::from_millis(300));
    check(ifconfig_status().contains("no carrier"), "and wlan0 loses carrier");
    rx_data(&mut sim, &from_ap(OWN, FAR, 0x0800, &icmp_echo(3)));
    check(sim_expect(&mut sim, SIM_TX, 300).is_none(), "and data is no longer answered");

    println!("aura-sim-check: {} checks passed", unsafe { PASSED });
}
