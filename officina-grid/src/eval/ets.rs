//! FORECAST.ETS, FORECAST.ETS.CONFINT, FORECAST.ETS.SEASONALITY and
//! FORECAST.ETS.STAT (docs/spec/officina.md §16.148): additive exponential
//! triple smoothing (Holt-Winters, ETS AAA) over a timeline with a constant
//! step.
//!
//! Excel doesn't publish how it fits the smoothing parameters, so its
//! numbers and these agree exactly on series a model fits exactly (a line,
//! a line plus a repeating pattern) and closely, not to the digit, on
//! noisy ones. The parameters minimise the squared one-step errors; the
//! search is a coarse grid then coordinate refinement, bounded by
//! [`MAX_POINTS`].

use super::{Ctx, Ev};
use crate::formula::Expr;
use crate::value::{ErrorKind, Value};

macro_rules! tri {
    ($x:expr) => {
        match $x {
            Ok(v) => v,
            Err(k) => return Ev::err(k),
        }
    };
}

/// The most points a timeline may have once its gaps are filled.
pub const MAX_POINTS: usize = 100_000;
/// The longest season Excel allows.
pub const MAX_SEASON: usize = 8_760;

type R<T> = Result<T, ErrorKind>;

fn missing(a: &Expr) -> bool {
    matches!(a, Expr::Name(n) if n.is_empty())
}

fn num(c: &Ctx, a: &Expr) -> R<f64> {
    c.number(&c.scalar(a))
}

fn opt(c: &Ctx, a: &[Expr], i: usize, default: f64) -> R<f64> {
    match a.get(i) {
        Some(e) if !missing(e) => num(c, e),
        _ => Ok(default),
    }
}

/// A regular series: the first time, the step, and a value at each step.
#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    pub start: f64,
    pub step: f64,
    pub ys: Vec<f64>,
}

/// Duplicate times are combined by `aggregation` (Excel's 1 AVERAGE, 2
/// COUNT, 3 COUNTA, 4 MAX, 5 MEDIAN, 6 MIN, 7 SUM).
fn combine(vals: &mut [f64], aggregation: u8) -> f64 {
    let n = vals.len() as f64;
    match aggregation {
        2 | 3 => n,
        4 => vals.iter().copied().fold(f64::MIN, f64::max),
        5 => {
            vals.sort_by(|a, b| a.total_cmp(b));
            let m = vals.len() / 2;
            if vals.len() % 2 == 1 { vals[m] } else { (vals[m - 1] + vals[m]) / 2.0 }
        }
        6 => vals.iter().copied().fold(f64::MAX, f64::min),
        7 => vals.iter().sum(),
        _ => vals.iter().sum::<f64>() / n,
    }
}

/// The series from `values` and `timeline`: sorted, duplicates combined,
/// gaps and blank values filled by interpolation (`complete`) or zero.
/// #N/A when the two differ in size, #VALUE! for a time that isn't a
/// number, #NUM! when the times have no common step, are too few, or
/// more than 30% of the steps would be filled.
pub fn series(values: &[Value], timeline: &[Value], complete: bool, aggregation: u8) -> R<Series> {
    if values.len() != timeline.len() {
        return Err(ErrorKind::NA);
    }
    let mut pts: Vec<(f64, Option<f64>)> = Vec::with_capacity(values.len());
    for (y, t) in values.iter().zip(timeline) {
        let t = match t {
            Value::Num(t) => *t,
            Value::Err(k) => return Err(*k),
            Value::Empty => continue,
            _ => return Err(ErrorKind::Value),
        };
        let y = match y {
            Value::Num(y) => Some(*y),
            Value::Err(k) => return Err(*k),
            _ => None,
        };
        pts.push((t, y));
    }
    pts.sort_by(|a, b| a.0.total_cmp(&b.0));
    // Distinct times, each with its values.
    let mut times: Vec<(f64, Vec<f64>)> = Vec::new();
    for (t, y) in pts {
        match times.last_mut() {
            Some(last) if last.0 == t => last.1.extend(y),
            _ => times.push((t, y.into_iter().collect())),
        }
    }
    if times.len() < 3 {
        return Err(ErrorKind::Num);
    }
    let step = times.windows(2).map(|w| w[1].0 - w[0].0).fold(f64::MAX, f64::min);
    if !(step > 0.0) || !step.is_finite() {
        return Err(ErrorKind::Num);
    }
    let start = times[0].0;
    let span = (times[times.len() - 1].0 - start) / step;
    if !(span.is_finite() && span < MAX_POINTS as f64) {
        return Err(ErrorKind::Num);
    }
    let len = span.round() as usize + 1;
    let mut ys: Vec<Option<f64>> = vec![None; len];
    for (t, mut v) in times {
        let k = (t - start) / step;
        // Every time must sit on the step's grid.
        if (k - k.round()).abs() > 1e-6 {
            return Err(ErrorKind::Num);
        }
        if !v.is_empty() {
            if let Some(slot) = ys.get_mut(k.round() as usize) {
                *slot = Some(combine(&mut v, aggregation));
            }
        }
    }
    let known: Vec<usize> = (0..len).filter(|&i| ys[i].is_some()).collect();
    // Excel fills at most 30% of the points.
    if known.len() < 2 || (len - known.len()) * 10 > len * 3 {
        return Err(ErrorKind::Num);
    }
    let mut out = Vec::with_capacity(len);
    let mut next = 0;
    for i in 0..len {
        if let Some(y) = ys[i] {
            out.push(y);
            continue;
        }
        if !complete {
            out.push(0.0);
            continue;
        }
        while next < known.len() && known[next] < i {
            next += 1;
        }
        let (a, b) = (known.get(next.wrapping_sub(1)).copied(), known.get(next).copied());
        out.push(match (a, b) {
            (Some(a), Some(b)) => {
                let (ya, yb) = (ys[a].unwrap_or(0.0), ys[b].unwrap_or(0.0));
                ya + (yb - ya) * (i - a) as f64 / (b - a) as f64
            }
            (Some(a), None) => ys[a].unwrap_or(0.0),
            (None, Some(b)) => ys[b].unwrap_or(0.0),
            (None, None) => 0.0,
        });
    }
    Ok(Series { start, step, ys: out })
}

