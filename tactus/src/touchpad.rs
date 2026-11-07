//! The touchpad stage (docs/spec/atrium-input.md §5.3): turns frames of
//! finger contacts into pointer motion, clicks and taps, two-finger
//! scrolling with phases (Pergola's ScrollView flings on `End`), and
//! swipe and pinch gestures. The same stage serves Precision Touchpads
//! over hidbus and, later, PS/2 pads (T4), since both arrive as
//! contacts in millimetres.
//!
//! Palms: contacts the pad marks unconfident are ignored, as are fingers
//! that land while the user is typing (until they lift).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::decode::Contact;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Begin,
    Update,
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GestureKind {
    Swipe,
    Pinch,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    /// Pointer motion in pixels (accelerated).
    Motion { dx: f32, dy: f32 },
    /// HID button number (1 primary, 2 secondary, 3 middle).
    Button { button: u8, down: bool },
    /// Two fingers moving, in pixels the fingers moved (not yet turned
    /// into a content direction).
    Scroll { dx: f32, dy: f32, phase: Phase },
    /// Three or more fingers moving (`dx`, `dy` in pixels), or two
    /// pinching (`scale` against the start).
    Gesture { kind: GestureKind, fingers: u8, phase: Phase, dx: f32, dy: f32, scale: f32 },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    /// A quick touch without moving clicks.
    pub tap_to_click: bool,
    /// Pixels per millimetre for slow pointer motion.
    pub px_per_mm: f32,
    /// Most the gain rises with speed.
    pub max_gain: f32,
    /// Pixels per millimetre of two-finger scrolling.
    pub scroll_px_per_mm: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { tap_to_click: true, px_per_mm: 5.0, max_gain: 3.0, scroll_px_per_mm: 8.0 }
    }
}

/// Longest touch that is still a tap.
const TAP_TIME: Duration = Duration::from_millis(200);
/// Most a finger may move and still tap, mm.
const TAP_SLOP: f32 = 1.5;
/// Two fingers must move this far (mm) before scroll or pinch is chosen.
const DECIDE: f32 = 1.0;
/// Speeds (mm per ms) between which the pointer gain rises.
const SLOW: f32 = 0.05;
const FAST: f32 = 0.6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Idle,
    Pointer,
    /// Two fingers down, not yet moved enough to choose.
    Two,
    Scroll,
    Pinch,
    Swipe(u8),
}

#[derive(Debug)]
struct Tap {
    at: Instant,
    fingers: usize,
    moved: bool,
}

#[derive(Debug)]
pub struct Touchpad {
    settings: Settings,
    /// Live contacts: id to (where it landed, where it is now).
    live: HashMap<u32, ((f32, f32), (f32, f32))>,
    /// Contacts landed while typing or marked as palms, ignored until they lift.
    ignored: Vec<u32>,
    mode: Mode,
    tap: Option<Tap>,
    clicked: Option<u8>,
    last: Option<Instant>,
    speed: f32,
    /// Two-finger distance at the start, and the centroid travel since.
    pinch_from: f32,
    travel: (f32, f32),
}

fn centroid(p: &[(f32, f32)]) -> (f32, f32) {
    let n = p.len().max(1) as f32;
    (p.iter().map(|q| q.0).sum::<f32>() / n, p.iter().map(|q| q.1).sum::<f32>() / n)
}

fn spread(p: &[(f32, f32)]) -> f32 {
    match p {
        [a, b, ..] => ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt(),
        _ => 0.0,
    }
}

impl Touchpad {
    pub fn new(settings: Settings) -> Touchpad {
        Touchpad {
            settings,
            live: HashMap::new(),
            ignored: Vec::new(),
            mode: Mode::Idle,
            tap: None,
            clicked: None,
            last: None,
            speed: 0.0,
            pinch_from: 0.0,
            travel: (0.0, 0.0),
        }
    }

