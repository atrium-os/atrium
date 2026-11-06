//! What a partial redraw costs against a full one as more of a window
//! changes: `cargo test --release --test damage_cost -- --ignored --nocapture`.

use fresco_compute::*;
use std::time::Instant;

const MAX: usize = tier2::Tier2::MAX_TILE_RECTS;

fn best<F: FnMut()>(mut f: F) -> f64 {
    let mut b = f64::MAX;
    for _ in 0..7 {
        let t = Instant::now();
        f();
        b = b.min(t.elapsed().as_secs_f64() * 1e3);
    }
    b
}

#[test]
#[ignore]
fn partial_against_full() {
    let dir = tempfile::tempdir().unwrap();
    tier2::Tier2::seed_cache(dir.path()).expect("cranelift");
    let mut t2 = tier2::Tier2::new(dir.path().to_path_buf(), "/nonexistent/atrium-spv-compile".into()).expect("tier2");
    t2.set_threads(1);
    let (w, h) = (1280, 800);
    // A busy window: a background, 3,000 small rects (text-like), the
    // fixed fills.
    let mut r = fuzz::Rng(7);
    let mut scene = Scene { fills: fuzz::fills(), ..Default::default() };
    scene.rects.push(Rect { position: [0.0, 0.0], size: [w as f32, h as f32], color: [0.9, 0.9, 0.9, 1.0], radius: 0.0 });
    for _ in 0..3000 {
        scene.rects.push(Rect {
            position: [r.f(0.0, w as f32), r.f(0.0, h as f32)],
            size: [r.f(4.0, 40.0), r.f(6.0, 14.0)],
            color: r.color(),
            radius: r.f(0.0, 3.0),
        });
    }
    let f = Frame::pack(&scene, w, h, w);
    let hd = f.header;
    let n = (hd.tiles_x * hd.tiles_y) as usize;
    let mut img = vec![0u32; f.target_words()];
    let sig_ms = best(|| { damage::tile_signatures(&f); });
    let full = best(|| t2.render(&f, &mut img).unwrap());
    println!("{w}x{h}, {} ops: signatures {sig_ms:.2} ms, full raster {full:.2} ms", hd.op_count);
    let mut rng = fuzz::Rng(3);
    for pct in [10u32, 25, 50, 75, 90, 100] {
        for scattered in [false, true] {
            let dirty: Vec<bool> = (0..n)
                .map(|i| if scattered { rng.chance(pct) } else { (i as u32) < n as u32 * pct / 100 })
                .collect();
            let tiles = damage::rects(&dirty, hd.tiles_x, hd.tiles_y, MAX);
            let covered: u32 = tiles.iter().map(|r| (r[2] - r[0]) * (r[3] - r[1])).sum();
            let part = best(|| {
                let tiles = damage::rects(&dirty, hd.tiles_x, hd.tiles_y, MAX);
                t2.render_tiles(&f, &mut img, &tiles).unwrap();
            });
            println!(
                "{pct:>3}% {:<9} {:>3} rects, {:>5.1}% of tiles rastered: partial {part:>7.2} ms = {:>5.2}x full",
                if scattered { "scattered" } else { "block" },
                tiles.len(),
                covered as f64 * 100.0 / n as f64,
                part / full
            );
        }
    }
    // Compose: the whole screen at once against the same area as a rect.
    use fresco_compute::compose::{Compose, Layer};
    let heap = img.clone();
    let c = Compose::new(&[Layer::at([0, w, w, h], 0, 0, 1.0)], w, h, w, heap.len() as u32);
    let mut screen = vec![0u32; (w * h) as usize];
    let whole = best(|| t2.compose(&c, &heap, &mut screen).unwrap());
    let rect = best(|| t2.compose_rects(&c, &heap, &mut screen, &[[0, 0, w, h]]).unwrap());
    let half = best(|| t2.compose_rects(&c, &heap, &mut screen, &[[0, 0, w, h / 2]]).unwrap());
    println!("compose: whole {whole:.2} ms, as a rect {rect:.2} ms ({:.2}x), half {half:.2} ms", rect / whole);
}
