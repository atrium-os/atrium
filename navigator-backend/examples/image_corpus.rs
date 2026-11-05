//! Decode every blob in one or more content-addressed blob directories with
//! the worker's decoder (src/image.rs) and report: success rate, failures by
//! cause, time, and peak memory.
//!
//!     cargo run --release --example image_corpus -- DIR [DIR...]
//!
//! ★ PEAK MEMORY IS MEASURED, NOT ESTIMATED: a counting global allocator
//! tracks live heap bytes, and each decode reports the high-water mark above
//! where it started. That includes the decoder's own scratch (zune's
//! coefficient planes, the PNG row buffers), not just the output frame.
//! Each image is decoded against a FRESH document budget, as if it were the
//! only image on its page; the per-document totals are reported separately.
//!
//! `--reference REFDIR` (before the dirs) also compares each decode with
//! `REFDIR/<hex>.png`, an independent decoder's output (macOS `sips -s
//! format png`), and reports per-image max and mean channel difference:
//! "decoded" has to mean decoded RIGHT, not merely without an error.
//!
//! Decoding is in-process here: it measures the decoder, not the pipe or the
//! jail around it (whose costs are measured in the backend spec §4.7a).

use navigator_backend::image::{decode, sniff, Ceilings};
use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

struct Counting;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() { let n = LIVE.fetch_add(l.size(), Relaxed) + l.size(); PEAK.fetch_max(n, Relaxed); }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) { unsafe { System.dealloc(p, l) }; LIVE.fetch_sub(l.size(), Relaxed); }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, n) };
        if !q.is_null() {
            if n > l.size() { let v = LIVE.fetch_add(n - l.size(), Relaxed) + n - l.size(); PEAK.fetch_max(v, Relaxed); }
            else { LIVE.fetch_sub(l.size() - n, Relaxed); }
        }
        q
    }
}
#[global_allocator]
static A: Counting = Counting;

fn pct(v: &mut Vec<f64>, p: f64) -> f64 {
    if v.is_empty() { return 0.0 }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() - 1) as f64 * p).round() as usize]
}

