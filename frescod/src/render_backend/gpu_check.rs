//! `FRESCOD_GPU_CHECK=<prefix>`: the gpu renderer against the compute
//! renderer, on the device, with no display (GPU driver on silicon, step 1
//! of the post-gate plan; docs/spec/r9700-bare-metal.md §4.6).
//!
//! Each scene is drawn by both renderers the way frescod draws a frame:
//! every window rastered into its own surface in its own VM (one queue fd
//! each), then composed into a VRAM target. The GPU's target is copied into
//! a System BO with CP DMA and read back; the two screens must match to the
//! last bit (`FRESCOD_GPU_CHECK_TOL` allows a per-channel difference). A
//! scene that differs leaves `<prefix>-<scene>-{cpu,gpu}.png`. The last
//! line is `RESULT: PASS` (exit 0) or `RESULT: FAIL` (exit 1).
//!
//! The scenes cover rects (opaque, translucent, rounded), paths, pixel
//! windows, windows partly off screen, a frame drawn incrementally after
//! another (damage), and several windows at once (a VM and a queue each).

use super::*;
use crate::pack::Pixels;
use atrium_gpu::amd::Gpu;
use fresco_vulkan::{PathNode, SceneNode};
use std::time::{Duration, Instant};

const W: u32 = 640;
const H: u32 = 480;

fn rect(x: f32, y: f32, w: f32, h: f32, color: [f32; 4], radius: f32) -> SceneNode {
    SceneNode { position: [x, y], size: [w, h], color, radius, _pad: [0.0; 3] }
}

fn line(cx: f32, cy: f32, len: f32, width: f32, angle: f32, color: [f32; 4]) -> PathNode {
    PathNode { model: [cx, cy, len, width], extra: [angle, 0.0, 0.0, 0.0], color }
}

fn rects_window(id: u32, pos: (f32, f32), size: (u32, u32), tint: f32) -> WindowFrame {
    let (w, h) = (size.0 as f32, size.1 as f32);
    WindowFrame {
        id,
        pos,
        size,
        rects: vec![
            rect(0.0, 0.0, w, h, [0.12, 0.14, 0.18 + tint, 1.0], 0.0),
            rect(8.0, 8.0, w - 16.0, 24.0, [0.85, 0.85, 0.9, 1.0], 6.0),
            rect(20.0, 40.0, w * 0.5, h * 0.4, [0.9, 0.2, 0.1, 1.0], 0.0),
            rect(w * 0.3, h * 0.3, w * 0.5, h * 0.5, [0.1, 0.4, 0.95, 0.5], 12.5),
            rect(w * 0.6, 10.5, 17.25, 9.75, [0.2, 0.9, 0.3, 0.75], 3.0),
        ],
        ..Default::default()
    }
}

fn paths_window(id: u32, pos: (f32, f32)) -> WindowFrame {
    let mut paths = Vec::new();
    for i in 0..12 {
        let a = i as f32 * std::f32::consts::PI / 12.0;
        paths.push(line(100.0, 100.0, 160.0, 1.0 + i as f32 * 0.5, a, [1.0 - i as f32 / 12.0, 0.6, i as f32 / 12.0, 1.0]));
    }
    WindowFrame {
        id,
        pos,
        size: (200, 200),
        rects: vec![rect(0.0, 0.0, 200.0, 200.0, [0.95, 0.95, 0.92, 1.0], 0.0)],
        paths,
        ..Default::default()
    }
}

fn pixels(gen: u64, w: u32, h: u32, salt: u32) -> Arc<Pixels> {
    let px = (0..w * h)
        .map(|i| {
            let (x, y) = (i % w, i / w);
            0xff00_0000 | ((x * 255 / w) << 16) | ((y * 255 / h) << 8) | ((x ^ y ^ salt) & 0xff)
        })
        .collect();
    Arc::new(Pixels { gen, width: w, height: h, px, seq: gen as u32, since: Vec::new() })
}

fn pixel_window(id: u32, pos: (f32, f32), px: &Arc<Pixels>) -> WindowFrame {
    WindowFrame { id, pos, size: (px.width, px.height), pixels: Some(px.clone()), ..Default::default() }
}

