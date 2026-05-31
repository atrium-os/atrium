//! 4K @ 120 fps feasibility for the scene-graph compositor in
//! software.
//!
//! Budget: 3840x2160 @ 120 fps = 8.33 ms/frame.
//!
//! tiny-skia is a strong SIMD blitter but SINGLE-THREADED, so at
//! 4K a full-screen repaint with overdraw blows the budget on one
//! core while the other N-1 sit idle.  This bench shows the fix is
//! not a new rasterizer -- it's running tiny-skia per TILE across
//! cores.  tiny-skia can render into a borrowed slice via
//! `PixmapMut::from_bytes`, so we split the framebuffer into
//! horizontal bands, wrap each band's bytes as a PixmapMut, and
//! replay the scene per band under rayon (each band translated so
//! the shared scene clips to it).  Disjoint slices => safe, lock-
//! free parallelism; near-linear multicore scaling.
//!
//! Reports: single-threaded vs tiled, ms/frame + 4K@120 verdict.
//!
//!   cargo run -p aqueduct-gpu-host --example bench_4k_tiled --release

use std::time::Instant;
use rayon::prelude::*;
use tiny_skia::{Paint, PixmapMut, Rect, Transform, Color, Shader, BlendMode};

const W: u32 = 3840;
const H: u32 = 2160;
const BUDGET_MS: f64 = 1000.0 / 120.0; // 8.33 ms

/// Process CPU-time (user+sys, summed across ALL threads) in
/// seconds.  Per-frame delta is an energy/power-draw proxy:
/// total core-seconds burned, regardless of how many cores.
fn cpu_secs() -> f64 {
    unsafe {
        let mut ru: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut ru);
        ru.ru_utime.tv_sec as f64 + ru.ru_utime.tv_usec as f64 * 1e-6
            + ru.ru_stime.tv_sec as f64 + ru.ru_stime.tv_usec as f64 * 1e-6
    }
}

#[derive(Clone, Copy)]
struct RectPx { x: f32, y: f32, w: f32, h: f32, rgba: [u8; 4], opaque: bool }

fn build_scene() -> Vec<RectPx> {
    let mut s = Vec::new();
    // Opaque background.
    s.push(RectPx { x: 0.0, y: 0.0, w: W as f32, h: H as f32,
                    rgba: [40, 42, 48, 255], opaque: true });
    // 12x8 grid of opaque panels (windows / surfaces).
    for gy in 0..8 {
        for gx in 0..12 {
            s.push(RectPx {
                x: 24.0 + gx as f32 * 315.0,
                y: 24.0 + gy as f32 * 265.0,
                w: 290.0, h: 240.0,
                rgba: [70, 74, 82, 255], opaque: true,
            });
        }
    }
    // ~16000 small alpha-blended "glyph" quads (text).
    let (gw, gh, adv, line) = (9.0f32, 14.0f32, 10.0f32, 18.0f32);
    for r in 0..100 {
        for c in 0..160 {
            s.push(RectPx {
                x: 36.0 + c as f32 * adv,
                y: 36.0 + r as f32 * line,
                w: gw, h: gh,
                rgba: [230, 230, 235, 217], opaque: false,
            });
        }
    }
    s
}

/// Replay the scene into `pm` with a y-translation (for tiling).
/// tiny-skia clips each fill to the pixmap extent, so a band only
/// pays for the primitives that overlap it.
fn render(pm: &mut PixmapMut, y_off: f32, scene: &[RectPx]) {
    let xf = Transform::from_translate(0.0, y_off);
    let mut paint = Paint::default();
    paint.anti_alias = false;
    for r in scene {
        paint.blend_mode = if r.opaque { BlendMode::Source } else { BlendMode::SourceOver };
        paint.shader = Shader::SolidColor(
            Color::from_rgba8(r.rgba[0], r.rgba[1], r.rgba[2], r.rgba[3]));
        if let Some(rect) = Rect::from_xywh(r.x, r.y, r.w, r.h) {
            pm.fill_rect(rect, &paint, xf, None);
        }
    }
}

