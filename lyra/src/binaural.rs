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
    /// The measured HRTF renderer (lyra-spatial.md S3), when there is one for
    /// this rate and period; the ITD/ILD model otherwise.
    hrtf: Option<crate::convolve::PartitionedConvolver>,
}

/// The 7.1 bed rendered through FABIAN's measured HRTFs (CC BY 4.0; made by
/// `scripts/hrtf-bed.py`, credits in `lyra/data/README.md`).
static HRTF_BED: &[u8] = include_bytes!("../data/hrtf-bed-fabian-48k.bin");

/// The bed's impulse responses from [`HRTF_BED`]: (rate, taps, [speaker][ear]
/// responses), or `None` if the data is malformed.
pub fn hrtf_bed() -> Option<(u32, usize, Vec<Vec<f32>>)> {
    let (head, body) = (HRTF_BED.get(..24)?, &HRTF_BED[24..]);
    let w = |i: usize| u32::from_le_bytes(head[i..i + 4].try_into().unwrap());
    if &head[..4] != b"LYHR" || w(4) != 1 || w(16) != 8 || w(20) != 2 {
        return None;
    }
    let (rate, taps) = (w(8), w(12) as usize);
    if body.len() != 16 * taps * 4 {
        return None;
    }
    let all: Vec<f32> = body.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect();
    if all.iter().any(|v| !v.is_finite() || v.abs() > 8.0) {
        return None;
    }
    Some((rate, taps, all.chunks(taps).map(|c| c.to_vec()).collect()))
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
        Virtualizer { spk, lp_coeff, hrtf: None }
    }

    /// The measured-HRTF virtualizer for periods of `period` frames, if the
    /// HRTF data is for rate `fs`; the ITD/ILD model otherwise. The
    /// convolution runs in blocks of the largest power of two dividing the
    /// period (at most 256), so it adds no latency.
    pub fn for_period(fs: f32, period: usize) -> Self {
        let mut v = Virtualizer::new(fs);
        let block = 1usize << period.trailing_zeros().min(8);
        if let Some((rate, taps, irs)) = hrtf_bed() {
            if rate as f32 == fs && period > 0 && block >= 16 {
                v.hrtf = Some(crate::convolve::PartitionedConvolver::new(block, 8, taps, |i, o| irs[i * 2 + o].clone()));
            }
        }
        v
    }

    /// Whether the measured HRTFs render (not the ITD/ILD model).
    pub fn is_hrtf(&self) -> bool {
        self.hrtf.is_some()
    }

    /// Render `bed` (interleaved 7.1) and ADD it to `out` (interleaved
    /// stereo). No allocation: it runs on the mix thread.
    pub fn render_add(&mut self, bed: &[f32], out: &mut [f32]) {
        if let Some(c) = &mut self.hrtf {
            c.process_add(bed, out);
            return;
        }
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
        if let Some(c) = &mut self.hrtf {
            c.reset();
        }
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

    /// Steady tone `hz` in bed channel `ch` through `v`; (left, right) power.
    fn bed_tone(v: &mut Virtualizer, ch: usize, hz: f32) -> (f32, f32) {
        let bed: Vec<f32> = (0..9600).flat_map(|i| {
            let mut f = [0.0f32; 8];
            f[ch] = (2.0 * std::f32::consts::PI * hz * i as f32 / 48_000.0).sin();
            f
        }).collect();
        let mut out = vec![0.0f32; 19200];
        for (b, o) in bed.chunks(128 * 8).zip(out.chunks_mut(256)) {
            v.render_add(b, o);
        }
        let p = |c: usize| out[9600..].iter().skip(c).step_by(2).map(|x| x * x).sum::<f32>() / 4800.0;
        (p(0), p(1))
    }

    #[test]
    fn the_measured_hrtfs_load_for_48k_and_whole_blocks_only() {
        let (rate, taps, irs) = hrtf_bed().expect("bundled HRTF bed");
        assert_eq!((rate, taps, irs.len()), (48_000, 256, 16));
        assert!(Virtualizer::for_period(48_000.0, 128).is_hrtf());
        assert!(Virtualizer::for_period(48_000.0, 480).is_hrtf(), "blocks of 32");
        assert!(!Virtualizer::for_period(44_100.0, 128).is_hrtf(), "data is for 48 kHz");
        assert!(!Virtualizer::for_period(48_000.0, 100).is_hrtf(), "blocks of 4 are too small");
    }

    #[test]
    fn hrtf_places_the_sides_and_keeps_stereo_as_loud() {
        let mut v = Virtualizer::for_period(48_000.0, 128);
        // Left surround: louder and earlier at the left ear.
        let (l, r) = bed_tone(&mut v, 4, 2000.0);
        assert!(10.0 * (l / r).log10() > 8.0, "{l} {r}");
        let mut v = Virtualizer::for_period(48_000.0, 128);
        let mut imp = vec![0.0f32; 256 * 8];
        imp[4] = 1.0;
        let mut out = vec![0.0f32; 512];
        v.render_add(&imp, &mut out);
        let onset = |c: usize| out.iter().skip(c).step_by(2).position(|x| x.abs() > 0.05).unwrap();
        assert!(onset(0) + 10 < onset(1), "ITD: left {} right {}", onset(0), onset(1));
        // Right surround mirrors it.
        let mut v = Virtualizer::for_period(48_000.0, 128);
        let (l, r) = bed_tone(&mut v, 5, 2000.0);
        assert!(10.0 * (r / l).log10() > 8.0);
        // Stereo content (both fronts, coherent) is as loud as before on
        // average over 100 Hz to 10 kHz (a sine has power 0.5); single tones
        // vary with the head's comb and resonances, never by much.
        let mut sum = 0.0;
        let hzs: Vec<f32> = (0..12).map(|k| 100.0 * 100f32.powf(k as f32 / 11.0)).collect();
        for &hz in &hzs {
            let mut v = Virtualizer::for_period(48_000.0, 128);
            let bed: Vec<f32> = (0..9600).flat_map(|i| {
                let x = (2.0 * std::f32::consts::PI * hz * i as f32 / 48_000.0).sin();
                [x, x, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]
            }).collect();
            let mut out = vec![0.0f32; 19200];
            v.render_add(&bed, &mut out);
            let p = out[9600..].iter().map(|x| x * x).sum::<f32>() / 9600.0;
            sum += p / 0.5;
            let db = 10.0 * (p / 0.5).log10();
            assert!(db < 9.0 && db > -30.0, "{hz} Hz: {db:.1} dB");
        }
        let avg = 10.0 * (sum / hzs.len() as f32).log10();
        assert!(avg.abs() < 1.5, "average {avg:.1} dB");
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
