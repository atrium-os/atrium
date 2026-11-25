//! Bond and money-market functions (docs/spec/officina.md §16.137): the
//! coupon schedule (COUPPCD, COUPNCD, COUPNUM, COUPDAYBS, COUPDAYS,
//! COUPDAYSNC), PRICE, YIELD, DURATION and MDURATION, accrued interest, the
//! discounted and at-maturity securities, and Treasury bills. Dates are
//! whole-day serials; day counts follow the `basis` argument as YEARFRAC's.

use super::dates::DateSystem;
use super::more::{days_360, year_frac};
use super::special::invert;
use super::{to_bool, Ctx, Ev};
use crate::formula::Expr;
use crate::value::{ErrorKind, Value};

type R<T> = Result<T, ErrorKind>;

fn missing(a: &Expr) -> bool {
    matches!(a, Expr::Name(n) if n.is_empty())
}
fn num(c: &Ctx, a: &[Expr], i: usize) -> R<f64> {
    c.number(&c.scalar(&a[i]))
}
fn ev(r: R<f64>) -> Ev {
    match r {
        Ok(x) if x.is_finite() => Ev::V(Value::num(x)),
        Ok(_) => Ev::err(ErrorKind::Num),
        Err(k) => Ev::err(k),
    }
}
fn bad<T>() -> R<T> {
    Err(ErrorKind::Num)
}

/// A date argument as a whole-day serial inside the calendar.
fn date(c: &Ctx, a: &[Expr], i: usize) -> R<f64> {
    let d = num(c, a, i)?.trunc();
    if c.wb.date_system().contains(d) { Ok(d) } else { bad() }
}
/// The optional `basis` at `i`: 0 to 4.
fn basis(c: &Ctx, a: &[Expr], i: usize) -> R<i32> {
    let b = match a.get(i) {
        Some(x) if !missing(x) => c.number(&c.scalar(x))?.trunc(),
        _ => 0.0,
    };
    if (0.0..=4.0).contains(&b) { Ok(b as i32) } else { bad() }
}
fn frequency(c: &Ctx, a: &[Expr], i: usize) -> R<f64> {
    let f = num(c, a, i)?.trunc();
    if matches!(f as i32, 1 | 2 | 4) { Ok(f) } else { bad() }
}
fn yf(sys: DateSystem, s: f64, t: f64, basis: i32) -> R<f64> {
    year_frac(sys, s.min(t), s.max(t), basis).ok_or(ErrorKind::Num)
}

/// Days from `s` to `t` as `basis` counts them.
fn day_count(sys: DateSystem, s: f64, t: f64, basis: i32) -> R<f64> {
    match basis {
        0 => days_360(sys, s, t, false).ok_or(ErrorKind::Num),
        4 => days_360(sys, s, t, true).ok_or(ErrorKind::Num),
        _ => Ok(t - s),
    }
}

/// `anchor` moved by `months`, keeping its day of the month, or the month's
/// last day when `anchor` is a month end (or the day doesn't exist).
fn shift(sys: DateSystem, anchor: f64, months: f64) -> R<f64> {
    let (_, _, day) = sys.ymd(anchor).ok_or(ErrorKind::Num)?;
    let eom = sys.month_end(anchor, 0.0) == Some(anchor);
    let last = sys.month_end(anchor, months).ok_or(ErrorKind::Num)?;
    let (_, _, last_day) = sys.ymd(last).ok_or(ErrorKind::Num)?;
    Ok(if eom { last } else { last - (last_day - day.min(last_day)) as f64 })
}

/// The coupon period around `settlement` for a bond paying `freq` times a
/// year until `maturity`: the previous and next coupon dates and the
/// number of coupons from the next one to maturity.
struct Period {
    pcd: f64,
    ncd: f64,
    count: f64,
}

