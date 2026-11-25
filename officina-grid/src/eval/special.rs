//! Special functions under the statistical distributions
//! (docs/spec/officina.md §16.136): log-gamma, the regularized incomplete
//! gamma and beta functions, the error function, and inversion of a
//! monotone function by bisection. Written from the textbook definitions:
//! Lanczos' approximation, the power series and Lentz's continued fractions.

/// Steps a series or continued fraction may take.
const MAX_STEPS: usize = 1000;
const EPS: f64 = 1e-16;
const TINY: f64 = 1e-300;

/// ln Γ(x) for x > 0 (Lanczos, g = 7, nine terms; ~15 digits).
pub fn ln_gamma(x: f64) -> f64 {
    const G: f64 = 7.0;
    const C: [f64; 9] = [
        0.999_999_999_999_809_9,
        676.520_368_121_885_1,
        -1_259.139_216_722_402_8,
        771.323_428_777_653_1,
        -176.615_029_162_140_6,
        12.507_343_278_686_905,
        -0.138_571_095_265_720_12,
        9.984_369_578_019_572e-6,
        1.505_632_735_149_311_6e-7,
    ];
    if x < 0.5 {
        // Reflection: Γ(x)Γ(1−x) = π / sin(πx).
        let s = (std::f64::consts::PI * x).sin().abs();
        return std::f64::consts::PI.ln() - s.ln() - ln_gamma(1.0 - x);
    }
    let x = x - 1.0;
    let mut sum = C[0];
    for (i, c) in C.iter().enumerate().skip(1) {
        sum += c / (x + i as f64);
    }
    let t = x + G + 0.5;
    0.5 * (2.0 * std::f64::consts::PI).ln() + (x + 0.5) * t.ln() - t + sum.ln()
}

/// Γ(x), for any x that isn't zero or a negative integer.
pub fn gamma(x: f64) -> Option<f64> {
    if x <= 0.0 && x.fract() == 0.0 {
        return None;
    }
    if x > 0.0 {
        if x.fract() == 0.0 && x <= 171.0 {
            // Exact factorials for whole numbers.
            return Some((1..x as u32).map(f64::from).product());
        }
        return Some(ln_gamma(x).exp());
    }
    // Γ(x) = π / (sin(πx) Γ(1−x)).
    Some(std::f64::consts::PI / ((std::f64::consts::PI * x).sin() * gamma(1.0 - x)?))
}

/// The regularized lower incomplete gamma function P(a, x), a > 0, x ≥ 0.
pub fn gamma_p(a: f64, x: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    if x < a + 1.0 { gamma_series(a, x) } else { 1.0 - gamma_cf(a, x) }
}

/// The regularized upper incomplete gamma function Q(a, x) = 1 − P(a, x),
/// computed directly so tails keep their digits.
pub fn gamma_q(a: f64, x: f64) -> f64 {
    if x <= 0.0 {
        return 1.0;
    }
    if x < a + 1.0 { 1.0 - gamma_series(a, x) } else { gamma_cf(a, x) }
}

fn gamma_series(a: f64, x: f64) -> f64 {
    let mut term = 1.0 / a;
    let mut sum = term;
    let mut ap = a;
    for _ in 0..MAX_STEPS {
        ap += 1.0;
        term *= x / ap;
        sum += term;
        if term.abs() < sum.abs() * EPS {
            break;
        }
    }
    sum * (-x + a * x.ln() - ln_gamma(a)).exp()
}

fn gamma_cf(a: f64, x: f64) -> f64 {
    // Lentz's method on Q(a, x)'s continued fraction.
    let mut b = x + 1.0 - a;
    let mut c = 1.0 / TINY;
    let mut d = 1.0 / b;
    let mut h = d;
    for i in 1..MAX_STEPS {
        let an = -(i as f64) * (i as f64 - a);
        b += 2.0;
        d = an * d + b;
        if d.abs() < TINY {
            d = TINY;
        }
        c = b + an / c;
        if c.abs() < TINY {
            c = TINY;
        }
        d = 1.0 / d;
        let delta = d * c;
        h *= delta;
        if (delta - 1.0).abs() < EPS {
            break;
        }
    }
    (-x + a * x.ln() - ln_gamma(a)).exp() * h
}

/// The regularized incomplete beta function I_x(a, b), a, b > 0, 0 ≤ x ≤ 1.
pub fn beta_i(x: f64, a: f64, b: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    let front = (ln_gamma(a + b) - ln_gamma(a) - ln_gamma(b) + a * x.ln() + b * (1.0 - x).ln()).exp();
    if x < (a + 1.0) / (a + b + 2.0) {
        front * beta_cf(x, a, b) / a
    } else {
        1.0 - front * beta_cf(1.0 - x, b, a) / b
    }
}

