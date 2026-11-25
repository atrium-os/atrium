//! The first tier of functions (docs/spec/officina.md §8.2): math, statistics, logic,
//! information, text, lookup and dates. Each function gets its argument expressions
//! unevaluated, so IF, IFERROR and CHOOSE evaluate only what they use.

use super::criteria::{Criterion, Key};
use super::ifcache;
use super::arrays::*;
use super::more::*;
use super::fin::*;
use super::textafter::textsplit;
use super::refs::{indirect, offset};
use super::conv::*;
use super::stats::*;
use super::dist::*;
use super::linest::*;
use super::bonds::*;
use super::{to_bool, to_text, Array, Ctx, Ev};
use crate::addr::Rect;
use crate::formula::Expr;
use crate::value::{cmp_text, compare, ErrorKind, Value};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

pub type FnImpl = fn(&Ctx, &[Expr]) -> Ev;

pub struct FuncSpec {
    pub name: &'static str,
    pub min: u8,
    pub max: u8,
    /// Recomputed on every recalculation.
    pub volatile: bool,
    pub f: FnImpl,
}

const N: u8 = 255;

macro_rules! f {
    ($name:literal, $min:expr, $max:expr, $f:ident) => {
        FuncSpec { name: $name, min: $min, max: $max, volatile: false, f: $f }
    };
    ($name:literal, $min:expr, $max:expr, $f:ident, volatile) => {
        FuncSpec { name: $name, min: $min, max: $max, volatile: true, f: $f }
    };
}

