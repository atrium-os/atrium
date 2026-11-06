//! scroll_bench <normalized.html> [--hz 60|120] [--pattern fling|flings|page|jump] [--secs N]
//!              [--scale 1|2] [--csv out.csv] [--inject-stall MS@T]
//!              [--compose full|strip] [--fast PX_PER_S] [--blur]
//!
//! ★ THE SCROLL HARNESS (backend §4.8, step 2): what scrolling a document
//! costs, frame by frame, with the TAIL and the TIMELINE reported — not just
//! a distribution. A scroll that sticks for seconds and then catches up
//! barely moves a p99 and vanishes in an average (asked by the user,
//! 2026-11-06), so every stall is listed with its start, length and catch-up.
//!
//! A replay on a virtual clock with REAL costs: each vsync samples the
//! scripted scroll target; the compositor's per-frame work is actually run —
//! cull the scene to the viewport, translate it into a Fresco frame (rects,
//! filled paths, text through a glyph atlas rasterised on first sight,
//! images as textures), pack it as frescod packs a window, and raster it on
//! frescod's CPU kernels (Tier-2, the Cranelift-compiled SPIR-V) — and the
//! measured time advances the clock. A frame presents at the first vsync
//! after its work ends; until then the previous frame stays up.
//!
//! What it does not model, stated: the GPU path, the socket, the scene
//! server's envelope; clips, groups and transforms are not applied (their
//! nodes are drawn as if unclipped). It measures the structure of the work:
//! today, one whole-page scene the compositor walks every frame.
//!
//! `--compose strip`: the window keeps its last frame, a scroll shifts it and
//! only the exposed rows are rastered (`strip_frame`); above `--fast` px/s
//! (2,500) new strips draw text as bars (greeked) and, with `--blur`, the
//! window gets a velocity-scaled vertical motion blur; when the fling slows,
//! the window is redrawn sharp once (`sharp_after_slowdown_ms`).
//!
//! `--inject-stall MS@T`: the frame starting at or after T ms takes MS ms
//! longer — the POSITIVE CONTROL: a harness that reports no stalls must be
//! shown to report one it was given (and its catch-up).
//!
//! Output: one summary block (TSV-friendly `key value` lines), and with
//! `--csv` one row per vsync: `t_ms target_y shown_y age_ms cost_ms missed`.

use fresco_compute::*;
use navigator_render::{fontset::FontSet, html::render_html, Scene as Nsg};
use navigator_style::cascade::Env;
use std::collections::HashMap;
use std::time::Instant;

const PX: i64 = 64;

fn srgb(c: u8) -> f32 { let c = c as f32 / 255.0; if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) } }
fn rgba(c: u32) -> [f32; 4] { [srgb((c >> 24) as u8), srgb((c >> 16) as u8), srgb((c >> 8) as u8), (c & 255) as f32 / 255.0] }

/// frescod's `translate_fill` (as in tests/fresco_integration.rs).
fn translate_fill(p: &fresco_protocol::FillParams, dy: f32) -> fill::Fill {
    use fill::{Fill, FillRule, Gradient, GradientShape};
    use fresco_protocol::FillPaint;
    let stops = |s: &[fresco_protocol::GradientStop]| s.iter().map(|s| (s.offset, [s.r, s.g, s.b, s.a])).collect();
    let (color, gradient) = match &p.paint {
        FillPaint::Solid { r, g, b, a } => ([*r, *g, *b, *a * p.opacity], None),
        FillPaint::Linear { x0, y0, x1, y1, stops: s } => ([0.0, 0.0, 0.0, p.opacity], Some(Gradient { shape: GradientShape::Linear { p0: [*x0, *y0], p1: [*x1, *y1] }, stops: stops(s) })),
        FillPaint::Radial { cx, cy, radius, stops: s } => ([0.0, 0.0, 0.0, p.opacity], Some(Gradient { shape: GradientShape::Radial { center: [*cx, *cy], radius: *radius }, stops: stops(s) })),
    };
    let mut t = p.transform;
    t[5] -= dy;
    Fill { verbs: p.verbs.clone(), points: p.points.clone(), transform: t,
           rule: if p.rule == 1 { FillRule::EvenOdd } else { FillRule::NonZero }, color, clip: p.clip.map(|c| [c[0], c[1] - dy, c[2], c[3] - dy]), gradient }
}

