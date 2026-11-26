//! Statistical distributions and tests (docs/spec/officina.md §16.136):
//! normal, lognormal, Student's t, chi-squared, F, binomial, negative
//! binomial, Poisson, exponential, gamma, beta, Weibull and hypergeometric,
//! each with its inverse where Excel has one; the error function; and the
//! t, z, chi-squared and F tests. Every argument check follows Excel's
//! documented errors.

use super::special::{self, beta_i, gamma_p, gamma_q, invert, ln_gamma, norm_cdf, norm_pdf};
use super::{to_bool, Ctx, Ev};
use crate::formula::Expr;
use crate::value::{ErrorKind, Value};

type R = Result<f64, ErrorKind>;

fn n(x: f64) -> Ev {
    if x.is_finite() { Ev::V(Value::num(x)) } else { Ev::err(ErrorKind::Num) }
}
fn missing(a: &Expr) -> bool {
    matches!(a, Expr::Name(n) if n.is_empty())
}
fn num(c: &Ctx, a: &[Expr], i: usize) -> R {
    c.number(&c.scalar(&a[i]))
}
fn opt(c: &Ctx, a: &[Expr], i: usize, default: f64) -> R {
    match a.get(i) {
        Some(x) if !missing(x) => c.number(&c.scalar(x)),
        _ => Ok(default),
    }
}
fn flag(c: &Ctx, a: &[Expr], i: usize) -> Result<bool, ErrorKind> {
    to_bool(&c.scalar(&a[i]))
}
fn num_err<T>() -> Result<T, ErrorKind> {
    Err(ErrorKind::Num)
}
/// A probability strictly between 0 and 1.
fn prob(p: f64) -> R {
    if p > 0.0 && p < 1.0 { Ok(p) } else { num_err() }
}
fn ev(r: R) -> Ev {
    match r {
        Ok(x) => n(x),
        Err(k) => Ev::err(k),
    }
}

// ---------- normal and lognormal ----------

fn normal(x: f64, mean: f64, sd: f64, cumulative: bool) -> R {
    if sd <= 0.0 {
        return num_err();
    }
    let z = (x - mean) / sd;
    Ok(if cumulative { norm_cdf(z) } else { norm_pdf(z) / sd })
}
pub(super) fn normal_inv(p: f64, mean: f64, sd: f64) -> R {
    let p = prob(p)?;
    if sd <= 0.0 {
        return num_err();
    }
    let z = invert(&norm_cdf, p, -40.0, 40.0).ok_or(ErrorKind::Num)?;
    Ok(mean + sd * z)
}

pub(super) fn norm_dist(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| normal(num(c, a, 0)?, num(c, a, 1)?, num(c, a, 2)?, flag(c, a, 3)?))())
}
pub(super) fn norm_inv(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| normal_inv(num(c, a, 0)?, num(c, a, 1)?, num(c, a, 2)?))())
}
/// NORM.S.DIST(z, cumulative); the legacy NORMSDIST(z) is always cumulative.
pub(super) fn norm_s_dist(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| normal(num(c, a, 0)?, 0.0, 1.0, if a.len() > 1 { flag(c, a, 1)? } else { true }))())
}
pub(super) fn norm_s_inv(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| normal_inv(num(c, a, 0)?, 0.0, 1.0))())
}
pub(super) fn phi(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| Ok(norm_pdf(num(c, a, 0)?)))())
}
/// GAUSS(z): the chance a standard normal value falls between the mean and z.
pub(super) fn gauss(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| Ok(norm_cdf(num(c, a, 0)?) - 0.5))())
}

/// LOGNORM.DIST(x, mean, sd, cumulative); LOGNORMDIST(x, mean, sd) is cumulative.
pub(super) fn lognorm_dist(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let (x, mean, sd) = (num(c, a, 0)?, num(c, a, 1)?, num(c, a, 2)?);
        let cumulative = if a.len() > 3 { flag(c, a, 3)? } else { true };
        if x <= 0.0 || sd <= 0.0 {
            return num_err();
        }
        let p = normal(x.ln(), mean, sd, cumulative)?;
        Ok(if cumulative { p } else { p / x })
    })())
}
pub(super) fn lognorm_inv(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| Ok(normal_inv(num(c, a, 0)?, num(c, a, 1)?, num(c, a, 2)?)?.exp()))())
}

// ---------- Student's t ----------

