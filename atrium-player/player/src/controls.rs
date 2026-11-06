//! The native controls (atrium-player.md §7), drawn in the player's own
//! window over the video layer: a bar along the bottom with play/pause and
//! a scrubber showing what is buffered and what has played. They hide after
//! a few idle seconds and come back on pointer motion or a key; they stay up
//! while paused.
//!
//! Drawn with Fresco scene nodes directly. Text (the time) waits for in-app
//! text shaping in Pergola (Pergola review decision 2); the controls move to
//! Pergola widgets once Pergola windows can hold a video layer.
use std::io;
use std::time::{Duration, Instant};

use fresco_client::Connection;
use fresco_protocol::{fill_verb, FillPaint, FillParams, RectParams};

pub const BAR_H: f32 = 48.0;
const PAD: f32 = 16.0;
const BUTTON_W: f32 = 48.0;
const TRACK_H: f32 = 4.0;
const THUMB: f32 = 12.0;
const RANGES_MAX: usize = 16;
pub const IDLE_HIDE: Duration = Duration::from_secs(3);

// Node ids (the window's scene is ours alone).
const N_BAR: u32 = 10;
const N_ICON: u32 = 11;
const N_ICON2: u32 = 12;
const N_TRACK: u32 = 13;
const N_RANGE0: u32 = 20;
const N_PLAYED: u32 = 40;
const N_THUMB: u32 = 41;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    Toggle,
    /// Seek to this fraction of the duration.
    Seek(f64),
    /// Step this many seconds.
    Step(f64),
}

#[derive(Debug, Clone, PartialEq, Default)]
struct Drawn {
    visible: bool,
    playing: bool,
    size: (u32, u32),
    /// In tenths of a pixel, so tiny moves don't redraw.
    played: i32,
    ranges: Vec<(i32, i32)>,
}

pub struct Controls {
    pub size: (u32, u32),
    pub playing: bool,
    pub duration_s: f64,
    pub position_s: f64,
    /// Buffered ranges in seconds, merged over tracks.
    pub buffered: Vec<(f64, f64)>,
    shown_until: Instant,
    drawn: Option<Drawn>,
}

impl Controls {
    pub fn new(size: (u32, u32)) -> Self {
        Controls { size, playing: false, duration_s: 0.0, position_s: 0.0, buffered: Vec::new(), shown_until: Instant::now() + IDLE_HIDE, drawn: None }
    }

    /// Pointer motion or a key: show for a while.
    pub fn poke(&mut self) {
        self.shown_until = Instant::now() + IDLE_HIDE;
    }

    pub fn visible(&self) -> bool {
        !self.playing || Instant::now() < self.shown_until
    }

    fn track(&self) -> (f32, f32, f32) {
        let x0 = BUTTON_W + PAD / 2.0;
        let x1 = (self.size.0 as f32 - PAD).max(x0 + 1.0);
        (x0, x1, self.size.1 as f32 - BAR_H / 2.0)
    }

    fn x_of(&self, secs: f64) -> f32 {
        let (x0, x1, _) = self.track();
        let f = if self.duration_s > 0.0 { (secs / self.duration_s).clamp(0.0, 1.0) } else { 0.0 };
        x0 + (x1 - x0) * f as f32
    }

    /// What a press at (x, y) means.
    pub fn press(&mut self, x: f32, y: f32) -> Option<Action> {
        let was_visible = self.visible();
        self.poke();
        let (w, h) = (self.size.0 as f32, self.size.1 as f32);
        if !(0.0..w).contains(&x) || !(0.0..h).contains(&y) {
            return None;
        }
        if y < h - BAR_H {
            // On the picture: play/pause, as most players do.
            return Some(Action::Toggle);
        }
        if !was_visible {
            // The first touch of a hidden bar only shows it.
            return None;
        }
        if x < BUTTON_W {
            return Some(Action::Toggle);
        }
        let (x0, x1, _) = self.track();
        if self.duration_s > 0.0 && x >= x0 - THUMB && x <= x1 + THUMB {
            return Some(Action::Seek((((x - x0) / (x1 - x0)) as f64).clamp(0.0, 1.0)));
        }
        None
    }

    /// A key press, by HID usage.
    pub fn key(&mut self, hid_usage: u16) -> Option<Action> {
        self.poke();
        match hid_usage {
            0x2C => Some(Action::Toggle),    // space
            0x4F => Some(Action::Step(5.0)), // right
            0x50 => Some(Action::Step(-5.0)),
            _ => None,
        }
    }

    fn state(&self) -> Drawn {
        let mut ranges: Vec<(i32, i32)> = self.buffered.iter().map(|&(a, b)| ((self.x_of(a) * 10.0) as i32, (self.x_of(b) * 10.0) as i32)).filter(|(a, b)| b > a).collect();
        ranges.truncate(RANGES_MAX);
        Drawn { visible: self.visible(), playing: self.playing, size: self.size, played: (self.x_of(self.position_s) * 10.0) as i32, ranges }
    }

