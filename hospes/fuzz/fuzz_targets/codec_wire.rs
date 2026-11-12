//! Hostile pictures and access units on hospes-codec's channel (docs/spec/hospes.md §11.6):
//! the worker trusts nothing the codec sends, and the codec host splits whatever Annex-B the
//! worker sends.
#![no_main]
use hospes_codec_wire::{i420_size, parse_header, split_annexb, Picture};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() >= 8 {
        let _ = parse_header(data[..8].try_into().unwrap());
    }
    if let Some(p) = Picture::parse(data, 7680, 4320) {
        assert!(p.width >= 1 && p.width <= 7680 && p.height >= 1 && p.height <= 4320);
        assert_eq!(p.y.len() + p.u.len() + p.v.len(), i420_size(p.width, p.height).unwrap());
    }
    let mut end = 0;
    for r in split_annexb(data) {
        assert!(r.start <= r.end && r.end <= data.len() && r.start >= end);
        end = r.end;
    }
});
