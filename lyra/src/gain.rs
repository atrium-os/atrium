//! Smoothed gain (subsumes the kernel's `feeder_volume.c`).
//!
//! Per-stream/per-channel volume. The only subtlety is the one most stacks get
//! wrong: a *stepped* gain change clicks ("zipper noise"), so the gain ramps to
//! its target with a one-pole smoother instead of jumping. Streaming state, runs
//! as a node (or folded into the mix).

/// How close the ramp must come before it lands on its target outright: about
/// −140 dB of full scale, far below anything a ramp step could be heard at.
const SNAP: f32 = 1e-7;

/// A one-channel smoothed gain.
pub struct Gain {
    current: f32,
    target: f32,
    /// Per-sample smoothing coefficient in (0, 1]; smaller = slower ramp.
    coeff: f32,
}

impl Gain {
    /// `ramp_ms` is the ~time-constant of the ramp at sample rate `fs`.
    pub fn new(initial: f32, fs: f32, ramp_ms: f32) -> Self {
        let samples = (ramp_ms * 0.001 * fs).max(1.0);
        Gain { current: initial, target: initial, coeff: 1.0 / samples }
    }

    pub fn set_target(&mut self, g: f32) {
        self.target = g;
    }

    /// One ramp step. A one-pole ramp in f32 stalls short of its target once
    /// a step rounds to nothing (at 384 kHz with a 20 ms ramp, about 2e-4
    /// below 1.0), and unity must be exactly 1.0 for a bit-perfect path: so
    /// a step that would not move, or one within [`SNAP`], lands on the target.
    #[inline]
    fn step(&mut self) {
        let d = self.target - self.current;
        let next = self.current + d * self.coeff;
        self.current = if d.abs() <= SNAP || next == self.current { self.target } else { next };
    }

    /// One sample's gain, advancing the ramp (for per-channel use).
    pub fn next(&mut self) -> f32 {
        self.step();
        self.current
    }

    /// At its target: no ramp in progress.
    pub fn settled(&self) -> bool {
        self.current == self.target
    }

    /// Go to `g` at once, with no ramp (for a stream nothing has heard yet).
    pub fn jump(&mut self, g: f32) {
        self.target = g;
        self.current = g;
    }

    pub fn target(&self) -> f32 {
        self.target
    }

    pub fn current(&self) -> f32 {
        self.current
    }

    /// Apply the (ramping) gain to interleaved frames of `ch` channels: one
    /// ramp step per frame, the same gain on every channel of a frame, so the
    /// ramp takes as long whatever the layout.
    pub fn process_frames(&mut self, buf: &mut [f32], ch: usize) {
        for f in buf.chunks_mut(ch.max(1)) {
            self.step();
            f.iter_mut().for_each(|s| *s *= self.current);
        }
    }

    /// Apply the (ramping) gain in place.
    pub fn process(&mut self, buf: &mut [f32]) {
        for s in buf.iter_mut() {
            self.step();
            *s *= self.current;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const FS: f32 = 48_000.0;

    #[test]
    fn steady_gain_just_scales() {
        let mut g = Gain::new(0.5, FS, 5.0);
        let mut buf = vec![1.0f32; 64];
        g.process(&mut buf);
        // already at target -> ~0.5 throughout.
        assert!(buf.iter().all(|&s| (s - 0.5).abs() < 1e-3));
    }

    #[test]
    fn change_ramps_not_steps() {
        let mut g = Gain::new(0.0, FS, 5.0);
        g.set_target(1.0);
        let mut buf = vec![1.0f32; 4];
        g.process(&mut buf);
        // first sample is NOT instantly 1.0 (that would zipper-click).
        assert!(buf[0] < 0.5, "ramps, not steps: {}", buf[0]);
        assert!(buf[3] > buf[0], "rising");
    }

    #[test]
    fn reaches_the_target() {
        let mut g = Gain::new(0.0, FS, 1.0);
        g.set_target(0.75);
        let mut buf = vec![1.0f32; 4096]; // >> ramp
        g.process(&mut buf);
        assert!((g.current() - 0.75).abs() < 1e-3, "settled to target");
        assert!((buf[4095] - 0.75).abs() < 1e-3);
    }

    #[test]
    fn mute_ramps_down() {
        let mut g = Gain::new(1.0, FS, 1.0);
        g.set_target(0.0);
        let mut buf = vec![1.0f32; 4096];
        g.process(&mut buf);
        assert!(g.current() < 1e-3, "ramped to silence");
        assert!(buf[0] > 0.5 && buf[4095] < 0.01, "smooth fade, not a cut");
    }

    #[test]
    fn a_ramp_to_unity_lands_exactly_on_it() {
        // Bit-perfect needs exactly 1.0, not the ulp below it a one-pole
        // ramp in f32 can stall at.
        let mut g = Gain::new(0.0, 384_000.0, 20.0);
        g.set_target(1.0);
        let mut buf = vec![0.123_456_78f32; 384_000];
        g.process_frames(&mut buf, 2);
        assert!(g.settled());
        assert_eq!(g.current(), 1.0);
        assert_eq!(*buf.last().unwrap(), 0.123_456_78);
    }
}
