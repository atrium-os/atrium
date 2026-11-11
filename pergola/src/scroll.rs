//! Scrolling (pergola.md §10.1, §10.4): the parts of `ScrollView` that
//! decide motion, zoom and what content to ask for. Pure state machines
//! on a caller-supplied clock (milliseconds), so they are tested without
//! a window and the Navigator scroll harness can drive the same code.
//!
//! - [`Physics`]: drag, fling (exponential decay), animated jumps for
//!   keys, bounds.
//! - [`Zoom`]: pinch / Ctrl-wheel / Ctrl +−0, live while the gesture
//!   runs, settled 100 ms after it ends.
//! - [`Viewport`]: the band interface's view side: what to `want`, where
//!   placeholders go, and anchoring when an estimate above the reader
//!   changes.
//!
//! Units: content pixels at zoom 1 (`f64`), except the band messages,
//! which use the Navigator's document units (1/64 px, [`DOC_PX`]).

/// Document units per pixel in band messages (the Navigator's `U`).
pub const DOC_PX: i64 = 64;

/// Above this speed (px/s) a fling is "fast": strips may be drawn
/// greeked and the window blurred (§10.1; tuned by eye).
pub const FAST_PX_PER_S: f64 = 2500.0;

/// Decay of a fling's speed per millisecond (0.998: about 500 ms time
/// constant, so a fling travels about half a second of its release speed).
pub const FLING_DECAY_PER_MS: f64 = 0.998;
/// Below this release speed (px/s) a drag just stops.
pub const FLING_MIN_PX_PER_S: f64 = 60.0;
/// A fling ends when it slows below this (px/s).
pub const FLING_STOP_PX_PER_S: f64 = 8.0;
/// Fastest fling (px/s): a runaway input device cannot throw the reader
/// across a whole book.
pub const FLING_MAX_PX_PER_S: f64 = 20_000.0;
/// Input deltas within this window (ms) are one continuous drag, and
/// set the release velocity.
pub const DRAG_WINDOW_MS: f64 = 80.0;
/// A drag with no new delta for this long (ms) has been released: about
/// three input reports at 120 Hz, for input with no gesture phases (a
/// wheel, or a device that does not say): there a release is inferred.
/// A touchpad's phases end the drag exactly ([`Physics::release`]).
/// Until it is, a drag whose next delta is late coasts at its speed, and
/// the fling continues from the coast: no pause and no jump at release.
pub const RELEASE_AFTER_MS: f64 = 30.0;
/// Animated jumps (page, arrows, Home/End, a discrete wheel notch).
pub const JUMP_MS: f64 = 200.0;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Motion {
    Idle,
    /// Following input; `last` is the time of the latest delta and
    /// `y_last` the position it left.
    Drag { last: f64, y_last: f64, held: bool },
    /// Released: speed `v0` (px/s, signed) at `t0`, from `y0`.
    Fling { t0: f64, y0: f64, v0: f64 },
    /// Easing from `y0` to `y1` over `JUMP_MS` from `t0`.
    Jump { t0: f64, y0: f64, y1: f64 },
}

/// Scroll position and motion along one axis.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Physics {
    /// Current position, px, in `0..=max`.
    pub y: f64,
    /// Largest position (content height minus viewport height).
    pub max: f64,
    motion: Motion,
    /// Recent input deltas (time ms, dy px), newest last; a ring of 8.
    samples: [(f64, f64); 8],
    n_samples: usize,
    /// The speed of the latest step (px/s, signed), for blur and greeking.
    pub velocity: f64,
}

impl Default for Physics {
    fn default() -> Self { Self::new(0.0) }
}

impl Physics {
    pub fn new(max: f64) -> Self {
        Self { y: 0.0, max: max.max(0.0), motion: Motion::Idle, samples: [(0.0, 0.0); 8], n_samples: 0, velocity: 0.0 }
    }

