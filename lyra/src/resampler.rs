//! Sample-rate conversion (subsumes the kernel's `feeder_rate.c`).
//!
//! The production counterpart of the gpusim clock-domain model
//! (`lyra_clock::DomainResampler`): a **streaming** resampler with a
//! **runtime-adjustable ratio**, which is what measured-drift reconciliation
//! (§4) needs — feed it `actual_rate_in / actual_rate_out` each control tick and
//! it tracks the drift. Also the client-rate → device-rate adapter (resample
//! *once*, §1).
//!
//! Linear interpolation here — the correct, streaming reference; a
//! windowed-sinc/polyphase kernel is the quality upgrade behind the same
//! streaming contract (the convolution machinery is already in `convolve.rs`),
//! noted for when SRC quality matters.

/// A one-channel streaming resampler.
pub struct Resampler {
    /// Input samples consumed per output sample (`rate_in / rate_out`).
    ratio: f64,
    /// Fractional read position within `buf`.
    frac: f64,
    /// Unconsumed input (carried across calls); always keeps ≥1 sample so the
    /// next interpolation has its left point.
    buf: Vec<f32>,
}

impl Resampler {
    pub fn new(rate_in: u32, rate_out: u32) -> Self {
        Resampler {
            ratio: rate_in as f64 / rate_out as f64,
            frac: 0.0,
            buf: Vec::new(),
        }
    }

    /// Update the ratio (drift correction): `rate_in / rate_out`, measured.
    pub fn set_ratio(&mut self, rate_in: f64, rate_out: f64) {
        self.ratio = rate_in / rate_out;
    }

    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    /// Resample `input`, returning as many output samples as the ratio and the
    /// buffered input allow. Streaming: state persists across calls.
    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        self.buf.extend_from_slice(input);
        let mut out = Vec::new();
        // produce while we have a right interpolation point.
        while (self.frac as usize) + 1 < self.buf.len() {
            let i = self.frac as usize;
            let f = (self.frac - i as f64) as f32;
            out.push(self.buf[i] * (1.0 - f) + self.buf[i + 1] * f);
            self.frac += self.ratio;
        }
        // drop fully-consumed samples, keeping the current left point.
        let consumed = (self.frac as usize).min(self.buf.len().saturating_sub(1));
        if consumed > 0 {
            self.buf.drain(0..consumed);
            self.frac -= consumed as f64;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unity_ratio_passes_through() {
        let mut r = Resampler::new(48_000, 48_000);
        // at unity ratio output equals input in order; the last sample of a
        // buffer is held as the next interpolation's left point (1-sample tail).
        let out = r.process(&[1.0, 2.0, 3.0, 4.0, 5.0]);
        assert_eq!(out.len(), 4, "{out:?}");
        assert!((out[0] - 1.0).abs() < 1e-5 && (out[3] - 4.0).abs() < 1e-5, "{out:?}");
    }

    #[test]
    fn upsample_2x_doubles_and_interpolates() {
        let mut r = Resampler::new(24_000, 48_000); // ratio 0.5
        let _ = r.process(&[0.0]); // warm up
        let out = r.process(&[0.0, 2.0, 4.0]);
        // ~2x as many samples; midpoints are linear interpolations.
        assert!(out.len() >= 5, "roughly doubled: {}", out.len());
        // somewhere a ~1.0 appears (midpoint of 0 and 2).
        assert!(out.iter().any(|&x| (x - 1.0).abs() < 0.1), "interpolated midpoint: {out:?}");
    }

    #[test]
    fn downsample_2x_halves() {
        let mut r = Resampler::new(48_000, 24_000); // ratio 2.0
        let input: Vec<f32> = (0..100).map(|i| i as f32).collect();
        let out = r.process(&input);
        assert!((out.len() as i32 - 50).abs() <= 1, "~half: {}", out.len());
    }

    #[test]
    fn output_rate_matches_the_ratio() {
        // 48k -> 44.1k: output length ~ input * 44100/48000.
        let mut r = Resampler::new(48_000, 44_100);
        let input = vec![0.5f32; 48_000]; // 1 s of input
        let out = r.process(&input);
        let expect = 44_100i32;
        assert!((out.len() as i32 - expect).abs() < 5, "got {} expect ~{expect}", out.len());
    }

    #[test]
    fn streaming_equals_one_shot() {
        let sig: Vec<f32> = (0..200).map(|i| (i as f32 * 0.21).sin()).collect();
        let mut whole = Resampler::new(48_000, 44_100);
        let one = whole.process(&sig);
        let mut split = Resampler::new(48_000, 44_100);
        let mut pieced = Vec::new();
        for c in sig.chunks(13) {
            pieced.extend(split.process(c));
        }
        assert_eq!(pieced.len(), one.len());
        for (a, b) in pieced.iter().zip(&one) {
            assert!((a - b).abs() < 1e-5, "split == whole");
        }
    }

    #[test]
    fn ratio_is_runtime_adjustable_for_drift() {
        let mut r = Resampler::new(48_000, 48_000);
        assert!((r.ratio() - 1.0).abs() < 1e-9);
        // a measured +50 ppm drift on the input clock.
        r.set_ratio(48_002.4, 48_000.0);
        assert!(r.ratio() > 1.0, "tracks measured drift");
    }
}
