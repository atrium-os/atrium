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

/// Many inputs to a stereo pair, each input with its own left and right
/// impulse response, convolved by uniformly partitioned overlap-save FFT
/// convolution. Both outputs are real, so they share one complex spectrum
/// (left + j·right): per block of `block` frames it takes one forward FFT per
/// pair of inputs (two real signals share one), one complex multiply-add per
/// input and partition, and a single
/// inverse FFT. Called with whole blocks it adds no latency. It allocates
/// only in `new`, so it runs on the mix thread.
pub struct PartitionedConvolver {
    fft: crate::fft::Fft,
    block: usize,
    ins: usize,
    parts: usize,
    /// Kernel spectra, left + j·right: [input][partition] × 2·block.
    k_re: Vec<f32>,
    k_im: Vec<f32>,
    /// Each input's last `parts` block spectra (a ring at `pos`).
    x_re: Vec<f32>,
    x_im: Vec<f32>,
    pos: usize,
    /// Each input's previous block (overlap-save keeps one block of history).
    prev: Vec<f32>,
    re: Vec<f32>,
    im: Vec<f32>,
    acc_re: Vec<f32>,
    acc_im: Vec<f32>,
}

impl PartitionedConvolver {
    /// `kernel(i, ear)` is the impulse response from input `i` to output
    /// `ear` (0 left, 1 right); all are cut to `taps`. `block` is a power of
    /// two.
    pub fn new(block: usize, ins: usize, taps: usize, kernel: impl Fn(usize, usize) -> Vec<f32>) -> Self {
        assert!(block.is_power_of_two() && ins > 0 && taps > 0);
        let n = 2 * block;
        let parts = taps.div_ceil(block);
        let fft = crate::fft::Fft::new(n);
        let mut k_re = vec![0.0; ins * parts * n];
        let mut k_im = vec![0.0; ins * parts * n];
        for i in 0..ins {
            let (l, r) = (kernel(i, 0), kernel(i, 1));
            for p in 0..parts {
                let at = (i * parts + p) * n;
                let (re, im) = (&mut k_re[at..at + n], &mut k_im[at..at + n]);
                let take = block.min(taps.saturating_sub(p * block));
                for (t, v) in l.iter().skip(p * block).take(take).enumerate() {
                    re[t] = *v;
                }
                for (t, v) in r.iter().skip(p * block).take(take).enumerate() {
                    im[t] = *v;
                }
                // FFT(l + j·r) = L + j·R, as the accumulator wants.
                fft.forward(re, im);
            }
        }
        PartitionedConvolver {
            fft,
            block,
            ins,
            parts,
            k_re,
            k_im,
            x_re: vec![0.0; ins * parts * n],
            x_im: vec![0.0; ins * parts * n],
            pos: 0,
            prev: vec![0.0; ins * block],
            re: vec![0.0; n],
            im: vec![0.0; n],
            acc_re: vec![0.0; n],
            acc_im: vec![0.0; n],
        }
    }

    pub fn block(&self) -> usize {
        self.block
    }

