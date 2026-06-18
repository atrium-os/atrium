//! atrium-splash — boot-time splash screen.
//!
//! Renders to `/dev/atrium-bootfb0` (the EFI GOP framebuffer the
//! bootloader handed to the kernel) using a `tiny_skia::Pixmap`.
//! Polls for `/dev/atrium-display0` to appear; when it does, the
//! native GPU driver (atrium-virtio-gpu / atrium-mali / etc.) has
//! taken over the scanout and our writes to the EFI framebuffer are
//! no longer visible — we exit cleanly so frescod takes
//! the screen from there.
//!
//! Visuals: dark background + centered "atrium" wordmark + a slow
//! orbiting indicator dot. No font subsystem yet — the wordmark is
//! a hand-drawn rectangle composition. Lightweight: ~30 fps render
//! into a Pixmap, then a single byte-swap-and-copy into the GOP fb.

use atrium_bootfb::{BootFb, PixelFormat};

use std::path::Path;
use std::time::{Duration, Instant};

use tiny_skia::{
    Color, FillRule, GradientStop, LinearGradient, Paint, PathBuilder, Pixmap, Point, Rect,
    Shader, SpreadMode, Transform,
};

const TARGET_FPS: u64 = 30;
const FRAME_NS: u64 = 1_000_000_000 / TARGET_FPS;
const HANDOFF_PATH: &str = "/dev/atrium-display0";

fn main() -> std::io::Result<()> {
    // Offline mode: render the splash artwork to an RGBA PNG for the
    // KERNEL boot splash. vt(4)'s DEV_SPLASH framework draws a
    // loader-preloaded PNG (loader `splash="..."` → MODINFOMD_SPLASH)
    // and suppresses video console text until a keypress — the proper
    // place for a boot splash (a userland splash runs far too late and
    // vt would overdraw it anyway). This reuses the very same
    // `draw_splash` we verified on the live framebuffer, so the boot
    // splash is pixel-identical to the userland prototype.
    //
    //   atrium-splash --gen-png <path> [WIDTHxHEIGHT]   (default 800x600)
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--gen-png") {
        let path = args
            .get(i + 1)
            .cloned()
            .unwrap_or_else(|| "atrium-splash.png".into());
        let (w, h) = args
            .get(i + 2)
            .and_then(|s| s.split_once('x'))
            .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)))
            .unwrap_or((800u32, 600u32));
        return gen_png(&path, w, h);
    }

    let mut fb = match BootFb::open() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("atrium-splash: cannot open /dev/atrium-bootfb0: {e}");
            eprintln!("atrium-splash: kmod not loaded, or this system has no EFI GOP framebuffer.");
            return Err(e);
        }
    };

    let w = fb.width();
    let h = fb.height();
    let stride = fb.stride() as usize;
    let format = fb.format();

    eprintln!(
        "atrium-splash: {}x{} stride={} format={:?}, painting until {} appears",
        w, h, stride, format, HANDOFF_PATH
    );

    // Render into our own pixmap, then copy out per frame. Pixmap is
    // RGBA8 premultiplied; the copy step swizzles to the GOP layout.
    let mut pixmap = Pixmap::new(w, h)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "pixmap allocate"))?;

    let started = Instant::now();
    let mut next = Instant::now() + Duration::from_nanos(FRAME_NS);
    loop {
        // Handoff: GPU driver came up, scanout is no longer the EFI
        // framebuffer. Stop drawing — frescod takes over.
        if Path::new(HANDOFF_PATH).exists() {
            eprintln!("atrium-splash: {HANDOFF_PATH} appeared — handing off to frescod");
            return Ok(());
        }

        let t = started.elapsed().as_secs_f32();
        draw_splash(&mut pixmap, w, h, t);
        present(&mut fb, &pixmap, format);

        let now = Instant::now();
        if next > now { std::thread::sleep(next - now); }
        next += Duration::from_nanos(FRAME_NS);
    }
}

/// Render one static frame of the splash to an RGBA PNG. The kernel
/// splash is a still image, so we draw at t=0. The loader's `png_open`
/// wants 32-bit RGBA (it sets `splash_info.si_depth = bpp` = 4 bytes,
/// which vt's `vtterm_splash` requires); tiny-skia's `encode_png`
/// produces exactly that.
fn gen_png(path: &str, w: u32, h: u32) -> std::io::Result<()> {
    let mut pixmap = Pixmap::new(w, h)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "pixmap allocate"))?;
    draw_static(&mut pixmap, w, h);
    let png = pixmap
        .encode_png()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("encode png: {e}")))?;
    std::fs::write(path, &png)?;
    eprintln!("atrium-splash: wrote {w}x{h} RGBA splash PNG to {path} ({} bytes)", png.len());
    Ok(())
}