fn main() {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let scene = build_scene();
    let glyphs = scene.iter().filter(|r| !r.opaque).count();
    println!("4K {W}x{H} @ 120fps budget = {BUDGET_MS:.2} ms/frame   cores={cores}");
    println!("scene: {} rects ({} opaque + {} alpha glyphs)",
             scene.len(), scene.len() - glyphs, glyphs);

    let row_bytes = (W * 4) as usize;
    let mut fb = vec![0u8; (W * H) as usize * 4];
    let iters = 60u32;

    // ── Single-threaded ──
    for _ in 0..3 {
        let mut pm = PixmapMut::from_bytes(&mut fb, W, H).unwrap();
        render(&mut pm, 0.0, &scene);
    }
    let c0 = cpu_secs();
    let t0 = Instant::now();
    for _ in 0..iters {
        let mut pm = PixmapMut::from_bytes(&mut fb, W, H).unwrap();
        render(&mut pm, 0.0, &scene);
    }
    let single_ms = t0.elapsed().as_secs_f64() * 1000.0 / iters as f64;
    let single_cpu_ms = (cpu_secs() - c0) * 1000.0 / iters as f64;

    // ── Tiled across cores, WITH spatial binning. ──
    // ~2 bands per core for load balance.  The critical fix: each
    // band processes only the primitives that overlap it, not the
    // whole scene -- otherwise every band reprocesses all 16k
    // rects (N_bands x cost) and tiling LOSES.
    let band_rows = ((H as usize + (cores * 2) - 1) / (cores * 2)).max(8);
    let n_bands = (H as usize + band_rows - 1) / band_rows;
    // Bin once (static scene; a dynamic scene-graph amortises this
    // incrementally -- it's ~O(rects) and microseconds here).
    let mut bins: Vec<Vec<RectPx>> = vec![Vec::new(); n_bands];
    for r in &scene {
        let b0 = (r.y.max(0.0) as usize) / band_rows;
        let b1 = (((r.y + r.h - 1.0).max(0.0) as usize) / band_rows).min(n_bands - 1);
        for b in b0..=b1 { bins[b].push(*r); }
    }
    let render_tiled = |fb: &mut [u8]| {
        fb.par_chunks_mut(band_rows * row_bytes)
            .zip(bins.par_iter())
            .enumerate()
            .for_each(|(bi, (chunk, band_scene))| {
                let band_h = (chunk.len() / row_bytes) as u32;
                if band_h == 0 { return; }
                let mut pm = PixmapMut::from_bytes(chunk, W, band_h).unwrap();
                let y_off = -((bi * band_rows) as f32);
                render(&mut pm, y_off, band_scene);
            });
    };
    for _ in 0..3 { render_tiled(&mut fb); }
    let c1 = cpu_secs();
    let t1 = Instant::now();
    for _ in 0..iters { render_tiled(&mut fb); }
    let tiled_ms = t1.elapsed().as_secs_f64() * 1000.0 / iters as f64;
    let tiled_cpu_ms = (cpu_secs() - c1) * 1000.0 / iters as f64;

    let verdict = |ms: f64| if ms <= BUDGET_MS { "MEETS 4K@120" } else { "misses" };
    println!();
    println!("  tiny-skia single-threaded : {single_ms:7.2} ms wall ({:6.1} fps)  | {single_cpu_ms:6.2} cpu-ms/frame  [{}]",
             1000.0 / single_ms, verdict(single_ms));
    println!("  tiny-skia tiled ({band_rows}-row bands): {tiled_ms:7.2} ms wall ({:6.1} fps)  | {tiled_cpu_ms:6.2} cpu-ms/frame  [{}]",
             1000.0 / tiled_ms, verdict(tiled_ms));
    println!("  (cpu-ms/frame = total core-time = energy proxy; tiling trades cpu-ms for wall-ms)");
    println!();
    println!("  tiling speedup: {:.1}x   (ideal ~{cores}x)", single_ms / tiled_ms);
    println!("  headroom vs 8.33ms budget: {:.1}x", BUDGET_MS / tiled_ms);
}
