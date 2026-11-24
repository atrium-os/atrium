//! The EAP peer against hostile packets: the parsers and the peer itself,
//! fed a run of length-prefixed packets. Nothing may panic.
#![no_main]

use aura_eap::{peap, tls, ttls, Config, Method, Peer, Trust};
use libfuzzer_sys::fuzz_target;
use zeroize::Zeroizing;

fuzz_target!(|data: &[u8]| {
    let Some((&sel, mut rest)) = data.split_first() else { return };
    let _ = ttls::parse(rest);
    let _ = peap::parse(rest);
    let _ = aura_eap::eap::parse(rest);
    let mut f = tls::Frames::default();
    for c in rest.chunks(7) {
        let _ = f.take(c);
    }
    let method = [Method::Peap, Method::TtlsPap, Method::TtlsMschapv2, Method::Tls][sel as usize % 4];
    let mut peer = Peer::new(Config {
        method,
        outer_identity: "anonymous@corp.example".into(),
        identity: "alice".into(),
        password: Zeroizing::new("pw".into()),
        trust: Trust::Unset,
        client: None,
    });
    while rest.len() >= 2 {
        let n = (u16::from_be_bytes([rest[0], rest[1]]) as usize).min(rest.len() - 2);
        let _ = peer.handle(&rest[2..2 + n]);
        rest = &rest[2 + n..];
    }
});
