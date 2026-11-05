//! ★★ IMAGES DECODE IN THE DOCUMENT'S WORKER, AND THE BROKER CHECKS WHAT
//! COMES BACK (threat model §2.4).
//!
//! The decoder (src/image.rs): each format decodes to the pixels it was
//! encoded from; the output is RGBA8 with straight alpha; the address is
//! checked; header dimensions are refused before a frame is allocated.
//!
//! The path (src/navigatord.rs, src/jailed.rs): `Request::Image` through
//! both hosts — the address must be one the recording names, the bytes come
//! from the broker's blob store, the budget binds across images, and a
//! worker that answers with the wrong shape is retired.

mod common;
use common::images::*;
use navigator_backend::image::{decode, Ceilings, DecodeError, Format, MAX_ENCODED_IMAGE_BYTES};
use navigator_backend::jailed::{Confinement, JailedHost, WorkerConfig};
use navigator_backend::navigatord::{DocumentHost, Event, InProcessHost, Navigatord, Request};
use navigator_backend::session::{SessionId, SessionLimits, Status};
use navigator_backend::store::{MemoryStore, Store};
use std::time::Duration;

const WORKER: &str = env!("CARGO_BIN_EXE_navigator-worker");

fn dec(bytes: &[u8]) -> Result<navigator_backend::image::Pixels, DecodeError> {
    decode(&address(bytes), bytes, Ceilings::default())
}

#[test]
fn png_decodes_exactly_with_straight_alpha() {
    let px = pattern(5, 3);
    let out = dec(&png_rgba(5, 3, &px)).expect("decodes");
    assert_eq!((out.width, out.height), (5, 3));
    assert_eq!(out.rgba, px, "a lossless PNG must decode to the pixels it was encoded from");
    // ★ Straight alpha: the half-transparent pixel keeps its full colour.
    // Premultiplied, its red would be halved.
    assert_eq!(&out.rgba[4..8], &px[4..8]);
    assert_eq!(out.rgba[7], 128);
}

#[test]
fn every_png_colour_type_becomes_rgba() {
    use png::{BitDepth, ColorType};
    // Grey: one sample, repeated, alpha 255.
    let g = dec(&png_typed(2, 1, ColorType::Grayscale, BitDepth::Eight, &[10, 200])).unwrap();
    assert_eq!(g.rgba, [10, 10, 10, 255, 200, 200, 200, 255]);
    // Grey + alpha.
    let ga = dec(&png_typed(1, 1, ColorType::GrayscaleAlpha, BitDepth::Eight, &[9, 77])).unwrap();
    assert_eq!(ga.rgba, [9, 9, 9, 77]);
    // RGB.
    let rgb = dec(&png_typed(1, 1, ColorType::Rgb, BitDepth::Eight, &[1, 2, 3])).unwrap();
    assert_eq!(rgb.rgba, [1, 2, 3, 255]);
    // 16-bit: the high byte.
    let deep = dec(&png_typed(1, 1, ColorType::Rgb, BitDepth::Sixteen, &[0xAB, 0xCD, 0x12, 0x34, 0xFF, 0x00])).unwrap();
    assert_eq!(deep.rgba, [0xAB, 0x12, 0xFF, 255]);
    // ★ Indexed, 1 bit per pixel: palette entries, not packed indices (the
    // bug the raster tool once had on BBC's placeholder).
    let ix = dec(&png_typed(2, 1, ColorType::Indexed, BitDepth::One, &[0b0100_0000])).unwrap();
    assert_eq!(ix.rgba, [255, 0, 0, 255, 0, 0, 255, 255]);
}