pub static FUNCS: &[FuncSpec] = &[
    // math
    f!("SUM", 1, N, sum),
    f!("PRODUCT", 1, N, product),
    f!("AVERAGE", 1, N, average),
    f!("MIN", 1, N, min),
    f!("MAX", 1, N, max),
    f!("COUNT", 1, N, count),
    f!("COUNTA", 1, N, counta),
    f!("COUNTBLANK", 1, 1, countblank),
    f!("ABS", 1, 1, abs),
    f!("ROUND", 2, 2, round),
    f!("ROUNDUP", 2, 2, roundup),
    f!("ROUNDDOWN", 2, 2, rounddown),
    f!("INT", 1, 1, int),
    f!("TRUNC", 1, 2, trunc),
    f!("MOD", 2, 2, modf),
    f!("POWER", 2, 2, power),
    f!("SQRT", 1, 1, sqrt),
    f!("EXP", 1, 1, exp),
    f!("LN", 1, 1, ln),
    f!("LOG", 1, 2, log),
    f!("LOG10", 1, 1, log10),
    f!("PI", 0, 0, pi),
    f!("SIGN", 1, 1, sign),
    f!("SUMPRODUCT", 1, N, sumproduct),
    f!("SUMIF", 2, 3, sumif),
    f!("SUMIFS", 3, N, sumifs),
    f!("COUNTIF", 2, 2, countif),
    f!("COUNTIFS", 2, N, countifs),
    f!("AVERAGEIF", 2, 3, averageif),
    f!("AVERAGEIFS", 3, N, averageifs),
    f!("RAND", 0, 0, rand, volatile),
    f!("RANDBETWEEN", 2, 2, randbetween, volatile),
    // statistics
    f!("MEDIAN", 1, N, median),
    f!("LARGE", 2, 2, large),
    f!("SMALL", 2, 2, small),
    f!("SUBTOTAL", 2, N, subtotal),
    f!("STDEV", 1, N, stdev_s),
    f!("STDEV.S", 1, N, stdev_s),
    f!("STDEV.P", 1, N, stdev_p),
    f!("STDEVP", 1, N, stdev_p),
    f!("VAR", 1, N, var_s),
    f!("VAR.S", 1, N, var_s),
    f!("VAR.P", 1, N, var_p),
    f!("VARP", 1, N, var_p),
    // logic
    f!("IF", 1, 3, iff),
    f!("IFS", 2, N, ifs),
    f!("IFERROR", 2, 2, iferror),
    f!("HYPERLINK", 1, 2, hyperlink),
    f!("IFNA", 2, 2, ifna),
    f!("AND", 1, N, and),
    f!("OR", 1, N, or),
    f!("XOR", 1, N, xor),
    f!("NOT", 1, 1, not),
    f!("TRUE", 0, 0, tru),
    f!("FALSE", 0, 0, fals),
    // information
    f!("FORMULATEXT", 1, 1, formulatext),
    f!("ISBLANK", 1, 1, isblank),
    f!("ISNUMBER", 1, 1, isnumber),
    f!("ISTEXT", 1, 1, istext),
    f!("ISLOGICAL", 1, 1, islogical),
    f!("ISERROR", 1, 1, iserror),
    f!("ISERR", 1, 1, iserr),
    f!("ISNA", 1, 1, isna),
    f!("NA", 0, 0, na),
    f!("ROW", 0, 1, row),
    f!("COLUMN", 0, 1, column),
    f!("ROWS", 1, 1, rows),
    f!("COLUMNS", 1, 1, columns),
    // text
    f!("LEN", 1, 1, len),
    f!("LEFT", 1, 2, left),
    f!("RIGHT", 1, 2, right),
    f!("MID", 3, 3, mid),
    f!("UPPER", 1, 1, upper),
    f!("LOWER", 1, 1, lower),
    f!("TRIM", 1, 1, trim),
    f!("CONCATENATE", 1, N, concatenate),
    f!("CONCAT", 1, N, concat),
    f!("TEXTJOIN", 3, N, textjoin),
    f!("REPT", 2, 2, rept),
    f!("FIND", 2, 3, find),
    f!("SEARCH", 2, 3, search),
    f!("SUBSTITUTE", 3, 4, substitute),
    f!("VALUE", 1, 1, value),
    f!("TEXT", 2, 2, text_fn),
    f!("EXACT", 2, 2, exact),
    f!("TEXTAFTER", 2, 6, textafter),
    // lookup
    f!("CHOOSE", 2, N, choose),
    f!("INDEX", 2, 3, index),
    f!("MATCH", 2, 3, matchf),
    f!("VLOOKUP", 3, 4, vlookup),
    f!("HLOOKUP", 3, 4, hlookup),
    f!("XLOOKUP", 3, 6, xlookup),
    // dates
    f!("DATE", 3, 3, date),
    f!("DATEVALUE", 1, 1, datevalue),
    f!("EOMONTH", 2, 2, eomonth),
    f!("YEAR", 1, 1, year),
    f!("MONTH", 1, 1, month),
    f!("DAY", 1, 1, day),
    f!("WEEKDAY", 1, 2, weekday),
    f!("TODAY", 0, 0, today, volatile),
    f!("NOW", 0, 0, now, volatile),
    // dynamic arrays (arrays.rs)
    f!("SEQUENCE", 1, 4, sequence),
    f!("RANDARRAY", 0, 5, randarray, volatile),
    f!("TRANSPOSE", 1, 1, transpose),
    f!("FILTER", 2, 3, filter),
    f!("SORT", 1, 4, sort),
    f!("SORTBY", 2, N, sortby),
    f!("UNIQUE", 1, 3, unique),
    f!("VSTACK", 1, N, vstack),
    f!("HSTACK", 1, N, hstack),
    f!("TAKE", 2, 3, take),
    f!("DROP", 2, 3, drop),
    f!("CHOOSEROWS", 2, N, chooserows),
    f!("CHOOSECOLS", 2, N, choosecols),
    f!("TOCOL", 1, 3, tocol),
    f!("TOROW", 1, 3, torow),
    f!("WRAPROWS", 2, 3, wraprows),
    f!("WRAPCOLS", 2, 3, wrapcols),
    f!("EXPAND", 2, 4, expand),
    f!("LET", 3, N, let_),
    f!("LAMBDA", 1, N, lambda),
    f!("MAP", 2, N, map),
    f!("REDUCE", 2, 3, reduce),
    f!("SCAN", 2, 3, scan),
    f!("BYROW", 2, 2, byrow),
    f!("BYCOL", 2, 2, bycol),
    f!("MAKEARRAY", 3, 3, makearray),
    f!("ISOMITTED", 1, 1, isomitted),
    // the second batch (more.rs), after the first so function ids stay put
    f!("CEILING", 2, 2, ceiling),
    f!("FLOOR", 2, 2, floor),
    f!("CEILING.MATH", 1, 3, ceiling_math),
    f!("FLOOR.MATH", 1, 3, floor_math),
    f!("MROUND", 2, 2, mround),
    f!("QUOTIENT", 2, 2, quotient),
    f!("EVEN", 1, 1, even),
    f!("ODD", 1, 1, odd),
    f!("FACT", 1, 1, fact),
    f!("COMBIN", 2, 2, combin),
    f!("GCD", 1, N, gcd),
    f!("LCM", 1, N, lcm),
    f!("SUMSQ", 1, N, sumsq),
    f!("RADIANS", 1, 1, radians),
    f!("DEGREES", 1, 1, degrees),
    f!("SIN", 1, 1, sin),
    f!("COS", 1, 1, cos),
    f!("TAN", 1, 1, tan),
    f!("ASIN", 1, 1, asin),
    f!("ACOS", 1, 1, acos),
    f!("ATAN", 1, 1, atan),
    f!("ATAN2", 2, 2, atan2),
    f!("TIME", 3, 3, time),
    f!("HOUR", 1, 1, hour),
    f!("MINUTE", 1, 1, minute),
    f!("SECOND", 1, 1, second),
    f!("EDATE", 2, 2, edate),
    f!("DAYS", 2, 2, days),
    f!("DATEDIF", 3, 3, datedif),
    f!("NETWORKDAYS", 2, 3, networkdays),
    f!("WORKDAY", 2, 3, workday),
    f!("WEEKNUM", 1, 2, weeknum),
    f!("ISOWEEKNUM", 1, 1, isoweeknum),
    f!("PROPER", 1, 1, proper),
    f!("CHAR", 1, 1, char_fn),
    f!("CODE", 1, 1, code),
    f!("UNICHAR", 1, 1, unichar),
    f!("UNICODE", 1, 1, unicode),
    f!("CLEAN", 1, 1, clean),
    f!("SWITCH", 3, N, switch),
    f!("ISEVEN", 1, 1, iseven),
    f!("ISODD", 1, 1, isodd),
    f!("N", 1, 1, n_fn),
    f!("T", 1, 1, t_fn),
    // the third batch (more.rs, fin.rs)
    f!("MAXIFS", 3, N, maxifs),
    f!("MINIFS", 3, N, minifs),
    f!("RANK", 2, 3, rank_eq),
    f!("RANK.EQ", 2, 3, rank_eq),
    f!("RANK.AVG", 2, 3, rank_avg),
    f!("PERCENTILE", 2, 2, percentile_inc),
    f!("PERCENTILE.INC", 2, 2, percentile_inc),
    f!("PERCENTILE.EXC", 2, 2, percentile_exc),
    f!("QUARTILE", 2, 2, quartile_inc),
    f!("QUARTILE.INC", 2, 2, quartile_inc),
    f!("QUARTILE.EXC", 2, 2, quartile_exc),
    f!("MODE", 1, N, mode_sngl),
    f!("MODE.SNGL", 1, N, mode_sngl),
    f!("XMATCH", 2, 4, xmatch),
    f!("PMT", 3, 5, pmt),
    f!("IPMT", 4, 6, ipmt),
    f!("PPMT", 4, 6, ppmt),
    f!("FV", 3, 5, fv),
    f!("PV", 3, 5, pv),
    f!("NPER", 3, 5, nper),
    f!("RATE", 3, 6, rate),
    f!("NPV", 2, N, npv),
    f!("IRR", 1, 2, irr),
    // the fourth batch
    f!("TEXTBEFORE", 2, 6, textbefore),
    f!("TEXTSPLIT", 2, 6, textsplit),
    f!("FIXED", 1, 3, fixed),
    f!("LOOKUP", 2, 3, lookup_fn),
    f!("ADDRESS", 2, 5, address),
    f!("ISREF", 1, 1, isref),
    f!("YEARFRAC", 2, 3, yearfrac),
    f!("SLN", 3, 3, sln),
    f!("SYD", 4, 4, syd),
    f!("DDB", 4, 5, ddb),
    f!("DB", 4, 5, db),
    // the fifth batch
    f!("INDIRECT", 1, 2, indirect, volatile),
    f!("OFFSET", 3, 5, offset, volatile),
    f!("AVEDEV", 1, N, avedev),
    f!("DEVSQ", 1, N, devsq),
    f!("GEOMEAN", 1, N, geomean),
    f!("HARMEAN", 1, N, harmean),
    f!("STANDARDIZE", 3, 3, standardize),
    f!("CORREL", 2, 2, correl),
    f!("PEARSON", 2, 2, correl),
    f!("RSQ", 2, 2, rsq),
    f!("SLOPE", 2, 2, slope),
    f!("INTERCEPT", 2, 2, intercept),
    f!("FORECAST", 3, 3, forecast),
    f!("FORECAST.LINEAR", 3, 3, forecast),
    f!("STEYX", 2, 2, steyx),
    f!("COVAR", 2, 2, covariance_p),
    f!("COVARIANCE.P", 2, 2, covariance_p),
    f!("COVARIANCE.S", 2, 2, covariance_s),
    f!("MODE.MULT", 1, N, mode_mult),
    f!("MAXA", 1, N, maxa),
    f!("MINA", 1, N, mina),
    f!("AVERAGEA", 1, N, averagea),
    f!("SINH", 1, 1, sinh),
    f!("COSH", 1, 1, cosh),
    f!("TANH", 1, 1, tanh),
    f!("ASINH", 1, 1, asinh),
    f!("ACOSH", 1, 1, acosh),
    f!("ATANH", 1, 1, atanh),
    f!("REPLACE", 4, 4, replace),
    f!("DOLLAR", 1, 2, dollar),
    f!("NUMBERVALUE", 1, 3, numbervalue),
    f!("ROMAN", 1, 2, roman),
    f!("ARABIC", 1, 1, arabic),
    f!("BASE", 2, 3, base),
    f!("DECIMAL", 2, 2, decimal),
    f!("BIN2DEC", 1, 1, bin2dec),
    f!("BIN2HEX", 1, 2, bin2hex),
    f!("BIN2OCT", 1, 2, bin2oct),
    f!("DEC2BIN", 1, 2, dec2bin),
    f!("DEC2HEX", 1, 2, dec2hex),
    f!("DEC2OCT", 1, 2, dec2oct),
    f!("HEX2BIN", 1, 2, hex2bin),
    f!("HEX2DEC", 1, 1, hex2dec),
    f!("HEX2OCT", 1, 2, hex2oct),
    f!("OCT2BIN", 1, 2, oct2bin),
    f!("OCT2DEC", 1, 1, oct2dec),
    f!("OCT2HEX", 1, 2, oct2hex),
    f!("TYPE", 1, 1, type_fn),
    f!("ERROR.TYPE", 1, 1, error_type),
    f!("SHEET", 0, 1, sheet),
    f!("SHEETS", 0, 1, sheets),
    f!("ISFORMULA", 1, 1, isformula),
    f!("ISNONTEXT", 1, 1, isnontext),
    // the sixth batch (dist.rs, linest.rs)
    f!("NORM.DIST", 4, 4, norm_dist),
    f!("NORMDIST", 4, 4, norm_dist),
    f!("NORM.INV", 3, 3, norm_inv),
    f!("NORMINV", 3, 3, norm_inv),
    f!("NORM.S.DIST", 2, 2, norm_s_dist),
    f!("NORMSDIST", 1, 1, norm_s_dist),
    f!("NORM.S.INV", 1, 1, norm_s_inv),
    f!("NORMSINV", 1, 1, norm_s_inv),
    f!("PHI", 1, 1, phi),
    f!("GAUSS", 1, 1, gauss),
    f!("LOGNORM.DIST", 4, 4, lognorm_dist),
    f!("LOGNORMDIST", 3, 3, lognorm_dist),
    f!("LOGNORM.INV", 3, 3, lognorm_inv),
    f!("LOGINV", 3, 3, lognorm_inv),
    f!("T.DIST", 3, 3, t_dist),
    f!("T.DIST.2T", 2, 2, t_dist_2t),
    f!("T.DIST.RT", 2, 2, t_dist_rt),
    f!("TDIST", 3, 3, tdist),
    f!("T.INV", 2, 2, t_inv_fn),
    f!("T.INV.2T", 2, 2, t_inv_2t),
    f!("TINV", 2, 2, t_inv_2t),
    f!("CHISQ.DIST", 3, 3, chisq_dist),
    f!("CHISQ.DIST.RT", 2, 2, chisq_dist_rt),
    f!("CHIDIST", 2, 2, chisq_dist_rt),
    f!("CHISQ.INV", 2, 2, chisq_inv),
    f!("CHISQ.INV.RT", 2, 2, chisq_inv_rt),
    f!("CHIINV", 2, 2, chisq_inv_rt),
    f!("F.DIST", 4, 4, f_dist),
    f!("F.DIST.RT", 3, 3, f_dist_rt),
    f!("FDIST", 3, 3, f_dist_rt),
    f!("F.INV", 3, 3, f_inv),
    f!("F.INV.RT", 3, 3, f_inv_rt),
    f!("FINV", 3, 3, f_inv_rt),
    f!("BINOM.DIST", 4, 4, binom_dist),
    f!("BINOMDIST", 4, 4, binom_dist),
    f!("BINOM.DIST.RANGE", 3, 4, binom_dist_range),
    f!("BINOM.INV", 3, 3, binom_inv),
    f!("CRITBINOM", 3, 3, binom_inv),
    f!("NEGBINOM.DIST", 4, 4, negbinom_dist),
    f!("NEGBINOMDIST", 3, 3, negbinom_dist),
    f!("POISSON.DIST", 3, 3, poisson),
    f!("POISSON", 3, 3, poisson),
    f!("HYPGEOM.DIST", 5, 5, hypgeom_dist),
    f!("HYPGEOMDIST", 4, 4, hypgeom_dist),
    f!("EXPON.DIST", 3, 3, expon_dist),
    f!("EXPONDIST", 3, 3, expon_dist),
    f!("GAMMA.DIST", 4, 4, gamma_dist),
    f!("GAMMADIST", 4, 4, gamma_dist),
    f!("GAMMA.INV", 3, 3, gamma_inv),
    f!("GAMMAINV", 3, 3, gamma_inv),
    f!("GAMMA", 1, 1, gamma_fn),
    f!("GAMMALN", 1, 1, gammaln),
    f!("GAMMALN.PRECISE", 1, 1, gammaln),
    f!("BETA.DIST", 4, 6, beta_dist_new),
    f!("BETADIST", 3, 5, betadist),
    f!("BETA.INV", 3, 5, beta_inv),
    f!("BETAINV", 3, 5, beta_inv),
    f!("WEIBULL.DIST", 4, 4, weibull),
    f!("WEIBULL", 4, 4, weibull),
    f!("ERF", 1, 2, erf),
    f!("ERF.PRECISE", 1, 1, erf_precise),
    f!("ERFC", 1, 1, erfc),
    f!("ERFC.PRECISE", 1, 1, erfc),
    f!("FISHER", 1, 1, fisher),
    f!("FISHERINV", 1, 1, fisherinv),
    f!("CONFIDENCE.NORM", 3, 3, confidence_norm),
    f!("CONFIDENCE", 3, 3, confidence_norm),
    f!("CONFIDENCE.T", 3, 3, confidence_t),
    f!("T.TEST", 4, 4, t_test),
    f!("TTEST", 4, 4, t_test),
    f!("Z.TEST", 2, 3, z_test),
    f!("ZTEST", 2, 3, z_test),
    f!("CHISQ.TEST", 2, 2, chisq_test),
    f!("CHITEST", 2, 2, chisq_test),
    f!("F.TEST", 2, 2, f_test),
    f!("FTEST", 2, 2, f_test),
    f!("LINEST", 1, 4, linest),
    f!("LOGEST", 1, 4, logest),
    f!("TREND", 1, 4, trend),
    f!("GROWTH", 1, 4, growth),
    // the seventh batch (bonds.rs, fin.rs, conv.rs)
    f!("COUPPCD", 3, 4, couppcd),
    f!("COUPNCD", 3, 4, coupncd),
    f!("COUPNUM", 3, 4, coupnum),
    f!("COUPDAYS", 3, 4, coupdays),
    f!("COUPDAYBS", 3, 4, coupdaybs),
    f!("COUPDAYSNC", 3, 4, coupdaysnc),
    f!("PRICE", 6, 7, price),
    f!("YIELD", 6, 7, yield_fn),
    f!("DURATION", 5, 6, duration),
    f!("MDURATION", 5, 6, mduration),
    f!("ACCRINT", 6, 8, accrint),
    f!("ACCRINTM", 4, 5, accrintm),
    f!("DISC", 4, 5, disc),
    f!("PRICEDISC", 4, 5, pricedisc),
    f!("YIELDDISC", 4, 5, yielddisc),
    f!("RECEIVED", 4, 5, received),
    f!("INTRATE", 4, 5, intrate),
    f!("PRICEMAT", 5, 6, pricemat),
    f!("YIELDMAT", 5, 6, yieldmat),
    f!("TBILLPRICE", 3, 3, tbillprice),
    f!("TBILLYIELD", 3, 3, tbillyield),
    f!("TBILLEQ", 3, 3, tbilleq),
    f!("XNPV", 3, 3, xnpv),
    f!("XIRR", 2, 3, xirr),
    f!("MIRR", 3, 3, mirr),
    f!("CUMIPMT", 6, 6, cumipmt),
    f!("CUMPRINC", 6, 6, cumprinc),
    f!("EFFECT", 2, 2, effect),
    f!("NOMINAL", 2, 2, nominal),
    f!("FVSCHEDULE", 2, 2, fvschedule),
    f!("PDURATION", 3, 3, pduration),
    f!("RRI", 3, 3, rri),
    f!("ISPMT", 4, 4, ispmt),
    f!("DOLLARDE", 2, 2, dollarde),
    f!("DOLLARFR", 2, 2, dollarfr),
    f!("VDB", 5, 7, vdb),
    f!("CELL", 1, 2, cell),
    f!("INFO", 1, 1, info, volatile),
];