/// The scenes, in order: each is drawn on the renderers as they were left
/// by the one before, so later scenes also test incremental drawing.
fn scenes() -> Vec<(&'static str, ScreenInput)> {
    let input = |windows| ScreenInput { windows, uploads: Vec::new(), clears: Vec::new() };
    let p1 = pixels(1, 160, 120, 0);
    let mut p2px = p1.px.clone();
    for y in 30..50 {
        for x in 40..90 {
            p2px[y * 160 + x] = 0xff20_4060;
        }
    }
    let p2 = Arc::new(Pixels { gen: 2, width: 160, height: 120, px: p2px, seq: 2, since: vec![(1, [40, 30, 90, 50])] });
    let mut damaged = rects_window(1, (40.0, 30.0), (300, 220), 0.0);
    damaged.rects[2].color = [0.1, 0.8, 0.2, 1.0];
    vec![
        ("rects", input(vec![rects_window(1, (40.0, 30.0), (300, 220), 0.0)])),
        ("damage", input(vec![damaged])),
        ("paths", input(vec![paths_window(2, (380.0, 40.0))])),
        ("pixels", input(vec![pixel_window(3, (-30.0, 300.0), &p1), rects_window(4, (100.0, 260.0), (260, 180), 0.2)])),
        ("pixels-damage", input(vec![pixel_window(3, (-30.0, 300.0), &p2), rects_window(4, (100.0, 260.0), (260, 180), 0.2)])),
        ("stack", input(vec![
            rects_window(1, (40.0, 30.0), (300, 220), 0.0),
            paths_window(2, (200.0, 120.0)),
            pixel_window(3, (420.0, 360.0), &p2),
            rects_window(4, (500.0, -40.0), (200, 160), 0.3),
        ])),
        ("moved", input(vec![
            rects_window(1, (60.0, 50.0), (300, 220), 0.0),
            paths_window(2, (220.0, 100.0)),
            pixel_window(3, (400.0, 340.0), &p2),
            rects_window(4, (480.0, -20.0), (200, 160), 0.3),
        ])),
        ("fullscreen", input(vec![rects_window(5, (0.0, 0.0), (W, H), 0.1)])),
    ]
}

/// Draw `f` on the gpu renderer into `target` and wait until every window
/// has landed and the target is composed from all of them.
fn gpu_frame(g: &mut GpuCompute<'_>, f: ScreenInput, target: &Bo<'_>, sync: &Syncobj<'_>, seq: &mut u64) -> Result<()> {
    let wait = Duration::from_millis(flip_wait_ms() as u64);
    g.prepare(f)?;
    let keep = g.pending.as_ref().map(|p| ScreenInputKeep::from(p));
    // Rounds: the first rasters (and composes what landed at once); later
    // rounds land what was still running, submit the rest of sliced
    // rasters, and compose. Done once a round started with nothing in
    // flight: its compose saw every window landed.
    for round in 0..64 {
        let idle = !g.rasters_pending();
        if round > 0 {
            let k = keep.as_ref().ok_or("no frame")?;
            g.pending = Some(k.to_pending());
        }
        *seq += 1;
        match g.submit(target, 0, (sync, *seq)) {
            Ok(()) => sync.wait(*seq, wait.as_millis() as u32)?,
            Err(e) if is_busy(&*e) => {
                *seq -= 1;
                std::thread::sleep(Duration::from_millis(2));
                continue;
            }
            Err(e) => return Err(e),
        }
        let t0 = Instant::now();
        while g.rasters_pending() {
            g.pump()?;
            if g.wins.values().all(|v| v.inflight.as_ref().is_none_or(|fl| fl.done(&v.sync))) {
                break;
            }
            if t0.elapsed() > wait {
                return Err("a window's raster did not finish".into());
            }
            std::thread::sleep(Duration::from_micros(200));
        }
        if round > 0 && idle && !g.retry {
            if !g.refusals.is_empty() {
                return Err(format!("raster refused for its cost: {:?}", g.refusals).into());
            }
            return Ok(());
        }
    }
    Err("the frame never settled".into())
}

/// The packed frame of one `prepare`, kept so later rounds compose the
/// same frame again (`submit` takes `pending`).
struct ScreenInputKeep {
    frames: Vec<(u32, Arc<fresco_compute::Frame>)>,
    pos: HashMap<u32, (i32, i32)>,
    pixels: HashMap<u32, Arc<crate::pack::Pixels>>,
}

impl ScreenInputKeep {
    fn from(p: &Pending) -> Self {
        Self { frames: p.frames.clone(), pos: p.pos.clone(), pixels: p.pixels.clone() }
    }
    fn to_pending(&self) -> Pending {
        // Scroll viewports are not part of these scenes.
        Pending { frames: self.frames.clone(), scrolls: HashMap::new(), pos: self.pos.clone(), pixels: self.pixels.clone() }
    }
}

/// Copy `target` (VRAM) into a System BO with CP DMA and read it.
fn read_back(g: &GpuCompute<'_>, target: &Bo<'_>, out: &mut [u8]) -> Result<()> {
    let vm = g.vm;
    let sys = vm.alloc(target.size(), 0)?;
    let ring = vm.alloc(RING_BYTES, 0)?;
    let done = vm.gpu().create_syncobj()?;
    let mut ib = g.arch.stream();
    ib.barrier();
    ib.copy(target.gpu_va(), sys.gpu_va(), out.len() as u64);
    ib.end(ring.gpu_va() + FENCE_OFFSET);
    run(vm, &ring, &*ib, (&done, 1), 0)?;
    done.wait(1, flip_wait_ms())?;
    sys.read(0, out)?;
    Ok(())
}