/// A glyph atlas, R8, rasterised on first sight (as a compositor's is).
struct Atlas { w: u32, h: u32, data: Vec<u8>, x: u32, y: u32, row: u32, slots: HashMap<(usize, i64, u16), Option<([f32; 4], [f32; 2])>> }

impl Atlas {
    fn new() -> Self { Atlas { w: 2048, h: 2048, data: vec![0; 2048 * 2048], x: 0, y: 0, row: 0, slots: HashMap::new() } }
    fn glyph(&mut self, fonts: &FontSet, ctx: &mut swash::scale::ScaleContext, face: usize, size: i64, gid: u16) -> Option<([f32; 4], [f32; 2])> {
        if let Some(s) = self.slots.get(&(face, size, gid)) { return *s }
        let font = swash::FontRef::from_index(&fonts.faces[face].bytes, 0)?;
        let mut scaler = ctx.builder(font).size(size as f32 / PX as f32).hint(false).build();
        let img = swash::scale::Render::new(&[swash::scale::Source::Outline]).format(swash::zeno::Format::Alpha)
            .render(&mut scaler, swash::GlyphId::from(gid));
        let slot = img.and_then(|img| {
            let (gw, gh) = (img.placement.width, img.placement.height);
            if gw == 0 || gh == 0 { return None }
            if self.x + gw > self.w { self.x = 0; self.y += self.row + 1; self.row = 0 }
            if self.y + gh > self.h { return None } // full: the glyph is skipped (counted below)
            for yy in 0..gh { for xx in 0..gw {
                self.data[((self.y + yy) * self.w + self.x + xx) as usize] = img.data[(yy * gw + xx) as usize];
            } }
            let uv = [self.x as f32, self.y as f32, gw as f32, gh as f32];
            self.x += gw + 1;
            self.row = self.row.max(gh);
            Some((uv, [img.placement.left as f32, img.placement.top as f32]))
        });
        self.slots.insert((face, size, gid), slot);
        slot
    }
}

impl Compositor<'_> {
    /// ★ SHIFT, THEN DRAW ONLY THE STRIP (asked by the user, 2026-11-06):
    /// the window keeps its last frame; a scroll moves those pixels by the
    /// whole-pixel delta and rasters only the rows it exposed. A delta as big
    /// as the window, or the first frame, redraws it all. `greek`: the new
    /// strip's text drawn as bars; `blur`: a vertical motion blur scaled to
    /// the velocity over the composed window (the CPU stands in for what is
    /// one GPU pass). Returns the rows rastered.
    fn strip_frame(&mut self, y: i64, greek: bool, blur_px: u32, force_full: bool) -> u32 {
        let (w, h) = (self.w as usize, self.h as usize);
        let y_dev = (y as f64 * self.scale as f64 / PX as f64).round() as i64;
        if self.buf.len() != w * h { self.buf = vec![0; w * h]; self.buf_y = None }
        let dy = self.buf_y.map(|b| y_dev - b);
        let rows = match dy {
            Some(d) if !force_full && d.unsigned_abs() < h as u64 => {
                let d = d as isize;
                if d > 0 {
                    self.buf.copy_within(d as usize * w.., 0);
                    let start = (h - d as usize) as i64;
                    self.region(((y_dev + start) as f64 * PX as f64 / self.scale as f64) as i64, d as u32, greek);
                    let n = d as usize * w;
                    self.buf[(h - d as usize) * w..].copy_from_slice(&self.target[..n]);
                    d as u32
                } else if d < 0 {
                    let d = (-d) as usize;
                    self.buf.copy_within(..(h - d) * w, d * w);
                    self.region(y, d as u32, greek);
                    self.buf[..d * w].copy_from_slice(&self.target[..d * w]);
                    d as u32
                } else { 0 }
            }
            _ => {
                self.region(y, h as u32, greek);
                self.buf.copy_from_slice(&self.target[..w * h]);
                h as u32
            }
        };
        self.buf_y = Some(y_dev);
        if greek && rows > 0 { self.blurred = true }
        if blur_px > 0 { vertical_blur(&mut self.buf, w, h, blur_px as usize); self.blurred = true }
        rows
    }
}