fn beta_cf(x: f64, a: f64, b: f64) -> f64 {
    let (qab, qap, qam) = (a + b, a + 1.0, a - 1.0);
    let mut c = 1.0;
    let mut d = 1.0 - qab * x / qap;
    if d.abs() < TINY {
        d = TINY;
    }
    d = 1.0 / d;
    let mut h = d;
    for m in 1..MAX_STEPS {
        let m = m as f64;
        let m2 = 2.0 * m;
        let aa = m * (b - m) * x / ((qam + m2) * (a + m2));
        d = 1.0 + aa * d;
        if d.abs() < TINY {
            d = TINY;
        }
        c = 1.0 + aa / c;
        if c.abs() < TINY {
            c = TINY;
        }
        d = 1.0 / d;
        h *= d * c;
        let aa = -(a + m) * (qab + m) * x / ((a + m2) * (qap + m2));
        d = 1.0 + aa * d;
        if d.abs() < TINY {
            d = TINY;
        }
        c = 1.0 + aa / c;
        if c.abs() < TINY {
            c = TINY;
        }
        d = 1.0 / d;
        let delta = d * c;
        h *= delta;
        if (delta - 1.0).abs() < EPS {
            break;
        }
    }
    h
}

/// erf(x) = P(½, x²) with x's sign.
pub fn erf(x: f64) -> f64 {
    let p = gamma_p(0.5, x * x);
    if x < 0.0 { -p } else { p }
}

/// erfc(x) = 1 − erf(x), keeping digits in the upper tail.
pub fn erfc(x: f64) -> f64 {
    if x < 0.0 { 1.0 + gamma_p(0.5, x * x) } else { gamma_q(0.5, x * x) }
}

/// The standard normal distribution function Φ(z).
pub fn norm_cdf(z: f64) -> f64 {
    0.5 * erfc(-z / std::f64::consts::SQRT_2)
}

/// The standard normal density.
pub fn norm_pdf(z: f64) -> f64 {
    (-0.5 * z * z).exp() / (2.0 * std::f64::consts::PI).sqrt()
}