    /// The content or the viewport changed size. Keeps the position in
    /// bounds; a fling past the new end stops there.
    pub fn set_max(&mut self, max: f64) {
        self.max = max.max(0.0);
        if self.y > self.max {
            self.y = self.max;
            if matches!(self.motion, Motion::Fling { .. }) {
                self.stop();
            }
        }
    }

    /// Whether the position will change without more input (a fling, a
    /// jump, or a drag not yet released): the caller keeps a frame clock
    /// running while this is true.
    pub fn is_moving(&self) -> bool { !matches!(self.motion, Motion::Idle) }

    /// Stop where it is (a press during a fling catches it).
    pub fn stop(&mut self) {
        self.motion = Motion::Idle;
        self.velocity = 0.0;
        self.n_samples = 0;
    }

    /// A continuous-input delta (trackpad, touch, a fast run of wheel
    /// events) of `dy` px at `t` ms. Moves at once; when input stops for
    /// [`RELEASE_AFTER_MS`], [`step`](Self::step) turns it into a fling.
    pub fn drag(&mut self, dy: f64, t: f64) {
        if !dy.is_finite() { return; }
        if !matches!(self.motion, Motion::Drag { .. }) {
            self.n_samples = 0;
        }
        let i = self.n_samples % self.samples.len();
        self.samples[i] = (t, dy);
        self.n_samples += 1;
        self.y = (self.y + dy).clamp(0.0, self.max);
        self.motion = Motion::Drag { last: t, y_last: self.y, held: false };
    }

    /// A delta of a gesture whose fingers are known to be down (touchpad
    /// phases): like [`drag`](Self::drag), but no release is inferred
    /// when the deltas pause; [`release`](Self::release) ends it.
    pub fn hold(&mut self, dy: f64, t: f64) {
        self.drag(dy, t);
        if let Motion::Drag { held, .. } = &mut self.motion {
            *held = true;
        }
    }

    /// The fingers lifted at `t` ms: fling at the drag's speed, unless
    /// they had stopped moving first.
    pub fn release(&mut self, t: f64) {
        let Motion::Drag { last, .. } = self.motion else { return };
        let v = if t - last <= DRAG_WINDOW_MS { self.release_velocity() } else { 0.0 };
        if v.abs() >= FLING_MIN_PX_PER_S {
            self.motion = Motion::Fling { t0: t, y0: self.y, v0: v };
            self.velocity = v;
        } else {
            self.stop();
        }
    }

    /// The release speed: the deltas of the last [`DRAG_WINDOW_MS`] over
    /// the time they span. Needs three samples, so a single wheel notch
    /// never flings.
    fn release_velocity(&self) -> f64 {
        let len = self.samples.len();
        let n = self.n_samples.min(len);
        if n < 3 { return 0.0; }
        let at = |k: usize| self.samples[(self.n_samples - 1 - k) % len];
        let t_end = at(0).0;
        // The samples within the window, newest first.
        let m = (0..n).take_while(|&k| t_end - at(k).0 <= DRAG_WINDOW_MS).count();
        if m < 3 { return 0.0; }
        // The oldest delta happened before its timestamp: it marks the
        // start of the span, and the others moved during it.
        let span = t_end - at(m - 1).0;
        let sum: f64 = (0..m - 1).map(|k| at(k).1).sum();
        if span <= 0.0 { return 0.0; }
        (sum / span * 1000.0).clamp(-FLING_MAX_PX_PER_S, FLING_MAX_PX_PER_S)
    }

    /// The mean time between the drag's recent deltas (ms), 4–25.
    fn input_interval(&self) -> f64 {
        let len = self.samples.len();
        let n = self.n_samples.min(len);
        if n < 2 { return 25.0; }
        let at = |k: usize| self.samples[(self.n_samples - 1 - k) % len].0;
        let m = (0..n).take_while(|&k| at(0) - at(k) <= DRAG_WINDOW_MS).count();
        if m < 2 { return 25.0; }
        ((at(0) - at(m - 1)) / (m - 1) as f64).clamp(4.0, 25.0)
    }

