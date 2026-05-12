//! `atrium-glyph-stress` — text-heavy scene-complexity sweep.
//!
//! Closer to a realistic desktop workload than `atrium-stress`:
//! emits LINES rows of CHARS-character text per frame, each row a
//! distinct glyph_run node. Each row scrolls horizontally so every
//! frame's pixels change.
//!
//! Usage:
//!   atrium-glyph-stress [SOCK] [LINES] [CHARS]
//!
//! Defaults: /tmp/frescod.sock, LINES=24, CHARS=80 (≈ 1920 glyphs/frame,
//! shape of one terminal window). Run with FRESCOD_UNCAPPED=1 on the
//! daemon side to measure the rasterisation ceiling.

use fresco_client::Connection;
use fresco_protocol::{GlyphInstance, GlyphRunParams, TextureFormat};
use fresco_text::shape_and_rasterize;

use std::time::{Duration, Instant};

const FONT_PATH: &str = "/mnt/host/test-assets/DejaVuSansMono.ttf";
const SIZE_PX:   f32  = 16.0;
const VIEW_W:    f32  = 1280.0;
const FPS:       u64  = 60;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let sock  = args.next().unwrap_or_else(|| "/tmp/frescod.sock".to_string());
    let lines: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(24);
    let chars: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(80);

    // Shape an 80-character template once. Reuse the same atlas
    // across all rows. Pick characters that exercise the full ASCII
    // glyph cache rather than 80 spaces.
    let template: String = (0..chars).map(|i| {
        // 32..126 is printable ASCII (' ' .. '~'). Cycle through.
        let c = (33 + (i % 94)) as u8;
        c as char
    }).collect();
    let font = std::fs::read(FONT_PATH)?;
    let atlas = shape_and_rasterize(&font, &template, SIZE_PX)?;
    eprintln!(
        "atrium-glyph-stress: shaped {} glyphs / atlas {}×{} / advance {:.1}px",
        atlas.glyphs.len(), atlas.width, atlas.height, atlas.advance,
    );

    let mut conn = Connection::connect(&sock)?;
    eprintln!("atrium-glyph-stress: {lines} rows × {chars} cols = {} glyphs/frame",
              lines as usize * atlas.glyphs.len());

    // Upload atlas once. fresco-text writes R8 coverage; the texture
    // engine multiplies against the run's tint colour.
    const ATLAS_SLOT: u32 = 100;
    let r8 = atlas.r8_pixels();
    let hash = conn.upload_blob(&r8)?;
    conn.slot_set_texture(
        ATLAS_SLOT, hash, atlas.width, atlas.height,
        TextureFormat::R8Unorm,
    )?;

    let started = Instant::now();
    let frame_dur = Duration::from_nanos(1_000_000_000 / FPS);
    let mut next = Instant::now() + frame_dur;
    let mut frame: u64 = 0;

    let row_h = SIZE_PX * 1.2;
    let span = VIEW_W - atlas.advance;

    loop {
        let t = started.elapsed().as_secs_f32();
        conn.scene_frame_begin()?;
        for row in 0..lines {
            // Per-row scroll offset so every row advances at a
            // different rate; the renderer can't cache identical
            // pixels.
            let phase = row as f32 * 0.17;
            let scroll = triangle(t * 0.3 + phase) * span;

            let params = build_run(
                &atlas,
                scroll,
                row_h * (row as f32 + 1.0),
                ATLAS_SLOT,
            );
            conn.scene_node_glyph_run(row, params)?;
        }
        conn.scene_frame_end()?;
        frame += 1;
        if frame % (FPS * 5) == 0 {
            eprintln!("atrium-glyph-stress: frame {frame}");
        }

        let now = Instant::now();
        if next > now { std::thread::sleep(next - now); }
        next += frame_dur;
    }
}

fn build_run(
    atlas: &fresco_text::GlyphAtlas,
    x: f32, y: f32,
    atlas_slot_id: u32,
) -> GlyphRunParams {
    let glyphs: Vec<GlyphInstance> = atlas.glyphs.iter().map(|q| {
        let au = (q.u0 * atlas.width  as f32).round() as u32;
        let av = (q.v0 * atlas.height as f32).round() as u32;
        let aw = ((q.u1 - q.u0) * atlas.width  as f32).round() as u32;
        let ah = ((q.v1 - q.v0) * atlas.height as f32).round() as u32;
        GlyphInstance {
            dx: q.dx0, dy: 0.0,
            atlas_u: au, atlas_v: av,
            atlas_w: aw, atlas_h: ah,
            bearing_x: 0.0, bearing_y: -q.dy0,
        }
    }).collect();
    GlyphRunParams {
        x, y,
        atlas_slot_id,
        atlas_width: atlas.width, atlas_height: atlas.height,
        r: 1.0, g: 1.0, b: 1.0, a: 1.0,
        glyphs,
    }
}

fn triangle(t: f32) -> f32 {
    let f = t - t.floor();
    if f < 0.5 { 2.0 * f } else { 2.0 - 2.0 * f }
}
