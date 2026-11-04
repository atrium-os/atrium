//! F2 (threat model §2.5.4): differential fuzzing of bin + raster.
//!
//! A frame is what a jailed renderer writes, so everything in it but the
//! header is hostile: op kinds, texture indices, bounding boxes, floats
//! (NaN, infinities, huge), texture tables pointing anywhere. The broker
//! checks only the header (`Frame::validate`). This fuzzer checks that for
//! every frame `validate` accepts:
//!
//! * the GIR interpreter (the IR the RDNA4 code is compiled from, with
//!   exact bounds checks on every buffer access) never goes out of range
//!   and draws what the reference draws;
//! * Tier-2 native code, with each buffer placed against a guard page
//!   (once ending at one, once starting after one), never faults and draws
//!   what the reference draws;
//! * the reference (indexing checked by Rust) never panics;
//! * nothing touches the target's padding;
//!
//! and that the corpus reached every branch the reference counts.
//!
//! `FRESCO_FUZZ_ITERS` (default 400) and `FRESCO_FUZZ_SEED` scale and vary
//! the run; a failure prints the seed and iteration to replay.

use fresco_compute::*;

use fresco_compute::fuzz::{frame, Rng};

fn words(b: &[u8]) -> Vec<u32> {
    b.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

fn gir(spv: &[u8]) -> atrium_gir::Function {
    let m = atrium_spv_frontend::translate(spv).expect("frontend");
    let g = atrium_gir::build::build(&m, 0).expect("GIR build");
    atrium_gir::verify::verify(&g).expect("GIR verify");
    g
}

/// Both kernels on the GIR interpreter, buffers exactly their bytes.
fn run_gir(bin: &atrium_gir::Function, raster: &atrium_gir::Function, f: &Frame) -> Result<Vec<u32>, String> {
    let mut bufs: atrium_gir::interp::Buffers =
        f.buffers().into_iter().enumerate().map(|(i, v)| ((0, i as u32), v)).collect();
    atrium_gir::interp::run(bin, f.bin_groups(), &mut bufs).map_err(|e| format!("bin: {e:?}"))?;
    atrium_gir::interp::run(raster, f.raster_groups(), &mut bufs).map_err(|e| format!("raster: {e:?}"))?;
    Ok(words(&bufs[&(0, binding::TARGET)]))
}

/// A buffer in its own mapping with PROT_NONE pages on both sides, its
/// bytes either ending at the upper guard or starting after the lower one.
struct Guarded {
    map: *mut libc::c_void,
    len: usize,
    data: *mut u8,
    n: usize,
}

impl Guarded {
    fn new(bytes: &[u8], at_end: bool) -> Guarded {
        let page = 4096usize;
        let n = bytes.len().max(4);
        let body = n.div_ceil(page) * page;
        let len = body + 2 * page;
        // SAFETY: a fresh anonymous mapping; the guards are its first and
        // last pages and `data..data+n` lies within the body between them.
        unsafe {
            let map = libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            );
            assert_ne!(map, libc::MAP_FAILED);
            let base = map as *mut u8;
            assert_eq!(libc::mprotect(map, page, libc::PROT_NONE), 0);
            assert_eq!(libc::mprotect(base.add(page + body) as *mut _, page, libc::PROT_NONE), 0);
            let data = if at_end { base.add(page + body - n) } else { base.add(page) };
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len());
            Guarded { map, len, data, n }
        }
    }
    fn words(&self) -> Vec<u32> {
        // SAFETY: `data` holds `n` initialised bytes inside the mapping.
        words(unsafe { std::slice::from_raw_parts(self.data, self.n) })
    }
}

impl Drop for Guarded {
    fn drop(&mut self) {
        // SAFETY: the mapping made in `new`, unmapped once.
        unsafe { libc::munmap(self.map, self.len) };
    }
}

/// Tier-2 with every binding against a guard page. An access outside a
/// buffer faults and kills the test (the failure message is the crash).
fn run_tier2(t2: &tier2::Tier2, f: &Frame, at_end: bool) -> Vec<u32> {
    let bufs: Vec<Guarded> = f.buffers().iter().map(|b| Guarded::new(b, at_end)).collect();
    let table: [u64; binding::COUNT] = std::array::from_fn(|i| bufs[i].data as u64);
    // SAFETY: each pointer addresses a live buffer of the size `buffers()`
    // gives for this header, which `validate` accepted; mask and target are
    // zeroed and writable; the header binding is `f.header`'s bytes.
    unsafe { t2.render_table(&table, &f.header) };
    bufs[binding::TARGET as usize].words()
}

/// The first differing pixel and the ops binned to it, for a failure.
fn report(f: &Frame, got: &[u32], want: &[u32]) {
    let i = got.iter().zip(want).position(|(a, b)| a != b).unwrap();
    let (x, y) = (i as u32 % f.header.pitch, i as u32 / f.header.pitch);
    eprintln!("first difference at ({x}, {y}): got {:#010x}, want {:#010x}", got[i], want[i]);
    eprintln!("header {:?}", f.header);
    let mask = reference::bin(f);
    let tile = (y / TILE) * f.header.tiles_x + x / TILE;
    for w in 0..f.header.wpt {
        let bits = mask[(tile * f.header.wpt + w) as usize];
        for b in 0..32 {
            if bits & (1 << b) != 0 {
                let oi = (w * 32 + b) as usize;
                let o = &f.ops[oi];
                let s = reference::shade(f, o, [x as f32 + 0.5, y as f32 + 0.5]);
                eprintln!("op {oi}: {o:?} -> {s:?}");
                if matches!(o.meta[0], 2 | 3) || o.meta[0] > 3 {
                    eprintln!("  tex {:?}", f.tex.get(o.meta[1] as usize));
                }
            }
        }
    }
}