    /// Redraw if anything visible changed.
    pub fn draw(&mut self, c: &mut Connection) -> io::Result<()> {
        let st = self.state();
        if self.drawn.as_ref() == Some(&st) {
            return Ok(());
        }
        let was = self.drawn.replace(st.clone());
        c.scene_frame_begin()?;
        if !st.visible {
            if was.is_some_and(|w| w.visible) {
                for n in [N_BAR, N_ICON, N_ICON2, N_TRACK, N_PLAYED, N_THUMB].into_iter().chain(N_RANGE0..N_RANGE0 + RANGES_MAX as u32) {
                    c.scene_node_clear(n)?;
                }
            }
            return c.scene_frame_end();
        }
        let (w, h) = (st.size.0 as f32, st.size.1 as f32);
        let rect = |x: f32, y: f32, w: f32, h: f32, rgb: f32, a: f32, radius: f32| RectParams { x, y, w, h, r: rgb, g: rgb, b: rgb, a, radius };
        c.scene_node_rect(N_BAR, rect(0.0, h - BAR_H, w, BAR_H, 0.0, 0.55, 0.0))?;
        // The button: a triangle to play, two bars to pause.
        let (cx, cy) = (BUTTON_W / 2.0, h - BAR_H / 2.0);
        if st.playing {
            c.scene_node_rect(N_ICON, rect(cx - 7.0, cy - 8.0, 5.0, 16.0, 1.0, 1.0, 1.0))?;
            c.scene_node_rect(N_ICON2, rect(cx + 2.0, cy - 8.0, 5.0, 16.0, 1.0, 1.0, 1.0))?;
        } else {
            c.scene_node_fill(N_ICON, &FillParams {
                rule: 0,
                transform: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
                paint: FillPaint::Solid { r: 1.0, g: 1.0, b: 1.0, a: 1.0 },
                opacity: 1.0,
                clip: None,
                verbs: vec![fill_verb::MOVE, fill_verb::LINE, fill_verb::LINE, fill_verb::CLOSE],
                points: vec![[cx - 6.0, cy - 9.0], [cx + 9.0, cy], [cx - 6.0, cy + 9.0]],
            })?;
            c.scene_node_clear(N_ICON2)?;
        }
        let (x0, x1, ty) = self.track();
        c.scene_node_rect(N_TRACK, rect(x0, ty - TRACK_H / 2.0, x1 - x0, TRACK_H, 1.0, 0.25, TRACK_H / 2.0))?;
        for i in 0..RANGES_MAX {
            let n = N_RANGE0 + i as u32;
            match st.ranges.get(i) {
                Some(&(a, b)) => c.scene_node_rect(n, rect(a as f32 / 10.0, ty - TRACK_H / 2.0, (b - a) as f32 / 10.0, TRACK_H, 1.0, 0.45, TRACK_H / 2.0))?,
                None => c.scene_node_clear(n)?,
            }
        }
        let px = st.played as f32 / 10.0;
        // The played part in the accent colour (linear RGB).
        c.scene_node_rect(N_PLAYED, RectParams { x: x0, y: ty - TRACK_H / 2.0, w: (px - x0).max(0.0), h: TRACK_H, r: 0.05, g: 0.45, b: 1.0, a: 1.0, radius: TRACK_H / 2.0 })?;
        c.scene_node_rect(N_THUMB, rect(px - THUMB / 2.0, ty - THUMB / 2.0, THUMB, THUMB, 1.0, 1.0, THUMB / 2.0))?;
        c.scene_frame_end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presses_map_to_actions() {
        let mut c = Controls::new((640, 360));
        c.duration_s = 100.0;
        c.playing = true;
        // Bar visible (just created): the button toggles, the track seeks.
        assert_eq!(c.press(20.0, 340.0), Some(Action::Toggle));
        let (x0, x1, y) = c.track();
        assert_eq!(c.press((x0 + x1) / 2.0, y), Some(Action::Seek(0.5)));
        assert_eq!(c.press(100.0, 100.0), Some(Action::Toggle));
        assert_eq!(c.press(-1.0, 100.0), None);
        // Hidden: the first press on the bar only shows it.
        c.shown_until = Instant::now() - Duration::from_secs(1);
        assert!(!c.visible());
        assert_eq!(c.press(20.0, 340.0), None);
        assert!(c.visible());
        // Paused: always visible.
        c.playing = false;
        c.shown_until = Instant::now() - Duration::from_secs(1);
        assert!(c.visible());
        assert_eq!(c.key(0x2C), Some(Action::Toggle));
    }
}
