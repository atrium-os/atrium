//! Fixtures are the repo's own OFL fonts, re-encoded here into WOFF and WOFF2
//! so every transport is tested without committing third-party files.
//!
//! - IBM Plex Mono Regular — static, TrueType-hinted, carries `DSIG` and `meta`
//! - IBM Plex Sans         — VARIABLE (`wght`), hinted, `cvar` included
//! - Phosphor              — unhinted icon font

use navigator_fonts::{canonicalize, Declared, Format, Refusal, MAX_FONT_BYTES};

fn font(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/../fonts/{name}", env!("CARGO_MANIFEST_DIR"))).expect("fixture font")
}
fn plex_mono() -> Vec<u8> { font("ibm-plex/IBMPlexMono-Regular.ttf") }
fn plex_sans() -> Vec<u8> { font("ibm-plex/IBMPlexSans.ttf") }
fn phosphor() -> Vec<u8> { font("phosphor/Phosphor.ttf") }
fn w(weight: u16) -> Declared { Declared { weight } }

// ---- minimal encoders, so each transport is exercised on a known font -------

/// (tag, bytes) of every table in a bare sfnt, in directory order.
fn tables(sfnt: &[u8]) -> (u32, Vec<([u8; 4], Vec<u8>)>) {
    let be32 = |i: usize| u32::from_be_bytes(sfnt[i..i + 4].try_into().unwrap());
    let n = u16::from_be_bytes([sfnt[4], sfnt[5]]) as usize;
    let v = (0..n).map(|i| {
        let r = 12 + 16 * i;
        let (off, len) = (be32(r + 8) as usize, be32(r + 12) as usize);
        (sfnt[r..r + 4].try_into().unwrap(), sfnt[off..off + len].to_vec())
    }).collect();
    (be32(0), v)
}

fn woff1(sfnt: &[u8]) -> Vec<u8> {
    let (flavor, ts) = tables(sfnt);
    let mut dir = vec![]; let mut data = vec![];
    let base = 44 + 20 * ts.len();
    for (tag, t) in &ts {
        let z = miniz_oxide::deflate::compress_to_vec_zlib(t, 6);
        let body = if z.len() < t.len() { z } else { t.clone() };
        dir.extend_from_slice(tag);
        dir.extend_from_slice(&((base + data.len()) as u32).to_be_bytes());
        dir.extend_from_slice(&(body.len() as u32).to_be_bytes());
        dir.extend_from_slice(&(t.len() as u32).to_be_bytes());
        dir.extend_from_slice(&0u32.to_be_bytes());
        data.extend_from_slice(&body);
        while data.len() % 4 != 0 { data.push(0) }
    }
    let mut h = vec![];
    h.extend_from_slice(b"wOFF"); h.extend_from_slice(&flavor.to_be_bytes());
    h.extend_from_slice(&((base + data.len()) as u32).to_be_bytes());
    h.extend_from_slice(&(ts.len() as u16).to_be_bytes()); h.extend_from_slice(&0u16.to_be_bytes());
    h.extend_from_slice(&(sfnt.len() as u32).to_be_bytes());
    h.extend_from_slice(&[0, 1, 0, 0]); h.extend_from_slice(&[0; 20]);
    [h, dir, data].concat()
}

/// WOFF2 with the NULL transform everywhere, and every table named by an
/// EXPLICIT tag (index 63) — the directory form that once desynced preflight.
fn woff2(sfnt: &[u8]) -> Vec<u8> {
    woff2_raw(sfnt, None)
}
fn woff2_raw(sfnt: &[u8], replace_block: Option<Vec<u8>>) -> Vec<u8> {
    let (flavor, ts) = tables(sfnt);
    let mut dir = vec![]; let mut stream = vec![];
    for (tag, t) in &ts {
        let null = if tag == b"glyf" || tag == b"loca" { 3u8 << 6 } else { 0 };
        dir.push(0x3f | null);
        dir.extend_from_slice(tag);
        dir.extend_from_slice(&base128(t.len() as u32));
        stream.extend_from_slice(t);
    }
    let block = replace_block.unwrap_or_else(|| brotli_compress(&stream));
    let mut h = vec![];
    h.extend_from_slice(b"wOF2"); h.extend_from_slice(&flavor.to_be_bytes());
    let len = 48 + dir.len() + block.len();
    h.extend_from_slice(&(len as u32).to_be_bytes());
    h.extend_from_slice(&(ts.len() as u16).to_be_bytes()); h.extend_from_slice(&0u16.to_be_bytes());
    h.extend_from_slice(&(sfnt.len() as u32).to_be_bytes());
    h.extend_from_slice(&(block.len() as u32).to_be_bytes());
    h.extend_from_slice(&[0, 1, 0, 0]); h.extend_from_slice(&[0; 20]);
    [h, dir, block].concat()
}
fn base128(mut v: u32) -> Vec<u8> {
    let mut out = vec![(v & 0x7f) as u8]; v >>= 7;
    while v > 0 { out.insert(0, 0x80 | (v & 0x7f) as u8); v >>= 7 }
    out
}
fn brotli_compress(b: &[u8]) -> Vec<u8> {
    let mut out = vec![];
    brotli::BrotliCompress(&mut &b[..], &mut out, &brotli::enc::BrotliEncoderParams::default()).unwrap();
    out
}