#[test]
fn jpeg_decodes_baseline_and_progressive_close_to_the_source() {
    let (w, h) = (16u16, 16u16);
    let px = pattern(w as u32, h as u32);
    for progressive in [false, true] {
        let out = dec(&jpeg(w, h, &px, progressive)).unwrap_or_else(|e| panic!("progressive={progressive}: {e}"));
        assert_eq!((out.width, out.height), (16, 16));
        assert_eq!(out.rgba.len(), 16 * 16 * 4);
        // Lossy: compare with a tolerance, and check every alpha is opaque.
        let worst = out.rgba.chunks_exact(4).zip(px.chunks_exact(4))
            .flat_map(|(a, b)| (0..3).map(move |i| (a[i] as i32 - b[i] as i32).abs()))
            .max().unwrap();
        assert!(worst <= 48, "progressive={progressive}: a channel is off by {worst}");
        assert!(out.rgba.chunks_exact(4).all(|p| p[3] == 255));
        // ★ The corners differ in the source, and must in the decode: a
        // transposed or flipped image would pass a mean-error check.
        let at = |x: usize, y: usize| out.rgba[(y * 16 + x) * 4];
        assert!(at(15, 0) > 200 && at(0, 0) < 40, "red does not ramp left to right");
        let g = |x: usize, y: usize| out.rgba[(y * 16 + x) * 4 + 1];
        assert!(g(0, 15) > 200 && g(0, 0) < 40, "green does not ramp top to bottom");
    }
}

#[test]
fn webp_lossless_decodes_exactly() {
    let px = pattern(7, 4);
    let out = dec(&webp_lossless(7, 4, &px)).expect("decodes");
    assert_eq!((out.width, out.height), (7, 4));
    assert_eq!(out.rgba, px);
}

#[test]
fn a_gif_frame_lands_at_its_offset_and_is_clipped_to_the_screen() {
    // A 4×3 screen with a 3×2 frame at (2, 2): it overhangs right and below.
    let out = dec(&gif(4, 3, 2, 2, 3, 2)).expect("decodes");
    assert_eq!((out.width, out.height), (4, 3));
    let at = |x: usize, y: usize| &out.rgba[(y * 4 + x) * 4..(y * 4 + x) * 4 + 4];
    // Outside the frame: transparent.
    assert_eq!(at(0, 0), [0, 0, 0, 0]);
    assert_eq!(at(1, 2), [0, 0, 0, 0]);
    // The frame's first pixel is its transparent index; the next is blue.
    assert_eq!(at(2, 2)[3], 0);
    assert_eq!(at(3, 2), [0, 0, 255, 255]);
}

#[test]
fn the_address_is_checked_by_the_decoder_itself() {
    let b = png_rgba(2, 2, &pattern(2, 2));
    let other = png_rgba(3, 2, &pattern(3, 2));
    assert_eq!(decode(&address(&other), &b, Ceilings::default()), Err(DecodeError::AddressMismatch));
    assert_eq!(decode("blake3:nothex", &b, Ceilings::default()), Err(DecodeError::AddressMismatch));
    assert_eq!(decode(&address(&b).to_uppercase(), &b, Ceilings::default()), Err(DecodeError::AddressMismatch));
}

#[test]
fn what_is_not_a_raster_is_named_and_refused() {
    for (bytes, kind) in [
        (&b"<svg xmlns='http://www.w3.org/2000/svg'/>"[..], "svg"),
        (b"<!DOCTYPE html><html>", "html"),
        (b"\0\0\0\x1cftypavif\0\0\0\0", "avif"),
        (b"BM\0\0", "bmp"),
        (b"", "empty"),
        (b"\x13\x37", "unknown"),
    ] {
        assert_eq!(dec(bytes), Err(DecodeError::NotARaster { kind }), "{kind}");
    }
}

#[test]
fn an_encoded_image_over_the_limit_is_refused_before_it_is_hashed() {
    let big = vec![0u8; MAX_ENCODED_IMAGE_BYTES + 1];
    assert_eq!(decode("blake3:x", &big, Ceilings::default()),
        Err(DecodeError::EncodedTooLarge { len: MAX_ENCODED_IMAGE_BYTES + 1 }));
}

/// Header-lie helpers: a valid small image whose header is rewritten.
pub fn png_claiming(w: u32, h: u32) -> Vec<u8> {
    let mut b = png_rgba(2, 2, &pattern(2, 2));
    b[16..20].copy_from_slice(&w.to_be_bytes());
    b[20..24].copy_from_slice(&h.to_be_bytes());
    b
}