    /// Start an eased jump to `y` (clamped) at `t` ms: page and arrow
    /// keys, Home/End, a discrete wheel notch, a find result.
    pub fn jump_to(&mut self, y: f64, t: f64) {
        let y1 = y.clamp(0.0, self.max);
        // A jump during a jump retargets from where it is now.
        self.step(t);
        self.motion = Motion::Jump { t0: t, y0: self.y, y1 };
        self.n_samples = 0;
    }

    /// A jump by `dy` from where the current jump would end (so repeated
    /// page-downs add up instead of restarting from the eased position).
    pub fn jump_by(&mut self, dy: f64, t: f64) {
        let from = match self.motion {
            Motion::Jump { y1, .. } => y1,
            _ => self.y,
        };
        self.jump_to(from + dy, t);
    }

    /// Put the position at `y` with no motion (anchoring, restoring a
    /// saved position). A fling in progress keeps going from there.
    pub fn set_position(&mut self, y: f64) {
        let d = y.clamp(0.0, self.max) - self.y;
        self.y += d;
        match &mut self.motion {
            Motion::Fling { y0, .. } => *y0 += d,
            Motion::Jump { y0, y1, .. } => { *y0 += d; *y1 = (*y1 + d).clamp(0.0, self.max); }
            _ => {}
        }
    }

    /// Advance to `t` ms. Returns the new position.
    pub fn step(&mut self, t: f64) -> f64 {
        match self.motion {
            Motion::Idle => {}
            // Fingers down and still: nothing moves until they do or lift.
            Motion::Drag { held: true, last, .. } => {
                self.velocity = if t - last <= DRAG_WINDOW_MS { self.release_velocity() } else { 0.0 };
            }
            Motion::Drag { last, y_last, held: false } => {
                let v = self.release_velocity();
                // A delta is late once a report interval has passed: from
                // then the content coasts at the drag's speed, as the next
                // delta would have moved it.
                let due = last + self.input_interval();
                if t - last >= RELEASE_AFTER_MS {
                    if v.abs() >= FLING_MIN_PX_PER_S {
                        // The fling takes over the coast where it stands,
                        // at the coast's speed.
                        let at = last + RELEASE_AFTER_MS;
                        let y0 = (y_last + v * (at - due).max(0.0) / 1000.0).clamp(0.0, self.max);
                        self.motion = Motion::Fling { t0: at, y0, v0: v };
                        return self.step(t);
                    }
                    self.stop();
                } else {
                    if t > due && v.abs() >= FLING_MIN_PX_PER_S {
                        self.y = (y_last + v * (t - due) / 1000.0).clamp(0.0, self.max);
                    }
                    // Following the finger: its speed drives greeking too.
                    self.velocity = v;
                }
            }
            Motion::Fling { t0, y0, v0 } => {
                // v(u) = v0·r^u (u in ms); distance = v0/1000 · (r^u − 1)/ln r.
                let u = (t - t0).max(0.0);
                let ln_r = FLING_DECAY_PER_MS.ln();
                let ru = (ln_r * u).exp();
                let y = y0 + v0 / 1000.0 * (ru - 1.0) / ln_r;
                let v = v0 * ru;
                self.y = y.clamp(0.0, self.max);
                if v.abs() < FLING_STOP_PX_PER_S || self.y != y {
                    self.stop();
                } else {
                    self.velocity = v;
                }
            }
            Motion::Jump { t0, y0, y1 } => {
                let u = ((t - t0) / JUMP_MS).clamp(0.0, 1.0);
                let e = 1.0 - (1.0 - u).powi(3); // ease-out cubic
                self.y = (y0 + (y1 - y0) * e).clamp(0.0, self.max);
                // d/du of the easing, in px/s.
                self.velocity = (y1 - y0) * 3.0 * (1.0 - u).powi(2) / JUMP_MS * 1000.0;
                if u >= 1.0 {
                    self.motion = Motion::Idle;
                    self.velocity = 0.0;
                }
            }
        }
        self.y
    }