impl Compositor<'_> {
    /// ★ SHARPEN PROGRESSIVELY: once a fling slows, the window it drew
    /// greeked or blurred is queued in `SHARPEN_SLICES` slices (page rows),
    /// and one is re-rastered sharp per frame — twelve cheap frames (100 ms
    /// at 120 Hz) instead of one full redraw that misses a vsync. Thirds
    /// missed 2% and sixths 3% on the HTML standard at 120 Hz: every frame
    /// also pays ~3 ms to walk its 935,000-node scene, so a 100-row slice
    /// cost ~9 ms. Rows that scrolled out are dropped.
    fn queue_sharpen(&mut self) {
        let (h, y) = (self.h as i64, self.buf_y.unwrap_or(0));
        let slice = (h + SHARPEN_SLICES - 1) / SHARPEN_SLICES;
        self.sharpen = (0..SHARPEN_SLICES).map(|k| (y + k * slice, slice.min(h - k * slice).max(0) as u32)).filter(|r| r.1 > 0).collect();
    }

    /// One queued slice, re-rastered sharp where it is in the window now.
    /// Returns the rows drawn.
    fn sharpen_step(&mut self) -> u32 {
        let Some((py, rows)) = (!self.sharpen.is_empty()).then(|| self.sharpen.remove(0)) else { return 0 };
        let (w, h) = (self.w as usize, self.h as i64);
        let top = (py - self.buf_y.unwrap_or(0)).max(0);
        let bot = (py + rows as i64 - self.buf_y.unwrap_or(0)).min(h);
        if bot <= top { return 0 }
        let n = (bot - top) as u32;
        let page = self.buf_y.unwrap_or(0) + top;
        self.region((page as f64 * PX as f64 / self.scale as f64) as i64, n, false);
        self.buf[top as usize * w..bot as usize * w].copy_from_slice(&self.target[..n as usize * w]);
        if self.sharpen.is_empty() { self.blurred = false }
        n
    }
}

/// How many slices a settled window is sharpened in, one per frame.
const SHARPEN_SLICES: i64 = 12;

/// A vertical box blur of radius `r` over a packed RGBA8 window: the
/// motion blur a fast fling is drawn with.
fn vertical_blur(buf: &mut [u32], w: usize, h: usize, r: usize) {
    let src = buf.to_vec();
    let n = (2 * r + 1) as u32;
    for x in 0..w {
        let mut acc = [0u32; 4];
        let px = |y: isize| src[(y.clamp(0, h as isize - 1) as usize) * w + x];
        for y in -(r as isize)..=(r as isize) { let p = px(y); for c in 0..4 { acc[c] += (p >> (8 * c)) & 255 } }
        for y in 0..h as isize {
            buf[y as usize * w + x] = (0..4).fold(0, |o, c| o | ((acc[c] / n) << (8 * c)));
            let (a, b) = (px(y + r as isize + 1), px(y - r as isize));
            for c in 0..4 { acc[c] = acc[c] + ((a >> (8 * c)) & 255) - ((b >> (8 * c)) & 255) }
        }
    }
}

/// Whether greeked rows may still be on screen (none queued, but greek
/// strips drawn since the last full sharpen): verification waits for sharp.
fn greek_frames_pending(c: &Compositor) -> bool { c.blurred }

/// The node's vertical extent in the scene, for culling.
fn extent(s: &Nsg, kind: u8, i: usize) -> Option<(i64, i64)> {
    match kind {
        0 => { let r = &s.rects[i]; Some((r.y, r.y + r.h)) }
        1 => { let r = &s.runs[i]; Some((r.y - r.size, r.y + r.size / 3)) }
        5 => { let a = &s.images[i].area; Some((a.y, a.y + a.h)) }
        6 => {
            let p = &s.paths[i];
            let (mut lo, mut hi) = (i64::MAX, i64::MIN);
            for &(x, y) in &p.points {
                let ty = (p.transform[1] as i128 * x as i128 + p.transform[3] as i128 * y as i128) / 65536 + p.transform[5] as i128;
                lo = lo.min(ty as i64); hi = hi.max(ty as i64);
            }
            (lo <= hi).then_some((lo, hi))
        }
        _ => None,
    }
}