/// The season length found in `ys`: the lag (2 up to half the series)
/// whose autocorrelation of the detrended series is highest and a clear
/// peak, or 0 when none stands out.
pub fn detect_season(ys: &[f64]) -> usize {
    let n = ys.len();
    if n < 6 {
        return 0;
    }
    // Take off the least-squares line.
    let nf = n as f64;
    let mx = (nf - 1.0) / 2.0;
    let my = ys.iter().sum::<f64>() / nf;
    let (mut sxx, mut sxy) = (0.0, 0.0);
    for (i, y) in ys.iter().enumerate() {
        sxx += (i as f64 - mx).powi(2);
        sxy += (i as f64 - mx) * (y - my);
    }
    let b = if sxx > 0.0 { sxy / sxx } else { 0.0 };
    let r: Vec<f64> = ys.iter().enumerate().map(|(i, y)| y - my - b * (i as f64 - mx)).collect();
    let var: f64 = r.iter().map(|x| x * x).sum();
    if var <= 1e-12 * (1.0 + my * my) * nf {
        return 0;
    }
    let max_lag = (n / 2).min(MAX_SEASON);
    // Bound the work: a long series looks at fewer lags.
    let max_lag = max_lag.min((50_000_000 / n).max(2));
    let acf = |lag: usize| r.iter().zip(&r[lag..]).map(|(a, b)| a * b).sum::<f64>() / var;
    let vals: Vec<f64> = (0..=max_lag + 1).map(|l| if l < n { acf(l) } else { 0.0 }).collect();
    let mut best = (0usize, 0.0f64);
    for lag in 2..=max_lag {
        let v = vals[lag];
        let peak = v > vals[lag - 1] && v >= vals.get(lag + 1).copied().unwrap_or(f64::MIN);
        // A multiple of a season found already is that season again.
        if peak && v > 0.3 && v > best.1 + 0.05 && !(best.0 > 0 && lag % best.0 == 0) {
            best = (lag, v);
        }
    }
    best.0
}

/// A fitted model.
#[derive(Debug, Clone)]
pub struct Fit {
    pub alpha: f64,
    pub beta: f64,
    pub gamma: f64,
    pub season: usize,
    level: f64,
    trend: f64,
    seasonal: Vec<f64>,
    /// One-step errors and the fitted values, from the first fitted point.
    pub errors: Vec<f64>,
    pub fitted: Vec<f64>,
    pub first: usize,
}

struct Run {
    sse: f64,
    level: f64,
    trend: f64,
    seasonal: Vec<f64>,
}

fn initial(ys: &[f64], m: usize) -> (f64, f64, Vec<f64>, usize) {
    if m >= 2 && ys.len() >= 2 * m {
        let a = ys[..m].iter().sum::<f64>() / m as f64;
        let b = ys[m..2 * m].iter().sum::<f64>() / m as f64;
        let trend = (b - a) / m as f64;
        // The first season's level is its mean, centred at (m-1)/2.
        let level = a - trend * (m as f64 - 1.0) / 2.0;
        let seasonal = (0..m).map(|i| ys[i] - (level + trend * i as f64)).collect();
        // The level and trend at the season's last point.
        (level + trend * (m as f64 - 1.0), trend, seasonal, m)
    } else {
        (ys[1], ys[1] - ys[0], Vec::new(), 2)
    }
}

