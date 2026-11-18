//! A crypta-wg peer on a UDP socket, without tun, for interop runs against
//! another WireGuard (scripts/crypta-wg-linux-vm.sh): it pings through the
//! tunnel, or answers pings that come through it. Our tunnel address is
//! 10.99.0.2, theirs 10.99.0.1.
//!
//!   udp_peer pubkey SEED_HEX
//!   udp_peer ping  SEED_HEX LISTEN_PORT PEER_ADDR:PORT PEER_PUB_B64 COUNT
//!   udp_peer serve SEED_HEX LISTEN_PORT PEER_PUB_B64 SECONDS
//!
//! Prints one line per event and ends with "udp_peer: N of M answered".
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crypta_wg::tunnel::Event;
use crypta_wg::{Action, Device, LocalKey, Peer, StaticKey};

const US: [u8; 4] = [10, 99, 0, 2];
const THEM: [u8; 4] = [10, 99, 0, 1];

fn b64(b: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for c in b.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= c.len() {
                s.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

fn unb64(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut n = 0u32;
    let mut bits = 0;
    for ch in s.bytes().filter(|&c| c != b'=') {
        let v = match ch {
            b'A'..=b'Z' => ch - b'A',
            b'a'..=b'z' => ch - b'a' + 26,
            b'0'..=b'9' => ch - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => panic!("not base64"),
        };
        n = (n << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((n >> bits) as u8);
        }
    }
    out
}

fn key(seed_hex: &str) -> LocalKey {
    let b: Vec<u8> = (0..32).map(|i| u8::from_str_radix(&seed_hex[2 * i..2 * i + 2], 16).unwrap()).collect();
    LocalKey::from_bytes(b.try_into().unwrap())
}

fn checksum(b: &[u8]) -> u16 {
    let mut s: u32 = b.chunks(2).map(|c| u32::from(u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]))).sum();
    while s > 0xffff {
        s = (s & 0xffff) + (s >> 16);
    }
    !(s as u16)
}

/// An ICMP echo (request 8, reply 0) from `src` to `dst`.
fn icmp(kind: u8, src: [u8; 4], dst: [u8; 4], id: u16, seq: u16, data: &[u8]) -> Vec<u8> {
    let len = 20 + 8 + data.len();
    let mut p = vec![0u8; len];
    p[0] = 0x45;
    p[2..4].copy_from_slice(&(len as u16).to_be_bytes());
    p[8] = 64;
    p[9] = 1;
    p[12..16].copy_from_slice(&src);
    p[16..20].copy_from_slice(&dst);
    let c = checksum(&p[..20]);
    p[10..12].copy_from_slice(&c.to_be_bytes());
    p[20] = kind;
    p[24..26].copy_from_slice(&id.to_be_bytes());
    p[26..28].copy_from_slice(&seq.to_be_bytes());
    p[28..].copy_from_slice(data);
    let c = checksum(&p[20..]);
    p[22..24].copy_from_slice(&c.to_be_bytes());
    p
}

/// (type, src, id, seq, data) of an ICMP echo packet.
fn parse_icmp(p: &[u8]) -> Option<(u8, [u8; 4], u16, u16, Vec<u8>)> {
    if p.len() < 28 || p[9] != 1 || checksum(&p[..20]) != 0 || checksum(&p[20..]) != 0 {
        return None;
    }
    Some((p[20], p[12..16].try_into().ok()?, u16::from_be_bytes([p[24], p[25]]), u16::from_be_bytes([p[26], p[27]]), p[28..].to_vec()))
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let k = key(&a[2]);
    if a[1] == "pubkey" {
        println!("{}", b64(&k.public()));
        return;
    }
    let port: u16 = a[3].parse().unwrap();
    let sock = UdpSocket::bind(("0.0.0.0", port)).unwrap();
    sock.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    let (mut endpoint, peer_pub, ping) = match a[1].as_str() {
        "ping" => (Some(a[4].parse::<SocketAddr>().unwrap()), unb64(&a[5]), Some(a[6].parse::<u16>().unwrap())),
        _ => (None, unb64(&a[4]), None),
    };
    let until = Instant::now() + Duration::from_secs(if ping.is_some() { 60 } else { a[5].parse().unwrap() });
    let mut dev = Device::new(Arc::new(k));
    dev.add_peer(Peer { public: peer_pub.try_into().unwrap(), psk: None, persistent_keepalive: None });
    let start = Instant::now();
    let (mut asked, mut answered) = (0u16, 0u16);
    let mut next_ping = Instant::now();
    let mut buf = vec![0u8; 65536];
    while Instant::now() < until {
        let now = start.elapsed();
        let mut acts = dev.peer(0).tick(now);
        if let Some(n) = ping {
            if asked < n && Instant::now() >= next_ping {
                asked += 1;
                next_ping = Instant::now() + Duration::from_millis(100);
                let data: Vec<u8> = (0..(56 + asked as usize * 20)).map(|i| i as u8).collect();
                acts.extend(dev.peer(0).encapsulate(&icmp(8, US, THEM, 0x4357, asked, &data), now));
            }
            if answered == n {
                break;
            }
        }
        if let Ok((len, from)) = sock.recv_from(&mut buf) {
            endpoint = Some(from); // roaming: answer where it came from
            let src = format!("{from}").into_bytes();
            let (_, more) = dev.decapsulate(&src, &buf[..len], start.elapsed());
            acts.extend(more);
        }
        let mut i = 0;
        while i < acts.len() {
            match acts[i].clone() {
                Action::Send(d) => {
                    if let Some(e) = endpoint {
                        sock.send_to(&d, e).unwrap();
                    }
                }
                Action::Deliver(p) => match parse_icmp(&p) {
                    Some((0, src, 0x4357, seq, data)) if src == THEM && data.len() == 56 + seq as usize * 20 => {
                        answered += 1;
                        println!("udp_peer: reply {seq} ({} bytes)", p.len());
                    }
                    Some((8, src, id, seq, data)) if src == THEM => {
                        println!("udp_peer: echo request {seq} from them, answering");
                        let reply = icmp(0, US, THEM, id, seq, &data);
                        acts.extend(dev.peer(0).encapsulate(&reply, start.elapsed()));
                        answered += 1;
                        asked += 1;
                    }
                    other => println!("udp_peer: delivered something else: {:?}", other.map(|o| (o.0, o.1))),
                },
                Action::Event(e) => {
                    if !matches!(e, Event::Dropped(_)) || std::env::var_os("UDP_PEER_VERBOSE").is_some() {
                        println!("udp_peer: {e:?}");
                    }
                }
            }
            i += 1;
        }
    }
    println!("udp_peer: {answered} of {} answered", ping.unwrap_or(asked));
    std::process::exit(if ping.is_some_and(|n| answered == n) || (ping.is_none() && answered > 0) { 0 } else { 1 });
}
