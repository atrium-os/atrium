//! Binaural rendering for headphones — the ITD/ILD cue model
//! (`docs/spec/atrium-lyra-architecture.md` §12 spatial, headphone path).
//!
//! `spatial.rs` pans objects to *speakers*; on headphones there are only two,
//! at the ears, so spatialisation comes from the two dominant binaural cues:
//! - **ITD** (interaural *time* difference) — sound reaches the near ear first;
//!   the contralateral (far) ear is delayed by up to ~0.66 ms. This is the
//!   dominant low-frequency localisation cue.
//! - **ILD** (interaural *level* difference) — the head shadows the far ear, so
//!   it is quieter.
//!
//! A full HRTF (head-related transfer function) convolves a measured impulse per
//! ear (`convolve.rs` is ready for it) — better, but a *dataset*. This
//! parametric ITD/ILD model needs no data and gives correct lateralisation; the
//! HRTF is a drop-in quality upgrade.

/// A fixed-capacity per-ear delay line (a ring of the recent input).
struct DelayLine {
    buf: Vec<f32>,
    pos: usize,
}

impl DelayLine {
    fn new(max_delay: usize) -> Self {
        DelayLine { buf: vec![0.0; max_delay + 1], pos: 0 }
    }
    /// Push `x`, return the sample `delay` samples ago.
    fn tick(&mut self, x: f32, delay: usize) -> f32 {
        self.buf[self.pos] = x;
        let n = self.buf.len();
        let read = (self.pos + n - delay.min(n - 1)) % n;
        let y = self.buf[read];
        self.pos = (self.pos + 1) % n;
        y
    }
}

/// A binaural renderer for one mono object.
pub struct Binaural {
    left: DelayLine,
    right: DelayLine,
    /// Maximum ITD in samples (~0.66 ms at the sample rate).
    max_itd: usize,
}

impl Binaural {
    pub fn new(fs: f32) -> Self {
        let max_itd = (0.00066 * fs).round() as usize;
        Binaural {
            left: DelayLine::new(max_itd),
            right: DelayLine::new(max_itd),
            max_itd,
        }
    }

    /// Render `mono` to interleaved stereo for an object at `azimuth_deg`
    /// (0 = front, + = right, − = left). The far ear is delayed (ITD) and
    /// attenuated (ILD). State persists across calls (streaming).
    pub fn render(&mut self, mono: &[f32], azimuth_deg: f32) -> Vec<f32> {
        let az = azimuth_deg.to_radians();
        let s = az.sin(); // + = right
        // ITD: the contralateral ear lags by max_itd·|sin az|.
        let itd = (self.max_itd as f32 * s.abs()).round() as usize;
        // ILD: far ear down to −6 dB at the side; near ear unity.
        let shadow = 1.0 - 0.5 * s.abs();
        // s > 0 (right) -> right ear near (no delay, unity); left far.
        let (l_delay, r_delay, l_gain, r_gain) = if s >= 0.0 {
            (itd, 0usize, shadow, 1.0)
        } else {
            (0usize, itd, 1.0, shadow)
        };
        let mut out = vec![0.0f32; mono.len() * 2];
        for (i, &x) in mono.iter().enumerate() {
            out[i * 2] = self.left.tick(x, l_delay) * l_gain;
            out[i * 2 + 1] = self.right.tick(x, r_delay) * r_gain;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const FS: f32 = 48_000.0;

    #[test]
    fn centred_object_is_symmetric() {
        let mut b = Binaural::new(FS);
        let out = b.render(&[1.0, 0.5, -0.3], 0.0);
        for f in 0..3 {
            assert!((out[f * 2] - out[f * 2 + 1]).abs() < 1e-6, "L==R at centre");
        }
    }

    #[test]
    fn hard_right_reaches_right_ear_first_and_louder() {
        let mut b = Binaural::new(FS);
        // an impulse, panned hard right.
        let mut sig = vec![0.0f32; 64];
        sig[0] = 1.0;
        let out = b.render(&sig, 90.0);
        // right ear: impulse at frame 0, full gain.
        assert!((out[0 * 2 + 1] - 1.0).abs() < 1e-6, "right ear immediate, unity");
        assert!(out[0 * 2].abs() < 1e-6, "left ear silent at frame 0 (delayed)");
        // left ear: the impulse appears later (the ITD) and attenuated.
        let itd = (0.00066 * FS).round() as usize;
        assert!(out[itd * 2] > 0.4 && out[itd * 2] < 1.0, "left delayed + shadowed");
    }

    #[test]
    fn hard_left_is_the_mirror() {
        let mut b = Binaural::new(FS);
        let mut sig = vec![0.0f32; 64];
        sig[0] = 1.0;
        let out = b.render(&sig, -90.0);
        assert!((out[0] - 1.0).abs() < 1e-6, "left ear immediate, unity");
        assert!(out[1].abs() < 1e-6, "right ear delayed");
    }

    #[test]
    fn itd_grows_with_azimuth() {
        // larger azimuth -> larger interaural delay.
        let delay_at = |deg: f32| {
            let mut b = Binaural::new(FS);
            let mut sig = vec![0.0f32; 64];
            sig[0] = 1.0;
            let out = b.render(&sig, deg);
            // find the frame where the (delayed) left ear gets the impulse.
            (0..64).find(|&f| out[f * 2] > 0.3).unwrap_or(0)
        };
        assert!(delay_at(90.0) > delay_at(30.0), "more lateral -> more ITD");
        assert!(delay_at(30.0) > delay_at(5.0));
    }
}