#[test]
fn header_dimensions_over_the_ceiling_are_refused_on_the_header() {
    let c = Ceilings::default();
    assert!(matches!(dec(&png_claiming(16_385, 1)), Err(DecodeError::TooLarge { format: Format::Png, width: 16_385, .. })));
    assert!(matches!(dec(&png_claiming(0, 5)), Err(DecodeError::Empty { format: Format::Png })));
    // 16,384² is within the per-axis cap and 1 GiB decoded: the frame
    // ceiling refuses it, painted small or not.
    assert!(matches!(dec(&png_claiming(16_384, 16_384)),
        Err(DecodeError::FrameTooLarge { format: Format::Png, needed, limit }) if needed == 1 << 30 && limit == c.frame));
    let b = png_claiming(16_384, 16_384);
    assert!(matches!(decode(&address(&b), &b, Ceilings { target: Some((10, 10)), ..c }),
        Err(DecodeError::FrameTooLarge { .. })));
    // And a document with only a little budget left refuses even a small one.
    let tiny = Ceilings { budget: 15, ..c };
    let b = png_rgba(2, 2, &pattern(2, 2));
    assert!(matches!(decode(&address(&b), &b, tiny), Err(DecodeError::OverBudget { needed: 16, left: 15, .. })));
}

#[test]
fn a_header_that_lies_small_does_not_decode_more_than_it_claims() {
    // The IHDR says 1×1; the data is 2×2. The decoder must not produce more
    // than the header's frame (it errors, or decodes one pixel) — and the
    // CRC no longer matches, so a strict decoder refuses.
    match dec(&png_claiming(1, 1)) {
        Ok(p) => assert_eq!((p.width, p.height, p.rgba.len()), (1, 1, 4)),
        Err(e) => assert!(matches!(e, DecodeError::Header { .. } | DecodeError::Corrupt { .. }), "{e:?}"),
    }
}

// ── Decoded to the painted size ────────────────────────────────────────────

fn dec_at(bytes: &[u8], target: (u32, u32)) -> Result<navigator_backend::image::Pixels, DecodeError> {
    decode(&address(bytes), bytes, Ceilings { target: Some(target), ..Ceilings::default() })
}

/// The 2×2 block means of an opaque image, computed here independently of
/// the resampler: what an exact area average at half size must produce.
fn halved(px: &[u8], w: u32, h: u32) -> Vec<u8> {
    let mut out = vec![];
    for y in 0..h / 2 {
        for x in 0..w / 2 {
            for c in 0..4 {
                let at = |dx: u32, dy: u32| px[(((y * 2 + dy) * w + x * 2 + dx) * 4 + c as u32) as usize] as u32;
                out.push(((at(0, 0) + at(1, 0) + at(0, 1) + at(1, 1) + 2) / 4) as u8);
            }
        }
    }
    out
}

#[test]
fn an_image_decodes_to_its_painted_size_as_an_area_average() {
    let mut px = pattern(8, 6);
    for p in px.chunks_exact_mut(4) { p[3] = 255 }
    let b = png_rgba(8, 6, &px);
    let out = dec_at(&b, (4, 3)).unwrap();
    assert_eq!((out.width, out.height), (4, 3));
    assert_eq!(out.rgba, halved(&px, 8, 6), "the area average of each 2x2 block");
    // ★ Every decoder's output goes through the same reduction. WebP is
    // lossless, so its result is exact too; JPEG (baseline and progressive)
    // and GIF are checked for shape.
    let w = dec_at(&webp_lossless(8, 6, &px), (4, 3)).unwrap();
    assert_eq!(w.rgba, halved(&px, 8, 6));
    for b in [jpeg(16, 16, &pattern(16, 16), false), jpeg(16, 16, &pattern(16, 16), true), gif(16, 16, 0, 0, 16, 16)] {
        assert_eq!(dec_at(&b, (4, 4)).map(|p| (p.width, p.height)), Ok((4, 4)));
    }
}