/// Degrees of freedom, truncated, at least `min`.
fn dof(x: f64, min: f64) -> R {
    let d = x.trunc();
    if d < min || d > 1e10 { num_err() } else { Ok(d) }
}
fn t_cdf(t: f64, df: f64) -> f64 {
    let tail = 0.5 * beta_i(df / (df + t * t), df / 2.0, 0.5);
    if t >= 0.0 { 1.0 - tail } else { tail }
}
/// The upper tail 1 − F(t), computed without cancellation for large t.
fn t_upper(t: f64, df: f64) -> f64 {
    let tail = 0.5 * beta_i(df / (df + t * t), df / 2.0, 0.5);
    if t >= 0.0 { tail } else { 1.0 - tail }
}
fn t_pdf(t: f64, df: f64) -> f64 {
    (ln_gamma((df + 1.0) / 2.0) - ln_gamma(df / 2.0)).exp() / (df * std::f64::consts::PI).sqrt() * (1.0 + t * t / df).powf(-(df + 1.0) / 2.0)
}
fn t_inv(p: f64, df: f64) -> R {
    let p = prob(p)?;
    invert(&|t| t_cdf(t, df), p, -1e10, 1.0).ok_or(ErrorKind::Num)
}

pub(super) fn t_dist(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let (t, df) = (num(c, a, 0)?, dof(num(c, a, 1)?, 1.0)?);
        Ok(if flag(c, a, 2)? { t_cdf(t, df) } else { t_pdf(t, df) })
    })())
}
pub(super) fn t_dist_2t(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let (t, df) = (num(c, a, 0)?, dof(num(c, a, 1)?, 1.0)?);
        if t < 0.0 { num_err() } else { Ok((2.0 * t_upper(t, df)).min(1.0)) }
    })())
}
pub(super) fn t_dist_rt(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| Ok(t_upper(num(c, a, 0)?, dof(num(c, a, 1)?, 1.0)?)))())
}
/// TDIST(x, df, tails): the legacy upper (1) or two-tailed (2) probability.
pub(super) fn tdist(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let (t, df, tails) = (num(c, a, 0)?, dof(num(c, a, 1)?, 1.0)?, num(c, a, 2)?.trunc());
        if t < 0.0 || !matches!(tails as i32, 1 | 2) {
            return num_err();
        }
        Ok((tails * t_upper(t, df)).min(1.0))
    })())
}
pub(super) fn t_inv_fn(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| t_inv(num(c, a, 0)?, dof(num(c, a, 1)?, 1.0)?))())
}
/// T.INV.2T and TINV: the t whose two tails hold probability p.
pub(super) fn t_inv_2t(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let (p, df) = (num(c, a, 0)?, dof(num(c, a, 1)?, 1.0)?);
        if p <= 0.0 || p > 1.0 {
            return num_err();
        }
        if p == 1.0 {
            return Ok(0.0);
        }
        t_inv(1.0 - p / 2.0, df)
    })())
}

// ---------- chi-squared and F ----------

fn chisq_cdf(x: f64, df: f64) -> f64 {
    gamma_p(df / 2.0, x / 2.0)
}
fn chisq_pdf(x: f64, df: f64) -> f64 {
    if x <= 0.0 {
        return if x == 0.0 && df == 2.0 { 0.5 } else { 0.0 };
    }
    ((df / 2.0 - 1.0) * x.ln() - x / 2.0 - (df / 2.0) * 2f64.ln() - ln_gamma(df / 2.0)).exp()
}
pub(super) fn chisq_dist(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let (x, df) = (num(c, a, 0)?, dof(num(c, a, 1)?, 1.0)?);
        if x < 0.0 {
            return num_err();
        }
        Ok(if flag(c, a, 2)? { chisq_cdf(x, df) } else { chisq_pdf(x, df) })
    })())
}
/// CHISQ.DIST.RT and CHIDIST: the upper tail.
pub(super) fn chisq_dist_rt(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let (x, df) = (num(c, a, 0)?, dof(num(c, a, 1)?, 1.0)?);
        if x < 0.0 { num_err() } else { Ok(gamma_q(df / 2.0, x / 2.0)) }
    })())
}
pub(super) fn chisq_inv(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let (p, df) = (num(c, a, 0)?, dof(num(c, a, 1)?, 1.0)?);
        if !(0.0..1.0).contains(&p) {
            return num_err();
        }
        invert(&|x| chisq_cdf(x, df), p, 0.0, df.max(1.0)).ok_or(ErrorKind::Num)
    })())
}
/// CHISQ.INV.RT and CHIINV: the x whose upper tail is p.
pub(super) fn chisq_inv_rt(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let (p, df) = (num(c, a, 0)?, dof(num(c, a, 1)?, 1.0)?);
        if p <= 0.0 || p > 1.0 {
            return num_err();
        }
        invert(&|x| -gamma_q(df / 2.0, x / 2.0), -p, 0.0, df.max(1.0)).ok_or(ErrorKind::Num)
    })())
}