fn env(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

#[test]
fn kernels_match_the_reference_on_hostile_frames() {
    let iters = env("FRESCO_FUZZ_ITERS", 400);
    let seed = env("FRESCO_FUZZ_SEED", 0xf2f2);
    let gir_every = env("FRESCO_FUZZ_GIR_EVERY", 4).max(1);
    let dir = tempfile::tempdir().unwrap();
    tier2::Tier2::seed_cache(dir.path()).expect("cranelift");
    let mut t2 = tier2::Tier2::new(dir.path().to_path_buf(), "/nonexistent/atrium-spv-compile".into()).expect("tier2");
    t2.set_threads(1);
    let (bin, raster) = (gir(BIN_SPV), gir(RASTER_SPV));
    let mut r = Rng(seed);
    let mut checked_gir = 0;
    for it in 0..iters {
        let f = frame(&mut r);
        let at = format!("seed {seed:#x} iteration {it}");
        f.validate(f.target_words()).unwrap_or_else(|e| panic!("{at}: generator made an invalid frame: {e}"));
        let want = reference::render(&f);
        let h = f.header;
        let padding_clear = |img: &[u32]| img.iter().enumerate().all(|(i, &p)| (i as u32 % h.pitch) < h.width || p == 0);
        assert!(padding_clear(&want), "{at}: reference wrote padding");
        if it % gir_every == 0 {
            let got = run_gir(&bin, &raster, &f).unwrap_or_else(|e| panic!("{at}: GIR out of range: {e}"));
            let bad = got.iter().zip(&want).filter(|(a, b)| a != b).count();
            if bad != 0 {
                report(&f, &got, &want);
            }
            assert_eq!(bad, 0, "{at}: GIR differs from the reference in {bad} pixels");
            checked_gir += 1;
        }
        for at_end in [true, false] {
            let got = run_tier2(&t2, &f, at_end);
            let bad = got.iter().zip(&want).filter(|(a, b)| a != b).count();
            if bad != 0 {
                report(&f, &got, &want);
            }
            assert_eq!(bad, 0, "{at}: tier2 (guard {}) differs from the reference in {bad} pixels", if at_end { "after" } else { "before" });
        }
    }
    let counts = reference::coverage::counts();
    let missed: Vec<_> = reference::coverage::NAMES.iter().zip(counts).filter(|(_, c)| *c == 0).map(|(n, _)| *n).collect();
    for (n, c) in reference::coverage::NAMES.iter().zip(counts) {
        eprintln!("{c:>10}  {n}");
    }
    eprintln!("{iters} frames on tier2 (x2 guard placements), {checked_gir} on GIR");
    assert!(missed.is_empty(), "the corpus never reached: {missed:?}");
}

/// Headers a hostile renderer might write: whatever `validate` accepts is
/// safe to run with exactly-sized buffers.
#[test]
fn validate_rejects_what_the_kernels_cannot_bound() {
    let iters = env("FRESCO_FUZZ_ITERS", 400);
    let mut r = Rng(env("FRESCO_FUZZ_SEED", 0xf2f2) ^ 0x4ead);
    let (bin, raster) = (gir(BIN_SPV), gir(RASTER_SPV));
    let (mut accepted, mut rejected) = (0, 0);
    for it in 0..iters {
        let mut f = frame(&mut r);
        let target = f.target_words();
        let h = &mut f.header;
        for _ in 0..1 + r.below(3) {
            match r.below(9) {
                0 => h.width = r.word(80),
                1 => h.height = r.word(60),
                2 => h.pitch = r.word(90),
                3 => h.tiles_x = r.word(6),
                4 => h.tiles_y = r.word(5),
                5 => h.op_count = r.word(40),
                6 => h.wpt = r.word(3),
                7 => h.tex_count = r.word(6),
                _ => h.texel_words = r.word(200),
            }
        }
        if f.validate(target).is_err() {
            rejected += 1;
            continue;
        }
        accepted += 1;
        // Small enough to interpret (the header may now say a bigger
        // target than before, within `target` words).
        if (f.header.width as u64) * (f.header.height as u64) > 4096 {
            continue;
        }
        let want = reference::render(&f);
        let got = run_gir(&bin, &raster, &f).unwrap_or_else(|e| panic!("iteration {it}: accepted header, GIR out of range: {e}"));
        assert_eq!(got, want, "iteration {it}");
    }
    eprintln!("{accepted} accepted, {rejected} rejected");
    assert!(accepted > 0 && rejected > 0);
}

/// The hit-test probe agrees with the kernels' alpha at every pixel.
#[test]
fn alpha_at_matches_the_rendered_alpha() {
    let mut r = fuzz::Rng(env("FRESCO_FUZZ_SEED", 0xa1fa));
    let mut checked = 0;
    for _ in 0..env("FRESCO_FUZZ_ITERS", 300) / 3 {
        let f = fuzz::frame(&mut r);
        if f.validate(f.target_words()).is_err() {
            continue;
        }
        let out = reference::render(&f);
        let h = &f.header;
        for y in 0..h.height {
            for x in 0..h.width {
                let want = (out[(y * h.pitch + x) as usize] >> 24) as u8;
                assert_eq!(reference::alpha_at(&f, x, y), want, "pixel ({x}, {y})");
            }
        }
        checked += 1;
    }
    assert!(checked > 10, "only {checked} valid frames");
}
