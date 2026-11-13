//! Everything a device says is attacker-chosen: no input makes a parser
//! panic or allocate past its caps. A cheap stand-in for the fuzz targets
//! (sigillum.md §15), run on every `cargo test`.

use sigillum_ctap::authdata::AuthData;
use sigillum_ctap::cmd::{Assertion, Attestation, ClientPinResponse, Info};
use sigillum_ctap::hid::{Reassembler, REPORT};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

/// Random bytes, and random mutations of a valid message.
fn inputs(seed: u64, valid: &[u8]) -> impl Iterator<Item = Vec<u8>> + '_ {
    let mut rng = Rng(seed);
    (0..4000).map(move |i| {
        if i % 2 == 0 {
            let n = (rng.next() % 300) as usize;
            rng.bytes(n)
        } else {
            let mut v = valid.to_vec();
            for _ in 0..1 + rng.next() % 4 {
                if v.is_empty() {
                    break;
                }
                let at = (rng.next() as usize) % v.len();
                match rng.next() % 3 {
                    0 => v[at] = rng.next() as u8,
                    1 => v.truncate(at),
                    _ => v.insert(at, rng.next() as u8),
                }
            }
            v
        }
    })
}

#[test]
fn parsers_survive_garbage() {
    let key = sigillum_softkey::SoftKey::new(Some(b"1234"));
    let info = sigillum_cbor::encode(&key.info().to_value());
    for b in inputs(1, &info) {
        let _ = Info::parse(&b);
        let _ = Attestation::parse(&b);
        let _ = Assertion::parse(&b, &[]);
        let _ = Assertion::parse(&b, &[vec![1; 16]]);
        let _ = ClientPinResponse::parse(&b);
        let _ = AuthData::parse(&b);
        let _ = sigillum_cbor::decode(&b);
    }
}

#[test]
fn reassembler_survives_garbage() {
    let mut rng = Rng(7);
    let mut r = Reassembler::new(0x0102_0304);
    for _ in 0..20000 {
        let mut p = [0u8; REPORT];
        p.copy_from_slice(&rng.bytes(REPORT));
        if rng.next() % 2 == 0 {
            p[..4].copy_from_slice(&0x0102_0304u32.to_be_bytes());
        }
        if r.feed(&p).is_err() {
            r = Reassembler::new(0x0102_0304);
        }
    }
}
