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
/// The PiP button: a screen, and the small window in its corner.
const N_PIP: u32 = 42;
const N_PIP2: u32 = 43;
/// Subtitle lines: a backing box and a glyph run each.
const N_SUB_BG0: u32 = 60;
const N_SUB0: u32 = 70;
const SUB_LINES: usize = 4;
/// The glyph atlas's texture slot on our connection.
const ATLAS_SLOT: u32 = 0x00A7_5B70;
static FONT: &[u8] = include_bytes!("../../../fonts/ibm-plex/IBMPlexSans.ttf");

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    Toggle,
    /// Seek to this fraction of the duration.
    Seek(f64),
    /// Step this many seconds.
    Step(f64),
    /// Change the volume by this much (0 to 1 scale).
    Volume(f32),
    ToggleMute,
    /// Subtitles off, or back on.
    ToggleSubtitles,
    /// A page's action (shift-N, shift-P): a `media_action` bit.
    PageAction(u16),
    /// Into picture-in-picture, or back.
    TogglePip,
}

#[derive(Debug, Clone, PartialEq, Default)]
struct Drawn {
    visible: bool,
    playing: bool,
    size: (u32, u32),
    /// In tenths of a pixel, so tiny moves don't redraw.
    played: i32,
    ranges: Vec<(i32, i32)>,
    subtitles: Vec<String>,
    pip_button: bool,
}

pub struct Controls {
    pub size: (u32, u32),
    pub playing: bool,
    pub duration_s: f64,
    pub position_s: f64,
    /// Buffered ranges in seconds, merged over tracks.
    pub buffered: Vec<(f64, f64)>,
    /// Subtitle lines to show now (at most four), top to bottom.
    pub subtitles: Vec<String>,
    /// A picture-in-picture button at the right of the bar (an embedded
    /// player with video).
    pub pip_button: bool,
    shown_until: Instant,
    drawn: Option<Drawn>,
    text: Option<Text>,
}

/// Shaped text for the controls: IBM Plex Sans, shaped here with swash and
/// drawn as Fresco glyph runs from Pergola's atlas (text is the app's to
/// shape, not frescod's).
struct Text {
    face: pergola::glyphs::Face,
    atlas: pergola::glyphs::Atlas,
    shaper: swash::shape::ShapeContext,
    bound: bool,
}

impl Text {
    fn new() -> Self {
        Text { face: pergola::glyphs::Face::new(FONT, 0), atlas: pergola::glyphs::Atlas::new(), shaper: swash::shape::ShapeContext::new(), bound: false }
    }

    /// Glyphs of `s` at `size` px from a pen at 0, and the run's width.
    fn shape(&mut self, s: &str, size: f32) -> (Vec<pergola::glyphs::Glyph>, f32) {
        let Some(font) = swash::FontRef::from_index(FONT, 0) else { return (Vec::new(), 0.0) };
        let mut sh = self.shaper.builder(font).size(size).build();
        sh.add_str(s);
        let (mut out, mut x) = (Vec::new(), 0f32);
        sh.shape_with(|cluster| {
            for g in cluster.glyphs {
                out.push(pergola::glyphs::Glyph { id: g.id, dx: x + g.x, dy: -g.y });
                x += g.advance;
            }
        });
        (out, x)
    }

    /// A glyph run at `origin` (baseline), uploading what the atlas gained.
    fn run(&mut self, c: &mut Connection, glyphs: &[pergola::glyphs::Glyph], size: f32, origin: (f32, f32)) -> io::Result<fresco_protocol::GlyphRunParams> {
        use pergola::glyphs::{ATLAS_H, ATLAS_W};
        let mut out = Vec::with_capacity(glyphs.len());
        for g in glyphs {
            if let Some(e) = self.atlas.glyph(&self.face, size, g.id) {
                out.push(fresco_protocol::GlyphInstance {
                    dx: g.dx.round(),
                    dy: g.dy.round(),
                    atlas_u: e.u,
                    atlas_v: e.v,
                    atlas_w: e.w,
                    atlas_h: e.h,
                    bearing_x: e.left,
                    bearing_y: e.top,
                });
            }
        }
        if !self.bound {
            let _ = self.atlas.take_dirty();
            let hash = c.upload_blob(self.atlas.pixels())?;
            c.slot_set_texture(ATLAS_SLOT, hash, ATLAS_W, ATLAS_H, fresco_protocol::TextureFormat::R8Unorm)?;
            self.bound = true;
        } else if let Some((y, rows, px)) = self.atlas.take_dirty() {
            let px = px.to_vec();
            c.slot_update_region(ATLAS_SLOT, 0, y, ATLAS_W, rows, px)?;
        }
        Ok(fresco_protocol::GlyphRunParams {
            x: origin.0.round(),
            y: origin.1.round(),
            atlas_slot_id: ATLAS_SLOT,
            atlas_width: ATLAS_W,
            atlas_height: ATLAS_H,
            r: 1.0,
            g: 1.0,
            b: 1.0,
            a: 1.0,
            glyphs: out,
        })
    }
}

