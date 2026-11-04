//! compose.comp against its Rust model (src/compose.rs): on the GIR
//! interpreter (exact bounds on every access) and on Tier-2, over fuzzed
//! layer lists; and the per-window path end to end (each window rastered
//! into its own surface, then composed).

use fresco_compute::compose::{self, Compose, Layer};
use fresco_compute::fuzz::{compose_case, Rng};
use fresco_compute::*;

fn words(b: &[u8]) -> Vec<u32> {
    b.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

fn gir() -> atrium_gir::Function {
    let m = atrium_spv_frontend::translate(compose::COMPOSE_SPV).expect("frontend");
    let g = atrium_gir::build::build(&m, 0).expect("GIR build");
    atrium_gir::verify::verify(&g).expect("GIR verify");
    g
}

fn run_gir(g: &atrium_gir::Function, c: &Compose, heap: &[u32]) -> Result<Vec<u32>, String> {
    let [hdr, layers, lut] = c.buffers();
    let mut bufs: atrium_gir::interp::Buffers = [
        (compose::binding::HEADER, hdr),
        (compose::binding::LAYERS, layers),
        (compose::binding::HEAP, bytes_of(heap)),
        (compose::binding::TARGET, vec![0; c.target_words() * 4]),
        (compose::binding::LUT, lut),
    ]
    .into_iter()
    .map(|(b, v)| ((0, b), v))
    .collect();
    atrium_gir::interp::run(g, c.groups(), &mut bufs).map_err(|e| format!("{e:?}"))?;
    Ok(words(&bufs[&(0, compose::binding::TARGET)]))
}

fn tier2() -> (tempfile::TempDir, tier2::Tier2) {
    let dir = tempfile::tempdir().unwrap();
    tier2::Tier2::seed_cache(dir.path()).expect("cranelift");
    let mut t2 = tier2::Tier2::new(dir.path().to_path_buf(), "/nonexistent/atrium-spv-compile".into()).expect("tier2");
    t2.set_threads(1);
    (dir, t2)
}

#[test]
fn compose_matches_the_reference_on_fuzzed_layer_lists() {
    let iters: u64 = std::env::var("FRESCO_FUZZ_ITERS").ok().and_then(|v| v.parse().ok()).unwrap_or(400);
    let g = gir();
    let (_d, t2) = tier2();
    let mut r = Rng(0xc0de);
    let (mut blended, mut clipped) = (0, 0);
    for it in 0..iters {
        let (c, heap) = compose_case(&mut r);
        c.validate(c.target_words(), heap.len()).unwrap_or_else(|e| panic!("iteration {it}: {e}"));
        let want = compose::render(&c, &heap);
        let mut got = vec![0u32; c.target_words()];
        t2.compose(&c, &heap, &mut got).unwrap();
        assert_eq!(got, want, "iteration {it}: tier2");
        if it % 4 == 0 {
            assert_eq!(run_gir(&g, &c, &heap).unwrap_or_else(|e| panic!("iteration {it}: GIR: {e}")), want, "iteration {it}: GIR");
        }
        let n = c.header.layer_count as usize;
        blended += c.layers[..n].iter().filter(|l| !(l.opacity >= 1.0)).count();
        clipped += c.layers[..n].iter().filter(|l| l.origin != [l.rect[0], l.rect[1]]).count();
    }
    assert!(blended > 0 && clipped > 0, "corpus too tame: {blended} blended, {clipped} clipped/scrolled layers");
}

/// Two windows rastered into their own surfaces, then composed: an opaque
/// window shows exactly its surface; a translucent one blends over the
/// window below; nothing of either shows outside its rect.
#[test]
fn per_window_surfaces_compose_into_the_screen() {
    let (_d, t2) = tier2();
    let scene = |c: [f32; 4]| Scene {
        rects: vec![Rect { position: [2.0, 2.0], size: [12.0, 8.0], color: c, radius: 3.0 }],
        ..Default::default()
    };
    let (aw, ah, bw, bh) = (20u32, 14u32, 16u32, 12u32);
    let fa = Frame::pack(&scene([0.9, 0.1, 0.1, 1.0]), aw, ah, aw);
    let fb = Frame::pack(&scene([0.1, 0.1, 0.9, 1.0]), bw, bh, bw);
    let mut heap = vec![0u32; (aw * ah + bw * bh) as usize];
    let (sa, sb) = heap.split_at_mut((aw * ah) as usize);
    t2.render(&fa, sa).unwrap();
    t2.render(&fb, sb).unwrap();
    let a = [0, aw, aw, ah];
    let b = [aw * ah, bw, bw, bh];
    let layers = [Layer::at(a, 4, 3, 1.0), Layer::at(b, 14, 9, 0.5)];
    let c = Compose::new(&layers, 40, 28, 40, heap.len() as u32);
    let mut screen = vec![0u32; c.target_words()];
    t2.compose(&c, &heap, &mut screen).unwrap();
    assert_eq!(screen, compose::render(&c, &heap));
    let at = |x: u32, y: u32| screen[(y * 40 + x) as usize];
    let clear = compose::pixel(&Compose::new(&[], 1, 1, 1, 0), &[], 0, 0);
    assert_eq!(at(4 + 5, 3 + 1), heap[(1 * aw + 5) as usize], "opaque window A passes through");
    assert_eq!(at(0, 0), clear);
    assert_eq!(at(39, 27), clear, "B ends at (30, 21)");
    let (under, over) = (heap[(8 * aw + 12) as usize], heap[(aw * ah + 2 * bw + 2) as usize]);
    let blended = at(16, 11);
    assert!(blended != under && blended != over, "B at 50% blends over A");
}