fn f_cdf(x: f64, d1: f64, d2: f64) -> f64 {
    if x <= 0.0 { 0.0 } else { beta_i(d1 * x / (d1 * x + d2), d1 / 2.0, d2 / 2.0) }
}
fn f_upper(x: f64, d1: f64, d2: f64) -> f64 {
    if x <= 0.0 { 1.0 } else { beta_i(d2 / (d2 + d1 * x), d2 / 2.0, d1 / 2.0) }
}
fn f_pdf(x: f64, d1: f64, d2: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    let ln = 0.5 * (d1 * (d1 * x).ln() + d2 * d2.ln() - (d1 + d2) * (d1 * x + d2).ln()) - x.ln() - (ln_gamma(d1 / 2.0) + ln_gamma(d2 / 2.0) - ln_gamma((d1 + d2) / 2.0));
    ln.exp()
}
fn f_args(c: &Ctx, a: &[Expr]) -> Result<(f64, f64, f64), ErrorKind> {
    let (x, d1, d2) = (num(c, a, 0)?, dof(num(c, a, 1)?, 1.0)?, dof(num(c, a, 2)?, 1.0)?);
    if x < 0.0 { num_err() } else { Ok((x, d1, d2)) }
}
pub(super) fn f_dist(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let (x, d1, d2) = f_args(c, a)?;
        Ok(if flag(c, a, 3)? { f_cdf(x, d1, d2) } else { f_pdf(x, d1, d2) })
    })())
}
/// F.DIST.RT and FDIST: the upper tail.
pub(super) fn f_dist_rt(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let (x, d1, d2) = f_args(c, a)?;
        Ok(f_upper(x, d1, d2))
    })())
}
pub(super) fn f_inv(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let (p, d1, d2) = f_args(c, a)?;
        if p >= 1.0 {
            return num_err();
        }
        invert(&|x| f_cdf(x, d1, d2), p, 0.0, 1.0).ok_or(ErrorKind::Num)
    })())
}
/// F.INV.RT and FINV: the x whose upper tail is p.
pub(super) fn f_inv_rt(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let (p, d1, d2) = f_args(c, a)?;
        if p <= 0.0 || p > 1.0 {
            return num_err();
        }
        invert(&|x| -f_upper(x, d1, d2), -p, 0.0, 1.0).ok_or(ErrorKind::Num)
    })())
}

// ---------- discrete ----------