/// The static splash artwork — background, stage panel, wordmark. This
/// is the base image both for the kernel boot splash (a still PNG; the
/// animated indicator is drawn over it by vt(4) each frame) and for the
/// animated live-framebuffer path below.
fn draw_static(pixmap: &mut Pixmap, w: u32, h: u32) {
    pixmap.fill(Color::from_rgba8(0x10, 0x12, 0x1a, 0xff));

    let cx = w as f32 / 2.0;
    let cy = h as f32 / 2.0;

    // Vertical gradient panel as the "stage".
    let pw = (w as f32 * 0.55).min(720.0);
    let ph = pw * 0.30;
    let px = cx - pw / 2.0;
    let py = cy - ph / 2.0;
    let stage = rounded_rect(px, py, pw, ph, 18.0);
    let mut paint = Paint::default();
    paint.shader = LinearGradient::new(
        Point::from_xy(px, py),
        Point::from_xy(px, py + ph),
        vec![
            GradientStop::new(0.0, Color::from_rgba8(0x1c, 0x22, 0x36, 0xff)),
            GradientStop::new(1.0, Color::from_rgba8(0x14, 0x18, 0x22, 0xff)),
        ],
        SpreadMode::Pad,
        Transform::identity(),
    )
    .unwrap_or(Shader::SolidColor(Color::from_rgba8(0x14, 0x18, 0x22, 0xff)));
    paint.anti_alias = true;
    pixmap.fill_path(&stage, &paint, FillRule::Winding, Transform::identity(), None);

    // Wordmark "atrium" — built from rectangles. 7 letter cells of
    // height `lh`, spaced by `lh*0.6`. Each letter is sketched with
    // a few rectangles to suggest its shape; intentionally minimal
    // until we have a real font subsystem here.
    let lh = ph * 0.55;
    let stroke = (lh * 0.13).max(2.0);
    let cell_w = lh * 0.75;
    let gap = lh * 0.20;
    let total_w = 6.0 * cell_w + 5.0 * gap;
    let mut x = cx - total_w / 2.0;
    let y = cy - lh / 2.0;
    let mut white = Paint::default();
    white.shader = Shader::SolidColor(Color::from_rgba8(0xee, 0xee, 0xf2, 0xff));
    white.anti_alias = true;
    for &letter in b"atrium" {
        draw_letter(pixmap, &white, letter, x, y, cell_w, lh, stroke);
        x += cell_w + gap;
    }
}

/// Animated splash for the live-framebuffer path: the static artwork plus
/// an orbiting "still-alive" indicator dot. The KERNEL boot splash is a
/// still image (it omits the dot — a frozen spinner reads as a stray
/// pixel) and gets its motion from vt(4) drawing an animated ball instead.
fn draw_splash(pixmap: &mut Pixmap, w: u32, h: u32, t: f32) {
    draw_static(pixmap, w, h);

    let cx = w as f32 / 2.0;
    let cy = h as f32 / 2.0;
    let pw = (w as f32 * 0.55).min(720.0);
    let ph = pw * 0.30;

    // Orbiting indicator dot just below the wordmark.
    let orbit_r = ph * 0.25;
    let orbit_t = (t * 1.2) % std::f32::consts::TAU;
    let dx = cx + orbit_t.cos() * orbit_r;
    let dy = cy + ph * 0.45 + orbit_t.sin() * orbit_r * 0.25;
    let dot = circle_path(dx, dy, 5.0);
    let mut accent = Paint::default();
    accent.shader = Shader::SolidColor(Color::from_rgba8(0xff, 0xcc, 0x33, 0xff));
    accent.anti_alias = true;
    pixmap.fill_path(&dot, &accent, FillRule::Winding, Transform::identity(), None);
}

fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> tiny_skia::Path {
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish().expect("rounded_rect path")
}

fn circle_path(cx: f32, cy: f32, r: f32) -> tiny_skia::Path {
    const K: f32 = 0.5522847498;
    let kr = r * K;
    let mut pb = PathBuilder::new();
    pb.move_to(cx + r, cy);
    pb.cubic_to(cx + r, cy + kr, cx + kr, cy + r, cx, cy + r);
    pb.cubic_to(cx - kr, cy + r, cx - r, cy + kr, cx - r, cy);
    pb.cubic_to(cx - r, cy - kr, cx - kr, cy - r, cx, cy - r);
    pb.cubic_to(cx + kr, cy - r, cx + r, cy - kr, cx + r, cy);
    pb.close();
    pb.finish().expect("circle path")
}