fn period(sys: DateSystem, settlement: f64, maturity: f64, freq: f64) -> R<Period> {
    if settlement >= maturity {
        return bad();
    }
    let step = 12.0 / freq;
    let (y1, m1, _) = sys.ymd(settlement).ok_or(ErrorKind::Num)?;
    let (y2, m2, _) = sys.ymd(maturity).ok_or(ErrorKind::Num)?;
    let months = ((y2 - y1) * 12 + (m2 - m1)) as f64;
    // Coupon k periods before maturity; start a little early and walk back.
    let mut k = (months / step).floor().max(0.0);
    while k > 0.0 && shift(sys, maturity, -k * step)? <= settlement {
        k -= 1.0;
    }
    let mut guard = 0;
    while shift(sys, maturity, -(k + 1.0) * step)? > settlement {
        k += 1.0;
        guard += 1;
        if guard > 8 {
            return bad();
        }
    }
    Ok(Period { pcd: shift(sys, maturity, -(k + 1.0) * step)?, ncd: shift(sys, maturity, -k * step)?, count: k + 1.0 })
}

/// The days in the coupon period (E), from its start to settlement (A) and
/// from settlement to the next coupon (DSC).
fn coupon_days(sys: DateSystem, p: &Period, settlement: f64, freq: f64, basis: i32) -> R<(f64, f64, f64)> {
    let e = match basis {
        1 => p.ncd - p.pcd,
        3 => 365.0 / freq,
        _ => 360.0 / freq,
    };
    let a = day_count(sys, p.pcd, settlement, basis)?;
    let dsc = match basis {
        0 | 4 => e - a,
        _ => p.ncd - settlement,
    };
    Ok((e, a, dsc))
}

/// The settlement, maturity, frequency and basis of the COUP functions.
fn coup_args(c: &Ctx, a: &[Expr]) -> R<(DateSystem, f64, f64, f64, i32, Period)> {
    let sys = c.wb.date_system();
    let (s, m, f, b) = (date(c, a, 0)?, date(c, a, 1)?, frequency(c, a, 2)?, basis(c, a, 3)?);
    let p = period(sys, s, m, f)?;
    Ok((sys, s, m, f, b, p))
}

pub(super) fn couppcd(c: &Ctx, a: &[Expr]) -> Ev {
    ev(coup_args(c, a).map(|x| x.5.pcd))
}
pub(super) fn coupncd(c: &Ctx, a: &[Expr]) -> Ev {
    ev(coup_args(c, a).map(|x| x.5.ncd))
}
pub(super) fn coupnum(c: &Ctx, a: &[Expr]) -> Ev {
    ev(coup_args(c, a).map(|x| x.5.count))
}
pub(super) fn coupdays(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let (sys, s, _, f, b, p) = coup_args(c, a)?;
        Ok(coupon_days(sys, &p, s, f, b)?.0)
    })())
}
pub(super) fn coupdaybs(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let (sys, s, _, f, b, p) = coup_args(c, a)?;
        Ok(coupon_days(sys, &p, s, f, b)?.1)
    })())
}
pub(super) fn coupdaysnc(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let (sys, s, _, f, b, p) = coup_args(c, a)?;
        Ok(coupon_days(sys, &p, s, f, b)?.2)
    })())
}

/// A coupon bond: what PRICE, YIELD and DURATION share.
struct Bond {
    rate: f64,
    redemption: f64,
    freq: f64,
    n: f64,
    e: f64,
    a: f64,
    dsc: f64,
}