fn ln_choose(n: f64, k: f64) -> f64 {
    ln_gamma(n + 1.0) - ln_gamma(k + 1.0) - ln_gamma(n - k + 1.0)
}
fn binom_pmf(k: f64, n: f64, p: f64) -> f64 {
    if p == 0.0 {
        return if k == 0.0 { 1.0 } else { 0.0 };
    }
    if p == 1.0 {
        return if k == n { 1.0 } else { 0.0 };
    }
    (ln_choose(n, k) + k * p.ln() + (n - k) * (1.0 - p).ln()).exp()
}
/// P(X ≤ k) for X ~ Binomial(n, p).
fn binom_cdf(k: f64, n: f64, p: f64) -> f64 {
    if k >= n {
        1.0
    } else if k < 0.0 {
        0.0
    } else if p == 0.0 {
        1.0
    } else if p == 1.0 {
        0.0
    } else {
        beta_i(1.0 - p, n - k, k + 1.0)
    }
}
fn binom_args(k: f64, n: f64, p: f64) -> Result<(f64, f64, f64), ErrorKind> {
    let (k, n) = (k.trunc(), n.trunc());
    if k < 0.0 || n < 0.0 || k > n || !(0.0..=1.0).contains(&p) || n > 1e15 { num_err() } else { Ok((k, n, p)) }
}
pub(super) fn binom_dist(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let (k, nn, p) = binom_args(num(c, a, 0)?, num(c, a, 1)?, num(c, a, 2)?)?;
        Ok(if flag(c, a, 3)? { binom_cdf(k, nn, p) } else { binom_pmf(k, nn, p) })
    })())
}
/// BINOM.DIST.RANGE(trials, p, s, [s2]): P(s ≤ X ≤ s2), or P(X = s).
pub(super) fn binom_dist_range(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let (nn, p, s) = (num(c, a, 0)?.trunc(), num(c, a, 1)?, num(c, a, 2)?.trunc());
        let s2 = opt(c, a, 3, s)?.trunc();
        if nn < 0.0 || !(0.0..=1.0).contains(&p) || s < 0.0 || s > nn || s2 < s || s2 > nn {
            return num_err();
        }
        Ok(if s2 == s { binom_pmf(s, nn, p) } else { (binom_cdf(s2, nn, p) - binom_cdf(s - 1.0, nn, p)).max(0.0) })
    })())
}
/// BINOM.INV(trials, p, alpha) and CRITBINOM: the smallest k with
/// P(X ≤ k) ≥ alpha.
pub(super) fn binom_inv(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let (nn, p, alpha) = (num(c, a, 0)?.trunc(), num(c, a, 1)?, num(c, a, 2)?);
        if nn < 0.0 || nn > 1e15 || !(0.0..=1.0).contains(&p) || !(0.0..=1.0).contains(&alpha) {
            return num_err();
        }
        let (mut lo, mut hi) = (0.0, nn);
        while lo < hi {
            let mid = ((lo + hi) / 2.0).floor();
            if binom_cdf(mid, nn, p) >= alpha * (1.0 - 1e-15) { hi = mid } else { lo = mid + 1.0 }
        }
        Ok(lo)
    })())
}
/// NEGBINOM.DIST(failures, successes, p, cumulative); NEGBINOMDIST is the
/// probability of exactly that many failures.
pub(super) fn negbinom_dist(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let (f, s, p) = (num(c, a, 0)?.trunc(), num(c, a, 1)?.trunc(), num(c, a, 2)?);
        let cumulative = if a.len() > 3 { flag(c, a, 3)? } else { false };
        if f < 0.0 || s < 1.0 || !(0.0..=1.0).contains(&p) || p == 0.0 {
            return num_err();
        }
        if p == 1.0 {
            return Ok(if f == 0.0 || cumulative { 1.0 } else { 0.0 });
        }
        Ok(if cumulative { beta_i(p, s, f + 1.0) } else { (ln_choose(f + s - 1.0, s - 1.0) + s * p.ln() + f * (1.0 - p).ln()).exp() })
    })())
}
/// POISSON.DIST(x, mean, cumulative) and POISSON.
pub(super) fn poisson(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let (x, mean) = (num(c, a, 0)?.trunc(), num(c, a, 1)?);
        if x < 0.0 || mean < 0.0 {
            return num_err();
        }
        if mean == 0.0 {
            return Ok(if x == 0.0 || flag(c, a, 2)? { 1.0 } else { 0.0 });
        }
        Ok(if flag(c, a, 2)? { gamma_q(x + 1.0, mean) } else { (x * mean.ln() - mean - ln_gamma(x + 1.0)).exp() })
    })())
}
/// HYPGEOM.DIST(sample_s, number_sample, population_s, number_pop,
/// cumulative); HYPGEOMDIST is the probability of exactly sample_s.
pub(super) fn hypgeom_dist(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let k = num(c, a, 0)?.trunc();
        let draws = num(c, a, 1)?.trunc();
        let successes = num(c, a, 2)?.trunc();
        let total = num(c, a, 3)?.trunc();
        let cumulative = if a.len() > 4 { flag(c, a, 4)? } else { false };
        if k < 0.0 || draws <= 0.0 || successes <= 0.0 || total <= 0.0 || k > draws || k > successes || draws > total || successes > total || k < draws - total + successes || total > 1e15 {
            return num_err();
        }
        let pmf = |i: f64| (ln_choose(successes, i) + ln_choose(total - successes, draws - i) - ln_choose(total, draws)).exp();
        if !cumulative {
            return Ok(pmf(k));
        }
        let first = (draws - total + successes).max(0.0);
        if k - first > 1_000_000.0 {
            return Err(ErrorKind::Limit);
        }
        let mut sum = 0.0;
        let mut i = first;
        while i <= k {
            sum += pmf(i);
            i += 1.0;
        }
        Ok(sum.min(1.0))
    })())
}