/// The id of a function by its upper-case name.
pub fn lookup(name: &str) -> Option<u16> {
    static MAP: OnceLock<HashMap<&'static str, u16>> = OnceLock::new();
    MAP.get_or_init(|| FUNCS.iter().enumerate().map(|(i, f)| (f.name, i as u16)).collect()).get(name).copied()
}

// ---------- helpers ----------

fn v(x: Value) -> Ev {
    Ev::V(x)
}
fn e(k: ErrorKind) -> Ev {
    Ev::err(k)
}
fn n(x: f64) -> Ev {
    Ev::V(Value::num(x))
}
fn missing(a: &Expr) -> bool {
    matches!(a, Expr::Name(n) if n.is_empty())
}

macro_rules! tri {
    ($x:expr) => {
        match $x {
            Ok(v) => v,
            Err(k) => return Ev::err(k),
        }
    };
}

fn num(c: &Ctx, a: &Expr) -> Result<f64, ErrorKind> {
    c.number(&c.scalar(a))
}
fn text(c: &Ctx, a: &Expr) -> Result<Arc<str>, ErrorKind> {
    to_text(&c.scalar(a))
}
fn boolean(c: &Ctx, a: &Expr) -> Result<bool, ErrorKind> {
    to_bool(&c.scalar(a))
}
fn opt_num(c: &Ctx, args: &[Expr], i: usize, default: f64) -> Result<f64, ErrorKind> {
    match args.get(i) {
        Some(a) if !missing(a) => num(c, a),
        _ => Ok(default),
    }
}

/// Visits the numbers in `args` as SUM does: in ranges and arrays only numbers count
/// (text and booleans are skipped); direct arguments are coerced (`SUM("3", TRUE)` is 4).
/// The first error stops it.
pub(super) fn each_num(c: &Ctx, args: &[Expr], f: &mut dyn FnMut(f64)) -> Result<(), ErrorKind> {
    each_num_slices(c, args, &mut |xs| xs.iter().for_each(|&x| f(x)))
}

/// The numbers of SUM-like arguments, in slices where storage allows: direct numbers,
/// text that reads as a number and booleans count; in ranges only numbers do, and the
/// first error in either stops it.
fn each_num_slices(c: &Ctx, args: &[Expr], f: &mut dyn FnMut(&[f64])) -> Result<(), ErrorKind> {
    for a in args {
        if missing(a) {
            continue;
        }
        match c.eval(a) {
            Ev::F(_) => return Err(ErrorKind::Calc),
            Ev::V(Value::Empty) => {}
            Ev::V(x) => f(&[c.number(&x)?]),
            Ev::R(r) => {
                let mut err = None;
                if let Some(cl) = c.clip(&r) {
                    let f = std::cell::RefCell::new(&mut *f);
                    c.wb.each_num_run(&cl, &mut |xs| (f.borrow_mut())(xs), &mut |_, _, x| match x {
                        crate::column::Cell::Num(y) => (f.borrow_mut())(&[y]),
                        crate::column::Cell::Err(k) if err.is_none() => err = Some(k),
                        _ => {}
                    });
                }
                if let Some(k) = err {
                    return Err(k);
                }
            }
            Ev::A(arr) => {
                for x in arr.vals.iter() {
                    match x {
                        Value::Num(y) => f(&[*y]),
                        Value::Err(k) => return Err(*k),
                        _ => {}
                    }
                }
            }
        }
    }
    Ok(())
}

fn collect(c: &Ctx, args: &[Expr]) -> Result<Vec<f64>, ErrorKind> {
    let mut v = Vec::new();
    each_num(c, args, &mut |x| v.push(x))?;
    Ok(v)
}

/// Every value in `args`, including empty cells inside ranges when `with_empty`.
fn each_value(c: &Ctx, args: &[Expr], f: &mut dyn FnMut(&Value)) {
    for a in args {
        match c.eval(a) {
            Ev::F(_) => f(&Value::Err(ErrorKind::Calc)),
            Ev::V(x) => f(&x),
            Ev::R(r) => {
                if let Some(cl) = c.clip(&r) {
                    c.wb.each_rowmajor(&cl, &mut |_, _, x| f(&x));
                }
            }
            Ev::A(arr) => arr.vals.iter().for_each(&mut *f),
        }
    }
}

// ---------- math ----------

fn sum(c: &Ctx, a: &[Expr]) -> Ev {
    let mut s = 0.0;
    tri!(each_num_slices(c, a, &mut |xs| s += xs.iter().sum::<f64>()));
    n(s)
}
fn product(c: &Ctx, a: &[Expr]) -> Ev {
    let mut p = 1.0;
    let mut any = false;
    tri!(each_num(c, a, &mut |x| {
        p *= x;
        any = true
    }));
    n(if any { p } else { 0.0 })
}
fn average(c: &Ctx, a: &[Expr]) -> Ev {
    let (mut s, mut k) = (0.0, 0u64);
    tri!(each_num_slices(c, a, &mut |xs| {
        s += xs.iter().sum::<f64>();
        k += xs.len() as u64
    }));
    if k == 0 {
        e(ErrorKind::Div0)
    } else {
        n(s / k as f64)
    }
}
fn min(c: &Ctx, a: &[Expr]) -> Ev {
    let mut m = f64::INFINITY;
    tri!(each_num(c, a, &mut |x| m = m.min(x)));
    n(if m.is_finite() { m } else { 0.0 })
}
fn max(c: &Ctx, a: &[Expr]) -> Ev {
    let mut m = f64::NEG_INFINITY;
    tri!(each_num(c, a, &mut |x| m = m.max(x)));
    n(if m.is_finite() { m } else { 0.0 })
}
fn count(c: &Ctx, a: &[Expr]) -> Ev {
    let mut k = 0u64;
    for x in a {
        match c.eval(x) {
            Ev::F(_) => {}
            Ev::V(val) => {
                if matches!(val, Value::Num(_) | Value::Bool(_)) || matches!(&val, Value::Str(s) if c.numeric_text(s).is_some()) {
                    k += 1
                }
            }
            Ev::R(r) => {
                if let Some(cl) = c.clip(&r) {
                    c.wb.each(&cl, &mut |_, _, val| {
                        if matches!(val, Value::Num(_)) {
                            k += 1
                        }
                    })
                }
            }
            Ev::A(arr) => k += arr.vals.iter().filter(|x| matches!(x, Value::Num(_))).count() as u64,
        }
    }
    n(k as f64)
}
fn counta(c: &Ctx, a: &[Expr]) -> Ev {
    let mut k = 0u64;
    for x in a {
        if missing(x) {
            continue;
        }
        match c.eval(x) {
            Ev::F(_) => k += 1,
            Ev::V(_) => k += 1,
            Ev::R(r) => {
                if let Some(cl) = c.clip(&r) {
                    c.wb.each(&cl, &mut |_, _, _| k += 1)
                }
            }
            Ev::A(arr) => k += arr.vals.iter().filter(|x| !x.is_empty()).count() as u64,
        }
    }
    n(k as f64)
}
fn countblank(c: &Ctx, a: &[Expr]) -> Ev {
    match c.eval(&a[0]) {
        Ev::R(r) => {
            let mut filled = 0u64;
            if let Some(cl) = c.clip(&r) {
                c.wb.each(&cl, &mut |_, _, x| {
                    if !matches!(&x, Value::Str(s) if s.is_empty()) {
                        filled += 1
                    }
                });
            }
            n((r.cells() - filled) as f64)
        }
        _ => e(ErrorKind::Value),
    }
}
fn abs(c: &Ctx, a: &[Expr]) -> Ev {
    n(tri!(num(c, &a[0])).abs())
}

/// Rounds `x` to `d` decimals on its 15-significant-digit decimal form, as Excel does
/// (so ROUND(2.675, 2) is 2.68, not 2.67). `mode`: 0 half away from zero, 1 away from
/// zero (ROUNDUP), -1 toward zero (ROUNDDOWN, TRUNC).
pub fn round_dec(x: f64, d: i32, mode: i8) -> f64 {
    if x == 0.0 || !x.is_finite() {
        return x;
    }
    let s = format!("{:.14e}", x.abs());
    let (m, ex) = s.split_once('e').unwrap();
    let ex: i64 = ex.parse().unwrap();
    let mut digits: Vec<u8> = m.bytes().filter(|b| b.is_ascii_digit()).map(|b| b - b'0').collect();
    // value = 0.D1 D2 ... x 10^(ex+1); keep k leading digits for d decimals.
    let k = ex + 1 + d.clamp(-400, 400) as i64;
    let unit_exp = -(d as i64); // the value of one unit in the last kept place
    let result = if k >= digits.len() as i64 {
        return x;
    } else if k < 0 {
        if mode == 1 { format!("1e{}", unit_exp) } else { "0".into() }
    } else {
        let k = k as usize;
        let rest = &digits[k..];
        let up = match mode {
            0 => rest[0] >= 5,
            1 => rest.iter().any(|&b| b != 0),
            _ => false,
        };
        digits.truncate(k);
        if up {
            let mut i = digits.len();
            loop {
                if i == 0 {
                    digits.insert(0, 1);
                    break;
                }
                i -= 1;
                if digits[i] == 9 {
                    digits[i] = 0;
                } else {
                    digits[i] += 1;
                    break;
                }
            }
        }
        if digits.is_empty() {
            "0".into()
        } else {
            let t: String = digits.iter().map(|b| (b'0' + b) as char).collect();
            format!("{}e{}", t, unit_exp)
        }
    };
    let r: f64 = result.parse().unwrap_or(0.0);
    if x < 0.0 {
        -r
    } else {
        r
    }
}

