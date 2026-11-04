//! The SPIR-V kernels against the Rust reference: on the GIR interpreter
//! (always) and on Tier-2 (when ATRIUM_SPV_COMPILE names the compiler).

use fresco_compute::*;

/// A scene with every op kind, overlaps, translucency, a rounded corner, a
/// rotated path and an edge-clamped texture, on a 72x40 target with padding.
pub fn scene() -> Scene {
    let rgba: Vec<u8> = (0..16u32)
        .flat_map(|i| [(i * 16) as u8, (255 - i * 16) as u8, (i * 37 % 256) as u8, if i % 3 == 0 { 128 } else { 255 }])
        .collect();
    let atlas: Vec<u8> = (0..64u32).map(|i| ((i * 29 + 7) % 256) as u8).collect();
    Scene {
        rects: vec![
            Rect { position: [2.0, 3.0], size: [30.0, 20.0], color: [0.9, 0.2, 0.1, 1.0], radius: 6.0 },
            Rect { position: [20.5, 10.25], size: [25.0, 18.0], color: [0.1, 0.4, 0.9, 0.5], radius: 0.0 },
            Rect { position: [-5.0, 30.0], size: [20.0, 20.0], color: [1.0, 1.0, 1.0, 0.75], radius: 30.0 },
        ],
        paths: vec![
            Path { model: [50.0, 20.0, 30.0, 3.0], angle: 0.6, color: [0.0, 0.8, 0.2, 1.0] },
            Path { model: [36.0, 5.0, 20.0, 2.0], angle: 0.0, color: [0.8, 0.8, 0.0, 0.6] },
        ],
        textures: vec![
            Texture { width: 4, height: 4, format: TexFormat::Rgba8Srgb, data: rgba },
            Texture { width: 8, height: 8, format: TexFormat::R8, data: atlas },
        ],
        texture_batches: vec![TextureBatch { texture: 0, quads: vec![[44.0, 24.0, 24.0, 14.0], [66.0, -2.0, 10.0, 10.0]] }],
        glyph_batches: vec![GlyphBatch {
            atlas: 1,
            runs: vec![GlyphRun {
                origin: [4.0, 36.0],
                color: [0.05, 0.05, 0.1, 1.0],
                glyphs: vec![
                    Glyph { d_offset: [0.0, 0.0], atlas_uv: [0.0, 0.0, 4.0, 8.0], bearing: [0.0, 7.0] },
                    Glyph { d_offset: [5.0, 0.0], atlas_uv: [4.0, 0.0, 4.0, 8.0], bearing: [1.0, 6.0] },
                ],
            }],
        }],
    }
}

pub const W: u32 = 72;
pub const H: u32 = 40;
pub const PITCH: u32 = 80;