/// Fill a rect at (x, y, w, h) into pixmap with `paint`.
fn rect_fill(pixmap: &mut Pixmap, paint: &Paint, x: f32, y: f32, w: f32, h: f32) {
    if let Some(r) = Rect::from_xywh(x, y, w, h) {
        let p = PathBuilder::from_rect(r);
        pixmap.fill_path(&p, paint, FillRule::Winding, Transform::identity(), None);
    }
}

/// Hand-drawn letter geometry. Uses a small fixed set of rectangles
/// per glyph to suggest the letter form. Coverage: a, t, r, i, u, m.
fn draw_letter(pixmap: &mut Pixmap, paint: &Paint, ch: u8, x: f32, y: f32, w: f32, h: f32, s: f32) {
    let mut r = |px, py, pw, ph| rect_fill(pixmap, paint, x + px, y + py, pw, ph);
    match ch {
        b'a' => {
            // Canonical single-story 'a': full top bar + full-height right
            // wall + a closed bowl in the lower half, with the left wall ONLY
            // in that lower half — so the UPPER-LEFT is open. That open
            // upper-left is exactly what distinguishes 'a' from 'o' (closed
            // box) and 'b'/'d' (full-height side wall).
            r(0.0, h * 0.45, w, s);             // top bar (full)
            r(w - s, h * 0.45, s, h * 0.55);    // right wall (full x-height)
            r(0.0, h * 0.70, w, s);             // mid bar (bowl top)
            r(0.0, h * 0.70, s, h * 0.30);      // left wall (lower half only)
            r(0.0, h - s, w, s);                // bottom
        }
        b't' => {
            // lowercase t: a short ascender stem with the crossbar at the
            // x-height line — not a full-height capital-T bar.
            r(w * 0.5 - s * 0.5, h * 0.25, s, h * 0.75);   // stem (ascender → bottom)
            r(w * 0.15, h * 0.45, w * 0.6, s);             // crossbar at x-height
        }
        b'r' => {
            r(0.0, h * 0.45, s, h * 0.55);      // stem
            r(0.0, h * 0.45, w, s);             // top
            r(w - s, h * 0.45, s, s * 1.5);     // hook
        }
        b'i' => {
            r(w * 0.5 - s * 0.5, h * 0.45, s, h * 0.55); // stem
            r(w * 0.5 - s * 0.5, h * 0.20, s, s);        // dot
        }
        b'u' => {
            r(0.0, h * 0.45, s, h * 0.55);      // left
            r(w - s, h * 0.45, s, h * 0.55);    // right
            r(0.0, h - s, w, s);                // bottom
        }
        b'm' => {
            r(0.0, h * 0.45, s, h * 0.55);      // left stem
            r(w * 0.5 - s * 0.5, h * 0.45, s, h * 0.55); // mid stem
            r(w - s, h * 0.45, s, h * 0.55);    // right stem
            r(0.0, h * 0.45, w, s);             // top bar
        }
        _ => {}
    }
}

/// Copy our RGBA premultiplied pixmap into the GOP framebuffer in
/// the format the firmware expects. EFI GOP rows may be wider than
/// `width * 4` (`stride` reports the actual byte pitch); we copy
/// row-by-row.
fn present(fb: &mut BootFb, pixmap: &Pixmap, format: PixelFormat) {
    let w = fb.width() as usize;
    let h = fb.height() as usize;
    let stride = fb.stride() as usize;
    let src = pixmap.data();
    let dst = fb.pixels_mut();
    for y in 0..h {
        let s = y * w * 4;
        let d = y * stride;
        let row = &src[s..s + w * 4];
        let out = &mut dst[d..d + w * 4];
        match format {
            // Memory order B,G,R,A (most EFI firmware on x86_64/aarch64).
            PixelFormat::Bgra8 | PixelFormat::Unknown => {
                for i in 0..w {
                    let o = i * 4;
                    out[o]     = row[o + 2];
                    out[o + 1] = row[o + 1];
                    out[o + 2] = row[o];
                    out[o + 3] = row[o + 3];
                }
            }
            PixelFormat::Rgba8 => {
                out.copy_from_slice(row);
            }
        }
    }
}
