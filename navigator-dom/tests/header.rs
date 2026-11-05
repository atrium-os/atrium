//! `header_size`: an image's size from its leading bytes (legacy-web
//! §5.4.5.7, cut A). The bytes are hostile, so the contract is checked
//! DIFFERENTIALLY against `natural_size` on the whole image, on every prefix:
//! `Sized` must be exactly what the whole image will say, and anything else
//! must decline rather than guess.
//!
//! The fuzz loop mutates header-shaped seeds of every format read here, and
//! ★ counts which outcomes it reached per format: a fuzzer that never
//! reaches `Sized` for a JPEG is not testing JPEG (the coverage-lies lesson),
//! so the test fails unless every outcome class is reached.
//! `NAVIGATOR_HEADER_FUZZ_ITERS` raises the iteration count;
//! `NAVIGATOR_HEADER_CORPUS=<dir>[:<dir>…]` adds every file in those
//! directories (real images by content address) as checked inputs and seeds.

use navigator_dom::natural::{header_size, natural_size, next_header_read, Header, Natural, MAX_HEADER_BYTES};
use std::collections::BTreeMap;

fn png(w: u32, h: u32) -> Vec<u8> {
    let mut v = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    v.extend(w.to_be_bytes()); v.extend(h.to_be_bytes());
    v.extend([8, 6, 0, 0, 0, 0, 0, 0, 0]); v.extend(b"\0\0\0\0IEND\xaeB`\x82"); v
}
fn gif(w: u16, h: u16) -> Vec<u8> {
    let mut v = b"GIF89a".to_vec(); v.extend(w.to_le_bytes()); v.extend(h.to_le_bytes());
    v.extend([0u8; 30]); v
}
fn jpeg(w: u16, h: u16, app: usize) -> Vec<u8> {
    let mut v = vec![0xFF, 0xD8];
    // An APP1 segment of `app` bytes (EXIF stand-in) before the frame.
    let len = (app + 2).min(0xFFFF) as u16;
    v.extend([0xFF, 0xE1]); v.extend(len.to_be_bytes()); v.extend(std::iter::repeat(0x45).take(len as usize - 2));
    v.extend([0xFF, 0xC0, 0x00, 0x11, 0x08]); v.extend(h.to_be_bytes()); v.extend(w.to_be_bytes());
    v.extend([3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
    v.extend([0xFF, 0xD9]); v
}
fn webp_x(w: u32, h: u32) -> Vec<u8> {
    let mut v = b"RIFF\0\0\0\0WEBPVP8X\x0a\0\0\0\0\0\0\0".to_vec();
    v.extend(&(w - 1).to_le_bytes()[..3]); v.extend(&(h - 1).to_le_bytes()[..3]); v.extend([0u8; 16]); v
}
fn webp_l(w: u32, h: u32) -> Vec<u8> {
    let mut v = b"RIFF\0\0\0\0WEBPVP8L\0\0\0\0\x2f".to_vec();
    let bits = (w - 1) | ((h - 1) << 14);
    v.extend(bits.to_le_bytes()); v.extend([0u8; 16]); v
}
fn webp_lossy(w: u16, h: u16) -> Vec<u8> {
    let mut v = b"RIFF\0\0\0\0WEBPVP8 \0\0\0\0\0\0\0\x9d\x01\x2a".to_vec();
    v.extend(w.to_le_bytes()); v.extend(h.to_le_bytes()); v.extend([0u8; 16]); v
}
fn svg(attrs: &str, pad: usize) -> Vec<u8> {
    let mut s = format!("<?xml version=\"1.0\"?>\n{}<svg xmlns=\"http://www.w3.org/2000/svg\" {attrs}><rect width=\"5\"/></svg>",
                        "<!-- pad -->".repeat(pad));
    s.push('\n'); s.into_bytes()
}

/// Every check the contract makes on one input, and what it reached.
fn check(full: &[u8], reached: &mut BTreeMap<(&'static str, &'static str), usize>) {
    let whole = natural_size(full);
    let kind = if full.starts_with(b"\x89PNG") { "png" } else if full.starts_with(&[0xFF, 0xD8]) { "jpeg" }
        else if full.starts_with(b"GIF8") { "gif" } else if full.starts_with(b"RIFF") { "webp" }
        else if String::from_utf8_lossy(&full[..full.len().min(4096)]).contains("<svg") { "svg" } else { "other" };
    let verdict = |p: usize| -> Header {
        let h = header_size(&full[..p], p == full.len());
        match h {
            Header::Sized(n) => assert_eq!(Some(n), whole, "Sized on a {p}-byte prefix of a {}-byte {kind} differs from the whole", full.len()),
            Header::More(n) => {
                assert!(n > p, "More({n}) asks for no more than the {p} bytes given");
                assert!(n <= MAX_HEADER_BYTES, "More({n}) past the cap");
                assert!(p < full.len(), "More on the complete image");
            }
            Header::Unreadable(_) => {}
        }
        h
    };
    // Every short prefix, then a spread of longer ones.
    let n = full.len();
    let mut lens: Vec<usize> = (0..=n.min(300)).collect();
    lens.extend((300..n.min(MAX_HEADER_BYTES + 64)).step_by(97));
    lens.extend([4095, 4096, 4097, MAX_HEADER_BYTES - 1, MAX_HEADER_BYTES, MAX_HEADER_BYTES + 1].into_iter().filter(|&l| l <= n));
    lens.push(n);
    for p in lens { verdict(p); }
    // ★ How the converter actually asks: from nothing, following `More`.
    // (`next_header_read`: at most four reads.)
    let (mut p, mut reads) = (0usize, 0);
    let last = loop {
        match verdict(p) {
            Header::More(m) => { reads += 1; p = next_header_read(p, m).min(n) }
            h => break h,
        }
        assert!(reads <= 4, "{reads} reads and still More on a {n}-byte {kind}");
    };
    let outcome = match last {
        Header::Sized(_) => "sized",
        Header::Unreadable(why) if why.contains("cap") || why.contains("budget") || why.contains("zero") => "refused-dims",
        Header::Unreadable(why) if why.contains("64 KiB") => "past-cap",
        Header::Unreadable(why) if why.contains("ends inside") => "truncated",
        Header::Unreadable(_) => "unreadable",
        Header::More(_) => unreachable!(),
    };
    *reached.entry((kind, outcome)).or_default() += 1;
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 { self.0 ^= self.0 << 13; self.0 ^= self.0 >> 7; self.0 ^= self.0 << 17; self.0 }
    fn below(&mut self, n: usize) -> usize { (self.next() % n.max(1) as u64) as usize }
}

fn mutate(r: &mut Rng, seed: &[u8]) -> Vec<u8> {
    let mut v = seed.to_vec();
    for _ in 0..1 + r.below(4) {
        match r.below(8) {
            0 if !v.is_empty() => { let i = r.below(v.len()); v[i] = r.next() as u8 }
            1 if !v.is_empty() => { let i = r.below(v.len()); v[i] ^= 1 << r.below(8) }
            2 => { let at = r.below(v.len() + 1); v.truncate(at) }
            3 if !v.is_empty() => {
                // A field-sized integer at a header offset: lengths and dimensions.
                let i = r.below(v.len().min(48));
                let val = [0u8, 1, 2, 0x7F, 0x80, 0xFF][r.below(6)];
                v[i] = val;
            }
            4 => { let at = r.below(v.len() + 1); let k = r.below(64); v.splice(at..at, std::iter::repeat(0xFF).take(k)); }
            5 if v.len() > 4 => { let i = r.below(v.len() - 2); v.drain(i..i + 1 + r.below(2.min(v.len() - i - 1))); }
            6 => { let k = r.below(70_000); v.extend(std::iter::repeat(r.next() as u8).take(k)); }
            _ => { let at = r.below(v.len().min(64) + 1); v.splice(at..at, [0xFF, 0xC0 + r.below(16) as u8]); }
        }
    }
    v
}

fn seeds() -> Vec<Vec<u8>> {
    let mut s = vec![
        png(640, 480), png(1, 1), png(16_384, 4_096), png(16_385, 2), png(0, 5), png(10_000, 10_000),
        gif(120, 60), gif(0, 0), jpeg(800, 600, 0), jpeg(800, 600, 5_000), jpeg(4_000, 3_000, 20_000),
        jpeg(64, 64, 65_000), webp_x(1_200, 800), webp_l(300, 200), webp_lossy(500, 250),
        svg("width=\"120\" height=\"40\"", 0), svg("viewBox=\"0 0 24 24\"", 0), svg("width=\"100%\"", 0),
        svg("width=\"300\" height=\"150\"", 400), b"<html>not an image</html>".to_vec(), vec![], vec![0xFF, 0xD8],
    ];
    // A JPEG whose frame is past the cap: two maximal APP segments, then one more.
    let mut far = vec![0xFF, 0xD8];
    for _ in 0..2 { far.extend([0xFF, 0xE1, 0xFF, 0xFF]); far.extend(std::iter::repeat(0).take(0xFFFD)); }
    far.extend(&jpeg(10, 10, 0)[2..]);
    s.push(far);
    s
}

fn corpus() -> Vec<Vec<u8>> {
    let Ok(dirs) = std::env::var("NAVIGATOR_HEADER_CORPUS") else { return vec![] };
    let mut v = vec![];
    for d in dirs.split(':') {
        for e in std::fs::read_dir(d).expect("NAVIGATOR_HEADER_CORPUS directory").flatten() {
            if let Ok(b) = std::fs::read(e.path()) { v.push(b) }
        }
    }
    v
}

#[test]
fn every_prefix_agrees_with_the_whole_image_or_declines() {
    let mut reached = BTreeMap::new();
    let seeds = seeds();
    for s in &seeds { check(s, &mut reached) }
    let real = corpus();
    for b in &real { check(b, &mut reached) }
    let iters: usize = std::env::var("NAVIGATOR_HEADER_FUZZ_ITERS").ok().and_then(|v| v.parse().ok()).unwrap_or(4_000);
    let mut r = Rng(0x9E37_79B9_7F4A_7C15);
    let pool: Vec<&Vec<u8>> = seeds.iter().chain(real.iter()).collect();
    for _ in 0..iters {
        let s = pool[r.below(pool.len())];
        let m = mutate(&mut r, s);
        check(&m, &mut reached);
    }
    eprintln!("header_size reached ({} seeds, {} real, {iters} mutants): {reached:?}", seeds.len(), real.len());
    // ★ What the run must have exercised, or it tested nothing there.
    for want in [("png", "sized"), ("png", "refused-dims"), ("png", "truncated"), ("png", "unreadable"),
                 ("jpeg", "sized"), ("jpeg", "past-cap"), ("jpeg", "truncated"), ("jpeg", "unreadable"),
                 ("gif", "sized"), ("gif", "refused-dims"), ("webp", "sized"), ("webp", "unreadable"),
                 ("svg", "sized"), ("other", "unreadable")] {
        assert!(reached.get(&want).copied().unwrap_or(0) > 0, "the fuzz run never reached {want:?}: {reached:?}");
    }
}

#[test]
fn what_each_format_needs() {
    assert_eq!(header_size(&png(640, 480)[..20], false), Header::More(32));
    assert_eq!(header_size(&png(640, 480)[..33], false), Header::Sized(Natural::sized(640, 480)));
    let j = jpeg(800, 600, 5_000);
    assert_eq!(header_size(&j[..4096], false), Header::More(2 + 2 + 5_002 + 9));
    assert_eq!(header_size(&j[..5_015], false), Header::Sized(Natural::sized(800, 600)));
    let s = svg("width=\"120\" height=\"40\"", 0);
    assert_eq!(header_size(&s[..40], false), Header::More(4096));
    assert!(matches!(header_size(&s, true), Header::Sized(Natural { width: Some(120), height: Some(40), .. })));
    assert_eq!(header_size(&png(16_385, 2), true), Header::Unreadable("over the dimension cap"));
    assert_eq!(header_size(&png(10_000, 10_000), true), Header::Unreadable("over the decode budget"));
    assert_eq!(header_size(&png(0, 5), true), Header::Unreadable("a zero dimension"));
    assert_eq!(header_size(b"<html>x</html>", true), Header::Unreadable("not an image read here"));
    assert_eq!(header_size(&[0xFF, 0xD8, 0xFF, 0xE1], true), Header::Unreadable("the image ends inside its header"));
}