// ---------- continuous on (0, ∞) or [A, B] ----------

/// EXPON.DIST(x, lambda, cumulative) and EXPONDIST.
pub(super) fn expon_dist(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let (x, l) = (num(c, a, 0)?, num(c, a, 1)?);
        if x < 0.0 || l <= 0.0 {
            return num_err();
        }
        Ok(if flag(c, a, 2)? { -(-l * x).exp_m1() } else { l * (-l * x).exp() })
    })())
}
/// GAMMA.DIST(x, alpha, beta, cumulative) and GAMMADIST.
pub(super) fn gamma_dist(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let (x, al, be) = (num(c, a, 0)?, num(c, a, 1)?, num(c, a, 2)?);
        if x < 0.0 || al <= 0.0 || be <= 0.0 {
            return num_err();
        }
        if flag(c, a, 3)? {
            return Ok(gamma_p(al, x / be));
        }
        if x == 0.0 {
            return Ok(if al == 1.0 { 1.0 / be } else { 0.0 });
        }
        Ok(((al - 1.0) * x.ln() - x / be - ln_gamma(al) - al * be.ln()).exp())
    })())
}
pub(super) fn gamma_inv(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let (p, al, be) = (num(c, a, 0)?, num(c, a, 1)?, num(c, a, 2)?);
        if !(0.0..1.0).contains(&p) || al <= 0.0 || be <= 0.0 {
            return num_err();
        }
        Ok(be * invert(&|x| gamma_p(al, x), p, 0.0, al.max(1.0)).ok_or(ErrorKind::Num)?)
    })())
}
/// GAMMA(x): #NUM! at zero and the negative integers.
pub(super) fn gamma_fn(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| special::gamma(num(c, a, 0)?).ok_or(ErrorKind::Num))())
}
/// GAMMALN and GAMMALN.PRECISE: ln Γ(x), x > 0.
pub(super) fn gammaln(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let x = num(c, a, 0)?;
        if x <= 0.0 { num_err() } else { Ok(ln_gamma(x)) }
    })())
}

/// The [A, B] bounds of BETA.DIST and BETA.INV (0 and 1 by default).
fn bounds(c: &Ctx, a: &[Expr], at: usize) -> Result<(f64, f64), ErrorKind> {
    let (lo, hi) = (opt(c, a, at, 0.0)?, opt(c, a, at + 1, 1.0)?);
    if lo >= hi { num_err() } else { Ok((lo, hi)) }
}
/// BETA.DIST(x, alpha, beta, cumulative, [A], [B]); BETADIST(x, alpha,
/// beta, [A], [B]) is cumulative.
pub(super) fn beta_dist(c: &Ctx, a: &[Expr], legacy: bool) -> Ev {
    ev((|| {
        let (x, al, be) = (num(c, a, 0)?, num(c, a, 1)?, num(c, a, 2)?);
        let (cumulative, (lo, hi)) = if legacy { (true, bounds(c, a, 3)?) } else { (flag(c, a, 3)?, bounds(c, a, 4)?) };
        if al <= 0.0 || be <= 0.0 || x < lo || x > hi {
            return num_err();
        }
        let y = (x - lo) / (hi - lo);
        if cumulative {
            return Ok(beta_i(y, al, be));
        }
        if y == 0.0 || y == 1.0 {
            return num_err();
        }
        let ln = (al - 1.0) * y.ln() + (be - 1.0) * (1.0 - y).ln() + ln_gamma(al + be) - ln_gamma(al) - ln_gamma(be);
        Ok(ln.exp() / (hi - lo))
    })())
}
pub(super) fn beta_dist_new(c: &Ctx, a: &[Expr]) -> Ev {
    beta_dist(c, a, false)
}
pub(super) fn betadist(c: &Ctx, a: &[Expr]) -> Ev {
    beta_dist(c, a, true)
}
/// BETA.INV(p, alpha, beta, [A], [B]) and BETAINV.
pub(super) fn beta_inv(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let (p, al, be) = (num(c, a, 0)?, num(c, a, 1)?, num(c, a, 2)?);
        let (lo, hi) = bounds(c, a, 3)?;
        if p <= 0.0 || p > 1.0 || al <= 0.0 || be <= 0.0 {
            return num_err();
        }
        let y = invert(&|y| beta_i(y, al, be), p, 0.0, 1.0).ok_or(ErrorKind::Num)?;
        Ok(lo + y * (hi - lo))
    })())
}
/// WEIBULL.DIST(x, alpha, beta, cumulative) and WEIBULL.
pub(super) fn weibull(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let (x, al, be) = (num(c, a, 0)?, num(c, a, 1)?, num(c, a, 2)?);
        if x < 0.0 || al <= 0.0 || be <= 0.0 {
            return num_err();
        }
        let r = (x / be).powf(al);
        Ok(if flag(c, a, 3)? { -(-r).exp_m1() } else { al / be.powf(al) * x.powf(al - 1.0) * (-r).exp() })
    })())
}