struct Compositor<'a> {
    nsg: &'a Nsg, fonts: &'a FontSet, ctx: swash::scale::ScaleContext, atlas: Atlas,
    textures: HashMap<String, usize>, tex_store: Vec<Texture>, t2: tier2::Tier2, w: u32, h: u32, scale: f32,
    target: Vec<u32>, glyphs_skipped: u64,
    /// `--compose strip`: the window as last composed, and where it was (device px).
    buf: Vec<u32>, buf_y: Option<i64>, blurred: bool,
    /// Rows still drawn greeked or blurred, as page rows (device px), to
    /// re-raster sharp a slice per frame once a fling slows.
    sharpen: Vec<(i64, u32)>,
}

impl Compositor<'_> {
    /// One frame at scroll `y` (1/64 px): cull, translate, pack, raster.
    fn frame(&mut self, y: i64) { self.region(y, self.h, false) }

    /// `rows` device rows of the page starting at `y` (1/64 CSS px) into
    /// `self.target` (`self.w` × `rows`). With `greek`, text is drawn as a
    /// bar per run, the cheap stand-in for text a fling moves too fast to read.
    fn region(&mut self, y: i64, rows: u32, greek: bool) {
        let (vw, vh) = (self.w as f32 / self.scale, rows as f32 / self.scale);
        let (y0, y1) = (y, y + (vh * PX as f32) as i64);
        let dy = y as f32 / PX as f32;
        let s = self.scale;
        let mut scene = Scene::default();
        let mut runs: Vec<GlyphRun> = vec![];
        let mut quads: HashMap<usize, Vec<[f32; 4]>> = HashMap::new();
        for &(kind, i) in &self.nsg.order {
            let Some((a, b)) = extent(self.nsg, kind, i) else { continue };
            if b < y0 || a > y1 { continue }
            match kind {
                0 => {
                    let r = &self.nsg.rects[i];
                    if r.ring > 0 { continue } // rings (borders) approximated away
                    scene.rects.push(Rect { position: [r.x as f32 / PX as f32 * s, (r.y as f32 / PX as f32 - dy) * s],
                        size: [r.w as f32 / PX as f32 * s, r.h as f32 / PX as f32 * s], color: rgba(r.rgba), radius: r.radii[0] as f32 / PX as f32 * s });
                }
                1 if greek => {
                    let r = &self.nsg.runs[i];
                    let w = r.glyphs.last().map(|g| g.1).unwrap_or(0) + r.size / 2;
                    let mut col = rgba(r.rgba);
                    col[3] *= 0.5;
                    scene.rects.push(Rect { position: [r.x as f32 / PX as f32 * s, ((r.y - r.size * 45 / 100) as f32 / PX as f32 - dy) * s],
                        size: [w as f32 / PX as f32 * s, (r.size * 40 / 100) as f32 / PX as f32 * s], color: col, radius: 0.0 });
                }
                1 => {
                    let r = &self.nsg.runs[i];
                    let size = (r.size as f32 * s) as i64;
                    let mut glyphs = vec![];
                    for &(gid, gx, gy) in &r.glyphs {
                        match self.atlas.glyph(self.fonts, &mut self.ctx, r.face, size, gid) {
                            Some((uv, bearing)) => glyphs.push(Glyph { d_offset: [gx as f32 / PX as f32 * s, gy as f32 / PX as f32 * s], atlas_uv: uv, bearing }),
                            None => self.glyphs_skipped += 1,
                        }
                    }
                    runs.push(GlyphRun { origin: [r.x as f32 / PX as f32 * s, (r.y as f32 / PX as f32 - dy) * s], color: rgba(r.rgba), glyphs });
                }
                5 => {
                    let im = &self.nsg.images[i];
                    let t = match self.textures.get(&im.address) { Some(t) => *t, None => {
                        // A decoded image of its painted size, at most 512 px a side.
                        let (tw, th) = (((im.area.w / PX) as u32).clamp(1, 512), ((im.area.h / PX) as u32).clamp(1, 512));
                        self.tex_store.push(Texture { width: tw, height: th, format: TexFormat::Rgba8Srgb, data: vec![128; (tw * th * 4) as usize] });
                        self.textures.insert(im.address.clone(), self.tex_store.len() - 1);
                        self.tex_store.len() - 1
                    } };
                    let a = &im.area;
                    quads.entry(t).or_default().push([a.x as f32 / PX as f32 * s, (a.y as f32 / PX as f32 - dy) * s, a.w as f32 / PX as f32 * s, a.h as f32 / PX as f32 * s]);
                }
                6 => {
                    let fp = navigator_render::path::fill_params(&self.nsg.paths[i], None);
                    let mut f = translate_fill(&fp, dy);
                    if s != 1.0 { for v in &mut f.transform { *v *= s } }
                    scene.fills.push(f);
                }
                _ => {}
            }
        }
        let _ = vw;
        // Textures: the images first, then the atlas, moved in and out.
        scene.textures = std::mem::take(&mut self.tex_store);
        for (t, q) in quads { scene.texture_batches.push(TextureBatch { texture: t, quads: q }) }
        let atlas_i = scene.textures.len();
        scene.textures.push(Texture { width: self.atlas.w, height: self.atlas.h, format: TexFormat::R8, data: std::mem::take(&mut self.atlas.data) });
        scene.glyph_batches.push(GlyphBatch { atlas: atlas_i, runs });
        let (frame, _) = Frame::pack_report(&scene, self.w, rows.max(1), self.w);
        self.target.resize(frame.target_words(), 0);
        let _ = self.t2.render(&frame, &mut self.target);
        // Give the textures back.
        let mut tex = std::mem::take(&mut scene.textures);
        self.atlas.data = tex.pop().expect("atlas").data;
        self.tex_store = tex;
    }
}

