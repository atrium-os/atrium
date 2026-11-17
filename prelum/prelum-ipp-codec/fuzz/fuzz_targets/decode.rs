#![no_main]
//! Hostile printer replies: decoding never panics, stays within its limits,
//! and decode∘encode is stable after one pass.
use libfuzzer_sys::fuzz_target;
use prelum_ipp_codec::printer::PrinterInfo;
use prelum_ipp_codec::{Limits, Message};

fuzz_target!(|data: &[u8]| {
    let lim = Limits { max_bytes: 1 << 16, max_attributes: 512, max_values: 2048, max_depth: 8, max_groups: 16 };
    let Ok((m1, off)) = Message::decode(data, &lim) else { return };
    assert!(off <= data.len());
    let p = PrinterInfo::from_response(&m1);
    assert!(p.info.len() <= 200 && p.location.len() <= 200 && p.make_and_model.len() <= 200);
    let _ = p.choose_format(false);
    let big = Limits { max_bytes: 1 << 24, max_attributes: 1 << 20, max_values: 1 << 20, max_depth: 8, max_groups: 1 << 10 };
    let (m2, _) = Message::decode(&m1.encode(), &big).expect("re-decode of our own encoding");
    let (m3, _) = Message::decode(&m2.encode(), &big).expect("second re-decode");
    assert_eq!(m2, m3);
});
