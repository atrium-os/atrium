//! The H4 reassembler on arbitrary bytes, split at an arbitrary point: never
//! panics, and frames out equal frames checked whole.
#![no_main]
use libfuzzer_sys::fuzz_target;
use vinculum_hci_proto::h4::{Decoder, Frame};

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    let cut = data[0] as usize % data.len();
    let body = &data[1..];
    let mut d = Decoder::new();
    let mut frames = Vec::new();
    let r = d
        .push(&body[..cut.min(body.len())], |f| frames.push(f))
        .and_then(|_| d.push(&body[cut.min(body.len())..], |f| frames.push(f)));
    let _ = r;
    for f in frames {
        assert_eq!(Frame::parse(&f.to_bytes()).unwrap(), f);
    }
});