fn table_tags(sfnt: &[u8]) -> Vec<String> {
    tables(sfnt).1.into_iter().map(|(t, _)| String::from_utf8_lossy(&t).into_owned()).collect()
}

// ---- the canonical form ------------------------------------------------------

#[test]
fn a_static_hinted_font_loses_its_hinting_and_extra_tables_and_nothing_else() {
    let c = canonicalize(&plex_mono(), w(400)).unwrap();
    assert_eq!(c.source, Format::TrueType);
    assert!(c.pinned.is_empty());
    for gone in ["fpgm", "prep", "cvt ", "gasp", "DSIG", "meta"] {
        assert!(c.dropped.iter().any(|d| d == gone), "{gone} should be dropped: {:?}", c.dropped);
    }
    let kept = table_tags(&c.sfnt);
    for keep in ["cmap", "glyf", "loca", "GSUB", "GPOS", "hmtx", "name"] {
        assert!(kept.iter().any(|k| k == keep), "{keep} should be kept: {kept:?}");
    }
    assert!(c.instruction_bytes_stripped > 0, "a hinted font's glyph instructions must be counted");
    assert!(c.sfnt.len() < plex_mono().len(), "stripping can only shrink it");
}

#[test]
fn an_unhinted_font_has_nothing_to_strip() {
    let c = canonicalize(&phosphor(), w(400)).unwrap();
    assert_eq!(c.instruction_bytes_stripped, 0);
}

#[test]
fn the_same_font_through_every_transport_has_ONE_address() {
    // The point of one canonical form: the store holds one copy of a face,
    // whichever encoding a site happened to serve it in.
    for src in [plex_mono(), phosphor()] {
        let a = canonicalize(&src, w(400)).unwrap();
        let b = canonicalize(&woff1(&src), w(400)).unwrap();
        let c = canonicalize(&woff2(&src), w(400)).unwrap();
        assert_eq!((b.source, c.source), (Format::Woff, Format::Woff2));
        assert_eq!(a.address, b.address, "WOFF must canonicalize to the TTF's bytes");
        assert_eq!(a.address, c.address, "WOFF2 must canonicalize to the TTF's bytes");
    }
}

#[test]
fn canonicalizing_is_deterministic_and_idempotent() {
    let a = canonicalize(&plex_mono(), w(400)).unwrap();
    assert_eq!(a, canonicalize(&plex_mono(), w(400)).unwrap());
    // A canonical font is already canonical.
    let again = canonicalize(&a.sfnt, w(400)).unwrap();
    assert_eq!(again.address, a.address);
    assert_eq!(again.instruction_bytes_stripped, 0);
}

// ---- pinning -----------------------------------------------------------------

#[test]
fn a_variable_font_is_pinned_at_the_declared_weight() {
    let regular = canonicalize(&plex_sans(), w(400)).unwrap();
    let bold = canonicalize(&plex_sans(), w(700)).unwrap();
    assert_eq!(regular.pinned.iter().find(|(t, _)| t == "wght").map(|p| p.1), Some(400.0));
    assert_eq!(bold.pinned.iter().find(|(t, _)| t == "wght").map(|p| p.1), Some(700.0));
    // The control: pinning must actually change the font.
    assert_ne!(regular.address, bold.address);
    for gone in ["fvar", "gvar", "HVAR", "MVAR", "STAT", "avar", "cvar"] {
        assert!(regular.dropped.iter().any(|d| d == gone), "{gone}: {:?}", regular.dropped);
    }
    let f = ttf_parser::Face::parse(&regular.sfnt, 0).unwrap();
    assert!(!f.is_variable());
}

#[test]
fn a_weight_outside_the_axis_is_clamped_not_refused() {
    let heavy = canonicalize(&plex_sans(), w(900)).unwrap();
    let src = plex_sans();
    let f = ttf_parser::Face::parse(&src, 0).unwrap();
    let max = f.variation_axes().into_iter().find(|a| &a.tag.to_bytes() == b"wght").unwrap().max_value;
    assert_eq!(heavy.pinned.iter().find(|(t, _)| t == "wght").unwrap().1, 900f32.min(max));
}