fn words(b: &[u8]) -> Vec<u32> {
    b.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

/// Compare two targets; returns (pixels that differ, largest channel delta).
fn diff(a: &[u32], b: &[u32]) -> (usize, u32) {
    let mut n = 0;
    let mut worst = 0;
    for (x, y) in a.iter().zip(b) {
        if x != y {
            n += 1;
            for s in [0, 8, 16, 24] {
                worst = worst.max(((x >> s) & 255).abs_diff((y >> s) & 255));
            }
        }
    }
    (n, worst)
}

#[test]
fn reference_draws_the_scene() {
    let f = Frame::pack(&scene(), W, H, PITCH);
    let out = reference::render(&f);
    let clear = reference::pixel(&Frame::pack(&Scene::default(), 1, 1, 1), &[0], 0, 0);
    assert_eq!(clear, 0xff0a808c, "clear colour encodes to #0A808C");
    let drawn = (0..H).flat_map(|y| (0..W).map(move |x| (x, y))).filter(|&(x, y)| out[(y * PITCH + x) as usize] != clear).count();
    assert!(drawn > 1000, "only {drawn} pixels drawn");
    assert!(out.iter().enumerate().all(|(i, &p)| (i as u32 % PITCH) < W || p == 0), "padding touched");
}

fn run_gir(spv: &[u8], bufs: &mut atrium_gir::interp::Buffers, groups: [u32; 3]) {
    let m = atrium_spv_frontend::translate(spv).expect("frontend");
    let g = atrium_gir::build::build(&m, 0).expect("GIR build");
    atrium_gir::verify::verify(&g).expect("GIR verify");
    atrium_gir::interp::run(&g, groups, bufs).expect("GIR run");
}

#[test]
fn gir_interpreter_bins_like_reference() {
    let f = Frame::pack(&scene(), W, H, PITCH);
    let mut bufs: atrium_gir::interp::Buffers =
        f.buffers().into_iter().enumerate().map(|(i, v)| ((0, i as u32), v)).collect();
    run_gir(BIN_SPV, &mut bufs, f.bin_groups());
    assert_eq!(words(&bufs[&(0, binding::MASK)]), reference::bin(&f), "tile masks");
}

/// The RDNA4 path goes through GIR, so this is the gate for D3.
#[test]
fn gir_interpreter_rasters_like_reference() {
    let f = Frame::pack(&scene(), W, H, PITCH);
    let mut bufs: atrium_gir::interp::Buffers =
        f.buffers().into_iter().enumerate().map(|(i, v)| ((0, i as u32), v)).collect();
    let mask: Vec<u8> = reference::bin(&f).iter().flat_map(|w| w.to_le_bytes()).collect();
    bufs.insert((0, binding::MASK), mask);
    run_gir(RASTER_SPV, &mut bufs, f.raster_groups());
    let got = words(&bufs[&(0, binding::TARGET)]);
    let (n, worst) = diff(&got, &reference::render(&f));
    assert_eq!(n, 0, "{n} pixels differ, worst channel delta {worst}");
}

#[test]
fn tier2_matches_reference() {
    // Seed the cache with Cranelift so the compiler binary is never spawned.
    let dir = tempfile::tempdir().unwrap();
    tier2::Tier2::seed_cache(dir.path()).expect("cranelift");
    let t2 = tier2::Tier2::new(dir.path().to_path_buf(), "/nonexistent/atrium-spv-compile".into()).expect("tier2 load");
    let f = Frame::pack(&scene(), W, H, PITCH);
    let mut got = vec![0u32; f.target_words()];
    t2.render(&f, &mut got).unwrap();
    let (n, worst) = diff(&got, &reference::render(&f));
    assert_eq!(n, 0, "{n} pixels differ, worst channel delta {worst}");
}

/// Re-rastering only the tiles whose signature changed, over the previous
/// image, gives the full render of the new scene; unchanged tiles are skipped.
#[test]
fn damage_redraw_matches_full_render() {
    use fresco_compute::damage;
    let before = scene();
    let mut after = scene();
    after.rects[1].position[0] += 9.0;
    after.paths[1].color = [0.2, 0.2, 0.9, 1.0];
    let (fa, fb) = (Frame::pack(&after, 72, 40, 80), Frame::pack(&before, 72, 40, 80));
    let d = damage::dirty(&damage::tile_signatures(&fa), Some(&damage::tile_signatures(&fb)));
    let h = fa.header;
    assert!(d.iter().any(|&x| !x) && d.iter().any(|&x| x), "{d:?}");
    let rects = damage::rects(&d, h.tiles_x, h.tiles_y, 16);
    let mut img = reference::render(&fb);
    let mask = reference::bin(&fa);
    for r in &rects {
        for y in r[1] * TILE..(r[3] * TILE).min(h.height) {
            for x in r[0] * TILE..(r[2] * TILE).min(h.width) {
                img[(y * h.pitch + x) as usize] = reference::pixel(&fa, &mask, x, y);
            }
        }
    }
    assert_eq!(img, reference::render(&fa));
    // A frame identical to what the buffer holds needs no raster at all.
    let same = damage::dirty(&damage::tile_signatures(&fa), Some(&damage::tile_signatures(&fa)));
    assert!(damage::rects(&same, h.tiles_x, h.tiles_y, 16).is_empty());
}