fn round_with(c: &Ctx, a: &[Expr], mode: i8) -> Ev {
    let x = tri!(num(c, &a[0]));
    let d = tri!(num(c, &a[1])).trunc();
    n(round_dec(x, d as i32, mode))
}
fn round(c: &Ctx, a: &[Expr]) -> Ev {
    round_with(c, a, 0)
}
fn roundup(c: &Ctx, a: &[Expr]) -> Ev {
    round_with(c, a, 1)
}
fn rounddown(c: &Ctx, a: &[Expr]) -> Ev {
    round_with(c, a, -1)
}
fn int(c: &Ctx, a: &[Expr]) -> Ev {
    n(tri!(num(c, &a[0])).floor())
}
fn trunc(c: &Ctx, a: &[Expr]) -> Ev {
    let x = tri!(num(c, &a[0]));
    let d = tri!(opt_num(c, a, 1, 0.0)).trunc();
    n(round_dec(x, d as i32, -1))
}
fn modf(c: &Ctx, a: &[Expr]) -> Ev {
    let x = tri!(num(c, &a[0]));
    let d = tri!(num(c, &a[1]));
    if d == 0.0 {
        return e(ErrorKind::Div0);
    }
    n(x - d * (x / d).floor())
}
fn power(c: &Ctx, a: &[Expr]) -> Ev {
    let x = tri!(num(c, &a[0]));
    let y = tri!(num(c, &a[1]));
    v(super::arith(crate::formula::BinOp::Pow, x, y))
}
fn sqrt(c: &Ctx, a: &[Expr]) -> Ev {
    let x = tri!(num(c, &a[0]));
    if x < 0.0 {
        e(ErrorKind::Num)
    } else {
        n(x.sqrt())
    }
}
fn exp(c: &Ctx, a: &[Expr]) -> Ev {
    n(tri!(num(c, &a[0])).exp())
}
fn ln(c: &Ctx, a: &[Expr]) -> Ev {
    let x = tri!(num(c, &a[0]));
    if x <= 0.0 {
        e(ErrorKind::Num)
    } else {
        n(x.ln())
    }
}
fn log(c: &Ctx, a: &[Expr]) -> Ev {
    let x = tri!(num(c, &a[0]));
    let b = tri!(opt_num(c, a, 1, 10.0));
    if x <= 0.0 || b <= 0.0 {
        return e(ErrorKind::Num);
    }
    if b == 1.0 {
        return e(ErrorKind::Div0);
    }
    n(x.ln() / b.ln())
}
fn log10(c: &Ctx, a: &[Expr]) -> Ev {
    let x = tri!(num(c, &a[0]));
    if x <= 0.0 {
        e(ErrorKind::Num)
    } else {
        n(x.log10())
    }
}
fn pi(_: &Ctx, _: &[Expr]) -> Ev {
    n(std::f64::consts::PI)
}
fn sign(c: &Ctx, a: &[Expr]) -> Ev {
    let x = tri!(num(c, &a[0]));
    n(if x > 0.0 {
        1.0
    } else if x < 0.0 {
        -1.0
    } else {
        0.0
    })
}
fn sumproduct(c: &Ctx, a: &[Expr]) -> Ev {
    let mut arrs = Vec::new();
    for x in a {
        arrs.push(tri!(c.to_array(c.eval(x))));
    }
    let (rows, cols) = (arrs[0].rows, arrs[0].cols);
    if arrs.iter().any(|x| x.rows != rows || x.cols != cols) {
        return e(ErrorKind::Value);
    }
    let mut s = 0.0;
    for i in 0..rows * cols {
        let mut p = 1.0;
        for arr in &arrs {
            match &arr.vals[i] {
                Value::Num(y) => p *= y,
                Value::Err(k) => return e(*k),
                _ => p = 0.0,
            }
        }
        s += p;
    }
    n(s)
}

pub(super) fn rect_arg(c: &Ctx, a: &Expr) -> Result<Rect, ErrorKind> {
    match c.eval(a) {
        Ev::R(r) => Ok(r),
        Ev::V(Value::Err(k)) => Err(k),
        _ => Err(ErrorKind::Value),
    }
}

/// SUMIF and AVERAGEIF use the optional range's top-left cell and the
/// criteria range's size. The -IFS forms require equal sizes.
fn paired_range(c: &Ctx, src: Rect, args: &[Expr]) -> Result<Rect, ErrorKind> {
    use crate::addr::{MAX_COLS, MAX_ROWS};
    let Some(target) = args.get(2).filter(|a| !missing(a)) else { return Ok(src) };
    let target = rect_arg(c, target)?;
    let r1 = target.r0.checked_add(src.r1 - src.r0).filter(|&end| end < MAX_ROWS).ok_or(ErrorKind::Ref)?;
    let c1 = target.c0.checked_add(src.c1 - src.c0).filter(|&end| end < MAX_COLS).ok_or(ErrorKind::Ref)?;
    Ok(Rect { r1, c1, ..target })
}

/// Empty cells in an explicit bounded range can match even outside its
/// sheet's used extent. Keep the used-area bound for very large ranges.
fn blank_extent(c: &Ctx, range: &Rect) -> Option<Rect> {
    if range.cells() <= super::MAX_ARRAY { Some(*range) } else { c.clip(range) }
}

/// Ordered criteria on one range, with equality criteria on any others:
/// answered from that range's numbers sorted once (see [`ifcache::lookup_sorted`]).
fn sorted_if(c: &Ctx, value_range: Option<Rect>, crits: &[(Rect, Criterion)], sum: bool) -> Option<ifcache::Agg> {
    let mut on: Option<Rect> = None;
    let mut bounds = Vec::new();
    let (mut eq_rects, mut keys) = (Vec::new(), Vec::new());
    for (r, cr) in crits {
        if let Some(b) = cr.bound() {
            if on.is_some_and(|o| o != *r) {
                return None;
            }
            on = Some(*r);
            bounds.push(b);
        } else {
            keys.push(cr.eq_key()?);
            eq_rects.push(*r);
        }
    }
    let on = on?;
    let shape = (std::iter::once(on).chain(eq_rects.iter().copied()).collect(), value_range);
    ifcache::lookup_sorted(shape, &keys, &bounds, sum, |emit| {
        let Some(cl) = c.clip(&on) else { return true };
        c.wb.each(&cl, &mut |row, col, x| {
            let Value::Num(x) = x else { return };
            let (dr, dc) = (row - on.r0, col - on.c0);
            let mut ks = Vec::with_capacity(eq_rects.len());
            for r in &eq_rects {
                let Some(k) = Key::of(&c.wb.value(r.sheet, r.r0 + dr, r.c0 + dc)) else { return };
                ks.push(k);
            }
            let v = value_range.map(|vr| c.wb.value(vr.sheet, vr.r0 + dr, vr.c0 + dc));
            emit(ks, x, v);
        });
        true
    })
}

fn multi_if(c: &Ctx, value_range: Option<Rect>, pairs_args: &[Expr]) -> Result<ifcache::Agg, ErrorKind> {
    let mut crits = Vec::new();
    for p in pairs_args.chunks(2) {
        if p.len() < 2 {
            return Err(ErrorKind::Value);
        }
        let r = rect_arg(c, &p[0])?;
        crits.push((r, Criterion::new(&c.scalar(&p[1]))));
    }
    matching(c, value_range, &crits)
}

/// Counts and totals only the rows that pass every criterion.
fn matching(c: &Ctx, value_range: Option<Rect>, crits: &[(Rect, Criterion)]) -> Result<ifcache::Agg, ErrorKind> {
    matching_with(c, value_range, crits, false)
}

/// MAXIFS and MINIFS: the matching cells' largest and smallest numbers, so
/// not from the sorted index, which keeps only running totals.
pub(super) fn matching_extremes(c: &Ctx, value_range: Rect, pairs_args: &[Expr]) -> Result<ifcache::Agg, ErrorKind> {
    let mut crits = Vec::new();
    for p in pairs_args.chunks(2) {
        if p.len() < 2 {
            return Err(ErrorKind::Value);
        }
        crits.push((rect_arg(c, &p[0])?, Criterion::new(&c.scalar(&p[1]))));
    }
    matching_with(c, Some(value_range), &crits, true)
}

fn matching_with(c: &Ctx, value_range: Option<Rect>, crits: &[(Rect, Criterion)], extremes: bool) -> Result<ifcache::Agg, ErrorKind> {
    let (first, fc) = &crits[0];
    if crits.iter().any(|(r, _)| r.rows() != first.rows() || r.cols() != first.cols()) {
        return Err(ErrorKind::Value);
    }
    if let Some(vr) = &value_range {
        if vr.rows() != first.rows() || vr.cols() != first.cols() {
            return Err(ErrorKind::Value);
        }
    }
    if let Some(keys) = crits.iter().map(|(_, cr)| cr.eq_key()).collect::<Option<Vec<Key>>>().filter(|_| first.c0 == first.c1) {
        let shape = (crits.iter().map(|(r, _)| *r).collect(), value_range);
        let built = ifcache::lookup(shape, &keys, |emit| {
            let mut walk = |row: u32, col: u32, x: &Value| {
                let Some(k0) = Key::of(x) else { return };
                let (dr, dc) = (row - first.r0, col - first.c0);
                let mut ks = Vec::with_capacity(crits.len());
                ks.push(k0);
                for (r, _) in &crits[1..] {
                    let Some(k) = Key::of(&c.wb.value(r.sheet, r.r0 + dr, r.c0 + dc)) else { return };
                    ks.push(k);
                }
                let v = value_range.map(|vr| c.wb.value(vr.sheet, vr.r0 + dr, vr.c0 + dc));
                emit(ks, v);
            };
            if let Some(cl) = blank_extent(c, first) {
                for row in cl.r0..=cl.r1 {
                    for col in cl.c0..=cl.c1 {
                        walk(row, col, &c.wb.value(first.sheet, row, col));
                    }
                }
            }
            true
        });
        if let Some(agg) = built { return Ok(agg); }
    }
    if !extremes {
        if let Some(a) = sorted_if(c, value_range, crits, value_range.is_some()) {
            return Ok(a);
        }
    }
    let with_empty = fc.matches_empty();
    let mut agg = ifcache::Agg::default();
    let mut visit = |row: u32, col: u32, x: &Value| {
        if !fc.test(x) {
            return;
        }
        let (dr, dc) = (row - first.r0, col - first.c0);
        for (r, cr) in &crits[1..] {
            if !cr.test(&c.wb.value(r.sheet, r.r0 + dr, r.c0 + dc)) {
                return;
            }
        }
        agg.add(value_range.map(|vr| c.wb.value(vr.sheet, vr.r0 + dr, vr.c0 + dc)));
    };
    if with_empty {
        if let Some(cl) = blank_extent(c, first) {
            for row in cl.r0..=cl.r1 {
                for col in cl.c0..=cl.c1 {
                    visit(row, col, &c.wb.value(first.sheet, row, col));
                }
            }
        }
    } else if let Some(cl) = c.clip(first) {
        c.wb.each(&cl, &mut |row, col, x| visit(row, col, &x));
    }
    Ok(agg)
}

fn conditional_result(agg: ifcache::Agg, average: bool) -> Ev {
    if let Some(error) = agg.error { return e(error); }
    if !average { return n(agg.sum); }
    if agg.nums == 0 { return e(ErrorKind::Div0); }
    n(agg.sum / agg.nums as f64)
}

fn single_if(c: &Ctx, a: &[Expr], average: bool) -> Ev {
    let src = tri!(rect_arg(c, &a[0]));
    let range = tri!(paired_range(c, src, a));
    let crit = Criterion::new(&c.scalar(&a[1]));
    conditional_result(tri!(matching(c, Some(range), &[(src, crit)])), average)
}

fn sumif(c: &Ctx, a: &[Expr]) -> Ev {
    single_if(c, a, false)
}
fn sumifs(c: &Ctx, a: &[Expr]) -> Ev {
    let range = tri!(rect_arg(c, &a[0]));
    conditional_result(tri!(multi_if(c, Some(range), &a[1..])), false)
}
fn countif(c: &Ctx, a: &[Expr]) -> Ev {
    countifs(c, a)
}
fn countifs(c: &Ctx, a: &[Expr]) -> Ev {
    n(tri!(multi_if(c, None, a)).count as f64)
}
fn averageif(c: &Ctx, a: &[Expr]) -> Ev {
    single_if(c, a, true)
}
fn averageifs(c: &Ctx, a: &[Expr]) -> Ev {
    let range = tri!(rect_arg(c, &a[0]));
    conditional_result(tri!(multi_if(c, Some(range), &a[1..])), true)
}