fn run(ys: &[f64], m: usize, alpha: f64, beta: f64, gamma: f64, mut keep: Option<(&mut Vec<f64>, &mut Vec<f64>)>) -> Run {
    let (mut level, mut trend, mut seasonal, first) = initial(ys, m);
    let seasonal_on = !seasonal.is_empty();
    let mut sse = 0.0;
    for (t, &y) in ys.iter().enumerate().skip(first) {
        let s = if seasonal_on { seasonal[t % m] } else { 0.0 };
        let guess = level + trend + s;
        let e = y - guess;
        sse += e * e;
        if let Some((errs, fits)) = keep.as_mut() {
            errs.push(e);
            fits.push(guess);
        }
        let new_level = alpha * (y - s) + (1.0 - alpha) * (level + trend);
        if seasonal_on {
            seasonal[t % m] = gamma * (y - level - trend) + (1.0 - gamma) * s;
        }
        trend = beta * (new_level - level) + (1.0 - beta) * trend;
        level = new_level;
    }
    Run { sse, level, trend, seasonal }
}

/// Fits the model with season `m` (0 or 1 for none).
pub fn fit(ys: &[f64], m: usize) -> R<Fit> {
    let m = if m >= 2 && ys.len() >= 2 * m { m } else { 0 };
    if ys.len() < 3 {
        return Err(ErrorKind::Num);
    }
    let gammas: &[f64] = if m >= 2 { &[0.05, 0.25, 0.5, 0.75, 0.95] } else { &[0.0] };
    let grid = [0.05, 0.25, 0.5, 0.75, 0.95];
    let sse = |a: f64, b: f64, g: f64| run(ys, m, a, b, g, None).sse;
    let mut best = (f64::MAX, 0.5, 0.1, 0.0);
    for &a in &grid {
        for &b in &grid {
            for &g in gammas {
                let e = sse(a, b, g);
                if e < best.0 {
                    best = (e, a, b, g);
                }
            }
        }
    }
    // Coordinate refinement with a halving step.
    let (mut e, mut a, mut b, mut g) = best;
    let mut h = 0.1;
    for _ in 0..6 {
        for which in 0..if m >= 2 { 3 } else { 2 } {
            for dir in [-1.0, 1.0] {
                let mut p = [a, b, g];
                p[which] = (p[which] + dir * h).clamp(0.001, 0.999);
                let ne = sse(p[0], p[1], p[2]);
                if ne < e {
                    (e, a, b, g) = (ne, p[0], p[1], p[2]);
                }
            }
        }
        h /= 2.0;
    }
    let (mut errors, mut fitted) = (Vec::new(), Vec::new());
    let r = run(ys, m, a, b, g, Some((&mut errors, &mut fitted)));
    let first = ys.len() - errors.len();
    Ok(Fit { alpha: a, beta: b, gamma: if m >= 2 { g } else { 0.0 }, season: m, level: r.level, trend: r.trend, seasonal: r.seasonal, errors, fitted, first })
}

impl Fit {
    /// The forecast `h` steps past the last point (`h` may be fractional:
    /// between two steps it is interpolated).
    pub fn forecast(&self, n: usize, h: f64) -> f64 {
        let at = |k: usize| {
            let s = if self.season >= 2 { self.seasonal[(n - 1 + k) % self.season] } else { 0.0 };
            self.level + self.trend * k as f64 + s
        };
        let lo = h.floor().max(1.0) as usize;
        let frac = h - lo as f64;
        if frac <= 0.0 { at(lo) } else { at(lo) + (at(lo + 1) - at(lo)) * frac }
    }

    /// The prediction interval's half width `h` steps ahead at `confidence`.
    pub fn interval(&self, h: f64, confidence: f64) -> R<f64> {
        let n = self.errors.len().max(1) as f64;
        let sigma2 = self.errors.iter().map(|e| e * e).sum::<f64>() / n;
        let steps = h.ceil().max(1.0) as usize;
        let mut var = 1.0;
        for j in 1..steps {
            let seasonal = if self.season >= 2 && j % self.season == 0 { self.gamma } else { 0.0 };
            var += (self.alpha * (1.0 + j as f64 * self.beta) + seasonal).powi(2);
        }
        let z = super::dist::normal_inv((1.0 + confidence) / 2.0, 0.0, 1.0)?;
        Ok(z * (sigma2 * var).sqrt())
    }
}

