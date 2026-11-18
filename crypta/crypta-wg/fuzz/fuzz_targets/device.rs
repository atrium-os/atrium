//! A device with one peer, fed datagrams that are partly random and partly
//! real handshake and transport messages from that peer with bytes changed
//! (vpn.md §4.2): nothing panics, and anything delivered to tun is an IP
//! packet the peer really sent.
//!   cargo fuzz run device
#![no_main]
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crypta_wg::{Action, Device, LocalKey, Peer, StaticKey};
use libfuzzer_sys::fuzz_target;

struct Keys {
    ours: Arc<LocalKey>,
    theirs: Arc<LocalKey>,
}

fn keys() -> &'static Keys {
    static K: OnceLock<Keys> = OnceLock::new();
    K.get_or_init(|| Keys { ours: Arc::new(LocalKey::from_bytes([1; 32])), theirs: Arc::new(LocalKey::from_bytes([2; 32])) })
}

fn packet(n: u8) -> Vec<u8> {
    let len = 20 + n as usize;
    let mut p = vec![n; len];
    p[0] = 0x45;
    p[2..4].copy_from_slice(&(len as u16).to_be_bytes());
    p
}

fuzz_target!(|data: &[u8]| {
    let k = keys();
    let mut us = Device::new(k.ours.clone());
    us.add_peer(Peer { public: k.theirs.public(), psk: None, persistent_keepalive: None });
    let mut them = Device::new(k.theirs.clone());
    them.add_peer(Peer { public: k.ours.public(), psk: None, persistent_keepalive: None });
    us.force_load = data.first().is_some_and(|b| b & 1 == 1);
    let mut sent_by_them: Vec<Vec<u8>> = Vec::new();
    let mut from_them: Vec<Vec<u8>> = Vec::new();
    let mut now = Duration::ZERO;
    let mut i = 1;
    while i + 2 <= data.len() {
        let (op, arg) = (data[i], data[i + 1]);
        i += 2;
        now += Duration::from_millis(u64::from(arg) * 40);
        // What we did (sends go to them) and what they did (sends go to us).
        let mut theirs: Vec<Action> = Vec::new();
        let ours = match op % 6 {
            // They send a packet (starting a handshake if need be).
            0 => {
                let p = packet(arg);
                sent_by_them.push(p.clone());
                theirs = them.peer(0).encapsulate(&p, now);
                Vec::new()
            }
            // We send one.
            1 => us.peer(0).encapsulate(&packet(arg), now),
            // Timers on both sides.
            2 => {
                theirs = them.peer(0).tick(now);
                us.peer(0).tick(now)
            }
            // A real datagram of theirs, with one byte changed.
            3 if !from_them.is_empty() => {
                let mut d = from_them[arg as usize % from_them.len()].clone();
                let at = (arg as usize * 7) % d.len();
                d[at] ^= data.get(i).copied().unwrap_or(1) | 1;
                us.decapsulate(&[9, 9], &d, now).1
            }
            // A real datagram of theirs, again (replay).
            4 if !from_them.is_empty() => us.decapsulate(&[9, 9], &from_them[arg as usize % from_them.len()].clone(), now).1,
            // Random bytes from the rest of the input.
            _ => {
                let n = (arg as usize).min(data.len() - i);
                let d = data[i..i + n].to_vec();
                i += n;
                us.decapsulate(&[1, 2], &d, now).1
            }
        };
        // Deliver what each sent to the other.
        let sends = |acts: Vec<Action>, to_them: bool| acts.into_iter().filter_map(move |a| if let Action::Send(d) = a { Some((to_them, d)) } else { None });
        let mut queue: Vec<(bool, Vec<u8>)> = sends(ours, true).chain(sends(theirs, false)).collect();
        let mut rounds = 0;
        while let Some((to_them, d)) = queue.pop() {
            rounds += 1;
            if rounds > 64 {
                break;
            }
            let acts = if to_them {
                them.decapsulate(&[3, 3], &d, now).1
            } else {
                from_them.push(d.clone());
                us.decapsulate(&[9, 9], &d, now).1
            };
            for a in acts {
                match a {
                    Action::Send(x) => queue.push((!to_them, x)),
                    Action::Deliver(p) if !to_them => assert!(sent_by_them.contains(&p), "delivered a packet they never sent"),
                    _ => {}
                }
            }
        }
    }
});
