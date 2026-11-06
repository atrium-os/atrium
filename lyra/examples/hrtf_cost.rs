//! Cost of the HRTF virtualizer per 128-frame period (lyra-spatial.md S3).
fn main() {
    let mut v = lyra::binaural::Virtualizer::for_period(48_000.0, 128);
    assert!(v.is_hrtf());
    let bed: Vec<f32> = (0..128 * 8).map(|i| ((i * 37 % 101) as f32 - 50.0) / 50.0).collect();
    let mut out = vec![0.0f32; 256];
    let n = 20_000;
    let t = std::time::Instant::now();
    for _ in 0..n {
        v.render_add(&bed, &mut out);
    }
    let per = t.elapsed().as_secs_f64() / n as f64;
    println!("{:.1} us per period of 2.67 ms: {:.2}% of one core", per * 1e6, per / (128.0 / 48_000.0) * 100.0);
}