impl Controls {
    pub fn new(size: (u32, u32)) -> Self {
        Controls { size, playing: false, duration_s: 0.0, position_s: 0.0, buffered: Vec::new(), subtitles: Vec::new(), pip_button: false, shown_until: Instant::now() + IDLE_HIDE, drawn: None, text: None }
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
        let right = if self.pip_button { BUTTON_W } else { 0.0 };
        let x1 = (self.size.0 as f32 - PAD - right).max(x0 + 1.0);
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
        if self.pip_button && x >= w - BUTTON_W {
            return Some(Action::TogglePip);
        }
        let (x0, x1, _) = self.track();
        if self.duration_s > 0.0 && x >= x0 - THUMB && x <= x1 + THUMB {
            return Some(Action::Seek((((x - x0) / (x1 - x0)) as f64).clamp(0.0, 1.0)));
        }
        None
    }

    /// A key press, by HID usage, with Fresco's modifier byte (HID: Shift
    /// is 0x02 or 0x20).
    pub fn key(&mut self, hid_usage: u16, modifiers: u8) -> Option<Action> {
        self.poke();
        if modifiers & 0x22 != 0 {
            return match hid_usage {
                0x11 => Some(Action::PageAction(limen::media_player::media_action::NEXT_TRACK)), // N
                0x13 => Some(Action::PageAction(limen::media_player::media_action::PREVIOUS_TRACK)), // P
                _ => None,
            };
        }
        // The keys most players share (YouTube's set, mostly).
        match hid_usage {
            0x2C | 0x0E => Some(Action::Toggle),  // space, k
            0x4F => Some(Action::Step(5.0)),       // right
            0x50 => Some(Action::Step(-5.0)),      // left
            0x0F => Some(Action::Step(10.0)),      // l
            0x0D => Some(Action::Step(-10.0)),     // j
            0x52 => Some(Action::Volume(0.05)),    // up
            0x51 => Some(Action::Volume(-0.05)),   // down
            0x10 => Some(Action::ToggleMute),      // m
            0x06 => Some(Action::ToggleSubtitles), // c
            0x0C => Some(Action::TogglePip),       // i
            0x4A => Some(Action::Seek(0.0)),       // home
            0x4D => Some(Action::Seek(1.0)),       // end
            0x27 => Some(Action::Seek(0.0)),       // 0
            // 1 to 9: a tenth of the way per digit.
            0x1E..=0x26 => Some(Action::Seek((hid_usage - 0x1D) as f64 / 10.0)),
            _ => None,
        }
    }

    fn state(&self) -> Drawn {
        let mut ranges: Vec<(i32, i32)> = self.buffered.iter().map(|&(a, b)| ((self.x_of(a) * 10.0) as i32, (self.x_of(b) * 10.0) as i32)).filter(|(a, b)| b > a).collect();
        ranges.truncate(RANGES_MAX);
        let mut subtitles = self.subtitles.iter().flat_map(|s| s.lines()).map(str::to_string).collect::<Vec<_>>();
        subtitles.truncate(SUB_LINES);
        Drawn { visible: self.visible(), playing: self.playing, size: self.size, played: (self.x_of(self.position_s) * 10.0) as i32, ranges, subtitles, pip_button: self.pip_button }
    }