/// Reads (values, timeline) and the optional seasonality, completion and
/// aggregation arguments from `a[i..]`: the series, the season used, the fit.
fn model(c: &Ctx, values: &Expr, timeline: &Expr, a: &[Expr], i: usize) -> R<(Series, Fit)> {
    let season = opt(c, a, i, 1.0)?;
    let complete = opt(c, a, i + 1, 1.0)?;
    let aggregation = opt(c, a, i + 2, 1.0)?;
    if !(0.0..=MAX_SEASON as f64).contains(&season) || !(0.0..=1.0).contains(&complete) || !(1.0..=7.0).contains(&aggregation) {
        return Err(ErrorKind::Num);
    }
    let vs = c.to_array(c.eval(values))?;
    let ts = c.to_array(c.eval(timeline))?;
    let s = series(&vs.vals, &ts.vals, complete.trunc() != 0.0, aggregation.trunc() as u8)?;
    let m = match season.trunc() as usize {
        1 => detect_season(&s.ys),
        m => m,
    };
    let f = fit(&s.ys, m)?;
    Ok((s, f))
}

/// How many steps past the timeline's end `target` is; #NUM! before it.
fn steps_ahead(s: &Series, target: f64) -> R<f64> {
    let end = s.start + s.step * (s.ys.len() - 1) as f64;
    let h = (target - end) / s.step;
    if !(h > 0.0) || h > MAX_POINTS as f64 {
        return Err(ErrorKind::Num);
    }
    Ok(h)
}

fn n(x: f64) -> Ev {
    if x.is_finite() { Ev::V(Value::num(x)) } else { Ev::err(ErrorKind::Num) }
}

/// FORECAST.ETS(target_date, values, timeline, [seasonality], [data_completion], [aggregation]).
pub(super) fn forecast_ets(c: &Ctx, a: &[Expr]) -> Ev {
    let target = tri!(num(c, &a[0]));
    let (s, f) = tri!(model(c, &a[1], &a[2], a, 3));
    let h = tri!(steps_ahead(&s, target));
    n(f.forecast(s.ys.len(), h))
}

/// FORECAST.ETS.CONFINT(target_date, values, timeline, [confidence_level], [seasonality], [data_completion], [aggregation]).
pub(super) fn forecast_ets_confint(c: &Ctx, a: &[Expr]) -> Ev {
    let target = tri!(num(c, &a[0]));
    let confidence = tri!(opt(c, a, 3, 0.95));
    if !(confidence > 0.0 && confidence < 1.0) {
        return Ev::err(ErrorKind::Num);
    }
    let (s, f) = tri!(model(c, &a[1], &a[2], a, 4));
    let h = tri!(steps_ahead(&s, target));
    n(tri!(f.interval(h, confidence)))
}

/// FORECAST.ETS.SEASONALITY(values, timeline, [data_completion], [aggregation]): the season found, or 0.
pub(super) fn forecast_ets_seasonality(c: &Ctx, a: &[Expr]) -> Ev {
    let complete = tri!(opt(c, a, 2, 1.0));
    let aggregation = tri!(opt(c, a, 3, 1.0));
    if !(0.0..=1.0).contains(&complete) || !(1.0..=7.0).contains(&aggregation) {
        return Ev::err(ErrorKind::Num);
    }
    let vs = tri!(c.to_array(c.eval(&a[0])));
    let ts = tri!(c.to_array(c.eval(&a[1])));
    let s = tri!(series(&vs.vals, &ts.vals, complete.trunc() != 0.0, aggregation.trunc() as u8));
    n(detect_season(&s.ys) as f64)
}