// ---------- error function, Fisher, confidence ----------

/// ERF(lower, [upper]): erf(lower), or the integral between the bounds.
pub(super) fn erf(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let lo = num(c, a, 0)?;
        match a.get(1).filter(|x| !missing(x)) {
            Some(_) => Ok(special::erf(num(c, a, 1)?) - special::erf(lo)),
            None => Ok(special::erf(lo)),
        }
    })())
}
pub(super) fn erf_precise(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| Ok(special::erf(num(c, a, 0)?)))())
}
pub(super) fn erfc(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| Ok(special::erfc(num(c, a, 0)?)))())
}
pub(super) fn fisher(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let x = num(c, a, 0)?;
        if x <= -1.0 || x >= 1.0 { num_err() } else { Ok(x.atanh()) }
    })())
}
pub(super) fn fisherinv(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| Ok(num(c, a, 0)?.tanh()))())
}
fn confidence_args(c: &Ctx, a: &[Expr]) -> Result<(f64, f64, f64), ErrorKind> {
    let (alpha, sd, size) = (num(c, a, 0)?, num(c, a, 1)?, num(c, a, 2)?.trunc());
    if alpha <= 0.0 || alpha >= 1.0 || sd <= 0.0 || size < 1.0 { num_err() } else { Ok((alpha, sd, size)) }
}
/// CONFIDENCE.NORM and CONFIDENCE: half the width of a normal interval.
pub(super) fn confidence_norm(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let (alpha, sd, size) = confidence_args(c, a)?;
        Ok(normal_inv(1.0 - alpha / 2.0, 0.0, 1.0)? * sd / size.sqrt())
    })())
}
/// CONFIDENCE.T: half the width of a Student's t interval.
pub(super) fn confidence_t(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let (alpha, sd, size) = confidence_args(c, a)?;
        if size < 2.0 {
            return Err(ErrorKind::Div0);
        }
        Ok(t_inv(1.0 - alpha / 2.0, size - 1.0)? * sd / size.sqrt())
    })())
}

// ---------- tests ----------

/// The numbers of an argument as a range holds them (text, logicals and
/// blanks skipped), with the place of each, for pairing.
fn sample(c: &Ctx, a: &Expr) -> Result<Vec<Option<f64>>, ErrorKind> {
    let arr = c.to_array(c.eval(a))?;
    arr.vals
        .iter()
        .map(|v| match v {
            Value::Num(x) => Ok(Some(*x)),
            Value::Err(k) => Err(*k),
            _ => Ok(None),
        })
        .collect()
}
fn mean_var(xs: &[f64]) -> (f64, f64) {
    let n = xs.len() as f64;
    let m = xs.iter().sum::<f64>() / n;
    (m, xs.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / (n - 1.0))
}