    /// Subtitle lines, centred near the bottom (above the bar when it shows),
    /// each on a dark box for contrast over any picture.
    fn draw_subtitles(&mut self, c: &mut Connection, st: &Drawn) -> io::Result<()> {
        let (w, h) = (st.size.0 as f32, st.size.1 as f32);
        let size = (h * 0.05).clamp(14.0, 48.0);
        let line_h = size * 1.35;
        let bottom = h - if st.visible { BAR_H } else { 0.0 } - size * 0.8;
        let n = st.subtitles.len();
        let text = self.text.get_or_insert_with(Text::new);
        for i in 0..SUB_LINES {
            let (bg, run) = (N_SUB_BG0 + i as u32, N_SUB0 + i as u32);
            let Some(line) = st.subtitles.get(i) else {
                c.scene_node_clear(bg)?;
                c.scene_node_clear(run)?;
                continue;
            };
            let (glyphs, width) = text.shape(line, size);
            // Wider than the window: the line is cut by the edges, not wrapped.
            let x = ((w - width) / 2.0).max(4.0);
            let baseline = bottom - (n - 1 - i) as f32 * line_h;
            let pad = size * 0.25;
            c.scene_node_rect(bg, RectParams { x: x - pad, y: baseline - size * 0.95, w: width + 2.0 * pad, h: line_h, r: 0.0, g: 0.0, b: 0.0, a: 0.7, radius: 2.0 })?;
            let params = text.run(c, &glyphs, size, (x, baseline))?;
            c.scene_node_glyph_run(run, params)?;
        }
        Ok(())
    }

    /// Redraw if anything visible changed.
    pub fn draw(&mut self, c: &mut Connection) -> io::Result<()> {
        let st = self.state();
        if self.drawn.as_ref() == Some(&st) {
            return Ok(());
        }
        let was = self.drawn.replace(st.clone());
        c.scene_frame_begin()?;
        if was.as_ref().map_or(true, |w| w.subtitles != st.subtitles || w.size != st.size || w.visible != st.visible) {
            self.draw_subtitles(c, &st)?;
        }
        if !st.visible {
            if was.is_some_and(|w| w.visible) {
                for n in [N_BAR, N_ICON, N_ICON2, N_TRACK, N_PLAYED, N_THUMB, N_PIP, N_PIP2].into_iter().chain(N_RANGE0..N_RANGE0 + RANGES_MAX as u32) {
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
        if st.pip_button {
            let (cx, cy) = (w - BUTTON_W / 2.0, h - BAR_H / 2.0);
            c.scene_node_rect(N_PIP, rect(cx - 11.0, cy - 8.0, 22.0, 16.0, 1.0, 0.4, 2.0))?;
            c.scene_node_rect(N_PIP2, rect(cx + 1.0, cy + 1.0, 8.0, 5.0, 1.0, 1.0, 1.0))?;
        } else if was.is_some_and(|w| w.pip_button) {
            c.scene_node_clear(N_PIP)?;
            c.scene_node_clear(N_PIP2)?;
        }
        c.scene_frame_end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtitle_text_shapes() {
        let mut t = Text::new();
        let (g, w) = t.shape("Hello, world", 20.0);
        assert_eq!(g.len(), 12);
        assert!((80.0..140.0).contains(&w), "width {w}");
        assert!(g.windows(2).all(|p| p[1].dx > p[0].dx), "pen moves right");
        // Lines over four are dropped; each cue's lines are split.
        let mut c = Controls::new((640, 360));
        c.subtitles = vec!["a\nb".into(), "c\nd\ne".into()];
        assert_eq!(c.state().subtitles, vec!["a", "b", "c", "d"]);
    }

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
        assert_eq!(c.key(0x2C, 0), Some(Action::Toggle));
        assert_eq!(c.key(0x0E, 0), Some(Action::Toggle));
        assert_eq!(c.key(0x0D, 0), Some(Action::Step(-10.0)));
        assert_eq!(c.key(0x20, 0), Some(Action::Seek(0.3)), "3");
        assert_eq!(c.key(0x26, 0), Some(Action::Seek(0.9)), "9");
        assert_eq!(c.key(0x10, 0), Some(Action::ToggleMute));
        assert_eq!(c.key(0x04, 0), None, "a");
        assert_eq!(c.key(0x11, 0x02), Some(Action::PageAction(1)), "shift-N");
        assert_eq!(c.key(0x0E, 0x02), None, "shift-K");
        assert_eq!(c.key(0x0C, 0), Some(Action::TogglePip), "i");
        // The PiP button takes the bar's right end, and the track gives way.
        let (_, x1, _) = c.track();
        c.pip_button = true;
        let (_, x1p, _) = c.track();
        assert!(x1p < x1);
        let (w, h) = (c.size.0 as f32, c.size.1 as f32);
        assert_eq!(c.press(w - 10.0, h - 10.0), Some(Action::TogglePip));
    }
}

/// Keys whose action repeats while held: the arrows (step, volume) and
/// j/l (step); toggles act once per press.
pub fn repeats(hid_usage: u16) -> bool {
    matches!(hid_usage, 0x4F..=0x52 | 0x0D | 0x0F)
}