/// FORECAST.ETS.STAT(values, timeline, statistic_type, [seasonality], [data_completion], [aggregation]):
/// 1 alpha, 2 beta, 3 gamma, 4 MASE, 5 SMAPE, 6 MAE, 7 RMSE, 8 the step.
pub(super) fn forecast_ets_stat(c: &Ctx, a: &[Expr]) -> Ev {
    let kind = tri!(num(c, &a[2])).trunc();
    if !(1.0..=8.0).contains(&kind) {
        return Ev::err(ErrorKind::Num);
    }
    let (s, f) = tri!(model(c, &a[0], &a[1], a, 3));
    let k = f.errors.len().max(1) as f64;
    let mae = f.errors.iter().map(|e| e.abs()).sum::<f64>() / k;
    n(match kind as u8 {
        1 => f.alpha,
        2 => f.beta,
        3 => f.gamma,
        4 => {
            let naive = s.ys.windows(2).map(|w| (w[1] - w[0]).abs()).sum::<f64>() / (s.ys.len() - 1) as f64;
            if naive == 0.0 { return Ev::err(ErrorKind::Div0) }
            mae / naive
        }
        5 => {
            let ys = &s.ys[f.first..];
            ys.iter().zip(&f.fitted).map(|(y, g)| { let d = (y.abs() + g.abs()) / 2.0; if d == 0.0 { 0.0 } else { (y - g).abs() / d } }).sum::<f64>() / k
        }
        6 => mae,
        7 => (f.errors.iter().map(|e| e * e).sum::<f64>() / k).sqrt(),
        _ => s.step,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nums(v: &[f64]) -> Vec<Value> {
        v.iter().map(|x| Value::Num(*x)).collect()
    }

    #[test]
    fn a_line_and_a_season_are_forecast_exactly() {
        let ys: Vec<f64> = (0..24).map(|i| 10.0 + 2.0 * i as f64 + [5.0, -1.0, -3.0, -1.0][i % 4]).collect();
        let ts: Vec<f64> = (0..24).map(|i| 100.0 + 7.0 * i as f64).collect();
        let s = series(&nums(&ys), &nums(&ts), true, 1).unwrap();
        assert_eq!((s.start, s.step, s.ys.len()), (100.0, 7.0, 24));
        assert_eq!(detect_season(&s.ys), 4);
        let f = fit(&s.ys, 4).unwrap();
        for h in 1..=8 {
            let want = 10.0 + 2.0 * (23 + h) as f64 + [5.0, -1.0, -3.0, -1.0][(23 + h) % 4];
            assert!((f.forecast(24, h as f64) - want).abs() < 1e-6, "h={h}: {} vs {want}", f.forecast(24, h as f64));
        }
        assert!(f.interval(1.0, 0.95).unwrap() < 1e-6);
        // A plain line has no season.
        let line: Vec<f64> = (0..12).map(|i| 3.0 * i as f64 + 1.0).collect();
        assert_eq!(detect_season(&line), 0);
        let f = fit(&line, 0).unwrap();
        assert!((f.forecast(12, 2.0) - 40.0).abs() < 1e-9);
        assert!((f.forecast(12, 1.5) - 38.5).abs() < 1e-9);
    }

    #[test]
    fn the_timeline_is_sorted_combined_and_filled() {
        // Unsorted, a duplicate (averaged), a gap (interpolated or zero).
        let s = series(&nums(&[4.0, 1.0, 3.0, 5.0, 2.0]), &nums(&[4.0, 1.0, 2.0, 2.0, 0.0]), true, 1).unwrap();
        assert_eq!(s.ys, vec![2.0, 1.0, 4.0, 4.0, 4.0]);
        let s = series(&nums(&[4.0, 1.0, 3.0, 5.0, 2.0]), &nums(&[4.0, 1.0, 2.0, 2.0, 0.0]), false, 7).unwrap();
        assert_eq!(s.ys, vec![2.0, 1.0, 8.0, 0.0, 4.0]);
        // Times off the step's grid, too few, or sizes that differ.
        assert_eq!(series(&nums(&[1.0, 2.0, 3.0]), &nums(&[0.0, 2.0, 3.5]), true, 1), Err(ErrorKind::Num));
        assert_eq!(series(&nums(&[1.0, 2.0]), &nums(&[0.0, 1.0]), true, 1), Err(ErrorKind::Num));
        assert_eq!(series(&nums(&[1.0, 2.0]), &nums(&[0.0, 1.0, 2.0]), true, 1), Err(ErrorKind::NA));
        assert_eq!(series(&nums(&[1.0, 2.0, 3.0]), &[Value::Num(0.0), Value::str("x"), Value::Num(2.0)], true, 1), Err(ErrorKind::Value));
        assert_eq!(series(&nums(&[1.0, 2.0, 3.0]), &nums(&[0.0, 1.0, 1e9]), true, 1), Err(ErrorKind::Num), "too many points to fill");
    }

    #[test]
    fn the_interval_widens_with_the_horizon() {
        let ys: Vec<f64> = (0..40).map(|i| 50.0 + i as f64 + ((i * 7919) % 13) as f64 - 6.0).collect();
        let f = fit(&ys, 0).unwrap();
        let (a, b) = (f.interval(1.0, 0.95).unwrap(), f.interval(10.0, 0.95).unwrap());
        assert!(a > 0.0 && b > a, "{a} {b}");
        assert!(f.interval(1.0, 0.99).unwrap() > a);
        assert!((0.0..=1.0).contains(&f.alpha) && (0.0..=1.0).contains(&f.beta));
    }
}
