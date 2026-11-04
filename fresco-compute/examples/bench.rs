//! Render a desktop-like 1920x1080 frame on Tier-2 and the reference; print
//! timings; write the Tier-2 frame to $OUT (PPM) if set.
use std::time::Instant;
use fresco_compute::*;

fn desktop() -> Scene {
    let mut s = Scene::default();
    // windows: shadow, body, title bar, controls
    for (i, (x, y, w, h)) in [(80.0, 60.0, 900.0, 600.0), (700.0, 300.0, 800.0, 560.0), (1300.0, 120.0, 520.0, 380.0)].iter().enumerate() {
        s.rects.push(Rect { position: [x - 8.0, y - 4.0], size: [w + 16.0, h + 16.0], color: [0.0, 0.0, 0.0, 0.25], radius: 14.0 });
        s.rects.push(Rect { position: [*x, *y], size: [*w, *h], color: [0.95, 0.95, 0.96, 1.0], radius: 10.0 });
        s.rects.push(Rect { position: [*x, *y], size: [*w, 36.0], color: [0.3 + 0.2 * i as f32, 0.35, 0.5, 1.0], radius: 10.0 });
        for b in 0..3 {
            s.rects.push(Rect { position: [x + 12.0 + 22.0 * b as f32, y + 11.0], size: [14.0, 14.0], color: [0.9, 0.3 + 0.3 * b as f32, 0.2, 1.0], radius: 7.0 });
        }
        for row in 0..30 {
            s.rects.push(Rect { position: [x + 20.0, y + 50.0 + 18.0 * row as f32], size: [w - 40.0 - (row * 7 % 200) as f32, 10.0], color: [0.2, 0.2, 0.25, 0.8], radius: 2.0 });
        }
    }
    // dock
    s.rects.push(Rect { position: [560.0, 1000.0], size: [800.0, 70.0], color: [0.1, 0.1, 0.12, 0.6], radius: 18.0 });
    for k in 0..12 {
        s.rects.push(Rect { position: [580.0 + 64.0 * k as f32, 1010.0], size: [50.0, 50.0], color: [0.2 + 0.05 * k as f32, 0.5, 0.8 - 0.05 * k as f32, 1.0], radius: 12.0 });
    }
    // a chart: path segments
    for k in 0..60 {
        let x = 120.0 + 12.0 * k as f32;
        s.paths.push(Path { model: [x, 500.0 + 60.0 * (k as f32 * 0.3).sin(), 13.0, 2.0], angle: 0.3 * (k as f32 * 0.3).cos(), color: [0.1, 0.5, 0.9, 1.0] });
    }
    s
}

fn main() {
    let (w, h) = (1920, 1080);
    let f = Frame::pack(&desktop(), w, h, w);
    eprintln!("{} ops", f.header.op_count);
    let t = Instant::now();
    let r = reference::render(&f);
    eprintln!("reference: {:?}", t.elapsed());
    let dir = std::env::temp_dir().join("fresco-compute-bench");
    tier2::Tier2::seed_cache(&dir).unwrap();
    let t2 = tier2::Tier2::new(dir, "/nonexistent".into()).unwrap();
    let mut out = vec![0u32; f.target_words()];
    for i in 0..3 {
        let t = Instant::now();
        t2.render(&f, &mut out).unwrap();
        eprintln!("tier2 frame {i}: {:?}", t.elapsed());
    }
    assert_eq!(out, r, "tier2 != reference");
    if let Some(p) = std::env::var_os("OUT") {
        let mut ppm = format!("P6 {w} {h} 255\n").into_bytes();
        for px in &out {
            let [b, g, r, _] = px.to_le_bytes();
            ppm.extend_from_slice(&[r, g, b]);
        }
        std::fs::write(p, ppm).unwrap();
    }
}