fn save_png(path: &str, px: &[u8]) {
    // BGRA8 in memory -> RGBA for the PNG.
    let rgba: Vec<u8> = px.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], p[3]]).collect();
    match image::RgbaImage::from_raw(W, H, rgba) {
        Some(img) => {
            if let Err(e) = img.save_with_format(path, image::ImageFormat::Png) {
                eprintln!("frescod-gpu-check: {path}: {e}");
            }
        }
        None => eprintln!("frescod-gpu-check: {path}: bad size"),
    }
}

/// (pixels that differ, the largest per-channel difference, the first
/// differing pixel).
fn compare(a: &[u8], b: &[u8]) -> (usize, u8, Option<(u32, u32)>) {
    let mut n = 0;
    let mut worst = 0u8;
    let mut first = None;
    for (i, (p, q)) in a.chunks_exact(4).zip(b.chunks_exact(4)).enumerate() {
        let d = p.iter().zip(q).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
        if d > 0 {
            n += 1;
            worst = worst.max(d);
            first.get_or_insert((i as u32 % W, i as u32 / W));
        }
    }
    (n, worst, first)
}

/// Run the check; the process's exit status is its result.
pub fn run_check(prefix: &str) -> Result<i32> {
    let tol: u8 = std::env::var("FRESCOD_GPU_CHECK_TOL").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let gpu = Gpu::open()?;
    let caps = gpu.caps()?;
    eprintln!("frescod-gpu-check: device {:?} isa {:?}, screen {W}x{H}, tolerance {tol}", caps.vendor, caps.isa_name);
    let vm = gpu.create_vm()?;
    let arch = gpu_arch(&vm)?;
    eprintln!("frescod-gpu-check: architecture {}", arch.name());
    let t = Instant::now();
    let mut g = GpuCompute::new(&vm, arch, W, H)?;
    eprintln!("frescod-gpu-check: code uploaded in {} ms", t.elapsed().as_millis());
    let mut cpu = Compute::new(W, H)?;
    let target = vm.alloc((W * H * 4) as u64, BO_VRAM)?;
    let sync = gpu.create_syncobj()?;
    let mut seq = 0u64;
    let (mut pass, mut fail) = (0, 0);
    let mut gpx = vec![0u8; (W * H * 4) as usize];
    // The same scenes twice: one for each renderer (frames do not clone).
    for ((name, f), (_, f2)) in scenes().into_iter().zip(scenes()) {
        let t = Instant::now();
        if let Err(e) = gpu_frame(&mut g, f, &target, &sync, &mut seq) {
            eprintln!("frescod-gpu-check: {name}: FAIL: gpu: {e}");
            fail += 1;
            continue;
        }
        let gpu_ms = t.elapsed().as_millis();
        read_back(&g, &target, &mut gpx)?;
        cpu.render(f2)?;
        let cpx: Vec<u8> = cpu.target.iter().flat_map(|w| w.to_le_bytes()).collect();
        let (n, worst, first) = compare(&cpx, &gpx);
        // Fills each renderer dropped (by window): the entries themselves
        // differ (the gpu renderer repacks every window each frame, the
        // compute one only changed ones, neither drops anything in pixel
        // mode), so count the drops, not the entries (fc-89bae7ab).
        let dropped = |d: &FillDrops| d.iter().map(|(_, v)| v.len()).sum::<usize>();
        let drops = (dropped(&g.fill_drops), dropped(&cpu.fill_drops));
        if worst <= tol && drops.0 == drops.1 {
            eprintln!("frescod-gpu-check: {name}: PASS ({n} pixels differ, max {worst}; gpu {gpu_ms} ms)");
            pass += 1;
        } else {
            eprintln!("frescod-gpu-check: {name}: FAIL: {n} pixels differ, max {worst}, first at {first:?}; fill drops gpu {} cpu {}",
                drops.0, drops.1);
            save_png(&format!("{prefix}-{name}-cpu.png"), &cpx);
            save_png(&format!("{prefix}-{name}-gpu.png"), &gpx);
            fail += 1;
        }
    }
    let q: Vec<String> = g.wins.keys().map(|id| id.to_string()).collect();
    eprintln!("frescod-gpu-check: {pass} passed, {fail} failed (window VMs at the end: {})", q.join(","));
    println!("RESULT: {}", if fail == 0 { "PASS" } else { "FAIL" });
    Ok(if fail == 0 { 0 } else { 1 })
}