#[test]
fn a_painted_size_never_upscales_and_keeps_the_aspect() {
    let b = png_rgba(8, 6, &pattern(8, 6));
    // Larger than it is: the natural size, untouched.
    let big = dec_at(&b, (100, 100)).unwrap();
    assert_eq!((big.width, big.height, big.rgba.clone()), (8, 6, pattern(8, 6)));
    // One axis larger: still the natural size, since that axis needs it all.
    assert_eq!(dec_at(&b, (8, 1)).map(|p| (p.width, p.height)), Ok((8, 6)));
    // ★ Painted 4x1 (a `cover` crop or a stretched fill): one scale, the
    // LARGER the box asks for, so the wide axis is not under-sampled.
    assert_eq!(dec_at(&b, (4, 1)).map(|p| (p.width, p.height)), Ok((4, 3)));
    // Rounded up, so the box is covered: 8x6 at 3 wide is 3 x 2.25 -> 3x3.
    assert_eq!(dec_at(&b, (3, 1)).map(|p| (p.width, p.height)), Ok((3, 3)));
}

/// ★ Straight alpha is filtered PREMULTIPLIED: an invisible pixel's colour
/// must not bleed into a visible neighbour (the dark or coloured fringe).
#[test]
fn a_reduction_does_not_bleed_invisible_colour() {
    // Left column: fully transparent RED; right: opaque blue.
    let px = [255, 0, 0, 0, 0, 0, 255, 255, 255, 0, 0, 0, 0, 0, 255, 255];
    let b = png_rgba(2, 2, &px);
    let out = dec_at(&b, (1, 1)).unwrap();
    assert_eq!(out.rgba, [0, 0, 255, 128], "half coverage, and the colour is all blue");
}

#[test]
fn the_budget_is_charged_at_the_painted_size() {
    let c = Ceilings { budget: 64 * 64 * 4, target: Some((64, 64)), ..Ceilings::default() };
    let px = vec![7u8; 512 * 512 * 4];
    let b = png_rgba(512, 512, &px);
    // 1 MiB natural, 16 KiB painted: fits a 16 KiB budget at its painted size.
    let p = decode(&address(&b), &b, c).unwrap();
    assert_eq!((p.width, p.height, p.rgba.len()), (64, 64, 64 * 64 * 4));
    // And one byte less refuses it — on the header, at the painted size.
    assert!(matches!(decode(&address(&b), &b, Ceilings { budget: 64 * 64 * 4 - 1, ..c }),
        Err(DecodeError::OverBudget { needed, .. }) if needed == 64 * 64 * 4));
}

// ── Through the broker ─────────────────────────────────────────────────────

struct Fixture { recording: Vec<u8>, a: String, b: String, unnamed: String, blobs: MemoryStore }

/// Two images the recording names, one stored blob it does not, all in the
/// blob store under their hex (the converter's `PRERENDER_BLOB_DIR` shape).
fn fixture() -> Fixture {
    let ia = png_rgba(3, 2, &pattern(3, 2));
    let ib = jpeg(8, 8, &pattern(8, 8), true);
    let other = webp_lossless(2, 2, &pattern(2, 2));
    let mut blobs = MemoryStore::default();
    for x in [&ia, &ib, &other] { blobs.put(x).unwrap(); }
    let (a, b, unnamed) = (address(&ia), address(&ib), address(&other));
    Fixture { recording: recording_with_images(&[a.clone(), b.clone()]), a, b, unnamed, blobs }
}

fn open<H: DocumentHost>(n: &mut Navigatord<H>, rec: &[u8]) -> SessionId {
    match n.handle(Request::OpenSession { recording: rec.to_vec() }).pop() {
        Some(Event::SessionOpened { session, .. }) => session,
        other => panic!("did not open: {other:?}"),
    }
}

fn image<H: DocumentHost>(n: &mut Navigatord<H>, session: SessionId, address: &str) -> Event {
    n.handle(Request::Image { session, address: address.into(), painted: None }).pop().expect("an answer")
}