/// The scripted scroll target at `t` ms (1/64 px), clamped to the page.
fn target(pattern: &str, t: f64, max_y: f64, vh: f64) -> f64 {
    let y = match pattern {
        // One fling: 4,000 px/s decaying with a 0.6 s time constant.
        "fling" => 4000.0 * 0.6 * (1.0 - (-t / 600.0).exp()),
        // A fling every 1.5 s, each from where the last stopped.
        "flings" => { let k = (t / 1500.0).floor(); let u = t - k * 1500.0;
                      (k * 4000.0 * 0.6 * (1.0 - (-1500.0f64 / 600.0).exp())) + 4000.0 * 0.6 * (1.0 - (-u / 600.0).exp()) }
        // Page-down every 250 ms.
        "page" => (t / 250.0).floor() * vh,
        // Jump to the end at 1 s, back to the top at 4 s.
        "jump" => if t < 1000.0 { 0.0 } else if t < 4000.0 { max_y } else { 0.0 },
        p => panic!("pattern {p}"),
    };
    y.clamp(0.0, max_y) * PX as f64
}

fn q(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() { return 0.0 }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[(((v.len() - 1) as f64) * p).round() as usize]
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |n: &str| args.iter().position(|a| a == n).and_then(|i| args.get(i + 1)).cloned();
    let hz: f64 = opt("--hz").map(|v| v.parse().unwrap()).unwrap_or(60.0);
    let pattern = opt("--pattern").unwrap_or_else(|| "flings".into());
    let secs: f64 = opt("--secs").map(|v| v.parse().unwrap()).unwrap_or(8.0);
    let scale: f32 = opt("--scale").map(|v| v.parse().unwrap()).unwrap_or(1.0);
    let strip = opt("--compose").as_deref() == Some("strip");
    let fast: f64 = opt("--fast").map(|v| v.parse().unwrap()).unwrap_or(2500.0);
    let blur = args.iter().any(|a| a == "--blur");
    let verify = args.iter().any(|a| a == "--verify");
    let (mut verified, mut verify_bad) = (0usize, 0usize);
    let mut inject: Option<(f64, f64)> = opt("--inject-stall").map(|v| {
        let (ms, at) = v.split_once('@').expect("MS@T");
        (ms.parse().unwrap(), at.parse().unwrap())
    });
    let html = std::fs::read_to_string(&args[0]).expect("normalized html");
    let fonts = FontSet::load().expect("fonts");
    let env = Env::default();
    let t0 = Instant::now();
    let o = render_html(&html, &fonts, &env);
    let first_ms = t0.elapsed().as_secs_f64() * 1e3;
    let nsg = o.scene;
    let (vw, vh) = (env.width_px, env.height_px);
    let max_y = ((nsg.height as f64 / PX as f64) - vh).max(0.0);
    let dir = tempfile::tempdir().unwrap();
    tier2::Tier2::seed_cache(dir.path()).expect("cranelift");
    let t2 = tier2::Tier2::new(dir.path().to_path_buf(), "/nonexistent/atrium-spv-compile".into()).expect("tier2");
    let mut c = Compositor { nsg: &nsg, fonts: &fonts, ctx: swash::scale::ScaleContext::new(), atlas: Atlas::new(), textures: HashMap::new(),
                             tex_store: vec![], t2, w: (vw as f32 * scale) as u32, h: (vh as f32 * scale) as u32, scale, target: vec![], glyphs_skipped: 0,
                             buf: vec![], buf_y: None, blurred: false, sharpen: vec![] };
    // ---- The replay. ----
    let period = 1000.0 / hz;
    let frames = (secs * hz) as usize;
    let mut free_at = 0.0f64;                  // when the compositor is idle again
    let mut pending: Option<(f64, f64, f64)> = None; // (present at, y, sampled at)
    let (mut shown_y, mut shown_sampled) = (0.0f64, 0.0f64);
    let mut costs: Vec<f64> = vec![];
    let (mut prev_goal, mut greek_on, mut greek_frames, mut rows_total, mut sharp_waits) = (0.0f64, false, 0usize, 0u64, Vec::<f64>::new());
    let mut slow_since: Option<f64> = None;
    let mut rows: Vec<(f64, f64, f64, f64, f64, bool)> = vec![];
    let (mut missed, mut run, mut longest_run) = (0usize, 0usize, 0usize);
    for k in 0..frames {
        let t = k as f64 * period;
        if let Some((at, y, sampled)) = pending { if at <= t + 1e-9 { shown_y = y; shown_sampled = sampled; pending = None } }
        let goal = target(&pattern, t, max_y, vh);
        let mut cost = 0.0;
        let busy = free_at > t + 1e-9;
        if !busy {
            // A new frame starts at this vsync, for the target as it is now.
            let s = Instant::now();
            if strip {
                // Velocity, px/s, from the targets of consecutive vsyncs.
                let v = (goal - prev_goal).abs() / PX as f64 * hz;
                let fast_now = v > fast;
                if !fast_now && slow_since.is_none() && greek_on { slow_since = Some(t) }
                // Settled: the window drawn greeked or blurred is queued, and
                // sharpened a third per frame (`queue_sharpen`).
                let settle = greek_on && !fast_now;
                let blur_px = if blur && fast_now { ((v / 1000.0) * 2.0).min(12.0) as u32 } else { 0 };
                rows_total += c.strip_frame(goal as i64, fast_now, blur_px, false) as u64;
                if fast_now { greek_frames += 1; c.sharpen.clear() }
                if settle { c.queue_sharpen() }
                if !fast_now && !c.sharpen.is_empty() {
                    rows_total += c.sharpen_step() as u64;
                    if c.sharpen.is_empty() { if let Some(s0) = slow_since.take() { sharp_waits.push(t + period - s0) } }
                }
                greek_on = fast_now;
                // ★ The composed window must be what a full redraw would draw
                // (outside a fling, where it is drawn sharp): checked, not
                // assumed, off the clock.
                if verify && !fast_now && !c.blurred && c.sharpen.is_empty() && !greek_frames_pending(&c) {
                    let elapsed = s.elapsed();
                    let y_dev = c.buf_y.expect("composed");
                    c.region(y_dev * PX / c.scale as i64, c.h, false);
                    let n = (c.w * c.h) as usize;
                    let bad = c.buf.iter().zip(&c.target[..n]).filter(|(a, b)| a != b).count();
                    verified += 1;
                    if bad > 0 { verify_bad += 1 }
                    let _ = elapsed;
                }
            } else {
                c.frame(goal as i64);
            }
            cost = s.elapsed().as_secs_f64() * 1e3;
            if verify { cost = 0.0 } // timings are not measured in a verify run
            if let Some((ms, at)) = inject { if t >= at { cost += ms; inject = None } }
            costs.push(cost);
            free_at = t + cost;
            let present = (free_at / period).ceil().max(k as f64 + 1.0) * period;
            pending = Some((present, goal, t));
        }
        // A vsync with nothing new to show while the target moved is a MISS.
        let moved = (goal - shown_y).abs() > 0.5;
        let miss = moved && pending.is_some_and(|(at, ..)| at > t + 1e-9) && busy;
        if miss { missed += 1; run += 1; longest_run = longest_run.max(run) } else if !busy { run = 0 }
        prev_goal = goal;
        let age = if (goal - shown_y).abs() <= 0.5 { 0.0 } else { t - shown_sampled };
        rows.push((t, goal / PX as f64, shown_y / PX as f64, age, cost, miss));
    }
    // ---- The report. ----
    let mut c2 = costs.clone();
    let deltas: Vec<f64> = costs.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
    let mut d2 = deltas.clone();
    // Stalls: a stretch where what is shown is older than 100 ms, listed with
    // its start, length, worst age, and catch-up (from its worst moment until
    // the age is back under two frames).
    let mut stalls: Vec<(f64, f64, f64, f64)> = vec![];
    let mut i = 0;
    while i < rows.len() {
        if rows[i].3 > 100.0 {
            let start = rows[i].0;
            let (mut worst, mut worst_t) = (0.0f64, start);
            let mut j = i;
            while j < rows.len() && rows[j].3 > 2.0 * period { if rows[j].3 > worst { worst = rows[j].3; worst_t = rows[j].0 } j += 1 }
            let end = rows.get(j).map(|r| r.0).unwrap_or(rows.last().unwrap().0);
            stalls.push((start, end - start, worst, end - worst_t));
            i = j.max(i + 1);
        } else { i += 1 }
    }
    let worst_window = (0..rows.len()).map(|a| {
        let b = rows.partition_point(|r| r.0 < rows[a].0 + 1000.0);
        let w: Vec<f64> = rows[a..b].iter().map(|r| r.3).collect();
        (w.iter().sum::<f64>() / w.len().max(1) as f64, rows[a].0)
    }).fold((0.0, 0.0), |m, x| if x.0 > m.0 { x } else { m });
    let max_age = rows.iter().map(|r| r.3).fold(0.0, f64::max);
    let max_lag_px = rows.iter().map(|r| (r.1 - r.2).abs()).fold(0.0, f64::max);
    println!("doc\t{}", args[0]);
    println!("pattern\t{pattern}\thz\t{hz}\tsecs\t{secs}\tscale\t{scale}");
    println!("scene\tnodes {}\textent_px {}\tfirst_display_ms {first_ms:.0}", nsg.order.len(), nsg.height / PX);
    println!("frame_ms\tp50 {:.2}\tp99 {:.2}\tp99.9 {:.2}\tmax {:.2}\tframes {}", q(&mut c2, 0.5), q(&mut c2, 0.99), q(&mut c2, 0.999), q(&mut c2, 1.0), costs.len());
    println!("jitter\tdelta_p99_ms {:.2}\tmissed {} ({:.1}%)\tlongest_missed_run {} frames ({:.0} ms)", q(&mut d2, 0.99), missed, 100.0 * missed as f64 / frames as f64, longest_run, longest_run as f64 * period);
    println!("lag\tmax_age_ms {max_age:.0}\tmax_px {max_lag_px:.0}\tworst_1s_window_mean_age_ms {:.0} at {:.0} ms", worst_window.0, worst_window.1);
    let over = |l: f64| stalls.iter().filter(|s| s.2 > l).count();
    println!("stalls\tover_100ms {}\tover_500ms {}\tover_1s {}", over(100.0), over(500.0), over(1000.0));
    for s in &stalls { println!("stall\tstart_ms {:.0}\tlength_ms {:.0}\tworst_age_ms {:.0}\tcatch_up_ms {:.0}", s.0, s.1, s.2, s.3) }
    if verify { println!("verify\tframes_checked {verified}\tframes_differing {verify_bad}") }
    if strip {
        println!("compose\tstrip\trows_rastered_per_frame {:.1} (window {})\tgreeked_frames {greek_frames}\tsharp_after_slowdown_ms {}", rows_total as f64 / costs.len().max(1) as f64, c.h,
                 if sharp_waits.is_empty() { "-".into() } else { format!("max {:.0}", sharp_waits.iter().cloned().fold(0.0, f64::max)) });
    }
    if c.glyphs_skipped > 0 { println!("note\tatlas full: {} glyph draws skipped", c.glyphs_skipped) }
    if let Some(p) = opt("--csv") {
        let mut out = String::from("t_ms,target_y,shown_y,age_ms,cost_ms,missed\n");
        for r in &rows { out.push_str(&format!("{:.2},{:.1},{:.1},{:.1},{:.2},{}\n", r.0, r.1, r.2, r.3, r.4, r.5 as u8)) }
        std::fs::write(p, out).unwrap();
    }
}
