//! ★★★ THE DECODE PATH, FUZZED — AND THE RUN MUST SHOW WHAT IT REACHED.
//!
//! The same discipline as tests/fuzz.rs: deterministic, seeded, and failing
//! unless every refusal the decoder can emit was produced by a mutated
//! input, for every format where it is meaningful. A fuzzer that only ever
//! produced `Corrupt` would have tested the decoders' front doors and none
//! of the admission checks that are the point of src/image.rs.
//!
//! ★★ AND MEMORY IS MEASURED, not trusted. A counting allocator (per
//! thread) records every decode's largest single allocation and its peak
//! live heap, and the run asserts:
//!
//! - a decode refused on its HEADER (too large, over budget, empty,
//!   unreadable) never allocated more than 1 MiB — the bomb was refused
//!   before the frame;
//! - no decode, refused or not, ever held more than the budget it was given
//!   plus a fixed 4 MiB of decoder state.
//!
//! The budget here is 8 MiB, small on purpose, so header lies routinely land
//! between "fits" and "does not" and both sides of the check are exercised.

mod common;
use common::images::*;
use navigator_backend::image::{decode, decode_unaddressed, Ceilings, DecodeError, Format, MAX_ENCODED_IMAGE_BYTES};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};

struct Counting;
thread_local! {
    static LIVE: Cell<usize> = const { Cell::new(0) };
    static PEAK: Cell<usize> = const { Cell::new(0) };
    static LARGEST: Cell<usize> = const { Cell::new(0) };
}
fn grew(n: usize) {
    let _ = LIVE.try_with(|l| {
        let v = l.get() + n;
        l.set(v);
        let _ = PEAK.try_with(|p| if v > p.get() { p.set(v) });
    });
    let _ = LARGEST.try_with(|m| if n > m.get() { m.set(n) });
}
fn shrank(n: usize) { let _ = LIVE.try_with(|l| l.set(l.get().saturating_sub(n))); }
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() { grew(l.size()) }
        p
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() { grew(l.size()) }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) { unsafe { System.dealloc(p, l) }; shrank(l.size()) }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, n) };
        if !q.is_null() {
            // A realloc that grows is an allocation of the new size, as far
            // as "how large a block did the decoder ask for" is concerned.
            shrank(l.size());
            grew(n);
        }
        q
    }
}
#[global_allocator]
static A: Counting = Counting;

/// Run `f`, returning (its result, largest single allocation, peak live
/// heap above the starting point).
fn measured<T>(f: impl FnOnce() -> T) -> (T, usize, usize) {
    let base = LIVE.with(|l| l.get());
    PEAK.with(|p| p.set(base));
    LARGEST.with(|m| m.set(0));
    let r = f();
    (r, LARGEST.with(|m| m.get()), PEAK.with(|p| p.get()) - base)
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12; x ^= x << 25; x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize { (self.next() % n as u64) as usize }
    fn byte(&mut self) -> u8 { (self.next() >> 24) as u8 }
}

fn seeds() -> Vec<(Format, Vec<u8>)> {
    use png::{BitDepth, ColorType};
    vec![
        (Format::Png, png_rgba(6, 5, &pattern(6, 5))),
        (Format::Png, png_typed(9, 2, ColorType::Indexed, BitDepth::One, &[0b1010_1010, 0b1000_0000, 0b0101_0101, 0])),
        (Format::Png, png_typed(2, 2, ColorType::Rgb, BitDepth::Sixteen, &[7u8; 24])),
        (Format::Jpeg, jpeg(16, 8, &pattern(16, 8), false)),
        (Format::Jpeg, jpeg(9, 17, &pattern(9, 17), true)),
        (Format::Gif, gif(5, 4, 1, 1, 3, 2)),
        (Format::Gif, gif(3, 3, 0, 0, 3, 3)),
        (Format::Webp, webp_lossless(5, 3, &pattern(5, 3))),
    ]
}