pub(super) fn rng() -> f64 {
    use std::cell::Cell;
    thread_local! {
        static S: Cell<u64> = Cell::new({
            let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1);
            t ^ (&t as *const u64 as u64).rotate_left(17) | 1
        });
    }
    S.with(|s| {
        let mut x = s.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.set(x);
        (x >> 11) as f64 / (1u64 << 53) as f64
    })
}
fn rand(_: &Ctx, _: &[Expr]) -> Ev {
    n(rng())
}
fn randbetween(c: &Ctx, a: &[Expr]) -> Ev {
    let lo = tri!(num(c, &a[0])).ceil();
    let hi = tri!(num(c, &a[1])).floor();
    if lo > hi {
        return e(ErrorKind::Num);
    }
    n(lo + (rng() * (hi - lo + 1.0)).floor())
}

// ---------- statistics ----------

fn median(c: &Ctx, a: &[Expr]) -> Ev {
    let mut v = tri!(collect(c, a));
    if v.is_empty() {
        return e(ErrorKind::Num);
    }
    v.sort_by(|x, y| x.partial_cmp(y).unwrap());
    let m = v.len() / 2;
    n(if v.len() % 2 == 1 { v[m] } else { (v[m - 1] + v[m]) / 2.0 })
}
fn large(c: &Ctx, a: &[Expr]) -> Ev {
    select_rank(c, a, true)
}

fn small(c: &Ctx, a: &[Expr]) -> Ev {
    select_rank(c, a, false)
}

fn select_rank(c: &Ctx, a: &[Expr], descending: bool) -> Ev {
    let mut values = Vec::new();
    let mut overflow = false;
    tri!(each_num(c, &a[..1], &mut |x| {
        if values.len() < super::MAX_ARRAY as usize {
            values.push(x);
        } else {
            overflow = true;
        }
    }));
    if overflow {
        return e(ErrorKind::Limit);
    }
    let rank = tri!(num(c, &a[1]));
    if !(1.0..=values.len() as f64).contains(&rank) {
        return e(ErrorKind::Num);
    }
    let index = if descending {
        values.len() - rank.ceil() as usize
    } else {
        rank.trunc() as usize - 1
    };
    n(*values.select_nth_unstable_by(index, f64::total_cmp).1)
}

fn subtotal(c: &Ctx, a: &[Expr]) -> Ev {
    let code = tri!(num(c, &a[0])).trunc() as u16;
    let skip_hidden = match code {
        1..=11 => false,
        101..=111 => true,
        _ => return e(ErrorKind::Value),
    };
    let op = code % 100;
    let (mut count, mut nonempty) = (0u64, 0u64);
    let mut total = match op {
        4 => f64::NEG_INFINITY,
        5 => f64::INFINITY,
        6 => 1.0,
        _ => 0.0,
    };
    let (mut mean, mut squares) = (0.0, 0.0);
    let mut error = None;
    for input in &a[1..] {
        let range = match c.eval(input) {
            Ev::R(range) => range,
            Ev::V(Value::Err(err)) => return e(err),
            _ => return e(ErrorKind::Value),
        };
        let sheet = c.wb.sheet(range.sheet);
        c.wb.each_cell(&range, &mut |row, col, cell| {
            use crate::column::Cell;
            if error.is_some() || sheet.row_filtered(row)
                || (skip_hidden && (sheet.row_hidden(row) || sheet.row_sizes.size(row) == 0.0))
                || c.wb.is_subtotal(range.sheet, row, col)
            {
                return;
            }
            if !matches!(cell, Cell::Empty) {
                nonempty += 1;
            }
            match cell {
                Cell::Num(value) => {
                    count += 1;
                    match op {
                        1 | 9 => total += value,
                        4 => total = total.max(value),
                        5 => total = total.min(value),
                        6 => total *= value,
                        7 | 8 | 10 | 11 => {
                            let delta = value - mean;
                            mean += delta / count as f64;
                            squares += delta * (value - mean);
                        }
                        _ => {}
                    }
                }
                Cell::Err(err) if op != 2 && op != 3 => error = Some(err),
                _ => {}
            }
        });
        if let Some(err) = error {
            return e(err);
        }
    }
    match op {
        1 if count == 0 => e(ErrorKind::Div0),
        1 => n(total / count as f64),
        2 => n(count as f64),
        3 => n(nonempty as f64),
        7 | 10 if count < 2 => e(ErrorKind::Div0),
        8 | 11 if count == 0 => e(ErrorKind::Div0),
        7 => n((squares / (count - 1) as f64).sqrt()),
        8 => n((squares / count as f64).sqrt()),
        10 => n(squares / (count - 1) as f64),
        11 => n(squares / count as f64),
        _ if count == 0 => n(0.0),
        _ => n(total),
    }
}

fn variance(c: &Ctx, a: &[Expr], sample: bool) -> Result<f64, ErrorKind> {
    let v = collect(c, a)?;
    let k = v.len() as f64;
    if v.is_empty() || (sample && v.len() < 2) {
        return Err(ErrorKind::Div0);
    }
    // Two-pass for accuracy.
    let mean = v.iter().sum::<f64>() / k;
    let ss: f64 = v.iter().map(|x| (x - mean) * (x - mean)).sum();
    Ok(ss / if sample { k - 1.0 } else { k })
}
fn var_s(c: &Ctx, a: &[Expr]) -> Ev {
    n(tri!(variance(c, a, true)))
}
fn var_p(c: &Ctx, a: &[Expr]) -> Ev {
    n(tri!(variance(c, a, false)))
}
fn stdev_s(c: &Ctx, a: &[Expr]) -> Ev {
    n(tri!(variance(c, a, true)).sqrt())
}
fn stdev_p(c: &Ctx, a: &[Expr]) -> Ev {
    n(tri!(variance(c, a, false)).sqrt())
}

// ---------- logic ----------

fn iff(c: &Ctx, a: &[Expr]) -> Ev {
    let t = tri!(boolean(c, &a[0]));
    if t {
        match a.get(1) {
            Some(x) if !missing(x) => c.eval(x),
            Some(_) => n(0.0),
            None => v(Value::Bool(true)),
        }
    } else {
        match a.get(2) {
            Some(x) if !missing(x) => c.eval(x),
            Some(_) => n(0.0),
            None => v(Value::Bool(false)),
        }
    }
}
fn ifs(c: &Ctx, a: &[Expr]) -> Ev {
    if a.len() % 2 != 0 {
        return e(ErrorKind::Value);
    }
    for p in a.chunks(2) {
        if tri!(boolean(c, &p[0])) {
            return c.eval(&p[1]);
        }
    }
    e(ErrorKind::NA)
}
/// HYPERLINK(address, [shown]): shows `shown` (else the address); the cell
/// links to the address ([`crate::book::Workbook::link_at`]).
fn hyperlink(c: &Ctx, a: &[Expr]) -> Ev {
    if a.len() > 1 && !missing(&a[1]) {
        return Ev::V(c.scalar(&a[1]));
    }
    Ev::V(c.scalar(&a[0]))
}
fn iferror(c: &Ctx, a: &[Expr]) -> Ev {
    let value = c.eval(&a[0]);
    let scalar = |v: &Ev| match v {
        Ev::V(v) => Some(v.clone()),
        Ev::R(r) if r.cells() == 1 => Some(c.wb.value(r.sheet, r.r0, r.c0)),
        _ => None,
    };
    let chosen = if matches!(scalar(&value), Some(Value::Err(_))) { c.eval(&a[1]) } else { value };
    if scalar(&chosen) == Some(Value::Empty) { v(Value::str("")) } else { chosen }
}
fn ifna(c: &Ctx, a: &[Expr]) -> Ev {
    match c.eval(&a[0]) {
        Ev::V(Value::Err(ErrorKind::NA)) => c.eval(&a[1]),
        Ev::R(r) if r.cells() == 1 && c.wb.value(r.sheet, r.r0, r.c0) == Value::Err(ErrorKind::NA) => c.eval(&a[1]),
        other => other,
    }
}
/// The truth values for AND / OR / XOR: ranges skip text and empties; none at all is
/// `#VALUE!`.
fn truths(c: &Ctx, a: &[Expr]) -> Result<Vec<bool>, ErrorKind> {
    let mut out = Vec::new();
    let mut err = None;
    for x in a {
        match c.eval(x) {
            Ev::V(val) => out.push(to_bool(&val)?),
            other => {
                let mut f = |val: &Value| match val {
                    Value::Num(y) => out.push(*y != 0.0),
                    Value::Bool(b) => out.push(*b),
                    Value::Err(k) if err.is_none() => err = Some(*k),
                    _ => {}
                };
                match other {
                    Ev::F(_) => {}
                    Ev::R(r) => {
                        if let Some(cl) = c.clip(&r) {
                            c.wb.each(&cl, &mut |_, _, val| f(&val))
                        }
                    }
                    Ev::A(arr) => arr.vals.iter().for_each(f),
                    Ev::V(_) => unreachable!(),
                }
            }
        }
    }
    if let Some(k) = err {
        return Err(k);
    }
    if out.is_empty() {
        return Err(ErrorKind::Value);
    }
    Ok(out)
}
fn and(c: &Ctx, a: &[Expr]) -> Ev {
    v(Value::Bool(tri!(truths(c, a)).iter().all(|&b| b)))
}
fn or(c: &Ctx, a: &[Expr]) -> Ev {
    v(Value::Bool(tri!(truths(c, a)).iter().any(|&b| b)))
}
fn xor(c: &Ctx, a: &[Expr]) -> Ev {
    v(Value::Bool(tri!(truths(c, a)).iter().filter(|&&b| b).count() % 2 == 1))
}
fn not(c: &Ctx, a: &[Expr]) -> Ev {
    v(Value::Bool(!tri!(boolean(c, &a[0]))))
}
fn tru(_: &Ctx, _: &[Expr]) -> Ev {
    v(Value::Bool(true))
}
fn fals(_: &Ctx, _: &[Expr]) -> Ev {
    v(Value::Bool(false))
}

// ---------- information ----------

fn is(c: &Ctx, a: &[Expr], f: fn(&Value) -> bool) -> Ev {
    v(Value::Bool(f(&c.scalar(&a[0]))))
}
fn formulatext(c: &Ctx, args: &[Expr]) -> Ev {
    let rect = tri!(rect_arg(c, &args[0]));
    let cell = crate::addr::CellAddr::new(rect.sheet, rect.r0, rect.c0);
    if c.wb.hides_formula(cell) {
        return e(ErrorKind::NA);
    }
    match c.wb.formula_text(cell) {
        Some(text) if text.encode_utf16().count() <= 8192 => s(text),
        _ => e(ErrorKind::NA),
    }
}