    /// One frame at `now`; `typing`: a key was pressed a moment ago.
    pub fn frame(&mut self, now: Instant, contacts: &[Contact], button: bool, typing: bool, out: &mut Vec<Out>) {
        let ms = self.last.map(|t| now.saturating_duration_since(t).as_secs_f32() * 1000.0).unwrap_or(8.0).clamp(1.0, 100.0);
        self.last = Some(now);

        // Who is down now, and how far the ones that stayed moved.
        let down: Vec<&Contact> = contacts.iter().filter(|c| c.tip).collect();
        self.ignored.retain(|id| down.iter().any(|c| c.id == *id));
        for c in &down {
            if !self.live.contains_key(&c.id) && !self.ignored.contains(&c.id) && (!c.confident || typing) {
                self.ignored.push(c.id);
            }
        }
        let active: Vec<&Contact> = down.iter().copied().filter(|c| !self.ignored.contains(&c.id)).collect();
        let (mut before, mut after) = (Vec::new(), Vec::new());
        for c in &active {
            if let Some(&(_, at)) = self.live.get(&c.id) {
                before.push(at);
                after.push((c.x, c.y));
            }
        }
        let stayed = before.len();
        let (c0, c1) = (centroid(&before), centroid(&after));
        let (dx, dy) = if stayed > 0 { (c1.0 - c0.0, c1.1 - c0.1) } else { (0.0, 0.0) };
        let (s0, s1) = (spread(&before), spread(&after));
        self.live.retain(|id, _| active.iter().any(|c| c.id == *id));
        for c in &active {
            let e = self.live.entry(c.id).or_insert(((c.x, c.y), (c.x, c.y)));
            e.1 = (c.x, c.y);
        }
        let n = active.len();

        // Taps: from the first finger down to the last one up.
        if n > 0 {
            let t = self.tap.get_or_insert(Tap { at: now, fingers: 0, moved: false });
            t.fingers = t.fingers.max(n);
            t.moved |= self.live.values().any(|(a, b)| ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt() > TAP_SLOP);
        }

        // The physical button: a right click with two fingers down.
        if button && self.clicked.is_none() {
            let b = if n >= 2 { 2 } else { 1 };
            self.clicked = Some(b);
            out.push(Out::Button { button: b, down: true });
            // A click is not also a tap.
            if let Some(t) = self.tap.as_mut() {
                t.moved = true;
            }
        } else if !button {
            if let Some(b) = self.clicked.take() {
                out.push(Out::Button { button: b, down: false });
            }
        }

        // Motion by finger count; a change of count ends what was going on.
        let want = match n {
            0 => Mode::Idle,
            1 => Mode::Pointer,
            2 => match self.mode {
                Mode::Scroll | Mode::Pinch => self.mode,
                _ => Mode::Two,
            },
            k => Mode::Swipe(k.min(5) as u8),
        };
        if want != self.mode {
            self.end(out);
            self.mode = want;
            self.travel = (0.0, 0.0);
            let now_at: Vec<(f32, f32)> = active.iter().map(|c| (c.x, c.y)).collect();
            self.pinch_from = spread(&now_at);
            self.speed = 0.0;
        } else if stayed == n && n > 0 {
            self.moved(dx, dy, s0, s1, ms, out);
        }

        if n == 0 {
            if let Some(t) = self.tap.take() {
                if self.settings.tap_to_click && !t.moved && now.saturating_duration_since(t.at) <= TAP_TIME && self.clicked.is_none() {
                    let b = match t.fingers {
                        1 => 1,
                        2 => 2,
                        _ => 3,
                    };
                    out.push(Out::Button { button: b, down: true });
                    out.push(Out::Button { button: b, down: false });
                }
            }
        }
    }