impl Bond {
    fn new(sys: DateSystem, s: f64, m: f64, rate: f64, redemption: f64, freq: f64, basis: i32) -> R<Bond> {
        let p = period(sys, s, m, freq)?;
        let (e, a, dsc) = coupon_days(sys, &p, s, freq, basis)?;
        Ok(Bond { rate, redemption, freq, n: p.count, e, a, dsc })
    }
    /// The clean price per 100 face at annual yield `y`.
    fn price(&self, y: f64) -> f64 {
        let coupon = 100.0 * self.rate / self.freq;
        let accrued = coupon * self.a / self.e;
        let g = 1.0 + y / self.freq;
        if self.n <= 1.0 {
            return (self.redemption + coupon) / (1.0 + self.dsc / self.e * y / self.freq) - accrued;
        }
        let lead = self.dsc / self.e;
        let mut sum = self.redemption / g.powf(self.n - 1.0 + lead);
        let mut k = 1.0;
        while k <= self.n {
            sum += coupon / g.powf(k - 1.0 + lead);
            k += 1.0;
        }
        sum - accrued
    }
    /// Macaulay duration in years at yield `y`.
    fn duration(&self, y: f64) -> f64 {
        let coupon = 100.0 * self.rate / self.freq;
        let g = 1.0 + y / self.freq;
        let lead = self.dsc / self.e;
        let (mut weighted, mut total) = (0.0, 0.0);
        let mut k = 1.0;
        while k <= self.n {
            let t = k - 1.0 + lead;
            let cash = if k == self.n { coupon + 100.0 } else { coupon };
            let pv = cash / g.powf(t);
            weighted += t * pv;
            total += pv;
            k += 1.0;
        }
        weighted / total / self.freq
    }
}

/// The most coupons a bond may have: 4 a year for 8,000 years.
const MAX_COUPONS: f64 = 32_000.0;

fn bond_args(c: &Ctx, a: &[Expr], at_freq: usize) -> R<(DateSystem, f64, f64, f64, i32)> {
    let sys = c.wb.date_system();
    let (s, m) = (date(c, a, 0)?, date(c, a, 1)?);
    let (f, b) = (frequency(c, a, at_freq)?, basis(c, a, at_freq + 1)?);
    if s >= m {
        return bad();
    }
    Ok((sys, s, m, f, b))
}

/// PRICE(settlement, maturity, rate, yld, redemption, frequency, [basis]).
pub(super) fn price(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let (sys, s, m, f, b) = bond_args(c, a, 5)?;
        let (rate, y, red) = (num(c, a, 2)?, num(c, a, 3)?, num(c, a, 4)?);
        if rate < 0.0 || y < 0.0 || red <= 0.0 {
            return bad();
        }
        let bond = Bond::new(sys, s, m, rate, red, f, b)?;
        if bond.n > MAX_COUPONS {
            return bad();
        }
        Ok(bond.price(y))
    })())
}

/// YIELD(settlement, maturity, rate, pr, redemption, frequency, [basis]):
/// the yield at which PRICE gives `pr`, found by bisection.
pub(super) fn yield_fn(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let (sys, s, m, f, b) = bond_args(c, a, 5)?;
        let (rate, pr, red) = (num(c, a, 2)?, num(c, a, 3)?, num(c, a, 4)?);
        if rate < 0.0 || pr <= 0.0 || red <= 0.0 {
            return bad();
        }
        let bond = Bond::new(sys, s, m, rate, red, f, b)?;
        if bond.n > MAX_COUPONS {
            return bad();
        }
        // Price falls as the yield rises.
        invert(&|y| -bond.price(y), -pr, -f * 0.999, 1.0).ok_or(ErrorKind::Num)
    })())
}

fn duration_of(c: &Ctx, a: &[Expr]) -> R<(f64, f64, f64)> {
    let (sys, s, m, f, b) = bond_args(c, a, 4)?;
    let (coupon, y) = (num(c, a, 2)?, num(c, a, 3)?);
    if coupon < 0.0 || y < 0.0 {
        return bad();
    }
    let bond = Bond::new(sys, s, m, coupon, 100.0, f, b)?;
    if bond.n > MAX_COUPONS {
        return bad();
    }
    Ok((bond.duration(y), y, f))
}
/// DURATION(settlement, maturity, coupon, yld, frequency, [basis]).
pub(super) fn duration(c: &Ctx, a: &[Expr]) -> Ev {
    ev(duration_of(c, a).map(|d| d.0))
}
/// MDURATION: Macaulay duration over 1 + yld / frequency.
pub(super) fn mduration(c: &Ctx, a: &[Expr]) -> Ev {
    ev(duration_of(c, a).map(|(d, y, f)| d / (1.0 + y / f)))
}

