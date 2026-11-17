#![no_main]
//! Hostile mDNS datagrams: parsing and the browser cache never panic and stay
//! bounded.
use libfuzzer_sys::fuzz_target;
use prelum_dnssd::{Browser, Packet, IPP, IPPS, MAX_SERVICES};

fuzz_target!(|data: &[u8]| {
    let mut b = Browser::new(&[IPP, IPPS]);
    // Feed the input as several datagrams so the cache sees updates.
    for (i, chunk) in data.split(|&x| x == 0xA5).enumerate() {
        if let Ok(p) = Packet::parse(chunk) {
            b.feed(&p, i as u64);
        }
    }
    if let Ok(p) = Packet::parse(data) {
        b.feed(&p, 1);
        b.feed(&p, 2);
    }
    let s = b.services(3);
    assert!(s.len() <= MAX_SERVICES);
    for x in &s {
        assert!(x.instance.len() <= 128 && x.addrs.len() <= 8);
    }
});
