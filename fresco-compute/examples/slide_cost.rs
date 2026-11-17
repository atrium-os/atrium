//! What one frame of a slide transition costs on the CPU path, part by
//! part (officina.md §16.37): a 1280x720 slide window with a full-window
//! background, a few boxes and a title, its glyphs in a 2048x2048 atlas as
//! Pergola's surfaces upload it. Prints pack, tile signatures, a full
//! Tier-2 raster and a three-layer compose.
//!
//!   cargo run --release --features cranelift --example slide_cost
use std::time::Instant;
use fresco_compute::*;

fn slide(dx: f32) -> Scene {
    let mut s = Scene::default();
    s.rects.push(Rect { position: [0.0, 0.0], size: [1280.0, 720.0], color: [0.0, 0.0, 0.0, 1.0], radius: 0.0 });
    s.rects.push(Rect { position: [dx, 0.0], size: [1280.0, 720.0], color: [1.0, 1.0, 1.0, 1.0], radius: 0.0 });
    for k in 0..3 {
        s.rects.push(Rect { position: [dx + 160.0 + 220.0 * k as f32, 360.0], size: [144.0, 144.0], color: [0.2, 0.4, 0.9, 1.0], radius: 0.0 });
    }
    let mut atlas = vec![0u8; 2048 * 2048];
    for (i, a) in atlas.iter_mut().enumerate().take(2048 * 64) {
        *a = (i % 251) as u8;
    }
    s.textures.push(Texture { width: 2048, height: 2048, format: TexFormat::R8, data: atlas });
    let glyphs = (0..20).map(|k| Glyph { d_offset: [k as f32 * 22.0, 0.0], atlas_uv: [k as f32 * 24.0, 0.0, 22.0, 30.0], bearing: [0.0, 24.0] }).collect();
    s.glyph_batches.push(GlyphBatch { atlas: 0, runs: vec![GlyphRun { origin: [dx + 80.0, 90.0], color: [0.0, 0.0, 0.0, 1.0], glyphs, turn: None }] });
    s
}

fn main() {
    let (w, h) = (1280, 720);
    let dir = std::env::temp_dir().join("fresco-compute-bench");
    tier2::Tier2::seed_cache(&dir).unwrap();
    let t2 = tier2::Tier2::new(dir, "/nonexistent".into()).unwrap();
    let mut out = vec![0u32; (w * h) as usize];
    let scenes: Vec<Scene> = (0..10).map(|i| slide(i as f32 * 40.0)).collect();
    let (mut pack, mut sig, mut raster) = (0.0, 0.0, 0.0);
    for s in &scenes {
        let t = Instant::now();
        let f = Frame::pack(s, w, h, w);
        pack += t.elapsed().as_secs_f64();
        let t = Instant::now();
        let _ = damage::tile_signatures(&f);
        sig += t.elapsed().as_secs_f64();
        let t = Instant::now();
        t2.render(&f, &mut out).unwrap();
        raster += t.elapsed().as_secs_f64();
    }
    let n = scenes.len() as f64;
    eprintln!("per frame: pack {:.2} ms, signatures {:.2} ms, raster {:.2} ms", pack / n * 1e3, sig / n * 1e3, raster / n * 1e3);
    // Three full-window layers composed (what a crossfade between two
    // cached slides over a background would cost).
    let words = (w * h) as usize;
    let heap: Vec<u32> = (0..3).flat_map(|_| out.iter().copied()).collect();
    let layers: Vec<compose::Layer> = (0..3).map(|i| compose::Layer::at([(i * words) as u32, w, w, h], 0, 0, if i == 2 { 0.5 } else { 1.0 })).collect();
    let c = compose::Compose::new(&layers, w, h, w, heap.len() as u32);
    let mut target = vec![0u32; words];
    let t = Instant::now();
    for _ in 0..10 {
        t2.compose(&c, &heap, &mut target).unwrap();
    }
    eprintln!("compose of 3 layers: {:.2} ms", t.elapsed().as_secs_f64() / 10.0 * 1e3);
    // The same screen through compose_rects (opaque pixels copied).
    let one = [compose::Layer::at([0, w, w, h], 0, 0, 1.0)];
    let c1 = compose::Compose::new(&one, w, h, w, heap.len() as u32);
    let t = Instant::now();
    for _ in 0..10 {
        t2.compose_rects(&c1, &heap, &mut target, &[[0, 0, w, h]]).unwrap();
    }
    eprintln!("screen compose of an opaque window (copy path): {:.2} ms", t.elapsed().as_secs_f64() / 10.0 * 1e3);
    // A transition frame drawn as sprites: two slides over the window,
    // one opaque at opacity 1 (a push), one faded (a dissolve).
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).saturating_sub(1).max(1);
    for op in [1.0f32, 0.5] {
        let t = Instant::now();
        for k in 0..10 {
            target.copy_from_slice(&out);
            compose::over(&mut target, w, &out, w, (w, h), [0, 0, w, h], (k * 7, 0), op, threads);
        }
        eprintln!("sprite composite at opacity {op}: {:.2} ms", t.elapsed().as_secs_f64() / 10.0 * 1e3);
    }
}