/// ACCRINT(issue, first_interest, settlement, rate, par, frequency,
/// [basis], [calc_method]): interest accrued from issue (or, when
/// calc_method is FALSE and settlement is past the first coupon, from the
/// first coupon) to settlement, period by period of the coupon schedule
/// through first_interest.
pub(super) fn accrint(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let sys = c.wb.date_system();
        let (issue, first, settle) = (date(c, a, 0)?, date(c, a, 1)?, date(c, a, 2)?);
        let (rate, par, freq, b) = (num(c, a, 3)?, num(c, a, 4)?, frequency(c, a, 5)?, basis(c, a, 6)?);
        let from_issue = match a.get(7) {
            Some(x) if !missing(x) => to_bool(&c.scalar(x))?,
            _ => true,
        };
        if rate <= 0.0 || par <= 0.0 || issue >= settle {
            return bad();
        }
        let start = if !from_issue && settle > first { first } else { issue };
        let step = 12.0 / freq;
        // The coupon period holding `start`: walk from first_interest.
        let mut k = 0.0;
        while shift(sys, first, k * step)? > start {
            k -= 1.0;
            if k < -MAX_COUPONS {
                return bad();
            }
        }
        while shift(sys, first, (k + 1.0) * step)? <= start {
            k += 1.0;
            if k > MAX_COUPONS {
                return bad();
            }
        }
        let mut sum = 0.0;
        loop {
            let (pcd, ncd) = (shift(sys, first, k * step)?, shift(sys, first, (k + 1.0) * step)?);
            if pcd >= settle {
                break;
            }
            let (from, to) = (start.max(pcd), settle.min(ncd));
            let length = match b {
                1 => ncd - pcd,
                3 => 365.0 / freq,
                _ => 360.0 / freq,
            };
            sum += day_count(sys, from, to, b)? / length;
            k += 1.0;
            if k > MAX_COUPONS {
                return bad();
            }
        }
        Ok(par * rate / freq * sum)
    })())
}

/// ACCRINTM(issue, settlement, rate, par, [basis]): interest of a security
/// paying at maturity.
pub(super) fn accrintm(c: &Ctx, a: &[Expr]) -> Ev {
    ev((|| {
        let (issue, settle, rate, par, b) = (date(c, a, 0)?, date(c, a, 1)?, num(c, a, 2)?, num(c, a, 3)?, basis(c, a, 4)?);
        if rate <= 0.0 || par <= 0.0 || issue >= settle {
            return bad();
        }
        Ok(par * rate * yf(c.wb.date_system(), issue, settle, b)?)
    })())
}

/// Settlement, maturity, two numbers (both must be positive) and a basis.
fn short_args(c: &Ctx, a: &[Expr]) -> R<(f64, f64, f64)> {
    let (s, m) = (date(c, a, 0)?, date(c, a, 1)?);
    let (x, y, b) = (num(c, a, 2)?, num(c, a, 3)?, basis(c, a, 4)?);
    if s >= m || x <= 0.0 || y <= 0.0 {
        return bad();
    }
    Ok((x, y, yf(c.wb.date_system(), s, m, b)?))
}
/// DISC(settlement, maturity, pr, redemption, [basis]).
pub(super) fn disc(c: &Ctx, a: &[Expr]) -> Ev {
    ev(short_args(c, a).map(|(pr, red, t)| (red - pr) / red / t))
}
/// PRICEDISC(settlement, maturity, discount, redemption, [basis]).
pub(super) fn pricedisc(c: &Ctx, a: &[Expr]) -> Ev {
    ev(short_args(c, a).map(|(d, red, t)| red - d * red * t))
}
/// YIELDDISC(settlement, maturity, pr, redemption, [basis]).
pub(super) fn yielddisc(c: &Ctx, a: &[Expr]) -> Ev {
    ev(short_args(c, a).map(|(pr, red, t)| (red - pr) / pr / t))
}
/// RECEIVED(settlement, maturity, investment, discount, [basis]).
pub(super) fn received(c: &Ctx, a: &[Expr]) -> Ev {
    ev(short_args(c, a).and_then(|(inv, d, t)| if d * t >= 1.0 { bad() } else { Ok(inv / (1.0 - d * t)) }))
}
/// INTRATE(settlement, maturity, investment, redemption, [basis]).
pub(super) fn intrate(c: &Ctx, a: &[Expr]) -> Ev {
    ev(short_args(c, a).map(|(inv, red, t)| (red - inv) / inv / t))
}