    /// Whether the current motion is fast enough for greeked strips and
    /// blur.
    pub fn is_fast(&self) -> bool { self.velocity.abs() > FAST_PX_PER_S }

    /// The vertical motion blur for a frame `period_ms` long, in px: none
    /// below [`FAST_PX_PER_S`] or with reduce-motion on, otherwise a
    /// fraction of the distance moved in a frame, capped at 12 px (the
    /// scroll harness's model).
    pub fn motion_blur_px(&self, period_ms: f64, reduce_motion: bool) -> f64 {
        if reduce_motion || !self.is_fast() { return 0.0; }
        let per_frame = self.velocity.abs() * period_ms / 1000.0;
        (per_frame * 0.25).min(12.0)
    }
}

/// Zoom levels Ctrl + and Ctrl − step through.
pub const ZOOM_STEPS: [f64; 13] = [0.5, 0.67, 0.75, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0];
pub const ZOOM_MIN: f64 = 0.25;
pub const ZOOM_MAX: f64 = 5.0;
/// A zoom gesture is over when input has been quiet this long (ms).
pub const ZOOM_SETTLE_MS: f64 = 100.0;

/// Zoom: `live` follows the gesture (the content group's scale, existing
/// content stretched); `settled` is what content was laid out at. When
/// the gesture has been quiet for [`ZOOM_SETTLE_MS`], [`Zoom::step`]
/// settles and the caller sends `want` at the new zoom.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Zoom {
    pub live: f64,
    pub settled: f64,
    last_input: Option<f64>,
    /// A pinch's fingers are down: no settling, however still they are.
    held: bool,
}

impl Default for Zoom {
    fn default() -> Self { Self { live: 1.0, settled: 1.0, last_input: None, held: false } }
}

impl Zoom {
    /// Multiply the live zoom by `factor` (a pinch step or a Ctrl-wheel
    /// notch) at `t` ms.
    pub fn pinch(&mut self, factor: f64, t: f64) {
        if !(factor.is_finite() && factor > 0.0) { return; }
        self.live = (self.live * factor).clamp(ZOOM_MIN, ZOOM_MAX);
        self.last_input = Some(t);
    }

    /// Ctrl + (`dir` > 0) or Ctrl − (`dir` < 0): the next step from the
    /// live zoom.
    pub fn step_level(&mut self, dir: i32, t: f64) {
        let z = self.live;
        let next = if dir > 0 {
            ZOOM_STEPS.iter().copied().find(|&s| s > z + 1e-6).unwrap_or(ZOOM_STEPS[ZOOM_STEPS.len() - 1])
        } else {
            ZOOM_STEPS.iter().rev().copied().find(|&s| s < z - 1e-6).unwrap_or(ZOOM_STEPS[0])
        };
        self.live = next;
        self.last_input = Some(t);
    }

    /// Ctrl 0.
    pub fn reset(&mut self, t: f64) {
        self.live = 1.0;
        self.last_input = Some(t);
    }

    /// Fingers down (`true`) or lifted on a pinch: it settles only once
    /// they lift and input has then been quiet.
    pub fn hold(&mut self, held: bool, t: f64) {
        self.held = held;
        self.last_input = Some(t);
    }

    /// Whether a gesture is waiting to settle.
    pub fn is_pending(&self) -> bool { self.last_input.is_some() }

    /// The scale to give content laid out at `settled` while the gesture
    /// runs.
    pub fn stretch(&self) -> f64 { self.live / self.settled }

    /// Settle if the gesture has been quiet long enough. Returns the new
    /// zoom when it settles.
    pub fn step(&mut self, t: f64) -> Option<f64> {
        let last = self.last_input?;
        if self.held || t - last < ZOOM_SETTLE_MS { return None; }
        self.last_input = None;
        if (self.live - self.settled).abs() < 1e-9 { return None; }
        self.settled = self.live;
        Some(self.settled)
    }
}