/// ★ Header lies, format by format: the dimensions rewritten to a value
/// drawn from the interesting band — zero, small, near the per-axis
/// ceiling, past it, and the largest the field can hold.
fn lie_about_size(rng: &mut Rng, f: Format, b: &mut [u8]) {
    let pick = |rng: &mut Rng, max: u32| -> u32 {
        match rng.below(7) {
            0 => 0, 1 => 1 + rng.below(8) as u32, 2 => 1000 + rng.below(2000) as u32,
            3 => 16_384, 4 => 16_385, 5 => max, _ => rng.next() as u32 % (max.max(1)),
        }
    };
    match f {
        Format::Png => if b.len() >= 24 {
            let (w, h) = (pick(rng, u32::MAX), pick(rng, u32::MAX));
            b[16..20].copy_from_slice(&w.to_be_bytes());
            b[20..24].copy_from_slice(&h.to_be_bytes());
        },
        Format::Gif => if b.len() >= 10 {
            let (w, h) = (pick(rng, 65_535) as u16, pick(rng, 65_535) as u16);
            if rng.below(2) == 0 {
                b[6..8].copy_from_slice(&w.to_le_bytes());
                b[8..10].copy_from_slice(&h.to_le_bytes());
            } else if let Some(i) = b.iter().position(|&x| x == 0x2C) {
                // The image descriptor: the FRAME claims a size, the screen
                // stays small.
                if i + 9 <= b.len() {
                    b[i + 5..i + 7].copy_from_slice(&w.to_le_bytes());
                    b[i + 7..i + 9].copy_from_slice(&h.to_le_bytes());
                }
            }
        },
        Format::Jpeg => {
            // The frame header's height and width, wherever SOF is.
            let mut i = 2;
            while i + 9 <= b.len() {
                if b[i] == 0xFF && matches!(b[i + 1], 0xC0 | 0xC1 | 0xC2) {
                    let (w, h) = (pick(rng, 65_535) as u16, pick(rng, 65_535) as u16);
                    b[i + 5..i + 7].copy_from_slice(&h.to_be_bytes());
                    b[i + 7..i + 9].copy_from_slice(&w.to_be_bytes());
                    break;
                }
                i += 1;
            }
        }
        Format::Webp => if b.len() >= 25 {
            // VP8L: 14-bit width-1 and height-1 after the signature byte.
            if &b[12..16] == b"VP8L" {
                let (w, h) = (pick(rng, 16_384).clamp(1, 16_384) - 1, pick(rng, 16_384).clamp(1, 16_384) - 1);
                let bits = u32::from_le_bytes([b[21], b[22], b[23], b[24]]);
                let bits = (bits & !0x0FFF_FFFF) | (w & 0x3FFF) | ((h & 0x3FFF) << 14);
                b[21..25].copy_from_slice(&bits.to_le_bytes());
            }
        },
    }
}

fn mutate(rng: &mut Rng, f: Format, src: &[u8]) -> Vec<u8> {
    let mut b = src.to_vec();
    match rng.below(9) {
        0 | 1 => lie_about_size(rng, f, &mut b),
        2 => { let i = rng.below(b.len() + 1); b.truncate(i); }
        3 => { for _ in 0..1 + rng.below(4) { let i = rng.below(b.len()); b[i] = rng.byte(); } }
        // A length field that lies: four 0xFF bytes somewhere past the magic.
        4 => { if b.len() > 16 { let i = 8 + rng.below(b.len() - 12); b[i..i + 4].fill(0xFF); } }
        5 => { let i = rng.below(b.len() + 1); let junk: Vec<u8> = (0..rng.below(64)).map(|_| rng.byte()).collect(); b.splice(i..i, junk); }
        6 => { if b.len() > 8 { let i = 8 + rng.below(b.len() - 8); let n = rng.below(b.len() - i + 1); b.drain(i..i + n); } }
        7 => { lie_about_size(rng, f, &mut b); let i = rng.below(b.len() + 1); b.truncate(i.max(30.min(b.len()))); }
        _ => { // a chunk duplicated
            if b.len() > 24 { let i = 8 + rng.below(b.len() - 16); let n = 8.min(b.len() - i); let c = b[i..i + n].to_vec(); b.splice(i..i, c); }
        }
    }
    b
}

const ROUNDS: usize = 20_000;
const BUDGET: usize = 8 << 20;
/// What a header refusal may allocate: the decoder objects, not a frame.
const HEADER_REFUSAL_ALLOC: usize = 1 << 20;
/// Decoder state beyond the budget (inflate windows, Huffman tables).
const DECODER_SLACK: usize = 4 << 20;

