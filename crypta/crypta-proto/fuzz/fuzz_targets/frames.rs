//! What a compromised engine could send cryptad: frames in any pieces.
#![no_main]
use crypta_proto::engine::{FromEngine, Reader, ToEngine};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let split = data.first().map_or(1, |b| usize::from(*b % 16) + 1);
    let mut r = Reader::default();
    for c in data.chunks(split) {
        r.push(c);
        loop {
            match r.next::<FromEngine>() {
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
    }
    let _ = crypta_proto::engine::decode::<ToEngine>(data);
});