    /// Convolve `input` (interleaved, `ins` channels) and ADD the result to
    /// `out` (interleaved stereo). Frames must be a whole number of blocks; a
    /// trailing partial block is ignored.
    pub fn process_add(&mut self, input: &[f32], out: &mut [f32]) {
        let (b, n) = (self.block, 2 * self.block);
        let frames = (input.len() / self.ins).min(out.len() / 2);
        for blk in 0..frames / b {
            let base = blk * b;
            self.acc_re.iter_mut().for_each(|v| *v = 0.0);
            self.acc_im.iter_mut().for_each(|v| *v = 0.0);
            // Two real inputs share one FFT: z = a + j·b, then split the
            // spectrum by its symmetry.
            for i in (0..self.ins).step_by(2) {
                let pair = i + 1 < self.ins;
                self.im.iter_mut().for_each(|v| *v = 0.0);
                for (j, dst) in [(i, 0usize), (i + 1, 1)] {
                    if j >= self.ins {
                        break;
                    }
                    let prev = &mut self.prev[j * b..(j + 1) * b];
                    let buf = if dst == 0 { &mut self.re } else { &mut self.im };
                    buf[..b].copy_from_slice(prev);
                    for t in 0..b {
                        let v = input[(base + t) * self.ins + j];
                        buf[b + t] = v;
                        prev[t] = v;
                    }
                }
                self.fft.forward(&mut self.re, &mut self.im);
                let at = (i * self.parts + self.pos) * n;
                if pair {
                    let bt = ((i + 1) * self.parts + self.pos) * n;
                    for k in 0..n {
                        let m = (n - k) & (n - 1);
                        let (zr, zi, mr, mi) = (self.re[k], self.im[k], self.re[m], self.im[m]);
                        self.x_re[at + k] = 0.5 * (zr + mr);
                        self.x_im[at + k] = 0.5 * (zi - mi);
                        self.x_re[bt + k] = 0.5 * (zi + mi);
                        self.x_im[bt + k] = 0.5 * (mr - zr);
                    }
                } else {
                    self.x_re[at..at + n].copy_from_slice(&self.re);
                    self.x_im[at..at + n].copy_from_slice(&self.im);
                }
            }
            for i in 0..self.ins {
                for p in 0..self.parts {
                    let xs = (i * self.parts + (self.pos + self.parts - p) % self.parts) * n;
                    let ks = (i * self.parts + p) * n;
                    let (xr, xi) = (&self.x_re[xs..xs + n], &self.x_im[xs..xs + n]);
                    let (kr, ki) = (&self.k_re[ks..ks + n], &self.k_im[ks..ks + n]);
                    for f in 0..n {
                        self.acc_re[f] += xr[f] * kr[f] - xi[f] * ki[f];
                        self.acc_im[f] += xr[f] * ki[f] + xi[f] * kr[f];
                    }
                }
            }
            self.fft.inverse(&mut self.acc_re, &mut self.acc_im);
            for t in 0..b {
                out[(base + t) * 2] += self.acc_re[b + t];
                out[(base + t) * 2 + 1] += self.acc_im[b + t];
            }
            self.pos = (self.pos + 1) % self.parts;
        }
    }

    pub fn reset(&mut self) {
        self.x_re.iter_mut().for_each(|v| *v = 0.0);
        self.x_im.iter_mut().for_each(|v| *v = 0.0);
        self.prev.iter_mut().for_each(|v| *v = 0.0);
        self.pos = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partitioned_matches_direct_convolution_across_blocks() {
        // Three inputs (a pair and an odd one) to a stereo pair, kernels
        // longer than a block (3 partitions).
        let k = |i: usize, o: usize| -> Vec<f32> { (0..40).map(|t| ((t * (i + 2) + o * 5) % 7) as f32 / 7.0 - 0.4).collect() };
        let mut pc = PartitionedConvolver::new(16, 3, 40, k);
        let input: Vec<f32> = (0..160 * 3).map(|j| ((j * 31 % 17) as f32 - 8.0) / 8.0).collect();
        let mut out = vec![0.0f32; 160 * 2];
        // Fed in two calls: streaming continuity across calls too.
        let (a, b) = input.split_at(64 * 3);
        let (oa, ob) = out.split_at_mut(64 * 2);
        pc.process_add(a, oa);
        pc.process_add(b, ob);
        for o in 0..2 {
            let mut want = vec![0.0f32; 160];
            for i in 0..3 {
                let x: Vec<f32> = input.iter().skip(i).step_by(3).copied().collect();
                let mut c = Convolver::new(k(i, o));
                want.iter_mut().zip(c.process(&x)).for_each(|(w, y)| *w += y);
            }
            for t in 0..160 {
                assert!((out[t * 2 + o] - want[t]).abs() < 1e-4, "out {o} frame {t}: {} vs {}", out[t * 2 + o], want[t]);
            }
        }
    }

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
