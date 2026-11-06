//! atrium-span-probe — a test game for spanning (fresco-span.md S5).
//!
//! It fills its window with 8 px columns whose colour encodes the column's
//! index across the whole window ([`stripe_rgb`]), so a screen dump of each
//! monitor shows which part of the window it scans out: a monitor whose
//! left edge shows column `x / 8` starts at window x. Across a span the
//! members must show the window continuously, skipping only the hidden
//! bezel strips the span event names. A small square runs along the bottom
//! of the window at 30 fps so frames keep flowing (and a direct-scanout
//! path sees damage, not just whole frames).
//!
//! Each tile it is told about (`EV_WINDOW_SPAN`) gets a 4 px white frame
//! in the top 40 rows (the "bezel lines" a real game would draw its HUD
//! inside), and it logs every span event, so a test can match what it was
//! told with what the monitors show.
//!
//! Usage: atrium-span-probe [fresco socket]

use std::time::{Duration, Instant};

use fresco_client::{Connection, Event};
use fresco_protocol::{RectParams, WindowHints};

/// Columns this wide carry one index.
pub const STRIPE: u32 = 8;
/// Rows at the top kept for the tile frames.
const FRAME_ROWS: f32 = 40.0;
const SQUARE: u32 = 1_000_000;
const TILE_LINES: u32 = 2_000_000;

/// The colour of stripe `i`: each channel holds 4 bits of it, in the middle
/// of a 16-step band, so a dump decodes it with up to 7 steps of error.
pub fn stripe_rgb(i: u32) -> [u8; 3] {
    let c = |n: u32| ((n & 15) * 16 + 8) as u8;
    [c(i), c(i >> 4), c(i >> 8)]
}

/// The stripe a dumped pixel shows.
pub fn stripe_of(rgb: [u8; 3]) -> u32 {
    let n = |v: u8| u32::from(v / 16);
    n(rgb[0]) | n(rgb[1]) << 4 | n(rgb[2]) << 8
}

/// A rect that shows as the sRGB bytes `rgb`: scene colours are linear,
/// and the screen is sRGB-encoded on the way out.
fn rect(x: f32, y: f32, w: f32, h: f32, rgb: [u8; 3]) -> RectParams {
    let lin = |v: u8| {
        let c = f32::from(v) / 255.0;
        if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    };
    RectParams { x, y, w, h, r: lin(rgb[0]), g: lin(rgb[1]), b: lin(rgb[2]), a: 1.0, radius: 0.0 }
}

struct Probe {
    w: u32,
    h: u32,
    stripes: u32,
    lines: u32,
    tiles: Vec<[u32; 4]>,
}

impl Probe {
    fn draw_all(&mut self, conn: &mut Connection) -> std::io::Result<()> {
        let n = self.w.div_ceil(STRIPE);
        for i in 0..n {
            conn.scene_node_rect(1 + i, rect((i * STRIPE) as f32, FRAME_ROWS, STRIPE as f32, self.h as f32 - FRAME_ROWS, stripe_rgb(i)))?;
        }
        for i in n..self.stripes {
            conn.scene_node_clear(1 + i)?;
        }
        self.stripes = n;
        self.draw_tiles(conn)
    }

    fn draw_tiles(&mut self, conn: &mut Connection) -> std::io::Result<()> {
        let mut k = 0;
        conn.scene_node_rect(TILE_LINES, rect(0.0, 0.0, self.w as f32, FRAME_ROWS, [26; 3]))?;
        for t in &self.tiles {
            let [x, y, w, _] = t.map(|v| v as f32);
            if y > 0.0 {
                continue; // only the top row of tiles carries frames
            }
            for r in [rect(x, 0.0, 4.0, FRAME_ROWS, [255; 3]), rect(x + w - 4.0, 0.0, 4.0, FRAME_ROWS, [255; 3]), rect(x, 0.0, w, 4.0, [255; 3])] {
                k += 1;
                conn.scene_node_rect(TILE_LINES + k, r)?;
            }
        }
        for i in k..self.lines {
            conn.scene_node_clear(TILE_LINES + 1 + i)?;
        }
        self.lines = k;
        Ok(())
    }
}

fn main() -> std::io::Result<()> {
    let path = std::env::args().nth(1).unwrap_or_else(|| "/tmp/frescod.sock".to_string());
    let mut conn = Connection::connect(&path)?;
    let win = conn.window_create(800, 600, "span-probe", WindowHints::default())?;
    conn.set_default_window(win as u16);
    eprintln!("span-probe: window {win}");
    let mut p = Probe { w: 800, h: 600, stripes: 0, lines: 0, tiles: Vec::new() };
    conn.scene_frame_begin()?;
    p.draw_all(&mut conn)?;
    conn.scene_frame_end()?;

    let started = Instant::now();
    let tick = Duration::from_millis(33);
    let mut next = Instant::now();
    let mut frame: u64 = 0;
    loop {
        let wait = next.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
        let mut redraw = false;
        let mut retile = false;
        if let Some(ev) = conn.wait_event(Some(wait))? {
            match ev {
                Event::Resized { window_id, width, height } if window_id == win => {
                    eprintln!("span-probe: resized {width}x{height}");
                    (p.w, p.h) = (width.max(1), height.max(1));
                    redraw = true;
                }
                Event::Span(s) if s.window_id == win => {
                    eprintln!("event: Span({s:?})");
                    let tiles: Vec<String> = s.tiles.iter().map(|t| format!("{:?}{}", t.rect, if t.present { "" } else { " out" })).collect();
                    eprintln!("span-probe: span tiles [{}] hidden {:?}", tiles.join(", "), s.hidden);
                    p.tiles = s.tiles.iter().map(|t| t.rect).collect();
                    retile = true;
                }
                Event::CloseRequested { window_id } if window_id == win => break,
                _ => {}
            }
        }
        if Instant::now() < next && !redraw && !retile {
            continue;
        }
        conn.scene_frame_begin()?;
        if redraw {
            p.draw_all(&mut conn)?;
        } else if retile {
            p.draw_tiles(&mut conn)?;
        }
        /* The runner: 32 px, along the bottom band, 4 s per crossing. */
        let t = started.elapsed().as_secs_f32() / 4.0;
        let x = t.fract() * (p.w.saturating_sub(32)) as f32;
        conn.scene_node_rect(SQUARE, rect(x, p.h as f32 - 48.0, 32.0, 32.0, [255, 216, 26]))?;
        conn.scene_frame_end()?;
        if Instant::now() >= next {
            next += tick;
            frame += 1;
            if frame % 300 == 0 {
                eprintln!("span-probe: frame {frame} at {}x{}", p.w, p.h);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stripe_survives_a_few_steps_of_error() {
        for i in [0, 1, 15, 16, 245, 490, 729, 4095] {
            let [r, g, b] = stripe_rgb(i);
            assert_eq!(stripe_of([r, g, b]), i);
            assert_eq!(stripe_of([r + 7, g - 7, b + 3]), i);
        }
    }
}