fn isblank(c: &Ctx, a: &[Expr]) -> Ev {
    is(c, a, |x| x.is_empty())
}
fn isnumber(c: &Ctx, a: &[Expr]) -> Ev {
    is(c, a, |x| matches!(x, Value::Num(_)))
}
fn istext(c: &Ctx, a: &[Expr]) -> Ev {
    is(c, a, |x| matches!(x, Value::Str(_)))
}
fn islogical(c: &Ctx, a: &[Expr]) -> Ev {
    is(c, a, |x| matches!(x, Value::Bool(_)))
}
fn iserror(c: &Ctx, a: &[Expr]) -> Ev {
    is(c, a, |x| matches!(x, Value::Err(_)))
}
fn iserr(c: &Ctx, a: &[Expr]) -> Ev {
    is(c, a, |x| matches!(x, Value::Err(k) if *k != ErrorKind::NA))
}
fn isna(c: &Ctx, a: &[Expr]) -> Ev {
    is(c, a, |x| matches!(x, Value::Err(ErrorKind::NA)))
}
fn na(_: &Ctx, _: &[Expr]) -> Ev {
    e(ErrorKind::NA)
}
fn row(c: &Ctx, a: &[Expr]) -> Ev {
    match a.first() {
        None => n(c.row as f64 + 1.0),
        Some(x) => match c.eval(x) {
            // Several rows: their numbers, as a column (Excel 365).
            Ev::R(r) if r.rows() > 1 && r.rows() <= super::MAX_ARRAY => Ev::A(Arc::new(Array { rows: r.rows() as usize, cols: 1, vals: (r.r0..=r.r1).map(|x| Value::Num(x as f64 + 1.0)).collect() })),
            Ev::R(r) => n(r.r0 as f64 + 1.0),
            _ => e(ErrorKind::Value),
        },
    }
}
fn column(c: &Ctx, a: &[Expr]) -> Ev {
    match a.first() {
        None => n(c.col as f64 + 1.0),
        Some(x) => match c.eval(x) {
            Ev::R(r) if r.cols() > 1 => Ev::A(Arc::new(Array { rows: 1, cols: r.cols() as usize, vals: (r.c0..=r.c1).map(|x| Value::Num(x as f64 + 1.0)).collect() })),
            Ev::R(r) => n(r.c0 as f64 + 1.0),
            _ => e(ErrorKind::Value),
        },
    }
}
fn rows(c: &Ctx, a: &[Expr]) -> Ev {
    match c.eval(&a[0]) {
        Ev::F(_) => e(ErrorKind::Calc),
        Ev::R(r) => n(r.rows() as f64),
        Ev::A(x) => n(x.rows as f64),
        Ev::V(Value::Err(k)) => e(k),
        Ev::V(_) => n(1.0),
    }
}
fn columns(c: &Ctx, a: &[Expr]) -> Ev {
    match c.eval(&a[0]) {
        Ev::F(_) => e(ErrorKind::Calc),
        Ev::R(r) => n(r.cols() as f64),
        Ev::A(x) => n(x.cols as f64),
        Ev::V(Value::Err(k)) => e(k),
        Ev::V(_) => n(1.0),
    }
}

// ---------- text ----------

fn s(x: String) -> Ev {
    v(Value::str(crate::strings::clip(&x)))
}
fn len(c: &Ctx, a: &[Expr]) -> Ev {
    n(tri!(text(c, &a[0])).chars().count() as f64)
}
fn left(c: &Ctx, a: &[Expr]) -> Ev {
    let t = tri!(text(c, &a[0]));
    let k = tri!(opt_num(c, a, 1, 1.0));
    if k < 0.0 {
        return e(ErrorKind::Value);
    }
    s(t.chars().take(k as usize).collect())
}
fn right(c: &Ctx, a: &[Expr]) -> Ev {
    let t = tri!(text(c, &a[0]));
    let k = tri!(opt_num(c, a, 1, 1.0));
    if k < 0.0 {
        return e(ErrorKind::Value);
    }
    let total = t.chars().count();
    s(t.chars().skip(total.saturating_sub(k as usize)).collect())
}
fn mid(c: &Ctx, a: &[Expr]) -> Ev {
    let t = tri!(text(c, &a[0]));
    let st = tri!(num(c, &a[1]));
    let k = tri!(num(c, &a[2]));
    if st < 1.0 || k < 0.0 {
        return e(ErrorKind::Value);
    }
    s(t.chars().skip(st as usize - 1).take(k as usize).collect())
}
fn upper(c: &Ctx, a: &[Expr]) -> Ev {
    s(tri!(text(c, &a[0])).to_uppercase())
}
fn lower(c: &Ctx, a: &[Expr]) -> Ev {
    s(tri!(text(c, &a[0])).to_lowercase())
}
fn trim(c: &Ctx, a: &[Expr]) -> Ev {
    s(tri!(text(c, &a[0])).split(' ').filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" "))
}
fn concatenate(c: &Ctx, a: &[Expr]) -> Ev {
    let mut out = String::new();
    for x in a {
        out.push_str(&tri!(text(c, x)));
        if out.len() > crate::strings::MAX_TEXT {
            return e(ErrorKind::Value);
        }
    }
    s(out)
}
fn concat(c: &Ctx, a: &[Expr]) -> Ev {
    let mut out = String::new();
    let mut err = None;
    each_value(c, a, &mut |x| match to_text(x) {
        Ok(t) => {
            if out.len() <= crate::strings::MAX_TEXT {
                out.push_str(&t)
            }
        }
        Err(k) => {
            err.get_or_insert(k);
        }
    });
    if let Some(k) = err {
        return e(k);
    }
    if out.len() > crate::strings::MAX_TEXT {
        return e(ErrorKind::Value);
    }
    s(out)
}
fn textjoin(c: &Ctx, a: &[Expr]) -> Ev {
    let d = tri!(text(c, &a[0]));
    let skip_empty = tri!(boolean(c, &a[1]));
    let mut parts: Vec<Arc<str>> = Vec::new();
    let mut err = None;
    for x in &a[2..] {
        match c.eval(x) {
            Ev::R(r) => {
                // Walk every cell so empties count when not skipped.
                if let Some(cl) = c.clip(&r) {
                    for row in cl.r0..=cl.r1 {
                        for col in cl.c0..=cl.c1 {
                            match to_text(&c.wb.value(r.sheet, row, col)) {
                                Ok(t) => parts.push(t),
                                Err(k) => {
                                    err.get_or_insert(k);
                                }
                            }
                        }
                    }
                }
            }
            other => {
                let arr = tri!(c.to_array(other));
                for val in arr.vals.iter() {
                    match to_text(val) {
                        Ok(t) => parts.push(t),
                        Err(k) => {
                            err.get_or_insert(k);
                        }
                    }
                }
            }
        }
    }
    if let Some(k) = err {
        return e(k);
    }
    let mut out = String::new();
    let mut first = true;
    for p in parts {
        if skip_empty && p.is_empty() {
            continue;
        }
        if !first {
            out.push_str(&d);
        }
        out.push_str(&p);
        first = false;
        if out.len() > crate::strings::MAX_TEXT {
            return e(ErrorKind::Value);
        }
    }
    s(out)
}
fn rept(c: &Ctx, a: &[Expr]) -> Ev {
    let t = tri!(text(c, &a[0]));
    let k = tri!(num(c, &a[1]));
    if k < 0.0 || t.len() as f64 * k.floor() > crate::strings::MAX_TEXT as f64 {
        return e(ErrorKind::Value);
    }
    s(t.repeat(k as usize))
}
fn find_in(hay: &str, needle: &str, start: usize, ci: bool) -> Option<usize> {
    let h: Vec<char> = if ci { hay.chars().flat_map(char::to_lowercase).collect() } else { hay.chars().collect() };
    let n: Vec<char> = if ci { needle.chars().flat_map(char::to_lowercase).collect() } else { needle.chars().collect() };
    if start > h.len() {
        return None;
    }
    if n.is_empty() {
        return Some(start);
    }
    (start..=h.len().saturating_sub(n.len())).find(|&i| h[i..i + n.len()] == n[..])
}
fn find(c: &Ctx, a: &[Expr]) -> Ev {
    let needle = tri!(text(c, &a[0]));
    let hay = tri!(text(c, &a[1]));
    let st = tri!(opt_num(c, a, 2, 1.0));
    if st < 1.0 {
        return e(ErrorKind::Value);
    }
    match find_in(&hay, &needle, st as usize - 1, false) {
        Some(i) => n(i as f64 + 1.0),
        None => e(ErrorKind::Value),
    }
}
fn search(c: &Ctx, a: &[Expr]) -> Ev {
    let needle = tri!(text(c, &a[0]));
    let hay = tri!(text(c, &a[1]));
    let st = tri!(opt_num(c, a, 2, 1.0));
    if st < 1.0 {
        return e(ErrorKind::Value);
    }
    let start = st as usize - 1;
    if needle.contains(['*', '?', '~']) {
        // Wildcards: the first position where the pattern matches a prefix of the rest.
        let chars: Vec<char> = hay.chars().collect();
        for i in start..=chars.len() {
            for j in i..=chars.len() {
                let sub: String = chars[i..j].iter().collect();
                if super::criteria::wildcard(&needle, &sub) {
                    return n(i as f64 + 1.0);
                }
            }
        }
        return e(ErrorKind::Value);
    }
    match find_in(&hay, &needle, start, true) {
        Some(i) => n(i as f64 + 1.0),
        None => e(ErrorKind::Value),
    }
}
fn substitute(c: &Ctx, a: &[Expr]) -> Ev {
    let t = tri!(text(c, &a[0]));
    let old = tri!(text(c, &a[1]));
    let new = tri!(text(c, &a[2]));
    if old.is_empty() {
        return v(Value::Str(t));
    }
    if a.len() > 3 {
        let k = tri!(num(c, &a[3]));
        if k < 1.0 {
            return e(ErrorKind::Value);
        }
        let k = k as usize;
        let mut out = String::new();
        let mut rest: &str = &t;
        let mut i = 0;
        while let Some(p) = rest.find(&*old) {
            i += 1;
            out.push_str(&rest[..p]);
            out.push_str(if i == k { &new } else { &old });
            rest = &rest[p + old.len()..];
        }
        out.push_str(rest);
        return s(out);
    }
    let out = t.replace(&*old, &new);
    if out.len() > crate::strings::MAX_TEXT {
        return e(ErrorKind::Value);
    }
    s(out)
}
fn textafter(c: &Ctx, a: &[Expr]) -> Ev {
    text_around(c, a, false)
}
fn textbefore(c: &Ctx, a: &[Expr]) -> Ev {
    text_around(c, a, true)
}
fn text_around(c: &Ctx, a: &[Expr], before: bool) -> Ev {
    let haystack = tri!(text(c, &a[0]));
    let delim = tri!(super::textafter::delimiters(c, c.eval(&a[1])));
    let instance = tri!(opt_num(c, a, 2, 1.0)).floor();
    let mode = tri!(opt_num(c, a, 3, 0.0));
    let match_end = tri!(opt_num(c, a, 4, 0.0));
    let explicit_instance = a.get(2).is_some_and(|arg| !missing(arg));
    let find = if before { super::textafter::before } else { super::textafter::after };
    match find(&haystack, &delim, instance, mode, match_end, explicit_instance) {
        Ok(Some(suffix)) => v(Value::str(suffix)),
        Ok(None) => match a.get(5).filter(|arg| !missing(arg)) {
            Some(fallback) => match c.scalar(fallback) {
                Value::Empty => v(Value::str("")),
                value => v(value),
            },
            None => e(ErrorKind::NA),
        },
        Err(err) => e(err),
    }
}
fn value(c: &Ctx, a: &[Expr]) -> Ev {
    match c.scalar(&a[0]) {
        Value::Num(x) => n(x),
        Value::Empty => n(0.0),
        Value::Str(t) => match c.numeric_text(&t) {
            Some(x) => n(x),
            None => e(ErrorKind::Value),
        },
        Value::Bool(_) => e(ErrorKind::Value),
        Value::Err(k) => e(k),
    }
}
/// TEXT(value, format): the value as a cell with that number format shows it.
/// Text that reads as a number is formatted as the number.
fn text_fn(c: &Ctx, a: &[Expr]) -> Ev {
    let x = match c.scalar(&a[0]) {
        Value::Empty => Value::Num(0.0),
        Value::Err(k) => return e(k),
        Value::Str(t) => match c.numeric_text(&t) {
            Some(x) => Value::Num(x),
            None => Value::Str(t),
        },
        v => v,
    };
    let code = tri!(text(c, &a[1]));
    let f = crate::numfmt::NumFmt::parse(&code).format_in(&x, c.wb.date_system());
    if f.hashes {
        return e(ErrorKind::Value);
    }
    s(f.text)
}
fn exact(c: &Ctx, a: &[Expr]) -> Ev {
    let x = tri!(text(c, &a[0]));
    let y = tri!(text(c, &a[1]));
    v(Value::Bool(x == y))
}