/// Settlement, maturity and issue of a security paying at maturity, as
/// year fractions: issue to maturity, issue to settlement, settlement to
/// maturity.
fn mat_args(c: &Ctx, a: &[Expr]) -> R<(f64, f64, f64, f64, f64)> {
    let sys = c.wb.date_system();
    let (s, m, issue) = (date(c, a, 0)?, date(c, a, 1)?, date(c, a, 2)?);
    let (rate, x, b) = (num(c, a, 3)?, num(c, a, 4)?, basis(c, a, 5)?);
    if s >= m || issue > s || rate < 0.0 || x < 0.0 {
        return bad();
    }
    Ok((yf(sys, issue, m, b)?, yf(sys, issue, s, b)?, yf(sys, s, m, b)?, rate, x))
}
/// PRICEMAT(settlement, maturity, issue, rate, yld, [basis]).
pub(super) fn pricemat(c: &Ctx, a: &[Expr]) -> Ev {
    ev(mat_args(c, a).map(|(dim, ai, dsm, rate, y)| (100.0 + dim * rate * 100.0) / (1.0 + dsm * y) - ai * rate * 100.0))
}
/// YIELDMAT(settlement, maturity, issue, rate, pr, [basis]).
pub(super) fn yieldmat(c: &Ctx, a: &[Expr]) -> Ev {
    ev(mat_args(c, a).and_then(|(dim, ai, dsm, rate, pr)| {
        if pr <= 0.0 {
            return bad();
        }
        Ok(((1.0 + dim * rate) / (pr / 100.0 + ai * rate) - 1.0) / dsm)
    }))
}

/// A Treasury bill's days to maturity (at most a year) and its number.
fn tbill_args(c: &Ctx, a: &[Expr]) -> R<(f64, f64)> {
    let (s, m, x) = (date(c, a, 0)?, date(c, a, 1)?, num(c, a, 2)?);
    let days = m - s;
    if days <= 0.0 || x <= 0.0 {
        return bad();
    }
    // Maturity no more than a year after settlement.
    let year_on = shift(c.wb.date_system(), s, 12.0)?;
    if m > year_on {
        return bad();
    }
    Ok((days, x))
}
/// TBILLPRICE(settlement, maturity, discount).
pub(super) fn tbillprice(c: &Ctx, a: &[Expr]) -> Ev {
    ev(tbill_args(c, a).and_then(|(d, disc)| {
        let p = 100.0 * (1.0 - disc * d / 360.0);
        if p <= 0.0 { bad() } else { Ok(p) }
    }))
}
/// TBILLYIELD(settlement, maturity, pr).
pub(super) fn tbillyield(c: &Ctx, a: &[Expr]) -> Ev {
    ev(tbill_args(c, a).map(|(d, pr)| (100.0 - pr) / pr * 360.0 / d))
}
/// TBILLEQ(settlement, maturity, discount): the bond-equivalent yield.
pub(super) fn tbilleq(c: &Ctx, a: &[Expr]) -> Ev {
    ev(tbill_args(c, a).and_then(|(d, disc)| {
        let price = 100.0 * (1.0 - disc * d / 360.0);
        if price <= 0.0 {
            return bad();
        }
        if d <= 182.0 {
            return Ok(365.0 * disc / (360.0 - disc * d));
        }
        // Over half a year: a semiannual coupon is assumed.
        let t = d / 365.0;
        let root = (t * t - (2.0 * t - 1.0) * (1.0 - 100.0 / price)).sqrt();
        Ok((-t + root) / (t - 0.5))
    }))
}
