//! A small in-place radix-2 complex FFT for the mix thread: twiddles and the
//! bit-reversal table are made once, so a transform allocates nothing.

pub struct Fft {
    n: usize,
    /// cos/sin of −2πk/n for k < n/2.
    cos: Vec<f32>,
    sin: Vec<f32>,
    rev: Vec<u32>,
}

impl Fft {
    /// `n` a power of two, at least 2.
    pub fn new(n: usize) -> Fft {
        assert!(n >= 2 && n.is_power_of_two());
        let bits = n.trailing_zeros();
        let (cos, sin) = (0..n / 2)
            .map(|k| {
                let a = -2.0 * std::f64::consts::PI * k as f64 / n as f64;
                (a.cos() as f32, a.sin() as f32)
            })
            .unzip();
        let rev = (0..n as u32).map(|i| i.reverse_bits() >> (32 - bits)).collect();
        Fft { n, cos, sin, rev }
    }

    pub fn len(&self) -> usize {
        self.n
    }

    /// Forward transform in place (no scaling).
    pub fn forward(&self, re: &mut [f32], im: &mut [f32]) {
        self.run(re, im, false);
    }

    /// Inverse transform in place, scaled by 1/n.
    pub fn inverse(&self, re: &mut [f32], im: &mut [f32]) {
        self.run(re, im, true);
        let s = 1.0 / self.n as f32;
        re.iter_mut().for_each(|x| *x *= s);
        im.iter_mut().for_each(|x| *x *= s);
    }

    fn run(&self, re: &mut [f32], im: &mut [f32], inverse: bool) {
        let n = self.n;
        assert!(re.len() == n && im.len() == n);
        for i in 0..n {
            let j = self.rev[i] as usize;
            if j > i {
                re.swap(i, j);
                im.swap(i, j);
            }
        }
        let mut half = 1;
        while half < n {
            let step = n / (2 * half);
            for start in (0..n).step_by(2 * half) {
                for k in 0..half {
                    let (c, s) = (self.cos[k * step], if inverse { -self.sin[k * step] } else { self.sin[k * step] });
                    let (a, b) = (start + k, start + k + half);
                    let tr = re[b] * c - im[b] * s;
                    let ti = re[b] * s + im[b] * c;
                    re[b] = re[a] - tr;
                    im[b] = im[a] - ti;
                    re[a] += tr;
                    im[a] += ti;
                }
            }
            half *= 2;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_direct_transform_and_inverts() {
        let n = 64;
        let f = Fft::new(n);
        let x: Vec<f32> = (0..n).map(|i| ((i * 7 % 13) as f32 - 6.0) / 6.0).collect();
        let (mut re, mut im) = (x.clone(), vec![0.0f32; n]);
        f.forward(&mut re, &mut im);
        for k in [0, 1, 5, 31, 32, 63] {
            let (mut dr, mut di) = (0.0f64, 0.0f64);
            for (t, &v) in x.iter().enumerate() {
                let a = -2.0 * std::f64::consts::PI * (k * t) as f64 / n as f64;
                dr += v as f64 * a.cos();
                di += v as f64 * a.sin();
            }
            assert!((re[k] as f64 - dr).abs() < 1e-4 && (im[k] as f64 - di).abs() < 1e-4, "bin {k}");
        }
        f.inverse(&mut re, &mut im);
        for (a, b) in re.iter().zip(&x) {
            assert!((a - b).abs() < 1e-5);
        }
        assert!(im.iter().all(|v| v.abs() < 1e-5));
    }
}