/// An image requested at a painted size, in CSS px (NSG's 1/64 units are
/// applied here).
fn image_at<H: DocumentHost>(n: &mut Navigatord<H>, session: SessionId, address: &str, css: (u32, u32)) -> Event {
    n.handle(Request::Image { session, address: address.into(), painted: Some((css.0 * 64, css.1 * 64)) }).pop().expect("an answer")
}

fn blocked(e: Event, has: &str) {
    match e {
        Event::Blocked { why, .. } => assert!(why.contains(has), "{why:?} does not say {has:?}"),
        other => panic!("not refused ({has}): {other:?}"),
    }
}

fn the_broker_serves_images<H: DocumentHost>(n: Navigatord<H>) {
    let f = fixture();
    let mut n = n.with_blobs(Box::new(f.blobs));
    n.tick(1_000);
    let s = open(&mut n, &f.recording);
    match image(&mut n, s, &f.a) {
        Event::Image { session, address, pixels } => {
            assert_eq!((session, address.as_str()), (s, f.a.as_str()));
            assert_eq!((pixels.width, pixels.height), (3, 2));
            assert_eq!(pixels.rgba, pattern(3, 2));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(n.decoded_bytes(s), 3 * 2 * 4);
    assert!(matches!(image(&mut n, s, &f.b), Event::Image { pixels, .. } if pixels.width == 8));
    assert_eq!(n.decoded_bytes(s), 24 + 8 * 8 * 4);
    // The same image again is not charged twice.
    assert!(matches!(image(&mut n, s, &f.a), Event::Image { .. }));
    assert_eq!(n.decoded_bytes(s), 24 + 256);

    // ★★ In the store, but not named by THIS recording: refused, so a
    // worker cannot pull another document's image out of a shared store.
    blocked(image(&mut n, s, &f.unnamed), "does not name");
    blocked(image(&mut n, s, "https://a.test/i0.png"), "not an image address");
    blocked(image(&mut n, s, &f.a.to_uppercase()), "not an image address");
    assert!(matches!(image(&mut n, 999, &f.a), Event::NoSuchSession { .. }));

    // Closing gives the budget back.
    n.handle(Request::Close { session: s });
    n.tick(2_000);
    assert_eq!(n.decoded_bytes(s), 0);
}

#[test]
fn the_in_process_broker_serves_images() {
    the_broker_serves_images(Navigatord::unconfined_for_testing(InProcessHost::default()));
}

/// ★ The same requests with the document in a worker process: the encoded
/// bytes cross the pipe to the worker, which decodes them; pixels cross back
/// and are checked twice (host, then broker).
#[test]
fn the_jailed_broker_serves_images() {
    let host = JailedHost::new(WorkerConfig::unconfined_for_testing(WORKER), SessionLimits::default());
    the_broker_serves_images(Navigatord::unconfined_for_testing(host));
}

#[test]
fn a_broker_without_a_blob_store_says_so() {
    let f = fixture();
    let mut n = Navigatord::unconfined_for_testing(InProcessHost::default());
    let s = open(&mut n, &f.recording);
    blocked(image(&mut n, s, &f.a), "no blob store");
}

/// A store that hands back the wrong bytes for one address.
struct Tampered(MemoryStore, String, Vec<u8>);
impl Store for Tampered {
    fn put(&mut self, b: &[u8]) -> Result<String, String> { self.0.put(b) }
    fn get(&self, a: &str) -> Result<Vec<u8>, String> {
        if a == self.1 { Ok(self.2.clone()) } else { self.0.get(a) }
    }
}

/// ★ A store that does not check its own addresses — a file replaced under
/// its name on a volume that does not verify — is caught by the decoder,
/// in the worker, because the worker does not take the broker's word.
#[test]
fn bytes_that_do_not_hash_to_the_address_are_refused_in_the_worker() {
    let f = fixture();
    let hex = f.a.strip_prefix("blake3:").unwrap().to_string();
    let swapped = png_rgba(3, 2, &[0u8; 24]);
    let host = JailedHost::new(WorkerConfig::unconfined_for_testing(WORKER), SessionLimits::default());
    let mut n = Navigatord::unconfined_for_testing(host).with_blobs(Box::new(Tampered(f.blobs, hex, swapped)));
    let s = open(&mut n, &f.recording);
    blocked(image(&mut n, s, &f.a), "do not hash");
    // An honest refusal: the worker stays.
    assert_eq!(n.host().status(s), Status::Open);
    assert!(matches!(image(&mut n, s, &f.b), Event::Image { .. }));
}

/// The document's decoded-bytes budget binds across images.
#[test]
fn the_budget_binds_across_images() {
    // A 4096×4096 PNG is 64 MiB decoded; four of them fill 256 MiB, and a
    // fifth distinct one is refused on its header.
    let imgs: Vec<Vec<u8>> = (0..5u8).map(|i| {
        let mut px = vec![0u8; 4096 * 4096 * 4];
        px[0] = i; // distinct bytes, distinct addresses
        png_rgba(4096, 4096, &px)
    }).collect();
    let addrs: Vec<String> = imgs.iter().map(|b| address(b)).collect();
    let mut blobs = MemoryStore::default();
    for b in &imgs { blobs.put(b).unwrap(); }
    let mut n = Navigatord::unconfined_for_testing(InProcessHost::default()).with_blobs(Box::new(blobs));
    let s = open(&mut n, &recording_with_images(&addrs));
    for a in &addrs[..4] { assert!(matches!(image(&mut n, s, a), Event::Image { .. })) }
    assert_eq!(n.decoded_bytes(s), 256 << 20);
    blocked(image(&mut n, s, &addrs[4]), "left in the document's budget");
    // Re-decoding one already charged still fits: it is not charged again.
    assert!(matches!(image(&mut n, s, &addrs[0]), Event::Image { .. }));
}

/// ★ The four-and-a-fifth images that were over the budget at their
/// natural size fit at their painted size, and the broker's device scale
/// sets what "painted size" means in pixels — through both hosts.
fn the_broker_decodes_to_the_painted_size<H: DocumentHost>(n: Navigatord<H>) {
    let imgs: Vec<Vec<u8>> = (0..5u8).map(|i| {
        let mut px = vec![0u8; 4096 * 4096 * 4];
        px[0] = i;
        png_rgba(4096, 4096, &px)
    }).collect();
    let addrs: Vec<String> = imgs.iter().map(|b| address(b)).collect();
    let mut blobs = MemoryStore::default();
    for b in &imgs { blobs.put(b).unwrap(); }
    // 2x: a 256x256 CSS box is 512x512 device pixels.
    let mut n = n.with_blobs(Box::new(blobs)).with_device_scale(128);
    let s = open(&mut n, &recording_with_images(&addrs));
    for a in &addrs {
        match image_at(&mut n, s, a, (256, 256)) {
            Event::Image { pixels, .. } => assert_eq!((pixels.width, pixels.height), (512, 512)),
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(n.decoded_bytes(s), 5 * 512 * 512 * 4, "charged at the painted size, not 5 x 64 MiB");
    // A new size (zoom) decodes again and REPLACES the charge.
    assert!(matches!(image_at(&mut n, s, &addrs[0], (128, 128)), Event::Image { pixels, .. } if pixels.width == 256));
    assert_eq!(n.decoded_bytes(s), 4 * 512 * 512 * 4 + 256 * 256 * 4);
    // Not painted means not painted.
    blocked(n.handle(Request::Image { session: s, address: addrs[1].clone(), painted: Some((0, 64)) }).pop().unwrap(), "zero dimension");
}

#[test]
fn the_in_process_broker_decodes_to_the_painted_size() {
    the_broker_decodes_to_the_painted_size(Navigatord::unconfined_for_testing(InProcessHost::default()));
}

#[test]
fn the_jailed_broker_decodes_to_the_painted_size() {
    let host = JailedHost::new(WorkerConfig::unconfined_for_testing(WORKER), SessionLimits::default());
    the_broker_decodes_to_the_painted_size(Navigatord::unconfined_for_testing(host));
}

#[test]
fn device_pixels_round_up_and_never_reach_zero() {
    let n = Navigatord::unconfined_for_testing(InProcessHost::default()).with_device_scale(96); // 1.5x
    assert_eq!(n.device_size((64 * 10, 64 * 3)), (15, 5)); // 4.5 rounds up
    assert_eq!(n.device_size((1, 1)), (1, 1));               // 1/64 px still covers a pixel
}

// ── A worker that lies ─────────────────────────────────────────────────────

/// A "worker" that answers OPEN with no triggers and then answers the image
/// request with `reply` — whatever was asked.
fn liar(reply: &str) -> JailedHost {
    let script = format!("printf 'OK 0\\n'; printf '{reply}'; cat >/dev/null");
    let cfg = WorkerConfig {
        worker: "unused".into(),
        confinement: Confinement::Launcher { program: "/bin/sh".into(), args: vec!["-c".into(), script, "sh".into()] },
        deadline: Duration::from_secs(5),
        shutdown_grace: Duration::from_secs(2),
    };
    JailedHost::new(cfg, SessionLimits::default())
}

/// ★★ The shapes a compromised worker could send back, each retired.
#[test]
fn a_worker_that_returns_a_bad_image_is_retired() {
    // An OK frame around a payload, its length counted here so the framing
    // is right and only the CONTENT lies.
    let ok = |payload: &str| format!("OK {}\\n{}", payload.len(), payload.replace('\n', "\\n"));
    for (reply, why) in [
        (ok("2 2\nAAAAAAA"), "16 bytes, the buffer is 7"),   // short buffer
        (ok("99999 1\nAAAA"), "over 16384"),                 // axis over the ceiling
        (ok("0 1\nAAA"), "zero dimension"),
        (ok("+1 1\nAAAA"), "not a dimension"),               // a second spelling
        (ok("AAAA"), "no header"),
        ("OK 999999999\\n".to_string(), "exceeds the limit"), // over the reply bound, before allocating
        ("IMG 0\\n".to_string(), "unknown reply"),
    ] {
        let reply = reply.as_str();
        let f = fixture();
        let mut n = Navigatord::unconfined_for_testing(liar(reply)).with_blobs(Box::new(f.blobs));
        let s = open(&mut n, &f.recording);
        blocked(image(&mut n, s, &f.a), why);
        assert_eq!(n.host().status(s), Status::Failed, "{reply:?}: the lying worker was not retired");
        assert_eq!(n.host().open_count(), 0);
    }
}

/// ★ A worker asked for a painted size that sends back more than it on
/// BOTH axes ignored it — a way to spend the document's budget the request
/// did not grant. Retired like any other lie.
#[test]
fn a_worker_that_ignores_the_painted_size_is_retired() {
    let payload = format!("2 2\n{}", "A".repeat(16));
    let reply = format!("OK {}\\n{}", payload.len(), payload.replace('\n', "\\n"));
    let f = fixture();
    let mut n = Navigatord::unconfined_for_testing(liar(&reply)).with_blobs(Box::new(f.blobs));
    let s = open(&mut n, &f.recording);
    blocked(image_at(&mut n, s, &f.a, (1, 1)), "over the painted size 1x1 on both axes");
    assert_eq!(n.host().status(s), Status::Failed);
}

/// And a worker cannot even be ASKED for a blob its recording does not
/// name: the broker refuses before the pipe.
#[test]
fn a_worker_is_never_sent_an_unnamed_blob() {
    let f = fixture();
    // This liar would accept anything; it is never asked.
    let mut n = Navigatord::unconfined_for_testing(liar("OK 0\\n")).with_blobs(Box::new(f.blobs));
    let s = open(&mut n, &f.recording);
    blocked(image(&mut n, s, &f.unnamed), "does not name");
    assert_eq!(n.host().status(s), Status::Open);
}