fn main() {
    let mut dirs: Vec<String> = std::env::args().skip(1).collect();
    let reference = if dirs.first().map(String::as_str) == Some("--reference") {
        dirs.remove(0); Some(dirs.remove(0))
    } else { None };
    assert!(!dirs.is_empty(), "usage: image_corpus DIR [DIR...]");
    for dir in dirs {
        let mut names: Vec<_> = std::fs::read_dir(&dir).expect("dir").flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.len() == 64).collect();
        names.sort();
        let mut by_format: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        let mut causes: BTreeMap<String, usize> = BTreeMap::new();
        let mut examples: BTreeMap<String, String> = BTreeMap::new();
        let (mut ms, mut mpx, mut peaks, mut ratio) = (vec![], vec![], vec![], vec![]);
        let (mut ok, mut max_peak, mut max_peak_name) = (0usize, 0usize, String::new());
        let mut diffs: Vec<(f64, u8, String)> = vec![];
        for n in &names {
            let bytes = std::fs::read(format!("{dir}/{n}")).expect("read");
            let kind = match sniff(&bytes) { Ok(f) => f.name(), Err(k) => k };
            let e = by_format.entry(kind.to_string()).or_default();
            e.0 += 1;
            let base = LIVE.load(Relaxed);
            PEAK.store(base, Relaxed);
            let t = Instant::now();
            let r = decode(&format!("blake3:{n}"), &bytes, Ceilings::default());
            let dt = t.elapsed().as_secs_f64() * 1e3;
            let peak = PEAK.load(Relaxed).saturating_sub(base);
            match r {
                Ok(px) => {
                    ok += 1;
                    e.1 += 1;
                    ms.push(dt);
                    let mp = px.width as f64 * px.height as f64 / 1e6;
                    mpx.push(dt / mp.max(1e-6));
                    peaks.push(peak as f64);
                    ratio.push(peak as f64 / px.rgba.len() as f64);
                    if peak > max_peak { max_peak = peak; max_peak_name = format!("{n} {kind} {}x{}", px.width, px.height) }
                    if let Some(r) = &reference {
                        if let Ok(rb) = std::fs::read(format!("{r}/{n}.png")) {
                            match decode_reference(&rb) {
                                Some((w, h, rr)) if (w, h) == (px.width, px.height) => {
                                    // Colour only where both are opaque enough to see it.
                                    let (mut sum, mut cnt, mut worst) = (0u64, 0u64, 0u8);
                                    for (a, b) in px.rgba.chunks_exact(4).zip(rr.chunks_exact(4)) {
                                        let ad = a[3].abs_diff(b[3]);
                                        worst = worst.max(ad); sum += ad as u64; cnt += 1;
                                        if a[3] > 0 && b[3] > 0 {
                                            for i in 0..3 { let d = a[i].abs_diff(b[i]); worst = worst.max(d); sum += d as u64; cnt += 1; }
                                        }
                                    }
                                    diffs.push((sum as f64 / cnt.max(1) as f64, worst, format!("{n} {kind} {}x{}", w, h)));
                                }
                                Some((w, h, _)) => diffs.push((f64::INFINITY, 255, format!("{n} {kind}: size {}x{} vs reference {w}x{h}", px.width, px.height))),
                                None => {}
                            }
                        }
                    }
                }
                Err(err) => {
                    let c = format!("{} ({kind})", err.cause());
                    *causes.entry(c.clone()).or_default() += 1;
                    examples.entry(c).or_insert_with(|| format!("{n}: {err}"));
                }
            }
        }
        println!("== {dir}");
        println!("blobs {}  decoded {ok}  ({:.1}%)", names.len(), 100.0 * ok as f64 / names.len().max(1) as f64);
        println!("by sniffed kind (total, decoded):");
        for (k, (t, d)) in &by_format { println!("  {k:<8} {t:>5} {d:>5}") }
        println!("failures by cause:");
        for (c, k) in &causes { println!("  {c:<28} {k:>5}   e.g. {}", examples[c]) }
        println!("time per decoded image ms: p50 {:.2} p95 {:.2} max {:.2}  total {:.0}",
            pct(&mut ms.clone(), 0.5), pct(&mut ms.clone(), 0.95), pct(&mut ms.clone(), 1.0), ms.iter().sum::<f64>());
        println!("ms per megapixel: p50 {:.1} p95 {:.1}", pct(&mut mpx.clone(), 0.5), pct(&mut mpx.clone(), 0.95));
        println!("peak heap per decode: p50 {:.0} KiB p95 {:.0} KiB max {:.1} MiB ({max_peak_name})",
            pct(&mut peaks.clone(), 0.5) / 1024.0, pct(&mut peaks.clone(), 0.95) / 1024.0, max_peak as f64 / 1048576.0);
        if !diffs.is_empty() {
            diffs.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
            let mut means: Vec<f64> = diffs.iter().map(|d| d.0).collect();
            let exact = diffs.iter().filter(|d| d.1 == 0).count();
            let close = diffs.iter().filter(|d| d.0 <= 1.0).count();
            println!("vs reference: {} compared, {exact} identical, {close} mean<=1 level; mean diff p50 {:.2} p95 {:.2}",
                diffs.len(), pct(&mut means.clone(), 0.5), pct(&mut means, 0.95));
            for k in ["png", "gif", "webp", "jpeg"] {
                let of: Vec<_> = diffs.iter().filter(|d| d.2.split(' ').nth(1) == Some(k)).collect();
                let worst = of.iter().map(|d| d.1).max().unwrap_or(0);
                println!("  {k:<5} {} compared, {} identical, worst channel diff {worst}", of.len(), of.iter().filter(|d| d.1 == 0).count());
            }
            for d in diffs.iter().take(3) { println!("  worst: mean {:.2} max {} {}", d.0, d.1, d.2) }
        }
        println!("peak / output frame: p50 {:.2} p95 {:.2} max {:.2}",
            pct(&mut ratio.clone(), 0.5), pct(&mut ratio.clone(), 0.95), pct(&mut ratio.clone(), 1.0));
    }
}

fn decode_reference(b: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(b));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut r = dec.read_info().ok()?;
    let mut buf = vec![0; r.output_buffer_size()];
    let info = r.next_frame(&mut buf).ok()?;
    let (ct, _) = r.output_color_type();
    let s = ct.samples();
    let rgba = match s {
        4 => buf,
        3 => buf.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        2 => buf.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        _ => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
    };
    Some((info.width, info.height, rgba))
}
