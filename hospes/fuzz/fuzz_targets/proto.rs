//! Hostile messages on hospes-cdm's channels (docs/spec/hospes.md §11.6): either side may be
//! compromised, so decoding must return, never panic, and what it accepts must re-encode to a
//! message that decodes the same. A decoded frame's planes must lie inside its data.
#![no_main]
use hospes_proto::{parse_header, Msg};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() < 8 {
        return;
    }
    let h: [u8; 8] = data[..8].try_into().unwrap();
    let k = match parse_header(&h) {
        Ok((k, _)) => k,
        Err(_) => u32::from_le_bytes([h[0], h[1], h[2], h[3]]),
    };
    if let Ok(m) = Msg::decode(k, &data[8..]) {
        let e = m.encode();
        let (k2, n) = parse_header(e[..8].try_into().unwrap()).expect("our own header");
        assert_eq!(n as usize, e.len() - 8);
        let m2 = Msg::decode(k2, &e[8..]).expect("our own encoding decodes");
        assert_eq!(m2.encode(), e);
        if let Msg::Decoded { frame } = &m {
            if let Some(p) = frame.planes(7680, 4320) {
                for (off, stride, rows) in p {
                    assert!(rows >= 1 && off + stride * (rows - 1) < frame.data.len());
                }
            }
        }
    }
});