    fn moved(&mut self, dx: f32, dy: f32, s0: f32, s1: f32, ms: f32, out: &mut Vec<Out>) {
        let s = &self.settings;
        match self.mode {
            Mode::Pointer => {
                let dist = (dx * dx + dy * dy).sqrt();
                self.speed = 0.6 * self.speed + 0.4 * dist / ms;
                let t = ((self.speed - SLOW) / (FAST - SLOW)).clamp(0.0, 1.0);
                let g = s.px_per_mm * (1.0 + t * (s.max_gain - 1.0));
                if dx != 0.0 || dy != 0.0 {
                    out.push(Out::Motion { dx: dx * g, dy: dy * g });
                }
            }
            Mode::Two => {
                self.travel.0 += dx;
                self.travel.1 += dy;
                let pinch = (s1 - self.pinch_from).abs();
                let slide = (self.travel.0.powi(2) + self.travel.1.powi(2)).sqrt();
                if pinch > 2.0 * DECIDE && pinch > slide * 1.5 {
                    self.mode = Mode::Pinch;
                    out.push(Out::Gesture { kind: GestureKind::Pinch, fingers: 2, phase: Phase::Begin, dx: 0.0, dy: 0.0, scale: s1 / self.pinch_from.max(0.1) });
                } else if slide > DECIDE {
                    self.mode = Mode::Scroll;
                    let k = s.scroll_px_per_mm;
                    out.push(Out::Scroll { dx: self.travel.0 * k, dy: self.travel.1 * k, phase: Phase::Begin });
                }
            }
            Mode::Scroll => {
                let k = s.scroll_px_per_mm;
                if dx != 0.0 || dy != 0.0 {
                    out.push(Out::Scroll { dx: dx * k, dy: dy * k, phase: Phase::Update });
                }
            }
            Mode::Pinch => {
                if s1 != s0 {
                    out.push(Out::Gesture { kind: GestureKind::Pinch, fingers: 2, phase: Phase::Update, dx: 0.0, dy: 0.0, scale: s1 / self.pinch_from.max(0.1) });
                }
            }
            Mode::Swipe(f) => {
                let k = s.scroll_px_per_mm;
                let first = self.travel == (0.0, 0.0);
                self.travel.0 += dx;
                self.travel.1 += dy;
                if dx != 0.0 || dy != 0.0 {
                    let phase = if first { Phase::Begin } else { Phase::Update };
                    out.push(Out::Gesture { kind: GestureKind::Swipe, fingers: f, phase, dx: dx * k, dy: dy * k, scale: 1.0 });
                }
            }
            Mode::Idle => {}
        }
    }

