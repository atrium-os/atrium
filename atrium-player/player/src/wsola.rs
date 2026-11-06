//! Playback rate without a pitch change: WSOLA (waveform-similarity
//! overlap-add). Output is built from 20 ms Hann-windowed segments of the
//! input overlapped by half; segment k is taken near input position
//! k·Ha (Ha = rate × the output hop), shifted by up to ±Δ to the place
//! that best continues the previous segment, so the waveform stays in
//! phase and the pitch is kept.
//!
//! Streaming: push input as it comes, pull output as there is room. Output
//! frame j stands for input frame j × rate.

/// Segment length and search range, in ms.
const SEG_MS: u32 = 20;
const SEARCH_MS: u32 = 8;

pub struct Wsola {
    ch: usize,
    rate: f64,
    seg: usize,
    hop: usize,
    search: usize,
    window: Vec<f32>,
    /// Input frames not yet dropped, interleaved; `base` is the absolute
    /// input frame of `input[0]`.
    input: Vec<f32>,
    base: u64,
    /// Where the next segment ideally starts (absolute input frames).
    ideal: f64,
    /// Where the last segment started.
    prev: Option<u64>,
    /// The second half of the last windowed segment, to overlap.
    tail: Vec<f32>,
    out: Vec<f32>,
}

impl Wsola {
    pub fn new(rate_hz: u32, channels: usize, rate: f64) -> Self {
        let seg = (rate_hz * SEG_MS / 1000) as usize & !1;
        let hop = seg / 2;
        // Periodic Hann: overlapping halves sum to exactly one.
        let window = (0..seg).map(|i| (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / seg as f64).cos()) as f32).collect();
        Wsola {
            ch: channels,
            rate,
            seg,
            hop,
            search: (rate_hz * SEARCH_MS / 1000) as usize,
            window,
            input: Vec::new(),
            base: 0,
            ideal: 0.0,
            prev: None,
            tail: vec![0.0; hop * channels],
            out: Vec::new(),
        }
    }

    pub fn rate(&self) -> f64 {
        self.rate
    }

    pub fn push(&mut self, pcm: &[f32]) {
        self.input.extend_from_slice(pcm);
        self.run();
    }

    /// Output frames ready.
    pub fn ready(&self) -> usize {
        self.out.len() / self.ch
    }

    /// Take up to `out.len() / channels` frames.
    pub fn pull(&mut self, out: &mut [f32]) -> usize {
        let n = (out.len() / self.ch).min(self.ready());
        out[..n * self.ch].copy_from_slice(&self.out[..n * self.ch]);
        self.out.drain(..n * self.ch);
        n
    }

    fn frames(&self) -> u64 {
        (self.input.len() / self.ch) as u64
    }

    fn at(&self, abs: u64, i: usize, c: usize) -> f32 {
        self.input[((abs - self.base) as usize + i) * self.ch + c]
    }

    /// How alike the segment at `cand` is to the natural continuation at
    /// `nat`, over the overlap (normalised cross-correlation on a mono mix,
    /// every other frame).
    fn similarity(&self, cand: u64, nat: u64) -> f32 {
        let (mut xy, mut xx, mut yy) = (0f32, 0f32, 0f32);
        for i in (0..self.hop).step_by(2) {
            let (mut a, mut b) = (0f32, 0f32);
            for c in 0..self.ch {
                a += self.at(cand, i, c);
                b += self.at(nat, i, c);
            }
            xy += a * b;
            xx += a * a;
            yy += b * b;
        }
        if xx <= 1e-12 || yy <= 1e-12 {
            return 0.0;
        }
        xy / (xx * yy).sqrt()
    }

    fn run(&mut self) {
        let (seg, hop, ch, search) = (self.seg as u64, self.hop as u64, self.ch, self.search as u64);
        loop {
            let ideal = self.ideal.round().max(0.0) as u64;
            let lo = ideal.saturating_sub(search).max(self.base);
            let hi = ideal + search;
            let nat = self.prev.map(|p| p + hop);
            let need = hi.max(nat.unwrap_or(0)) + seg;
            if self.base + self.frames() < need {
                return;
            }
            let start = match nat {
                None => ideal,
                Some(nat) => {
                    // Coarse search every 4 frames, then refine around the best.
                    let (mut best, mut best_s) = (ideal, f32::MIN);
                    let mut c = lo;
                    while c <= hi {
                        let s = self.similarity(c, nat);
                        if s > best_s {
                            (best, best_s) = (c, s);
                        }
                        c += 4;
                    }
                    for c in best.saturating_sub(3).max(lo)..=(best + 3).min(hi) {
                        let s = self.similarity(c, nat);
                        if s > best_s {
                            (best, best_s) = (c, s);
                        }
                    }
                    best
                }
            };
            for i in 0..self.hop {
                for c in 0..ch {
                    let v = self.at(start, i, c) * self.window[i] + self.tail[i * ch + c];
                    self.out.push(v);
                }
            }
            for i in 0..self.hop {
                for c in 0..ch {
                    self.tail[i * ch + c] = self.at(start, self.hop + i, c) * self.window[self.hop + i];
                }
            }
            self.prev = Some(start);
            self.ideal += hop as f64 * self.rate;
            // Drop input no later segment can reach.
            let keep = (self.ideal.round() as u64).saturating_sub(search).min(start + hop);
            if keep > self.base {
                let n = ((keep - self.base) as usize).min(self.frames() as usize);
                self.input.drain(..n * ch);
                self.base += n as u64;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(f: f64, n: usize) -> Vec<f32> {
        (0..n).map(|i| (2.0 * std::f64::consts::PI * f * i as f64 / 48_000.0).sin() as f32 * 0.5).collect()
    }

    /// The strongest frequency, by zero crossings.
    fn pitch(x: &[f32]) -> f64 {
        let z = x.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
        z as f64 * 48_000.0 / x.len() as f64
    }

    #[test]
    fn rate_changes_length_not_pitch() {
        for rate in [0.5, 0.75, 1.5, 2.0] {
            let mut w = Wsola::new(48_000, 1, rate);
            let x = tone(440.0, 48_000 * 2);
            for c in x.chunks(1000) {
                w.push(c);
            }
            let mut out = vec![0f32; 48_000 * 8];
            let n = w.pull(&mut out);
            let want = (x.len() as f64 / rate) as usize;
            assert!((n as f64 - want as f64).abs() < 0.02 * want as f64 + 2000.0, "rate {rate}: {n} frames, want about {want}");
            // Past the fade-in, the pitch is the input's.
            let p = pitch(&out[2000..n - 2000]);
            assert!((p - 440.0).abs() < 6.0, "rate {rate}: pitch {p:.1} Hz");
            // And the level holds (no gaps or doubled overlaps).
            let rms = (out[2000..n - 2000].iter().map(|v| v * v).sum::<f32>() / (n - 4000) as f32).sqrt();
            assert!((rms - 0.3536).abs() < 0.03, "rate {rate}: rms {rms}");
        }
    }

    #[test]
    fn stereo_frames_stay_together() {
        let mut w = Wsola::new(48_000, 2, 1.25);
        let x: Vec<f32> = (0..48_000).flat_map(|i| {
            let v = (2.0 * std::f64::consts::PI * 300.0 * i as f64 / 48_000.0).sin() as f32;
            [v, -v]
        }).collect();
        w.push(&x);
        let mut out = vec![0f32; 48_000 * 2];
        let n = w.pull(&mut out);
        assert!(n > 30_000);
        assert!(out[..n * 2].chunks(2).all(|f| (f[0] + f[1]).abs() < 1e-5), "left and right stay mirrored");
    }
}
