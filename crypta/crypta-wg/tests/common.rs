// Shared by the integration tests.
#![allow(dead_code)]

use crypta_wg::Action;

/// An IPv4 packet of `len` bytes (header + `fill` bytes).
pub fn ipv4(len: usize, fill: u8) -> Vec<u8> {
    let mut p = vec![fill; len];
    p[0] = 0x45;
    p[1] = 0;
    p[2..4].copy_from_slice(&(len as u16).to_be_bytes());
    p[9] = 17;
    p[12..16].copy_from_slice(&[10, 0, 0, 1]);
    p[16..20].copy_from_slice(&[10, 0, 0, 2]);
    p
}

pub fn sends(a: &[Action]) -> Vec<Vec<u8>> {
    a.iter().filter_map(|x| if let Action::Send(v) = x { Some(v.clone()) } else { None }).collect()
}

pub fn delivers(a: &[Action]) -> Vec<Vec<u8>> {
    a.iter().filter_map(|x| if let Action::Deliver(v) = x { Some(v.clone()) } else { None }).collect()
}

pub fn events(a: &[Action]) -> Vec<crypta_wg::tunnel::Event> {
    a.iter().filter_map(|x| if let Action::Event(e) = x { Some(e.clone()) } else { None }).collect()
}