    /// End a scroll or gesture that was going on.
    fn end(&mut self, out: &mut Vec<Out>) {
        match self.mode {
            Mode::Scroll => out.push(Out::Scroll { dx: 0.0, dy: 0.0, phase: Phase::End }),
            Mode::Pinch => out.push(Out::Gesture { kind: GestureKind::Pinch, fingers: 2, phase: Phase::End, dx: 0.0, dy: 0.0, scale: 1.0 }),
            Mode::Swipe(f) if self.travel != (0.0, 0.0) => {
                out.push(Out::Gesture { kind: GestureKind::Swipe, fingers: f, phase: Phase::End, dx: 0.0, dy: 0.0, scale: 1.0 })
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Run {
        pad: Touchpad,
        t0: Instant,
        ms: u64,
    }

    impl Run {
        fn new() -> Run {
            Run { pad: Touchpad::new(Settings::default()), t0: Instant::now(), ms: 0 }
        }
        /// One frame 8 ms after the last; fingers as (id, x, y).
        fn f(&mut self, fingers: &[(u32, f32, f32)], button: bool) -> Vec<Out> {
            self.ms += 8;
            let c: Vec<Contact> = fingers.iter().map(|&(id, x, y)| Contact { id, x, y, tip: true, confident: true }).collect();
            let mut out = Vec::new();
            self.pad.frame(self.t0 + Duration::from_millis(self.ms), &c, button, false, &mut out);
            out
        }
    }

    #[test]
    fn one_finger_moves_the_pointer_and_taps_click() {
        let mut r = Run::new();
        assert_eq!(r.f(&[(1, 10.0, 10.0)], false), vec![]);
        let m = r.f(&[(1, 13.0, 10.0)], false);
        let Out::Motion { dx, dy } = m[0] else { panic!("{m:?}") };
        assert!(dx >= 15.0 && dy == 0.0, "{dx}");
        assert_eq!(r.f(&[], false), vec![], "moved: no tap");
        r.f(&[(2, 20.0, 20.0)], false);
        r.f(&[(2, 20.2, 20.0)], false);
        let up = r.f(&[], false);
        assert_eq!(up[up.len() - 2..], [Out::Button { button: 1, down: true }, Out::Button { button: 1, down: false }]);
        r.f(&[(3, 20.0, 20.0), (4, 40.0, 20.0)], false);
        assert_eq!(r.f(&[], false), vec![Out::Button { button: 2, down: true }, Out::Button { button: 2, down: false }], "two-finger tap");
        r.f(&[(5, 20.0, 20.0)], false);
        r.ms += 300;
        assert_eq!(r.f(&[], false), vec![], "too long for a tap");
    }

    #[test]
    fn the_button_is_a_right_click_with_two_fingers() {
        let mut r = Run::new();
        r.f(&[(1, 10.0, 10.0)], false);
        assert_eq!(r.f(&[(1, 10.0, 10.0)], true), vec![Out::Button { button: 1, down: true }]);
        assert_eq!(r.f(&[(1, 10.0, 10.0)], false), vec![Out::Button { button: 1, down: false }]);
        assert_eq!(r.f(&[], false), vec![], "a click is not also a tap");
        r.f(&[(2, 10.0, 10.0), (3, 30.0, 10.0)], false);
        assert_eq!(r.f(&[(2, 10.0, 10.0), (3, 30.0, 10.0)], true), vec![Out::Button { button: 2, down: true }]);
    }

    #[test]
    fn two_fingers_scroll_with_phases() {
        let mut r = Run::new();
        r.f(&[(1, 10.0, 30.0), (2, 30.0, 30.0)], false);
        assert_eq!(r.f(&[(1, 10.0, 29.5), (2, 30.0, 29.5)], false), vec![], "not decided yet");
        let b = r.f(&[(1, 10.0, 28.5), (2, 30.0, 28.5)], false);
        assert!(matches!(b[..], [Out::Scroll { phase: Phase::Begin, dy, .. }] if dy < 0.0), "{b:?}");
        let u = r.f(&[(1, 10.0, 27.5), (2, 30.0, 27.5)], false);
        assert_eq!(u, vec![Out::Scroll { dx: 0.0, dy: -8.0, phase: Phase::Update }]);
        assert_eq!(r.f(&[], false), vec![Out::Scroll { dx: 0.0, dy: 0.0, phase: Phase::End }]);
    }

    #[test]
    fn pinch_and_three_finger_swipe() {
        let mut r = Run::new();
        r.f(&[(1, 20.0, 20.0), (2, 30.0, 20.0)], false);
        let p = r.f(&[(1, 17.0, 20.0), (2, 33.0, 20.0)], false);
        assert!(matches!(p[..], [Out::Gesture { kind: GestureKind::Pinch, phase: Phase::Begin, scale, .. }] if scale > 1.5), "{p:?}");
        assert_eq!(r.f(&[], false).last(), Some(&Out::Gesture { kind: GestureKind::Pinch, fingers: 2, phase: Phase::End, dx: 0.0, dy: 0.0, scale: 1.0 }));
        r.ms += 500;
        r.f(&[(3, 10.0, 20.0), (4, 20.0, 20.0), (5, 30.0, 20.0)], false);
        let s = r.f(&[(3, 12.0, 20.0), (4, 22.0, 20.0), (5, 32.0, 20.0)], false);
        assert_eq!(s, vec![Out::Gesture { kind: GestureKind::Swipe, fingers: 3, phase: Phase::Begin, dx: 16.0, dy: 0.0, scale: 1.0 }]);
        let e = r.f(&[], false);
        assert_eq!(e[0], Out::Gesture { kind: GestureKind::Swipe, fingers: 3, phase: Phase::End, dx: 0.0, dy: 0.0, scale: 1.0 });
    }

    #[test]
    fn palms_and_fingers_landing_while_typing_are_ignored() {
        let mut pad = Touchpad::new(Settings::default());
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        let mut out = Vec::new();
        let palm = |x| [Contact { id: 7, x, y: 5.0, tip: true, confident: false }];
        pad.frame(at(0), &palm(10.0), false, false, &mut out);
        pad.frame(at(8), &palm(20.0), false, false, &mut out);
        let typed = |x| [Contact { id: 8, x, y: 5.0, tip: true, confident: true }];
        pad.frame(at(16), &typed(10.0), false, true, &mut out);
        pad.frame(at(24), &typed(20.0), false, false, &mut out);
        pad.frame(at(32), &[], false, false, &mut out);
        assert_eq!(out, vec![], "no motion, no tap");
    }
}