/// T.TEST(array1, array2, tails, type) and TTEST: type 1 paired, 2 equal
/// variances, 3 unequal variances (Welch).
pub(super) fn t_test(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let (x, y) = (sample(c, &a[0])?, sample(c, &a[1])?);
        let (tails, kind) = (num(c, a, 2)?.trunc(), num(c, a, 3)?.trunc());
        if !matches!(tails as i32, 1 | 2) || !matches!(kind as i32, 1..=3) {
            return num_err();
        }
        let (t, df) = if kind == 1.0 {
            if x.len() != y.len() {
                return Err(ErrorKind::NA);
            }
            let d: Vec<f64> = x.iter().zip(&y).filter_map(|p| match p { (Some(a), Some(b)) => Some(a - b), _ => None }).collect();
            if d.len() < 2 {
                return Err(ErrorKind::Div0);
            }
            let (m, v) = mean_var(&d);
            (m / (v / d.len() as f64).sqrt(), d.len() as f64 - 1.0)
        } else {
            let x: Vec<f64> = x.into_iter().flatten().collect();
            let y: Vec<f64> = y.into_iter().flatten().collect();
            if x.len() < 2 || y.len() < 2 {
                return Err(ErrorKind::Div0);
            }
            let (n1, n2) = (x.len() as f64, y.len() as f64);
            let ((m1, v1), (m2, v2)) = (mean_var(&x), mean_var(&y));
            if kind == 2.0 {
                let pooled = ((n1 - 1.0) * v1 + (n2 - 1.0) * v2) / (n1 + n2 - 2.0);
                ((m1 - m2) / (pooled * (1.0 / n1 + 1.0 / n2)).sqrt(), n1 + n2 - 2.0)
            } else {
                let (s1, s2) = (v1 / n1, v2 / n2);
                let df = (s1 + s2).powi(2) / (s1 * s1 / (n1 - 1.0) + s2 * s2 / (n2 - 1.0));
                ((m1 - m2) / (s1 + s2).sqrt(), df)
            }
        };
        if !t.is_finite() {
            return Err(ErrorKind::Div0);
        }
        Ok((tails * t_upper(t.abs(), df)).min(1.0))
    })())
}

/// Z.TEST(array, x, [sigma]) and ZTEST: the one-tailed probability of a mean
/// at least this far above x.
pub(super) fn z_test(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let xs: Vec<f64> = sample(c, &a[0])?.into_iter().flatten().collect();
        let x = num(c, a, 1)?;
        if xs.is_empty() {
            return Err(ErrorKind::NA);
        }
        let (m, v) = mean_var(&xs);
        let sigma = match a.get(2).filter(|e| !missing(e)) {
            Some(_) => num(c, a, 2)?,
            None => v.sqrt(),
        };
        if !(sigma > 0.0) {
            return Err(ErrorKind::Div0);
        }
        Ok(1.0 - norm_cdf((m - x) / (sigma / (xs.len() as f64).sqrt())))
    })())
}

/// CHISQ.TEST(actual, expected) and CHITEST: the chi-squared test of
/// independence (a table) or goodness of fit (one row or column).
pub(super) fn chisq_test(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let actual = c.to_array(c.eval(&a[0]))?;
        let expected = c.to_array(c.eval(&a[1]))?;
        if actual.rows != expected.rows || actual.cols != expected.cols {
            return Err(ErrorKind::NA);
        }
        let mut chi = 0.0;
        for (o, e) in actual.vals.iter().zip(&expected.vals) {
            match (o, e) {
                (Value::Err(k), _) | (_, Value::Err(k)) => return Err(*k),
                (Value::Num(o), Value::Num(e)) => {
                    if *e == 0.0 {
                        return Err(ErrorKind::Div0);
                    }
                    if *e < 0.0 {
                        return num_err();
                    }
                    chi += (o - e) * (o - e) / e;
                }
                _ => {}
            }
        }
        let (r, k) = (actual.rows as f64, actual.cols as f64);
        let df = if r > 1.0 && k > 1.0 { (r - 1.0) * (k - 1.0) } else { r * k - 1.0 };
        if df < 1.0 {
            return Err(ErrorKind::NA);
        }
        Ok(gamma_q(df / 2.0, chi / 2.0))
    })())
}

/// F.TEST(array1, array2) and FTEST: the two-tailed probability that the
/// variances differ this much by chance.
pub(super) fn f_test(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let x: Vec<f64> = sample(c, &a[0])?.into_iter().flatten().collect();
        let y: Vec<f64> = sample(c, &a[1])?.into_iter().flatten().collect();
        if x.len() < 2 || y.len() < 2 {
            return Err(ErrorKind::Div0);
        }
        let ((_, v1), (_, v2)) = (mean_var(&x), mean_var(&y));
        if v1 == 0.0 || v2 == 0.0 {
            return Err(ErrorKind::Div0);
        }
        let (d1, d2) = (x.len() as f64 - 1.0, y.len() as f64 - 1.0);
        let f = v1 / v2;
        Ok((2.0 * f_cdf(f, d1, d2).min(f_upper(f, d1, d2))).min(1.0))
    })())
}
