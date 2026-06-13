//! Streaming FIR convolution — the DSP workhorse
//! (`docs/spec/atrium-lyra-architecture.md` §13: reverb, HRTF/binaural, and the
//! audio-ray-tracing renderer all run on this).
//!
//! Convolving a signal with an impulse response (IR) is how you apply *any*
//! linear acoustic effect: a room's reverb tail, a head-related transfer
//! function for binaural, a ray-traced arrival path. The non-obvious requirement
//! is **streaming continuity** — audio arrives one buffer at a time, and the IR
//! tail from the previous buffer must bleed into the next, or you get a click at
//! every buffer boundary (the failure `effect_mode` taught us to respect). A
//! [`Convolver`] carries the history that makes `process()` over many small
//! buffers identical to one convolution over the whole stream.
//!
//! Time-domain here (exact, the reference); long IRs (seconds of reverb) want
//! uniformly-partitioned FFT convolution for cost — a drop-in replacement behind
//! the same streaming contract, noted for when reverb lands.

/// A single-channel streaming FIR convolver. One per channel (a binaural
/// renderer uses two: left-ear and right-ear IRs).
pub struct Convolver {
    kernel: Vec<f32>,
    /// The last `kernel.len() - 1` input samples, so the IR tail spans buffers.
    history: Vec<f32>,
}

impl Convolver {
    pub fn new(kernel: Vec<f32>) -> Self {
        let n = kernel.len().saturating_sub(1);
        Convolver { kernel, history: vec![0.0; n] }
    }

    /// Convolve `input` with the kernel, returning one output sample per input
    /// sample. State persists, so consecutive calls stream seamlessly.
    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        let h = self.history.len(); // kernel.len() - 1
        // x = history ++ input; y[i] = Σ_k kernel[k] · x[i + h − k]
        let mut x = Vec::with_capacity(h + input.len());
        x.extend_from_slice(&self.history);
        x.extend_from_slice(input);
        let mut out = Vec::with_capacity(input.len());
        for i in 0..input.len() {
            let base = i + h; // index in x of the current input sample
            let mut acc = 0.0f32;
            for (k, &c) in self.kernel.iter().enumerate() {
                acc += c * x[base - k];
            }
            out.push(acc);
        }
        // carry the last h input samples as the next call's history.
        if h > 0 {
            let start = x.len() - h;
            self.history.copy_from_slice(&x[start..]);
        }
        out
    }

    /// Group delay introduced (kernel length − 1) — feeds plugin delay
    /// compensation ([`crate::...`] / the gpusim `lyra_pdc` model).
    pub fn latency(&self) -> usize {
        self.kernel.len().saturating_sub(1)
    }

    /// Reset the streaming state (e.g. on a discontinuity / re-route).
    pub fn reset(&mut self) {
        self.history.iter_mut().for_each(|s| *s = 0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: &[f32], b: &[f32]) {
        assert_eq!(a.len(), b.len(), "len {} vs {}", a.len(), b.len());
        for (x, y) in a.iter().zip(b) {
            assert!((x - y).abs() < 1e-5, "{a:?} vs {b:?}");
        }
    }

    #[test]
    fn unit_impulse_is_identity() {
        let mut c = Convolver::new(vec![1.0]);
        approx(&c.process(&[1.0, 2.0, 3.0]), &[1.0, 2.0, 3.0]);
    }

    #[test]
    fn delayed_impulse_delays() {
        // kernel [0, 1] = a one-sample delay.
        let mut c = Convolver::new(vec![0.0, 1.0]);
        approx(&c.process(&[1.0, 2.0, 3.0]), &[0.0, 1.0, 2.0]);
        // the tail (3.0) bleeds into the next buffer — streaming continuity.
        approx(&c.process(&[0.0]), &[3.0]);
    }

    #[test]
    fn known_kernel_known_output() {
        // 2-tap moving average [0.5, 0.5].
        let mut c = Convolver::new(vec![0.5, 0.5]);
        approx(&c.process(&[2.0, 4.0, 6.0]), &[1.0, 3.0, 5.0]);
    }

    #[test]
    fn streaming_equals_one_shot() {
        // THE property: split buffers convolve identically to one big buffer.
        let kernel = vec![0.2, -0.5, 0.3, 0.1, -0.05];
        let sig: Vec<f32> = (0..64).map(|i| ((i * 13 % 7) as f32 - 3.0) * 0.1).collect();

        let mut whole = Convolver::new(kernel.clone());
        let one_shot = whole.process(&sig);

        let mut streamed = Convolver::new(kernel);
        let mut pieced = Vec::new();
        for chunk in sig.chunks(7) {
            // deliberately ragged buffer sizes
            pieced.extend(streamed.process(chunk));
        }
        approx(&pieced, &one_shot);
    }

    #[test]
    fn latency_is_kernel_minus_one() {
        assert_eq!(Convolver::new(vec![1.0; 128]).latency(), 127);
        assert_eq!(Convolver::new(vec![1.0]).latency(), 0);
    }

    #[test]
    fn reset_clears_the_tail() {
        let mut c = Convolver::new(vec![0.0, 1.0]);
        c.process(&[5.0]); // tail = 5.0 pending
        c.reset();
        approx(&c.process(&[0.0]), &[0.0]); // tail gone after reset
    }
}