// ---------- lookup ----------

fn choose(c: &Ctx, a: &[Expr]) -> Ev {
    let i = tri!(num(c, &a[0])).trunc();
    if i < 1.0 || i as usize >= a.len() {
        return e(ErrorKind::Value);
    }
    c.eval(&a[i as usize])
}

fn index(c: &Ctx, a: &[Expr]) -> Ev {
    let r = tri!(opt_num(c, a, 1, 0.0)).trunc();
    let k = tri!(opt_num(c, a, 2, 0.0)).trunc();
    if r < 0.0 || k < 0.0 {
        return e(ErrorKind::Value);
    }
    match c.eval(&a[0]) {
        Ev::F(_) => e(ErrorKind::Value),
        Ev::R(rect) => {
            // A single row or column takes its one index from whichever is given.
            let (r, k) = if a.len() == 2 && rect.rows() == 1 { (1.0, r) } else { (r, k) };
            let (r, k) = (r as u64, k as u64);
            if r > rect.rows() || k > rect.cols() {
                return e(ErrorKind::Ref);
            }
            let (r0, r1) = if r == 0 { (rect.r0, rect.r1) } else { (rect.r0 + r as u32 - 1, rect.r0 + r as u32 - 1) };
            let (c0, c1) = if k == 0 { (rect.c0, rect.c1) } else { (rect.c0 + k as u32 - 1, rect.c0 + k as u32 - 1) };
            Ev::R(Rect { sheet: rect.sheet, r0, r1, c0, c1 })
        }
        Ev::A(arr) => {
            let (r, k) = if a.len() == 2 && arr.rows == 1 { (1.0, r) } else { (r, k) };
            let (r, k) = (r as usize, k as usize);
            if r == 0 || k == 0 && arr.cols > 1 {
                return e(ErrorKind::Value);
            }
            let k = k.max(1);
            if r > arr.rows || k > arr.cols {
                return e(ErrorKind::Ref);
            }
            v(arr.vals[(r - 1) * arr.cols + (k - 1)].clone())
        }
        Ev::V(Value::Err(k)) => e(k),
        Ev::V(x) => {
            if r <= 1.0 && k <= 1.0 {
                v(x)
            } else {
                e(ErrorKind::Ref)
            }
        }
    }
}

/// A vector to search: a single row or column of a range, or a 1-D array.
#[derive(Clone)]
enum Vector {
    Col { sheet: u16, col: u32, r0: u32, len: u32 },
    Row { sheet: u16, row: u32, c0: u32, len: u32 },
    Arr(Arc<Array>),
}

impl Vector {
    fn from(c: &Ctx, ev: Ev) -> Result<Vector, ErrorKind> {
        Ok(match ev {
            Ev::R(r) if r.cols() == 1 => {
                // Whole-column searches stop at the used rows.
                let len = c.clip(&r).map_or(0, |cl| cl.rows() as u32);
                Vector::Col { sheet: r.sheet, col: r.c0, r0: r.r0, len }
            }
            Ev::R(r) if r.rows() == 1 => {
                let len = c.clip(&r).map_or(0, |cl| cl.cols() as u32);
                Vector::Row { sheet: r.sheet, row: r.r0, c0: r.c0, len }
            }
            Ev::R(_) => return Err(ErrorKind::NA),
            Ev::V(Value::Err(k)) => return Err(k),
            other => {
                let a = c.to_array(other)?;
                if a.rows != 1 && a.cols != 1 {
                    return Err(ErrorKind::NA);
                }
                Vector::Arr(a)
            }
        })
    }
    fn len(&self) -> u32 {
        match self {
            Vector::Col { len, .. } | Vector::Row { len, .. } => *len,
            Vector::Arr(a) => a.vals.len() as u32,
        }
    }
    fn get(&self, c: &Ctx, i: u32) -> Value {
        match self {
            Vector::Col { sheet, col, r0, .. } => c.wb.value(*sheet, r0 + i, *col),
            Vector::Row { sheet, row, c0, .. } => c.wb.value(*sheet, *row, c0 + i),
            Vector::Arr(a) => a.vals[i as usize].clone(),
        }
    }
}

/// Exact match (case-insensitive, wildcards when `wild`), first hit. Long columns use the
/// workbook's lookup index (built once per column version).
fn find_exact(c: &Ctx, vec: &Vector, key: &Value, wild: bool, reverse: bool) -> Option<u32> {
    if let Value::Str(t) = key {
        if wild && t.contains(['*', '?', '~']) {
            let it: Box<dyn Iterator<Item = u32>> = if reverse { Box::new((0..vec.len()).rev()) } else { Box::new(0..vec.len()) };
            for i in it {
                if let Value::Str(x) = vec.get(c, i) {
                    if super::criteria::wildcard(t, &x) {
                        return Some(i);
                    }
                }
            }
            return None;
        }
    }
    if let (Vector::Col { sheet, col, r0, len }, false) = (vec, reverse) {
        if *len > 64 {
            return c.wb.lookup_index(*sheet, *col, *r0, *len).get(key);
        }
    }
    let eq = |x: &Value| !x.is_empty() && compare(x, key) == Ordering::Equal && std::mem::discriminant(x) == std::mem::discriminant(key);
    if reverse {
        (0..vec.len()).rev().find(|&i| eq(&vec.get(c, i)))
    } else {
        (0..vec.len()).find(|&i| eq(&vec.get(c, i)))
    }
}

/// Approximate match on sorted data by binary search: the last item <= key (ascending),
/// or the last item >= key (descending), as Excel's MATCH 1 and -1 do.
fn find_sorted(c: &Ctx, vec: &Vector, key: &Value, descending: bool) -> Option<u32> {
    let (mut lo, mut hi) = (0i64, vec.len() as i64 - 1);
    let mut best = None;
    while lo <= hi {
        let mid = (lo + hi) / 2;
        let x = vec.get(c, mid as u32);
        let same_kind = std::mem::discriminant(&x) == std::mem::discriminant(key);
        let o = compare(&x, key);
        let ok = if descending { o != Ordering::Less } else { o != Ordering::Greater };
        if ok {
            if same_kind {
                best = Some(mid as u32);
            }
            lo = mid + 1;
        } else {
            hi = mid - 1;
        }
    }
    best
}

fn matchf(c: &Ctx, a: &[Expr]) -> Ev {
    let key = c.scalar(&a[0]);
    if let Value::Err(k) = key {
        return e(k);
    }
    let vec = tri!(Vector::from(c, c.eval(&a[1])));
    let t = tri!(opt_num(c, a, 2, 1.0));
    let hit = if t == 0.0 { find_exact(c, &vec, &key, true, false) } else { find_sorted(c, &vec, &key, t < 0.0) };
    match hit {
        Some(i) => n(i as f64 + 1.0),
        None => e(ErrorKind::NA),
    }
}

fn vhlookup(c: &Ctx, a: &[Expr], vertical: bool) -> Ev {
    let key = c.scalar(&a[0]);
    if let Value::Err(k) = key {
        return e(k);
    }
    // The table: a range, or an array constant / array result.
    let (rows, cols, arr, rect) = match c.eval(&a[1]) {
        Ev::R(r) => (r.rows(), r.cols(), None, Some(r)),
        Ev::A(x) => (x.rows as u64, x.cols as u64, Some(x), None),
        Ev::V(Value::Err(k)) => return e(k),
        _ => return e(ErrorKind::Value),
    };
    let k = tri!(num(c, &a[2])).trunc();
    let approx = if a.len() > 3 && !missing(&a[3]) { tri!(boolean(c, &a[3])) } else { true };
    let span = if vertical { cols } else { rows };
    if k < 1.0 {
        return e(ErrorKind::Value);
    }
    if k as u64 > span {
        return e(ErrorKind::Ref);
    }
    let k = k as u32 - 1;
    let first = match (&rect, &arr) {
        (Some(t), _) => Ev::R(if vertical { Rect { c1: t.c0, ..*t } } else { Rect { r1: t.r0, ..*t } }),
        (_, Some(x)) => {
            let vals: Vec<Value> = if vertical { (0..x.rows).map(|r| x.get(r, 0).clone()).collect() } else { (0..x.cols).map(|j| x.get(0, j).clone()).collect() };
            let n = vals.len();
            Ev::A(Arc::new(Array { rows: if vertical { n } else { 1 }, cols: if vertical { 1 } else { n }, vals }))
        }
        _ => unreachable!(),
    };
    let vec = tri!(Vector::from(c, first));
    let hit = if approx { find_sorted(c, &vec, &key, false) } else { find_exact(c, &vec, &key, true, false) };
    let Some(i) = hit else { return e(ErrorKind::NA) };
    let (r, col) = if vertical { (i, k) } else { (k, i) };
    match (rect, arr) {
        (Some(t), _) => v(c.wb.value(t.sheet, t.r0 + r, t.c0 + col)),
        (_, Some(x)) => v(x.get(r as usize, col as usize).clone()),
        _ => unreachable!(),
    }
}
fn vlookup(c: &Ctx, a: &[Expr]) -> Ev {
    vhlookup(c, a, true)
}
fn hlookup(c: &Ctx, a: &[Expr]) -> Ev {
    vhlookup(c, a, false)
}

/// XLOOKUP's and XMATCH's search: the position of `key` in `look` by
/// match mode (0 exact, -1 or next smaller, 1 or next larger, 2 wildcards)
/// and search mode (1 first to last, -1 last to first; the binary modes
/// 2 and -2 search the same way, as their results agree on sorted data).
fn xfind(c: &Ctx, look: &Vector, key: &Value, mode: i32, search: i32) -> Result<Option<u32>, ErrorKind> {
    if !matches!(search, 1 | -1 | 2 | -2) {
        return Err(ErrorKind::Value);
    }
    let reverse = search == -1 || search == -2;
    Ok(match mode {
        0 => find_exact(c, look, key, false, reverse),
        2 => find_exact(c, look, key, true, reverse),
        -1 | 1 => {
            // Exact, else the next smaller (-1) or next larger (1), by a scan.
            let mut best: Option<(u32, Value)> = None;
            let it: Box<dyn Iterator<Item = u32>> = if reverse { Box::new((0..look.len()).rev()) } else { Box::new(0..look.len()) };
            let mut exact = None;
            for i in it {
                let x = look.get(c, i);
                if x.is_empty() || std::mem::discriminant(&x) != std::mem::discriminant(key) {
                    continue;
                }
                let o = compare(&x, key);
                if o == Ordering::Equal {
                    exact = Some(i);
                    break;
                }
                let better = match (&best, mode) {
                    (None, -1) => o == Ordering::Less,
                    (None, _) => o == Ordering::Greater,
                    (Some((_, b)), -1) => o == Ordering::Less && compare(&x, b) == Ordering::Greater,
                    (Some((_, b)), _) => o == Ordering::Greater && compare(&x, b) == Ordering::Less,
                };
                if better {
                    best = Some((i, x));
                }
            }
            exact.or(best.map(|b| b.0))
        }
        _ => return Err(ErrorKind::Value),
    })
}

