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

/// One fixed virtual speaker for [`Virtualizer`]: its ITD/ILD, precomputed.
struct VirtualSpeaker {
    left: DelayLine,
    right: DelayLine,
    l_delay: usize,
    r_delay: usize,
    l_gain: f32,
    r_gain: f32,
    /// Behind the listener: a gentle low-pass, the one cue this model has
    /// for front/back (the pinna dulls sound from behind).
    rear: bool,
    lp: f32,
}

/// Headphone virtual surround (lyra-spatial.md §3.1): a 7.1 bed (FL FR FC
/// LFE SL SR RL RR) rendered as eight speakers around the head. LFE goes to
/// both ears unchanged. The output is scaled so the loudest case (a sound in
/// both front speakers) doesn't get louder than it was on plain stereo.
pub struct Virtualizer {
    spk: Vec<VirtualSpeaker>,
    lp_coeff: f32,
}

/// Azimuths of the 7.1 bed's channels (degrees, + = right); `None` for LFE.
const BED_AZ: [Option<f32>; 8] =
    [Some(-30.0), Some(30.0), Some(0.0), None, Some(-110.0), Some(110.0), Some(-150.0), Some(150.0)];
/// Near ear 1 + far ear 0.75 for a front speaker at ±30°: 1/1.75.
const BED_SCALE: f32 = 1.0 / 1.75;

impl Virtualizer {
    pub fn new(fs: f32) -> Self {
        let max_itd = (0.00066 * fs).round() as usize;
        let spk = BED_AZ
            .iter()
            .map(|az| {
                let az = az.unwrap_or(0.0);
                let s = az.to_radians().sin();
                let itd = (max_itd as f32 * s.abs()).round() as usize;
                let shadow = 1.0 - 0.5 * s.abs();
                let (l_delay, r_delay, l_gain, r_gain) =
                    if s >= 0.0 { (itd, 0, shadow, 1.0) } else { (0, itd, 1.0, shadow) };
                VirtualSpeaker {
                    left: DelayLine::new(max_itd),
                    right: DelayLine::new(max_itd),
                    l_delay,
                    r_delay,
                    l_gain,
                    r_gain,
                    rear: az.abs() > 90.0,
                    lp: 0.0,
                }
            })
            .collect();
        // One-pole low-pass at about 5 kHz.
        let lp_coeff = 1.0 - (-2.0 * std::f32::consts::PI * 5_000.0 / fs).exp();
        Virtualizer { spk, lp_coeff }
    }

    /// Render `bed` (interleaved 7.1) and ADD it to `out` (interleaved
    /// stereo). No allocation: it runs on the mix thread.
    pub fn render_add(&mut self, bed: &[f32], out: &mut [f32]) {
        for (f, o) in bed.chunks_exact(8).zip(out.chunks_exact_mut(2)) {
            let (mut l, mut r) = (0.0f32, 0.0f32);
            for (c, (sp, &x)) in self.spk.iter_mut().zip(f).enumerate() {
                if c == 3 {
                    // LFE: no direction.
                    l += x;
                    r += x;
                    continue;
                }
                let x = if sp.rear {
                    sp.lp += (x - sp.lp) * self.lp_coeff;
                    sp.lp
                } else {
                    x
                };
                l += sp.left.tick(x, sp.l_delay) * sp.l_gain;
                r += sp.right.tick(x, sp.r_delay) * sp.r_gain;
            }
            o[0] += l * BED_SCALE;
            o[1] += r * BED_SCALE;
        }
    }

    /// Forget the delay lines and filters (when it is switched on again, so
    /// an old tail doesn't play).
    pub fn reset(&mut self) {
        for sp in &mut self.spk {
            sp.left.buf.iter_mut().for_each(|v| *v = 0.0);
            sp.right.buf.iter_mut().for_each(|v| *v = 0.0);
            sp.lp = 0.0;
        }
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

#[cfg(test)]
mod virtualizer_tests {
    use super::*;

    fn impulse_in(ch: usize) -> Vec<f32> {
        let mut bed = vec![0.0f32; 64 * 8];
        bed[ch] = 1.0;
        bed
    }

    #[test]
    fn the_left_surround_reaches_the_left_ear_first() {
        let mut v = Virtualizer::new(48_000.0);
        let mut out = vec![0.0f32; 128];
        v.render_add(&impulse_in(4), &mut out); // SL at -110°
        assert!(out[0] > 0.0 && out[1] == 0.0, "left ear first: {:?}", &out[..4]);
        let r_at = (0..64).find(|&f| out[f * 2 + 1] != 0.0).unwrap();
        assert!(r_at > 10, "right ear later: {r_at}");
    }

    #[test]
    fn front_centre_is_symmetric_and_stereo_never_gets_louder() {
        let mut v = Virtualizer::new(48_000.0);
        let mut out = vec![0.0f32; 128];
        v.render_add(&impulse_in(2), &mut out);
        assert!((out[0] - out[1]).abs() < 1e-6);
        // A steady sound in both front speakers (stereo's loudest case).
        let mut v = Virtualizer::new(48_000.0);
        let bed: Vec<f32> = (0..4800).flat_map(|_| [1.0f32, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]).collect();
        let mut out = vec![0.0f32; 9600];
        v.render_add(&bed, &mut out);
        assert!(out.iter().all(|x| x.abs() <= 1.0 + 1e-5), "max {}", out.iter().cloned().fold(0.0, f32::max));
    }

    #[test]
    fn behind_is_duller_than_in_front() {
        // Same side, front (FL -30°) vs rear (RL -150°): equal ITD/ILD, so
        // only the rear low-pass tells them apart: a 10 kHz tone is quieter.
        let tone = |ch: usize| {
            let mut v = Virtualizer::new(48_000.0);
            let bed: Vec<f32> = (0..4800).flat_map(|i| {
                let mut f = [0.0f32; 8];
                f[ch] = (2.0 * std::f32::consts::PI * 10_000.0 * i as f32 / 48_000.0).sin();
                f
            }).collect();
            let mut out = vec![0.0f32; 9600];
            v.render_add(&bed, &mut out);
            out[4800..].iter().step_by(2).map(|x| x * x).sum::<f32>()
        };
        assert!(tone(6) < tone(0) * 0.5);
    }
}
