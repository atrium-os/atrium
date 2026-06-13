//! Biquad filters — the parametric EQ building block
//! (subsumes the kernel's `feeder_eq.c`, §4.2).
//!
//! A biquad is a second-order IIR section; cascade them for a graphic/parametric
//! EQ, a crossover, or a tone control. Coefficients are the RBJ Audio-EQ-Cookbook
//! forms (lowpass / highpass / peaking / shelves). Per-sample streaming state, so
//! it runs as an effect node one buffer at a time. The kernel runs a fixed
//! `feeder_eq` in-band at interrupt time; Lyra runs this in a capability-jailed
//! node on the deadline lane.

/// A second-order section: `y = b0·x + b1·x₋₁ + b2·x₋₂ − a1·y₋₁ − a2·y₋₂`
/// (coefficients pre-normalised by a0).
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Biquad {
    fn from_raw(b0: f32, b1: f32, b2: f32, a0: f32, a1: f32, a2: f32) -> Self {
        Biquad {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    fn omega_alpha(f0: f32, fs: f32, q: f32) -> (f32, f32, f32) {
        let w0 = 2.0 * std::f32::consts::PI * f0 / fs;
        let (sn, cs) = w0.sin_cos();
        (w0, cs, sn / (2.0 * q))
    }

    /// 2nd-order lowpass at `f0` (Hz), quality `q` (0.707 = Butterworth).
    pub fn lowpass(f0: f32, fs: f32, q: f32) -> Self {
        let (_w, cs, al) = Self::omega_alpha(f0, fs, q);
        Self::from_raw((1.0 - cs) / 2.0, 1.0 - cs, (1.0 - cs) / 2.0, 1.0 + al, -2.0 * cs, 1.0 - al)
    }

    /// 2nd-order highpass.
    pub fn highpass(f0: f32, fs: f32, q: f32) -> Self {
        let (_w, cs, al) = Self::omega_alpha(f0, fs, q);
        Self::from_raw((1.0 + cs) / 2.0, -(1.0 + cs), (1.0 + cs) / 2.0, 1.0 + al, -2.0 * cs, 1.0 - al)
    }

    /// Peaking (parametric) EQ: `db_gain` boost/cut at `f0`, bandwidth from `q`.
    pub fn peaking(f0: f32, fs: f32, q: f32, db_gain: f32) -> Self {
        let a = 10.0f32.powf(db_gain / 40.0);
        let (_w, cs, al) = Self::omega_alpha(f0, fs, q);
        Self::from_raw(
            1.0 + al * a,
            -2.0 * cs,
            1.0 - al * a,
            1.0 + al / a,
            -2.0 * cs,
            1.0 - al / a,
        )
    }

    /// Process one sample (Direct Form I).
    #[inline]
    pub fn tick(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }

    /// Process a buffer in place (streaming state persists).
    pub fn process(&mut self, buf: &mut [f32]) {
        for s in buf.iter_mut() {
            *s = self.tick(*s);
        }
    }

    /// |H(f)| — magnitude response at frequency `f` (Hz), for verification.
    pub fn magnitude(&self, f: f32, fs: f32) -> f32 {
        let w = 2.0 * std::f32::consts::PI * f / fs;
        // evaluate on the unit circle z = e^{jw}
        let (c1, s1) = w.cos_neg_sin(); // z^-1
        let (c2, s2) = (2.0 * w).cos_neg_sin(); // z^-2
        let num_re = self.b0 + self.b1 * c1 + self.b2 * c2;
        let num_im = self.b1 * s1 + self.b2 * s2;
        let den_re = 1.0 + self.a1 * c1 + self.a2 * c2;
        let den_im = self.a1 * s1 + self.a2 * s2;
        ((num_re * num_re + num_im * num_im) / (den_re * den_re + den_im * den_im)).sqrt()
    }
}

trait CosNegSin {
    fn cos_neg_sin(self) -> (f32, f32);
}
impl CosNegSin for f32 {
    /// (cos w, −sin w) — the real/imag of e^{−jw}.
    fn cos_neg_sin(self) -> (f32, f32) {
        let (s, c) = self.sin_cos();
        (c, -s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const FS: f32 = 48_000.0;

    #[test]
    fn lowpass_passes_dc_blocks_nyquist() {
        let lp = Biquad::lowpass(1_000.0, FS, 0.707);
        assert!((lp.magnitude(0.0, FS) - 1.0).abs() < 1e-4, "DC passes");
        assert!(lp.magnitude(FS / 2.0, FS) < 0.01, "Nyquist blocked");
        // −3 dB at the cutoff (Butterworth Q).
        assert!((lp.magnitude(1_000.0, FS) - 0.7071).abs() < 0.02, "−3 dB at f0");
    }

    #[test]
    fn highpass_blocks_dc_passes_nyquist() {
        let hp = Biquad::highpass(1_000.0, FS, 0.707);
        assert!(hp.magnitude(0.0, FS) < 1e-4, "DC blocked");
        assert!((hp.magnitude(FS / 2.0, FS) - 1.0).abs() < 1e-3, "Nyquist passes");
    }

    #[test]
    fn peaking_boosts_at_centre() {
        let pk = Biquad::peaking(1_000.0, FS, 1.0, 6.0); // +6 dB
        let g = pk.magnitude(1_000.0, FS);
        let want = 10.0f32.powf(6.0 / 20.0); // ~2.0
        assert!((g - want).abs() < 0.05, "boost {g} vs {want}");
        // far from centre, unity.
        assert!((pk.magnitude(50.0, FS) - 1.0).abs() < 0.05, "unity away from f0");
    }

    #[test]
    fn lowpass_actually_smooths_a_step() {
        // a step into a lowpass rises gradually, never overshoots past ~1 much.
        let mut lp = Biquad::lowpass(1_000.0, FS, 0.707);
        let mut buf = vec![1.0f32; 256];
        lp.process(&mut buf);
        assert!(buf[0] < 0.2, "starts low: {}", buf[0]);
        assert!(buf[255] > 0.9, "settles to ~1: {}", buf[255]);
    }

    #[test]
    fn streaming_state_persists_across_buffers() {
        let mk = || Biquad::lowpass(2_000.0, FS, 0.707);
        let sig: Vec<f32> = (0..128).map(|i| (i as f32 * 0.3).sin()).collect();
        let mut whole = mk();
        let mut a = sig.clone();
        whole.process(&mut a);
        let mut split = mk();
        let mut b = sig.clone();
        let (l, r) = b.split_at_mut(50);
        split.process(l);
        split.process(r);
        for (x, y) in a.iter().zip(&b) {
            assert!((x - y).abs() < 1e-5, "split == whole");
        }
    }
}