/// The x in [lo, hi] where the increasing function `f` reaches `target`, by
/// bisection (the bracket doubles upward while `f(hi)` is short of it).
/// None when no bracket holds it.
pub fn invert(f: &dyn Fn(f64) -> f64, target: f64, mut lo: f64, mut hi: f64) -> Option<f64> {
    let mut grow = 0;
    while f(hi) < target {
        if grow == 200 {
            return None;
        }
        lo = hi;
        hi = if hi <= 0.0 { 1.0 } else { hi * 2.0 };
        grow += 1;
    }
    if f(lo) > target {
        return None;
    }
    for _ in 0..300 {
        let mid = 0.5 * (lo + hi);
        if mid <= lo || mid >= hi {
            break;
        }
        if f(mid) < target {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Some(0.5 * (lo + hi))
}

// ---------- Bessel functions of integer order ----------

/// Points one panel of composite Gauss–Legendre quadrature uses.
const GL_POINTS: usize = 16;

/// Gauss–Legendre nodes and weights on [−1, 1], found by Newton's method on
/// the Legendre polynomial once.
fn gauss_legendre() -> &'static [(f64, f64)] {
    static NODES: std::sync::OnceLock<Vec<(f64, f64)>> = std::sync::OnceLock::new();
    NODES.get_or_init(|| {
        let n = GL_POINTS;
        (0..n)
            .map(|i| {
                let mut x = (std::f64::consts::PI * (i as f64 + 0.75) / (n as f64 + 0.5)).cos();
                let mut dp = 1.0;
                for _ in 0..100 {
                    // P_n(x) and its derivative by the three-term recurrence.
                    let (mut p0, mut p1) = (1.0, x);
                    for k in 2..=n {
                        let k = k as f64;
                        (p0, p1) = (p1, ((2.0 * k - 1.0) * x * p1 - (k - 1.0) * p0) / k);
                    }
                    dp = n as f64 * (x * p1 - p0) / (x * x - 1.0);
                    let dx = p1 / dp;
                    x -= dx;
                    if dx.abs() < 1e-16 {
                        break;
                    }
                }
                (x, 2.0 / ((1.0 - x * x) * dp * dp))
            })
            .collect()
    })
}

/// ∫ f over [a, b] in `panels` equal panels of Gauss–Legendre.
fn quad(f: &dyn Fn(f64) -> f64, a: f64, b: f64, panels: usize) -> f64 {
    let h = (b - a) / panels as f64;
    let nodes = gauss_legendre();
    let mut sum = 0.0;
    for p in 0..panels {
        let mid = a + (p as f64 + 0.5) * h;
        for &(x, w) in nodes {
            sum += w * f(mid + 0.5 * h * x);
        }
    }
    sum * 0.5 * h
}

/// Σ (±x²/4)^k / (k! (k+n)!) times (x/2)^n: J_n (alternating) or I_n.
fn bessel_series(x: f64, n: u32, alternate: bool) -> f64 {
    let q = x * x / 4.0 * if alternate { -1.0 } else { 1.0 };
    let (mut term, mut sum) = (1.0f64, 1.0f64);
    for k in 1..20_000u32 {
        term *= q / (f64::from(k) * f64::from(k + n));
        sum += term;
        if term.abs() <= sum.abs() * 1e-17 {
            break;
        }
    }
    let lead = (f64::from(n) * (x.abs() / 2.0).ln() - ln_gamma(f64::from(n) + 1.0)).exp();
    let sign = if x < 0.0 && n % 2 == 1 { -1.0 } else { 1.0 };
    if x == 0.0 {
        return if n == 0 { 1.0 } else { 0.0 };
    }
    sign * lead * sum
}

/// The largest |x| the Bessel functions take.
pub const BESSEL_MAX_X: f64 = 1e5;

/// J_n(x): the series for small x (or order large beside x), else
/// (1/π)∫₀^π cos(nτ − x sin τ) dτ by the trapezoid rule, which converges
/// geometrically on that periodic integrand.
pub fn bessel_j(x: f64, n: u32) -> f64 {
    let nf = f64::from(n);
    if x.abs() <= 5.0 || nf > x * x / 4.0 {
        return bessel_series(x, n, true);
    }
    let steps = (2.0 * (x.abs() + nf)) as usize + 64;
    let h = std::f64::consts::PI / steps as f64;
    let f = |t: f64| (nf * t - x * t.sin()).cos();
    let mut sum = 0.5 * (f(0.0) + f(std::f64::consts::PI));
    for k in 1..steps {
        sum += f(k as f64 * h);
    }
    sum * h / std::f64::consts::PI
}

/// I_n(x) by its series, whose terms all have one sign.
pub fn bessel_i(x: f64, n: u32) -> f64 {
    bessel_series(x, n, false)
}

/// Where e^(−x cosh t + n t) has fallen below e^−745 beside its peak.
fn tail_end(x: f64, n: f64, grow: fn(f64) -> f64) -> f64 {
    let peak = if n > x { (n / x).acosh() } else { 0.0 };
    let top = n * peak - x * grow(peak);
    let mut t = peak + 1.0;
    while t < 800.0 && n * t - x * grow(t) > top - 745.0 {
        t += 1.0;
    }
    t
}

/// K_n(x), x > 0: ∫₀^∞ e^(−x cosh t) cosh(nt) dt.
pub fn bessel_k(x: f64, n: u32) -> f64 {
    let nf = f64::from(n);
    let end = tail_end(x, nf, f64::cosh);
    let f = |t: f64| (-x * t.cosh() + nf * t).exp() * 0.5 * (1.0 + (-2.0 * nf * t).exp());
    quad(&f, 0.0, end, (end * 4.0).ceil() as usize)
}

/// Y_n(x), x > 0: (1/π)∫₀^π sin(x sin θ − nθ) dθ
/// − (1/π)∫₀^∞ (e^(nt) + (−1)^n e^(−nt)) e^(−x sinh t) dt.
pub fn bessel_y(x: f64, n: u32) -> f64 {
    let nf = f64::from(n);
    let pi = std::f64::consts::PI;
    let first = quad(&|t: f64| (x * t.sin() - nf * t).sin(), 0.0, pi, (x + nf).ceil() as usize + 4);
    let sign = if n % 2 == 0 { 1.0 } else { -1.0 };
    let end = tail_end(x, nf, f64::sinh);
    let second = quad(&|t: f64| (nf * t - x * t.sinh()).exp() + sign * (-nf * t - x * t.sinh()).exp(), 0.0, end, (end * 4.0).ceil() as usize);
    (first - second) / pi
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() <= tol * b.abs().max(1e-300)
    }

    #[test]
    fn known_values() {
        assert!(close(ln_gamma(0.5), std::f64::consts::PI.sqrt().ln(), 1e-14));
        assert_eq!(gamma(5.0), Some(24.0));
        assert!(close(gamma(-0.5).unwrap(), -2.0 * std::f64::consts::PI.sqrt(), 1e-13));
        assert!(gamma(-2.0).is_none());
        assert!(close(erf(1.0), 0.842_700_792_949_714_9, 1e-14));
        assert!(close(erfc(5.0), 1.537_459_794_428_034_8e-12, 1e-10));
        assert!(close(norm_cdf(-1.0), 0.158_655_253_931_457_05, 1e-14));
        assert!(close(gamma_p(3.0, 2.0), 0.323_323_583_816_936_5, 1e-14));
        assert!(close(beta_i(0.4, 2.0, 3.0), 0.5248, 1e-13));
        assert!(close(bessel_j(1.9, 2), 0.329_925_727_692_387_15, 1e-12));
        assert!(close(bessel_j(30.0, 3), 0.129_211_228_759_725, 1e-10));
        assert!(close(bessel_i(1.5, 1), 0.981_666_428_577_907_8, 1e-13));
        assert!(close(bessel_k(1.5, 1), 0.277_387_800_456_843_8, 1e-12));
        assert!(close(bessel_y(2.5, 1), 0.145_918_137_966_785_8, 1e-12));
        assert!(close(bessel_y(0.1, 10), -1.183_133_513_204_519_7e18, 1e-10));
        assert!(close(bessel_k(0.01, 0), 4.721_244_730_161_095, 1e-12));
        let x = invert(&norm_cdf, 0.975, -40.0, 40.0).unwrap();
        assert!(close(x, 1.959_963_984_540_054, 1e-12));
    }
}