/// Where the reader's point `focus_px` (px from the viewport top) stays
/// fixed when zoom goes from `z0` to `z1`: the new scroll position, in
/// content px at zoom 1. `y` is the scroll position in content px.
pub fn zoom_about(y: f64, focus_px: f64, z0: f64, z1: f64) -> f64 {
    let content = y + focus_px / z0;
    content - focus_px / z1
}

// ---------------------------------------------------------------------
// The band interface (§10.4): messages, and the view's bookkeeping.

/// A range of the document, `[start, end)`, in 1/64 px at zoom 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DocRange {
    pub start: i64,
    pub end: i64,
}

impl DocRange {
    pub fn new(start: i64, end: i64) -> Self { Self { start, end: end.max(start) } }
    pub fn len(&self) -> i64 { self.end - self.start }
    pub fn is_empty(&self) -> bool { self.end <= self.start }
    pub fn contains_range(&self, o: &DocRange) -> bool { self.start <= o.start && o.end <= self.end }
    pub fn meets(&self, o: &DocRange) -> bool { self.start < o.end && o.start < self.end }
}

/// View → source.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Want {
    /// Keep this ready (what is on screen).
    pub range: DocRange,
    /// At this zoom.
    pub zoom: f64,
    /// A hint: this much more above and below (about a viewport).
    pub prefetch: i64,
}

/// Source → view.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SourceEvent {
    /// Estimated total height; `settled` once exact.
    Extent { height: i64, settled: bool },
    /// Band `band` covers `range`; its scene group is written.
    BandReady { band: u32, range: DocRange },
    /// Its content or extent changed.
    BandUpdated { band: u32, range: DocRange },
    /// Gone; what of it is visible shows placeholders.
    BandEvicted { band: u32 },
}

/// A content source (the Navigator worker, a `DocumentView`, a long
/// list). In process this trait; out of process the same messages over
/// the doc-viewer channel (limen.md §2.1). The source writes each band's
/// scene group under the content group itself; the view never copies
/// scene data.
pub trait BandSource {
    fn want(&mut self, want: Want);
    /// Events since the last call.
    fn poll(&mut self) -> Vec<SourceEvent>;
}

/// The view's side of the band interface.
#[derive(Debug, Clone, Default)]
pub struct Viewport {
    /// Estimated content height (1/64 px) and whether it is exact.
    pub extent: i64,
    pub settled: bool,
    /// Resident bands, by id.
    bands: std::collections::BTreeMap<u32, DocRange>,
    /// The last `want` sent.
    last: Option<Want>,
}

impl Viewport {
    pub fn new() -> Self { Self::default() }