fn xmatch(c: &Ctx, a: &[Expr]) -> Ev {
    let key = c.scalar(&a[0]);
    if let Value::Err(k) = key {
        return e(k);
    }
    let look = tri!(Vector::from(c, c.eval(&a[1])));
    let mode = tri!(opt_num(c, a, 2, 0.0)) as i32;
    let search = tri!(opt_num(c, a, 3, 1.0)) as i32;
    match tri!(xfind(c, &look, &key, mode, search)) {
        Some(i) => n(f64::from(i) + 1.0),
        None => e(ErrorKind::NA),
    }
}

fn xlookup(c: &Ctx, a: &[Expr]) -> Ev {
    let key = c.scalar(&a[0]);
    if let Value::Err(k) = key {
        return e(k);
    }
    let lv = c.eval(&a[1]);
    let look = tri!(Vector::from(c, lv.clone()));
    let ret = c.eval(&a[2]);
    let mode = tri!(opt_num(c, a, 4, 0.0)) as i32;
    let search = tri!(opt_num(c, a, 5, 1.0)) as i32;
    let hit = tri!(xfind(c, &look, &key, mode, search));
    match hit {
        Some(i) => match ret {
            Ev::R(r) => {
                if let Ev::R(lr) = lv {
                    if lr.cols() == 1 {
                        return Ev::R(Rect { r0: r.r0 + i, r1: r.r0 + i, ..r });
                    }
                    return Ev::R(Rect { c0: r.c0 + i, c1: r.c0 + i, ..r });
                }
                Ev::R(Rect { r0: r.r0 + i, r1: r.r0 + i, ..r })
            }
            other => {
                let arr = tri!(c.to_array(other));
                v(arr.vals.get(i as usize).cloned().unwrap_or(Value::Err(ErrorKind::NA)))
            }
        },
        None => match a.get(3) {
            Some(x) if !missing(x) => c.eval(x),
            _ => e(ErrorKind::NA),
        },
    }
}

// ---------- dates ----------

fn date(c: &Ctx, a: &[Expr]) -> Ev {
    let y = tri!(num(c, &a[0]));
    let m = tri!(num(c, &a[1]));
    let d = tri!(num(c, &a[2]));
    match c.wb.date_system().date(y, m, d) {
        Some(s) => n(s),
        None => e(ErrorKind::Num),
    }
}
fn datevalue(c: &Ctx, a: &[Expr]) -> Ev {
    match c.scalar(&a[0]) {
        Value::Str(text) => match c.wb.date_system().datevalue(&text) {
            Some(serial) => n(serial),
            None => e(ErrorKind::Value),
        },
        Value::Err(err) => e(err),
        _ => e(ErrorKind::Value),
    }
}
fn eomonth(c: &Ctx, a: &[Expr]) -> Ev {
    let serial = tri!(num(c, &a[0]));
    let months = tri!(num(c, &a[1]));
    match c.wb.date_system().month_end(serial, months) {
        Some(last_day) => n(last_day),
        None => e(ErrorKind::Num),
    }
}
fn ymd_part(c: &Ctx, a: &[Expr], part: usize) -> Ev {
    date_part(c, &c.scalar(&a[0]), part)
}
fn date_part(c: &Ctx, value: &Value, part: usize) -> Ev {
    let x = tri!(c.number(value));
    match c.wb.date_system().ymd(x) {
        Some((y, m, d)) => n([y, m, d][part] as f64),
        None => e(ErrorKind::Num),
    }
}
fn year(c: &Ctx, a: &[Expr]) -> Ev {
    ymd_part(c, a, 0)
}
fn month(c: &Ctx, a: &[Expr]) -> Ev {
    let value = c.scalar(&a[0]);
    // Excel files use MONTH(header & 0) to turn a month name into its number.
    if let Value::Str(text) = &value {
        if let Some(month) = text.trim().strip_suffix('0').and_then(|s| super::dates::english_month(s.trim_end())) {
            return n(f64::from(month));
        }
    }
    date_part(c, &value, 1)
}
fn day(c: &Ctx, a: &[Expr]) -> Ev {
    ymd_part(c, a, 2)
}
fn weekday(c: &Ctx, a: &[Expr]) -> Ev {
    let x = tri!(num(c, &a[0]));
    let t = tri!(opt_num(c, a, 1, 1.0)) as i64;
    let system = c.wb.date_system();
    if !system.contains(x) {
        return e(ErrorKind::Num);
    }
    let sun0 = system.weekday(x.floor() as i64);
    let r = match t {
        1 => sun0 + 1,
        2 => (sun0 + 6) % 7 + 1,
        3 => (sun0 + 6) % 7,
        _ => return e(ErrorKind::Num),
    };
    n(r as f64)
}
fn today(c: &Ctx, _: &[Expr]) -> Ev {
    n(c.wb.date_system().now().floor())
}
fn now(c: &Ctx, _: &[Expr]) -> Ev {
    n(c.wb.date_system().now())
}

/// Text compare helper re-exported for the lookup index.
pub fn text_key(s: &str) -> String {
    s.chars().flat_map(char::to_lowercase).collect()
}

#[allow(dead_code)]
fn _unused(a: &str, b: &str) -> Ordering {
    cmp_text(a, b)
}

/// LOOKUP(value, lookup_vector, [result_vector]), or LOOKUP(value, array):
/// the last place whose value of the same type is at most `value` (on sorted
/// data, where Excel's binary search lands). The array form searches the
/// first row of a wide array, else the first column, and answers from the
/// last row or column.
fn lookup_fn(c: &Ctx, a: &[Expr]) -> Ev {
    let key = c.scalar(&a[0]);
    if let Value::Err(k) = key {
        return e(k);
    }
    let (look, result) = if a.len() == 3 {
        let look = tri!(Vector::from(c, c.eval(&a[1])));
        let result = tri!(Vector::from(c, c.eval(&a[2])));
        (look, result)
    } else {
        tri!(lookup_array_form(c, c.eval(&a[1])))
    };
    let mut hit = None;
    for i in 0..look.len() {
        let x = look.get(c, i);
        if x.is_empty() || std::mem::discriminant(&x) != std::mem::discriminant(&key) {
            continue;
        }
        if compare(&x, &key) != Ordering::Greater {
            hit = Some(i);
        }
    }
    let Some(i) = hit else { return e(ErrorKind::NA) };
    match &result {
        // A short result array has nothing at that place.
        Vector::Arr(arr) if i as usize >= arr.vals.len() => e(ErrorKind::NA),
        _ => v(result.get(c, i)),
    }
}

/// LOOKUP's array form: the vector to search and the one to answer from.
fn lookup_array_form(c: &Ctx, ev: Ev) -> Result<(Vector, Vector), ErrorKind> {
    match ev {
        Ev::R(r) if r.rows() > 1 && r.cols() > 1 => {
            let r = c.clip(&r).unwrap_or(r);
            let wide = r.cols() > r.rows();
            let (look, last) = if wide {
                (Rect { r1: r.r0, ..r }, Rect { r0: r.r1, ..r })
            } else {
                (Rect { c1: r.c0, ..r }, Rect { c0: r.c1, ..r })
            };
            Ok((Vector::from(c, Ev::R(look))?, Vector::from(c, Ev::R(last))?))
        }
        Ev::R(_) => {
            let vector = Vector::from(c, ev)?;
            Ok((vector.clone(), vector))
        }
        Ev::V(Value::Err(k)) => Err(k),
        other => {
            let arr = c.to_array(other)?;
            let (rows, cols) = (arr.rows, arr.cols);
            let wide = cols > rows;
            let pick = |fixed_row: Option<usize>, fixed_col: Option<usize>| {
                let vals: Vec<Value> = if let Some(row) = fixed_row {
                    (0..cols).map(|col| arr.vals[row * cols + col].clone()).collect()
                } else {
                    let col = fixed_col.unwrap_or(0);
                    (0..rows).map(|row| arr.vals[row * cols + col].clone()).collect()
                };
                let (r, k) = if fixed_row.is_some() { (1, vals.len()) } else { (vals.len(), 1) };
                Vector::Arr(Arc::new(Array { rows: r, cols: k, vals }))
            };
            if rows == 0 || cols == 0 {
                return Err(ErrorKind::NA);
            }
            Ok(if wide {
                (pick(Some(0), None), pick(Some(rows - 1), None))
            } else {
                (pick(None, Some(0)), pick(None, Some(cols - 1)))
            })
        }
    }
}

/// ADDRESS(row, column, [abs_num], [a1], [sheet_text]): a cell's address as
/// text, in A1 or R1C1 style.
fn address(c: &Ctx, a: &[Expr]) -> Ev {
    let row = tri!(num(c, &a[0])).trunc();
    let col = tri!(num(c, &a[1])).trunc();
    let abs = tri!(opt_num(c, a, 2, 1.0)).trunc();
    let a1 = match a.get(3).filter(|x| !missing(x)) {
        Some(x) => tri!(to_bool(&c.scalar(x))),
        None => true,
    };
    if !(1.0..=f64::from(crate::addr::MAX_ROWS)).contains(&row)
        || !(1.0..=f64::from(crate::addr::MAX_COLS)).contains(&col)
        || !matches!(abs as i32, 1..=4)
        || abs.fract() != 0.0
    {
        return e(ErrorKind::Value);
    }
    let (row, col, abs) = (row as u32, col as u32, abs as i32);
    let mut out = String::new();
    if let Some(sheet) = a.get(4).filter(|x| !missing(x)) {
        let name = tri!(text(c, sheet));
        crate::formula::print::push_sheet_name(&name, &mut out);
        out.push('!');
    }
    let (row_abs, col_abs) = (matches!(abs, 1 | 2), matches!(abs, 1 | 3));
    if a1 {
        if col_abs {
            out.push('$');
        }
        out.push_str(&crate::addr::col_name(col - 1));
        if row_abs {
            out.push('$');
        }
        out.push_str(&row.to_string());
    } else {
        let part = |letter: char, n: u32, absolute: bool| if absolute { format!("{letter}{n}") } else { format!("{letter}[{n}]") };
        out.push_str(&part('R', row, row_abs));
        out.push_str(&part('C', col, col_abs));
    }
    s(out)
}

/// ISREF(value): whether the argument is a reference.
fn isref(c: &Ctx, a: &[Expr]) -> Ev {
    v(Value::Bool(matches!(c.eval(&a[0]), Ev::R(_))))
}
