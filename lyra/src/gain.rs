//! Smoothed gain (subsumes the kernel's `feeder_volume.c`).
//!
//! Per-stream/per-channel volume. The only subtlety is the one most stacks get
//! wrong: a *stepped* gain change clicks ("zipper noise"), so the gain ramps to
//! its target with a one-pole smoother instead of jumping. Streaming state, runs
//! as a node (or folded into the mix).

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

    pub fn current(&self) -> f32 {
        self.current
    }

    /// Apply the (ramping) gain in place.
    pub fn process(&mut self, buf: &mut [f32]) {
        for s in buf.iter_mut() {
            self.current += (self.target - self.current) * self.coeff;
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
}
