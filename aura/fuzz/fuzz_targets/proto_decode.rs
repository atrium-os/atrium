//! Any bytes, any direction: decode never panics, and whatever decodes
//! re-encodes to exactly the input (aura-proto has one encoding per message).
#![no_main]

use aura_proto::{decode, encode, Direction, HEADER_LEN, MAX_BODY};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    for d in [
        Direction::MlmeToKernel,
        Direction::KeysToKernel,
        Direction::KernelToMlme,
        Direction::KernelToKeys,
    ] {
        if let Ok((h, m)) = decode(data, d) {
            let mut out = [0u8; MAX_BODY + HEADER_LEN];
            let n = encode(&m, h.seq, &mut out).expect("a decoded message encodes");
            assert_eq!(&out[..n], data);
        }
    }
});