// ---- refusals ----------------------------------------------------------------

#[test]
fn eot_and_unknown_bytes_are_refused_as_unknown_formats() {
    // EOT begins with its own little-endian size; there is no magic to accept.
    let mut eot = vec![0u8; 64];
    eot[..4].copy_from_slice(&64u32.to_le_bytes());
    assert_eq!(canonicalize(&eot, w(400)), Err(Refusal::UnknownFormat));
    assert_eq!(canonicalize(b"<html>not a font</html>", w(400)), Err(Refusal::UnknownFormat));
    assert_eq!(canonicalize(b"", w(400)), Err(Refusal::UnknownFormat));
}

#[test]
fn a_collection_is_refused() {
    let mut ttc = b"ttcf".to_vec(); ttc.extend_from_slice(&[0; 60]);
    assert_eq!(canonicalize(&ttc, w(400)), Err(Refusal::Collection));
}

#[test]
fn a_woff2_decompression_bomb_is_refused_before_the_decoder_runs() {
    // A real font's directory, but a Brotli block that inflates to 64 MiB of
    // zeros — a few kilobytes on the wire.
    let bomb = brotli_compress(&vec![0u8; 64 * 1024 * 1024]);
    let f = woff2_raw(&phosphor(), Some(bomb.clone()));
    assert!(f.len() < 64 * 1024, "the bomb is small on the wire: {}", f.len());
    assert_eq!(canonicalize(&f, w(400)), Err(Refusal::TooLarge { what: "woff2 brotli block" }));
    // Control: the same construction just UNDER the limit is not refused for
    // size — so the refusal above is caused by the size, not the construction.
    let small = brotli_compress(&vec![0u8; MAX_FONT_BYTES - 1]);
    let r = canonicalize(&woff2_raw(&phosphor(), Some(small)), w(400));
    assert!(!matches!(r, Err(Refusal::TooLarge { .. })), "{r:?}");
}

#[test]
fn a_woff_table_that_inflates_past_its_declared_size_is_refused() {
    let mut f = woff1(&phosphor());
    // Rewrite the first compressed table to a zlib stream of 16 MiB of zeros,
    // keeping its declared original length.
    let z = miniz_oxide::deflate::compress_to_vec_zlib(&vec![0u8; 16 * 1024 * 1024], 6);
    let n = u16::from_be_bytes([f[12], f[13]]) as usize;
    let i = (0..n).find(|&i| {
        let e = 44 + 20 * i;
        u32::from_be_bytes(f[e + 8..e + 12].try_into().unwrap()) < u32::from_be_bytes(f[e + 12..e + 16].try_into().unwrap())
    }).expect("a compressed table");
    let e = 44 + 20 * i;
    let off = f.len() as u32;
    f[e + 4..e + 8].copy_from_slice(&off.to_be_bytes());
    f[e + 8..e + 12].copy_from_slice(&(z.len() as u32).to_be_bytes());
    f.extend_from_slice(&z);
    let r = canonicalize(&f, w(400));
    assert!(matches!(r, Err(Refusal::TooLarge { .. }) | Err(Refusal::SizeMismatch { .. })), "{r:?}");
}

#[test]
fn damaged_fonts_are_refused_never_passed_through_and_never_panic() {
    // Truncations and byte flips across the whole file. Every result must be a
    // canonical font that re-validated, or a refusal — and not a panic, which
    // `canonicalize` reports as DecoderPanicked when unwinding is available.
    let src = plex_mono();
    let mut outcomes = std::collections::BTreeMap::<String, u32>::new();
    let mut state = 0x2545F4914F6CDD1Du64;
    for round in 0..300 {
        let mut f = src.clone();
        if round % 3 == 0 {
            f.truncate(src.len() * round / 300);
        } else {
            for _ in 0..8 {
                state ^= state << 13; state ^= state >> 7; state ^= state << 17;
                let i = (state as usize) % f.len();
                f[i] ^= (state >> 32) as u8 | 1;
            }
        }
        let k = match canonicalize(&f, w(400)) {
            Ok(c) => { assert!(ttf_parser::Face::parse(&c.sfnt, 0).is_ok()); "ok".to_string() }
            Err(Refusal::DecoderPanicked) => "PANIC".to_string(),
            Err(r) => format!("{r:?}").split(['(', ' ', '{']).next().unwrap().to_string(),
        };
        *outcomes.entry(k).or_default() += 1;
    }
    eprintln!("damage outcomes: {outcomes:?}");
    assert_eq!(outcomes.get("PANIC"), None, "the decoder panicked on damaged input: {outcomes:?}");
    assert!(outcomes.len() > 1, "the damage must actually reach the refusal paths: {outcomes:?}");
}
