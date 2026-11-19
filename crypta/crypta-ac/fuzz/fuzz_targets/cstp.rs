//! Any bytes in the CSTP tunnel (vpn.md §9.4): the deframer never panics,
//! however the TLS stream splits them, and every frame it gives frames back
//! to the bytes it came from.
//!   cargo fuzz run cstp
#![no_main]
use crypta_ac::cstp;
use libfuzzer_sys::fuzz_target;

fn run(data: &[u8], step: usize) -> (Vec<(u8, Vec<u8>)>, bool) {
    let mut d = cstp::Deframer::default();
    let mut out = Vec::new();
    for piece in data.chunks(step.max(1)) {
        d.push(piece);
        loop {
            match d.next() {
                Ok(Some(f)) => out.push(f),
                Ok(None) => break,
                Err(_) => return (out, true),
            }
        }
    }
    (out, false)
}

fuzz_target!(|data: &[u8]| {
    let Some((&step, data)) = data.split_first() else { return };
    let (whole, failed) = run(data, data.len());
    let (split, failed2) = run(data, step as usize);
    assert_eq!(whole, split);
    assert_eq!(failed, failed2);
    let mut again = Vec::new();
    for (k, p) in &whole {
        again.extend_from_slice(&cstp::frame(*k, p));
    }
    assert_eq!(&data[..again.len()], &again[..]);
});