#[test]
fn mutated_images_never_panic_reach_every_refusal_and_stay_in_their_budget() {
    let seeds = seeds();
    let c = Ceilings { budget: BUDGET, ..Ceilings::default() };
    let mut rng = Rng(0x1A6E_5EED_0000_0042);
    let mut reached: BTreeSet<(String, &'static str)> = BTreeSet::new();
    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    let (mut max_header_alloc, mut max_peak) = (0usize, 0usize);
    for round in 0..ROUNDS {
        let (f, src) = &seeds[rng.below(seeds.len())];
        let input = if round % 11 == 0 { src.clone() } else { mutate(&mut rng, *f, src) };
        let probe = input.clone();
        let (r, largest, peak) = measured(|| std::panic::catch_unwind(|| decode_unaddressed(&probe, c)));
        let r = r.unwrap_or_else(|_| panic!("PANIC decoding round {round} ({}): {:?}", f.name(), input));
        let cause = match &r { Ok(_) => "ok", Err(e) => e.cause() };
        let fmt = match &r {
            Ok(_) => f.name().to_string(),
            Err(DecodeError::NotARaster { kind }) => format!("not:{kind}"),
            Err(DecodeError::Header { format, .. } | DecodeError::Empty { format } | DecodeError::TooLarge { format, .. }
                | DecodeError::OverBudget { format, .. } | DecodeError::Corrupt { format, .. }) => format.name().to_string(),
            Err(e) => panic!("unexpected {e:?}"),
        };
        reached.insert((fmt.clone(), cause));
        *counts.entry(cause).or_default() += 1;
        if let Ok(p) = &r {
            assert_eq!(p.rgba.len(), p.width as usize * p.height as usize * 4, "round {round}: shape");
            // ★ The positive control: the counter must SEE the frame it was
            // handed back, or every bound below is vacuous.
            assert!(peak >= p.rgba.len() && largest >= p.rgba.len(),
                "round {round}: the allocator counted {peak} live / {largest} largest for a {}-byte frame", p.rgba.len());
        }
        // ★★ The bomb is refused before the frame.
        if matches!(r, Err(DecodeError::TooLarge { .. } | DecodeError::OverBudget { .. } | DecodeError::Empty { .. })) {
            max_header_alloc = max_header_alloc.max(largest);
            assert!(largest <= HEADER_REFUSAL_ALLOC,
                "round {round} ({fmt} {cause}): refused on its header but allocated {largest} bytes first: {:?}", &input[..input.len().min(64)]);
        }
        max_peak = max_peak.max(peak);
        assert!(peak <= BUDGET + DECODER_SLACK,
            "round {round} ({fmt} {cause}): held {peak} bytes on an {BUDGET}-byte budget: {:?}", &input[..input.len().min(64)]);
    }
    eprintln!("image fuzz: {counts:?}");
    eprintln!("image fuzz: largest allocation before a header refusal {max_header_alloc} B; peak live {max_peak} B (budget {BUDGET})");
    eprintln!("image fuzz: reached {reached:?}");

    // ★★ THE REACH ASSERTION. Every admission refusal, for every format —
    // these are the checks this module exists for — plus decoding success
    // and decoder refusal for every format.
    for f in [Format::Png, Format::Jpeg, Format::Gif, Format::Webp] {
        let causes: &[&str] = match f {
            // A VP8L size field cannot exceed 16,384, the per-axis ceiling,
            // so a lossless WebP can be over budget but not too large.
            Format::Webp => &["ok", "corrupt", "header", "over-budget"],
            _ => &["ok", "corrupt", "header", "too-large", "over-budget", "empty"],
        };
        for &cause in causes {
            assert!(reached.contains(&(f.name().to_string(), cause)),
                "the fuzzer never produced {cause} for {}: unreachable, or the generator cannot \
                 build an input that reaches it. Reached: {reached:?}", f.name());
        }
    }
    assert!(reached.iter().any(|(f, _)| f.starts_with("not:")), "a mutation never destroyed a magic number");
}

/// The refusals the byte-mutation loop cannot produce, because they are
/// decided before the bytes are looked at — pinned directly.
#[test]
fn the_refusals_decided_before_the_bytes_are_reached_too() {
    let b = png_rgba(2, 2, &pattern(2, 2));
    assert_eq!(decode("blake3:0000000000000000000000000000000000000000000000000000000000000000", &b, Ceilings::default()),
        Err(DecodeError::AddressMismatch));
    let big = vec![0u8; MAX_ENCODED_IMAGE_BYTES + 1];
    assert!(matches!(decode_unaddressed(&big, Ceilings::default()), Err(DecodeError::EncodedTooLarge { .. })));
}

/// A WebP whose VP8X canvas claims 16,384² (1 GiB), and one past the
/// per-axis ceiling: built, because the lossless seeds carry no VP8X chunk
/// to lie in, and the canvas is 24 bits per axis where VP8L's is 14.
#[test]
fn a_webp_canvas_that_lies_is_refused_on_its_header() {
    let vp8x = |side: u32| {
        let mut b = b"RIFF\0\0\0\0WEBPVP8X\x0a\0\0\0\0\0\0\0".to_vec();
        let d = (side - 1).to_le_bytes();
        b.extend_from_slice(&d[..3]);
        b.extend_from_slice(&d[..3]);
        let n = (b.len() - 8) as u32;
        b[4..8].copy_from_slice(&n.to_le_bytes());
        b
    };
    let (r, largest, _) = measured(|| decode_unaddressed(&vp8x(16_384), Ceilings::default()));
    assert!(matches!(r, Err(DecodeError::OverBudget { format: Format::Webp, needed, .. }) if needed == 1 << 30), "{r:?}");
    assert!(largest <= HEADER_REFUSAL_ALLOC, "allocated {largest} before refusing");
    let (r, largest, _) = measured(|| decode_unaddressed(&vp8x(1 << 24), Ceilings::default()));
    assert!(matches!(r, Err(DecodeError::TooLarge { format: Format::Webp, width: 16_777_216, .. })), "{r:?}");
    assert!(largest <= HEADER_REFUSAL_ALLOC, "allocated {largest} before refusing");
}

#[test]
fn every_seed_decodes() {
    for (i, (f, s)) in seeds().iter().enumerate() {
        let p = decode_unaddressed(s, Ceilings::default()).unwrap_or_else(|e| panic!("seed {i} ({}): {e}", f.name()));
        assert!(p.width > 0);
    }
}
