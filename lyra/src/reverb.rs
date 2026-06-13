//! Reverberation — a Schroeder/Freeverb comb+allpass network.
//!
//! The room-sound effect, and the *parametric* sibling of convolution reverb
//! (`convolve.rs` with a measured/ray-traced IR, §13): parallel damped **comb**
//! filters build the decaying echo density, series **allpass** filters smear it
//! into diffusion. `room_size` sets the decay time (comb feedback), `damping`
//! rolls off the high frequencies in the tail (a real room absorbs treble
//! faster). Mono here; the graph runs two with decorrelated delays for stereo.

/// A damped comb: feedback delay with a one-pole lowpass in the loop.
struct Comb {
    buf: Vec<f32>,
    pos: usize,
    store: f32,
    feedback: f32,
    damp: f32,
}
impl Comb {
    fn new(delay: usize, feedback: f32, damp: f32) -> Self {
        Comb { buf: vec![0.0; delay.max(1)], pos: 0, store: 0.0, feedback, damp }
    }
    fn tick(&mut self, x: f32) -> f32 {
        let out = self.buf[self.pos];
        self.store = out * (1.0 - self.damp) + self.store * self.damp;
        self.buf[self.pos] = x + self.store * self.feedback;
        self.pos = (self.pos + 1) % self.buf.len();
        out
    }
}

/// A Schroeder allpass: diffuses without colouring the magnitude response.
struct Allpass {
    buf: Vec<f32>,
    pos: usize,
    feedback: f32,
}
impl Allpass {
    fn new(delay: usize, feedback: f32) -> Self {
        Allpass { buf: vec![0.0; delay.max(1)], pos: 0, feedback }
    }
    fn tick(&mut self, x: f32) -> f32 {
        let buffered = self.buf[self.pos];
        let out = -x + buffered;
        self.buf[self.pos] = x + buffered * self.feedback;
        self.pos = (self.pos + 1) % self.buf.len();
        out
    }
}

pub struct Reverb {
    combs: Vec<Comb>,
    allpasses: Vec<Allpass>,
    /// Wet (reverb) mix added to the dry signal, 0..1.
    wet: f32,
}

impl Reverb {
    /// `room_size` and `damping` in 0..1.
    pub fn new(fs: f32, room_size: f32, damping: f32) -> Self {
        let scale = fs / 44_100.0;
        let s = |d: usize| ((d as f32) * scale) as usize;
        // Freeverb's mutually-detuned comb/allpass delays.
        let fb = 0.70 + 0.28 * room_size.clamp(0.0, 1.0); // 0.70..0.98
        let dp = (0.4 * damping.clamp(0.0, 1.0)).min(0.99);
        let combs = [1116, 1188, 1277, 1356, 1422, 1491]
            .iter()
            .map(|&d| Comb::new(s(d), fb, dp))
            .collect();
        let allpasses =
            [556, 441, 341].iter().map(|&d| Allpass::new(s(d), 0.5)).collect();
        Reverb { combs, allpasses, wet: 0.3 }
    }

    fn tick(&mut self, x: f32) -> f32 {
        let mut acc = 0.0;
        for c in self.combs.iter_mut() {
            acc += c.tick(x);
        }
        acc /= self.combs.len() as f32;
        for a in self.allpasses.iter_mut() {
            acc = a.tick(acc);
        }
        acc
    }

    /// Add reverb to `buf` in place (dry + wet·tail).
    pub fn process(&mut self, buf: &mut [f32]) {
        for s in buf.iter_mut() {
            let wet = self.tick(*s);
            *s += self.wet * wet;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const FS: f32 = 48_000.0;

    fn impulse_response(rev: &mut Reverb, n: usize) -> Vec<f32> {
        let mut buf = vec![0.0f32; n];
        buf[0] = 1.0;
        rev.process(&mut buf);
        buf
    }

    #[test]
    fn impulse_produces_a_decaying_tail() {
        let mut r = Reverb::new(FS, 0.7, 0.3);
        let ir = impulse_response(&mut r, 48_000); // 1 s
        // there is energy well after the impulse (a tail exists).
        let late: f32 = ir[10_000..20_000].iter().map(|x| x * x).sum();
        assert!(late > 1e-6, "a reverb tail exists: {late}");
        // and it decays: later energy < earlier energy.
        let early: f32 = ir[2_000..12_000].iter().map(|x| x * x).sum();
        let later: f32 = ir[20_000..30_000].iter().map(|x| x * x).sum();
        assert!(later < early, "tail decays ({later} < {early})");
    }

    #[test]
    fn bigger_room_decays_slower() {
        let energy_after = |room: f32| {
            let mut r = Reverb::new(FS, room, 0.3);
            let ir = impulse_response(&mut r, 48_000);
            ir[24_000..].iter().map(|x| x * x).sum::<f32>()
        };
        assert!(energy_after(0.9) > energy_after(0.5), "bigger room rings longer");
    }

    #[test]
    fn stays_stable() {
        // feed a second of full-scale noise; output must not blow up.
        let mut r = Reverb::new(FS, 0.9, 0.2);
        let mut buf: Vec<f32> = (0..48_000).map(|i| if i % 2 == 0 { 1.0 } else { -1.0 }).collect();
        r.process(&mut buf);
        assert!(buf.iter().all(|s| s.is_finite() && s.abs() < 10.0), "bounded");
    }

    #[test]
    fn dry_is_preserved_at_the_onset() {
        // the dry impulse still passes through at t=0 (dry + wet, wet~0 at t=0).
        let mut r = Reverb::new(FS, 0.7, 0.3);
        let ir = impulse_response(&mut r, 1000);
        assert!(ir[0] >= 1.0, "dry impulse preserved: {}", ir[0]);
    }
}