    pub fn bands(&self) -> impl Iterator<Item = (u32, DocRange)> + '_ {
        self.bands.iter().map(|(&b, &r)| (b, r))
    }

    /// The `want` for a viewport at `top` (1/64 px) of height `vh` px at
    /// `zoom`, or `None` when the last one still covers it: sent again
    /// only once the visible range leaves the last prefetched range by
    /// more than half the prefetch, or the zoom changed.
    pub fn want(&mut self, top: i64, vh: f64, zoom: f64) -> Option<Want> {
        let h = ((vh / zoom) * DOC_PX as f64).round() as i64;
        let range = DocRange::new(top, top + h);
        let w = Want { range, zoom, prefetch: h };
        if let Some(l) = self.last {
            let slack = l.prefetch / 2;
            let kept = DocRange::new(l.range.start - slack, l.range.end + slack);
            if (l.zoom - zoom).abs() < 1e-9 && kept.contains_range(&range) {
                return None;
            }
        }
        self.last = Some(w);
        Some(w)
    }

    /// Apply a source event. `top` is the scroll position (1/64 px) and
    /// `vh` the viewport height in document units. Returns the scroll
    /// position to keep the reader's place (anchoring): when what lies
    /// above the first visible band changes height, the position moves by
    /// the same amount, so the band and its offset stay fixed on screen.
    pub fn apply(&mut self, ev: SourceEvent, top: i64) -> i64 {
        let anchor = self.anchor(top);
        match ev {
            SourceEvent::Extent { height, settled } => {
                self.extent = height.max(0);
                self.settled = settled;
            }
            SourceEvent::BandReady { band, range } | SourceEvent::BandUpdated { band, range } => {
                self.bands.insert(band, range);
            }
            SourceEvent::BandEvicted { band } => {
                self.bands.remove(&band);
            }
        }
        match anchor {
            Some((band, within)) => match self.bands.get(&band) {
                Some(r) => (r.start + within).max(0),
                None => top,
            },
            None => top,
        }
    }

    /// The first resident band at or below `top`'s line, and how far
    /// into it `top` is.
    fn anchor(&self, top: i64) -> Option<(u32, i64)> {
        self.bands
            .iter()
            .filter(|(_, r)| r.end > top)
            .min_by_key(|(_, r)| r.start)
            .map(|(&b, r)| (b, top - r.start))
    }

    /// The parts of `[top, top + h)` no resident band covers: where the
    /// view draws placeholders (§10.1), never a blank or a frozen frame.
    pub fn placeholders(&self, top: i64, h: i64) -> Vec<DocRange> {
        let view = DocRange::new(top, top + h);
        let mut covered: Vec<DocRange> = self.bands.values().copied().filter(|r| r.meets(&view)).collect();
        covered.sort_by_key(|r| r.start);
        let mut out = Vec::new();
        let mut at = view.start;
        for r in covered {
            if r.start > at {
                out.push(DocRange::new(at, r.start.min(view.end)));
            }
            at = at.max(r.end);
            if at >= view.end { break; }
        }
        if at < view.end {
            out.push(DocRange::new(at, view.end));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trackpad swipe: `n` deltas of `dy` px, every `dt` ms from `t`.
    fn swipe(p: &mut Physics, t: f64, n: usize, dy: f64, dt: f64) -> f64 {
        for i in 0..n {
            p.drag(dy, t + i as f64 * dt);
        }
        t + (n - 1) as f64 * dt
    }

    #[test]
    fn touchpad_phases_hold_still_and_fling_on_lift() {
        let mut p = Physics::new(10_000.0);
        let mut t = 0.0;
        for _ in 0..8 {
            t += 8.0;
            p.hold(16.0, t);
        }
        let y = p.y;
        // Fingers resting on the pad: no coast, no fling, however long.
        assert_eq!(p.step(t + 200.0), y);
        assert!(p.is_moving(), "still a gesture");
        // Lifted after resting: no fling.
        p.release(t + 200.0);
        assert!(!p.is_moving());

        // Moving, then lifted: it flings from the lift, at the drag's speed.
        for _ in 0..8 {
            t += 8.0;
            p.hold(16.0, t);
        }
        let y = p.y;
        p.release(t + 4.0);
        assert!((p.velocity - 2000.0).abs() < 1.0, "{}", p.velocity);
        assert!(p.step(t + 104.0) > y + 100.0);
        // Fingers down again catch it.
        p.stop();
        assert!(!p.is_moving());
    }

    #[test]
    fn a_swipe_flings_and_comes_to_rest() {
        let mut p = Physics::new(1e9);
        // 4,000 px/s: 32 px every 8 ms.
        let end = swipe(&mut p, 0.0, 8, 32.0, 8.0);
        assert!(p.is_moving());
        let released = p.step(end + RELEASE_AFTER_MS);
        // 4,000 px/s at release, decayed over the coast since the last delta.
        assert!(p.velocity > 3700.0 && p.velocity <= 4000.0, "release speed {}", p.velocity);
        assert!(p.is_fast());
        let mut t = end + RELEASE_AFTER_MS;
        while p.is_moving() {
            t += 1000.0 / 60.0;
            p.step(t);
            assert!(t < 10_000.0, "a fling must stop");
        }
        // About v·τ (τ = −1/ln 0.998 ≈ 500 ms) past the release point.
        let travelled = p.y - 8.0 * 32.0;
        assert!((travelled - 4000.0 * 0.4995).abs() < 150.0, "travelled {travelled}");
        assert!(released > 0.0);
    }

    #[test]
    fn a_fling_is_continuous_frame_to_frame() {
        let mut p = Physics::new(1e9);
        let end = swipe(&mut p, 0.0, 8, 32.0, 8.0);
        let mut last = p.y;
        let mut t = end;
        for _ in 0..120 {
            t += 1000.0 / 120.0;
            let y = p.step(t);
            assert!(y >= last, "never moves backwards");
            // At most 4,000 px/s over an 8.3 ms frame, plus the release step.
            assert!(y - last <= 4000.0 / 120.0 + 1.0, "jump of {}", y - last);
            last = y;
        }
    }

    #[test]
    fn the_release_neither_pauses_nor_jumps() {
        let mut p = Physics::new(1e9);
        let end = swipe(&mut p, 0.0, 8, 32.0, 8.0);
        let mut last = p.y;
        // Millisecond steps through the late delta, the coast and the fling.
        for ms in 1..=100 {
            let y = p.step(end + ms as f64);
            let d = y - last;
            if ms > 8 {
                assert!(d > 3.0 && d < 4.2, "moved {d} px at +{ms} ms");
            }
            last = y;
        }
    }

    #[test]
    fn a_single_wheel_notch_does_not_fling() {
        let mut p = Physics::new(1e9);
        p.drag(40.0, 0.0);
        p.step(RELEASE_AFTER_MS + 1.0);
        assert!(!p.is_moving());
        assert_eq!(p.y, 40.0);
    }

    #[test]
    fn a_fling_stops_at_the_end() {
        let mut p = Physics::new(500.0);
        let end = swipe(&mut p, 0.0, 8, 32.0, 8.0);
        let mut t = end;
        while p.is_moving() {
            t += 16.0;
            p.step(t);
        }
        assert_eq!(p.y, 500.0);
        assert_eq!(p.velocity, 0.0);
    }

    #[test]
    fn a_press_catches_a_fling() {
        let mut p = Physics::new(1e9);
        let end = swipe(&mut p, 0.0, 8, 32.0, 8.0);
        p.step(end + 100.0);
        let y = p.y;
        p.stop();
        assert_eq!(p.step(end + 500.0), y);
    }

    #[test]
    fn page_downs_add_up_and_ease_in() {
        let mut p = Physics::new(1e9);
        p.jump_by(600.0, 0.0);
        p.jump_by(600.0, 50.0);
        let mid = p.step(100.0);
        assert!(mid > 0.0 && mid < 1200.0);
        p.step(50.0 + JUMP_MS);
        assert_eq!(p.y, 1200.0);
        assert!(!p.is_moving());
    }

    #[test]
    fn shrinking_content_keeps_the_position_in_bounds() {
        let mut p = Physics::new(1000.0);
        p.jump_to(900.0, 0.0);
        p.step(JUMP_MS);
        p.set_max(400.0);
        assert_eq!(p.y, 400.0);
    }

    #[test]
    fn blur_only_when_fast_and_not_with_reduce_motion() {
        let mut p = Physics::new(1e9);
        let end = swipe(&mut p, 0.0, 8, 32.0, 8.0);
        p.step(end + RELEASE_AFTER_MS);
        assert!(p.motion_blur_px(16.7, false) > 0.0);
        assert_eq!(p.motion_blur_px(16.7, true), 0.0);
        let mut q = Physics::new(1e9);
        let end = swipe(&mut q, 0.0, 8, 4.0, 8.0); // 500 px/s
        q.step(end + RELEASE_AFTER_MS);
        assert_eq!(q.motion_blur_px(16.7, false), 0.0);
    }

    #[test]
    fn zoom_settles_after_the_gesture_is_quiet() {
        let mut z = Zoom::default();
        z.pinch(1.1, 0.0);
        z.pinch(1.1, 30.0);
        assert_eq!(z.step(100.0), None, "still within 100 ms of the last input");
        assert!((z.stretch() - 1.21).abs() < 1e-9);
        assert_eq!(z.step(130.0), Some(1.1 * 1.1));
        assert_eq!(z.stretch(), 1.0);
        assert!(!z.is_pending());
    }

    #[test]
    fn ctrl_plus_and_minus_step_through_levels() {
        let mut z = Zoom::default();
        z.step_level(1, 0.0);
        assert_eq!(z.live, 1.1);
        z.step_level(-1, 0.0);
        z.step_level(-1, 0.0);
        assert_eq!(z.live, 0.9);
        z.reset(0.0);
        assert_eq!(z.live, 1.0);
        assert_eq!(z.step(ZOOM_SETTLE_MS), None, "back where it settled");
    }

    #[test]
    fn zooming_about_a_point_keeps_it_still() {
        let (y, f) = (1000.0, 300.0);
        let y2 = zoom_about(y, f, 1.0, 2.0);
        // The content point under the focus is the same before and after.
        assert!((y + f / 1.0 - (y2 + f / 2.0)).abs() < 1e-9);
    }

    const P: i64 = DOC_PX;

    #[test]
    fn want_is_sent_again_only_when_the_view_leaves_the_prefetch() {
        let mut v = Viewport::new();
        let w = v.want(0, 600.0, 1.0).unwrap();
        assert_eq!(w.range, DocRange::new(0, 600 * P));
        assert_eq!(w.prefetch, 600 * P);
        assert_eq!(v.want(200 * P, 600.0, 1.0), None);
        assert!(v.want(400 * P, 600.0, 1.0).is_some());
        assert!(v.want(400 * P, 600.0, 2.0).is_some(), "a zoom change always asks");
    }

    #[test]
    fn placeholders_fill_what_no_band_covers() {
        let mut v = Viewport::new();
        v.apply(SourceEvent::BandReady { band: 0, range: DocRange::new(0, 100 * P) }, 0);
        v.apply(SourceEvent::BandReady { band: 2, range: DocRange::new(300 * P, 400 * P) }, 0);
        assert_eq!(v.placeholders(50 * P, 300 * P), vec![DocRange::new(100 * P, 300 * P)]);
        assert_eq!(v.placeholders(350 * P, 100 * P), vec![DocRange::new(400 * P, 450 * P)]);
        v.apply(SourceEvent::BandEvicted { band: 0 }, 0);
        assert_eq!(v.placeholders(0, 100 * P), vec![DocRange::new(0, 100 * P)]);
    }

    #[test]
    fn a_band_above_growing_keeps_the_reader_in_place() {
        let mut v = Viewport::new();
        v.apply(SourceEvent::BandReady { band: 0, range: DocRange::new(0, 1000 * P) }, 0);
        v.apply(SourceEvent::BandReady { band: 1, range: DocRange::new(1000 * P, 2000 * P) }, 0);
        // Reading 200 px into band 1.
        let top = 1200 * P;
        // Band 0 grows by 50 px (an image arrived); band 1 moves down.
        let t1 = v.apply(SourceEvent::BandUpdated { band: 0, range: DocRange::new(0, 1050 * P) }, top);
        assert_eq!(t1, top, "band 1 has not moved yet");
        let t2 = v.apply(SourceEvent::BandUpdated { band: 1, range: DocRange::new(1050 * P, 2050 * P) }, t1);
        assert_eq!(t2, 1250 * P);
    }
}
