//! Conditional formats (docs/spec/officina.md §16.31): rules over ranges that change how
//! cells look by their values: comparisons, formulas, text, blanks and errors, top and
//! bottom N, above or below average, duplicates, colour scales and data bars.
//!
//! They are evaluated when cells are drawn, not in recalculation: [`Workbook::look`]
//! answers for one cell. Rules that need the whole range (scales, bars, top N, average,
//! duplicates) compute its statistics once per recalculation and keep them.

use crate::addr::{CellAddr, Rect, MAX_COLS, MAX_ROWS};
use crate::book::Workbook;
use crate::eval::{to_bool, Ctx};
use crate::formula::{self, Expr, ParseError};
use crate::value::{cmp_text, compare, Value};
use officina_core::wire::{Reader, WireError, Writer};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Conditional formats per workbook, at most.
pub const MAX_FORMATS: usize = 65_536;
/// Rules per format, at most.
pub const MAX_RULES: usize = 64;
/// Ranges per format, at most.
pub const MAX_RANGES: usize = 1024;
/// Cells whose values a range rule's statistics may read.
pub const MAX_STAT_CELLS: u64 = 4_000_000;

/// How a matching rule changes a cell: what it sets (Excel's "differential" format).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Dxf {
    /// Text colour, 0xRRGGBB.
    pub color: Option<u32>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub underline: Option<bool>,
    pub strike: Option<bool>,
    /// Background, 0xRRGGBB.
    pub fill: Option<u32>,
    /// A number format code.
    pub num_fmt: Option<Arc<str>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CmpOp {
    Between,
    NotBetween,
    Equal,
    NotEqual,
    Greater,
    Less,
    GreaterOrEqual,
    LessOrEqual,
}

impl CmpOp {
    pub const ALL: [CmpOp; 8] = [CmpOp::Between, CmpOp::NotBetween, CmpOp::Equal, CmpOp::NotEqual, CmpOp::Greater, CmpOp::Less, CmpOp::GreaterOrEqual, CmpOp::LessOrEqual];
    /// OOXML's name (`operator=`).
    pub fn name(self) -> &'static str {
        ["between", "notBetween", "equal", "notEqual", "greaterThan", "lessThan", "greaterThanOrEqual", "lessThanOrEqual"][self as usize]
    }
    pub fn from_name(s: &str) -> Option<CmpOp> {
        CmpOp::ALL.into_iter().find(|o| o.name() == s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TextOp {
    Contains,
    NotContains,
    BeginsWith,
    EndsWith,
}

impl TextOp {
    pub const ALL: [TextOp; 4] = [TextOp::Contains, TextOp::NotContains, TextOp::BeginsWith, TextOp::EndsWith];
    /// OOXML's rule type.
    pub fn name(self) -> &'static str {
        ["containsText", "notContainsText", "beginsWith", "endsWith"][self as usize]
    }
    pub fn from_name(s: &str) -> Option<TextOp> {
        TextOp::ALL.into_iter().find(|o| o.name() == s)
    }
}

/// A point of a colour scale or data bar (OOXML's `cfvo`).
#[derive(Debug, Clone, PartialEq)]
pub enum Cfvo {
    Min,
    Max,
    Num(f64),
    /// Of the way from the lowest to the highest value, 0–100.
    Percent(f64),
    Percentile(f64),
    Formula(Arc<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum RuleKind {
    /// The cell's value against one or two values (`b` for between).
    CellIs { op: CmpOp, a: Arc<Expr>, b: Option<Arc<Expr>> },
    /// A formula true for the cell, written for the format's first cell (relative
    /// references move with the cell).
    Expression(Arc<Expr>),
    Text { op: TextOp, text: Arc<str> },
    /// Blank cells (`true`) or ones that aren't.
    Blanks(bool),
    Errors(bool),
    /// The `n` highest (or lowest) values, or `n` percent of them.
    Top { n: u32, percent: bool, bottom: bool },
    /// Above (or below) the average; `equal` counts the average itself, `std_dev`
    /// moves the line by that many standard deviations.
    Average { below: bool, equal: bool, std_dev: u8 },
    /// Values seen more than once in the range (or, `unique`, just once).
    Duplicates { unique: bool },
    /// Two or three points with their colours.
    ColorScale(Vec<(Cfvo, u32)>),
    DataBar { min: Cfvo, max: Cfvo, color: u32 },
    /// An icon by where the value falls: `set` indexes [`ICON_SETS`]; one
    /// threshold per icon from the lowest (the first is where icon 0 starts,
    /// usually 0 %), each met by values at or above it (`true`) or above it.
    /// `reverse` gives the highest values the first icon; `show_value: false`
    /// shows only the icon.
    IconSet { set: u8, points: Vec<(Cfvo, bool)>, reverse: bool, show_value: bool },
}

/// Icon sets, by their file names, and how many icons each has. The first
/// seventeen are Excel 2007's; the last three came with Excel 2010.
pub const ICON_SETS: [(&str, u8); 20] = [
    ("3Arrows", 3),
    ("3ArrowsGray", 3),
    ("3Flags", 3),
    ("3TrafficLights1", 3),
    ("3TrafficLights2", 3),
    ("3Signs", 3),
    ("3Symbols", 3),
    ("3Symbols2", 3),
    ("4Arrows", 4),
    ("4ArrowsGray", 4),
    ("4RedToBlack", 4),
    ("4Rating", 4),
    ("4TrafficLights", 4),
    ("5Arrows", 5),
    ("5ArrowsGray", 5),
    ("5Rating", 5),
    ("5Quarters", 5),
    ("3Stars", 3),
    ("3Triangles", 3),
    ("5Boxes", 5),
];

/// The icon set named `name` (as files spell it).
pub fn icon_set(name: &str) -> Option<u8> {
    ICON_SETS.iter().position(|s| s.0 == name).map(|i| i as u8)
}

/// Excel's default thresholds for a set of `n` icons: equal percent steps.
pub fn icon_points(n: u8) -> Vec<(Cfvo, bool)> {
    (0..n).map(|i| (Cfvo::Percent((100.0 * i as f64 / n as f64).round()), true)).collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    pub kind: RuleKind,
    /// What it sets (colour scales and data bars don't use it).
    pub dxf: Dxf,
    /// Later rules don't apply to a cell this one matches.
    pub stop: bool,
}

/// Rules over ranges of one sheet; earlier rules come first.
#[derive(Debug, Clone, PartialEq)]
pub struct CondFormat {
    pub sheet: u16,
    pub ranges: Vec<Rect>,
    pub rules: Vec<Rule>,
}

impl CondFormat {
    /// The cell formulas are written for: the first range's top-left.
    pub fn origin(&self) -> (u32, u32) {
        self.ranges.first().map_or((0, 0), |r| (r.r0, r.c0))
    }
    pub fn covers(&self, a: CellAddr) -> bool {
        a.sheet == self.sheet && self.ranges.iter().any(|r| r.contains(a))
    }
}

/// How conditional formats make a cell look.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Look {
    pub dxf: Dxf,
    /// A colour scale's colour (the fill).
    pub scale: Option<u32>,
    /// A data bar: how much of the cell's width (0–1), and its colour.
    pub bar: Option<(f32, u32)>,
    /// An icon: its set ([`ICON_SETS`]) and which of it, 0 the lowest.
    pub icon: Option<(u8, u8)>,
    /// Only the icon shows, not the value.
    pub hide_value: bool,
}

impl Look {
    pub fn is_empty(&self) -> bool {
        *self == Look::default()
    }
}

/// A range's numbers, sorted, and its values' counts (for duplicates).
#[derive(Debug, Default)]
pub(crate) struct Stats {
    nums: Vec<f64>,
    sum: f64,
    counts: HashMap<Key, u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    Num(u64),
    Text(String),
    Bool(bool),
}

fn key(v: &Value) -> Option<Key> {
    match v {
        Value::Num(x) => Some(Key::Num(if *x == 0.0 { 0 } else { x.to_bits() })),
        Value::Str(s) => Some(Key::Text(s.to_lowercase())),
        Value::Bool(b) => Some(Key::Bool(*b)),
        _ => None,
    }
}

/// The statistics cache: (format, rule) → (recalculation it was made at, stats).
pub(crate) type StatCache = Mutex<HashMap<(usize, usize), (u64, Arc<Stats>)>>;

impl Workbook {
    pub fn cond_formats(&self) -> &[CondFormat] {
        &self.cond_formats
    }

    /// Adds a conditional format; its rules go after those of formats already there.
    pub fn add_cond_format(&mut self, f: CondFormat) -> Result<usize, &'static str> {
        if self.cond_formats.len() >= MAX_FORMATS {
            return Err("too many conditional formats");
        }
        if f.ranges.is_empty() || f.ranges.len() > MAX_RANGES || f.rules.is_empty() || f.rules.len() > MAX_RULES {
            return Err("a conditional format needs ranges and rules");
        }
        if (f.sheet as usize) >= self.sheet_count() || f.ranges.iter().any(|r| r.sheet != f.sheet || r.r1 >= MAX_ROWS || r.c1 >= MAX_COLS || r.r0 > r.r1 || r.c0 > r.c1) {
            return Err("a conditional format's range is off its sheet");
        }
        self.cond_formats.push(f);
        self.cond_stats.lock().unwrap().clear();
        Ok(self.cond_formats.len() - 1)
    }

    pub fn remove_cond_format(&mut self, i: usize) -> Option<CondFormat> {
        (i < self.cond_formats.len()).then(|| {
            self.cond_stats.lock().unwrap().clear();
            self.cond_formats.remove(i)
        })
    }

    /// Parses a rule's formula as written for the format's first cell.
    pub fn cond_formula(&self, src: &str, sheet: u16, origin: (u32, u32)) -> Result<Arc<Expr>, ParseError> {
        let src = if src.starts_with('=') { src.to_string() } else { format!("={src}") };
        self.parse(&src, sheet, origin.0, origin.1).map(Arc::new)
    }

    /// A rule formula's text as written for the format's first cell (no `=`).
    pub fn cond_formula_text(&self, e: &Expr, origin: (u32, u32)) -> String {
        let t = formula::to_a1(e, origin.0, origin.1, &|s| self.sheet_name(s));
        t.strip_prefix('=').unwrap_or(&t).to_string()
    }

    /// How the conditional formats over a cell make it look.
    pub fn look(&self, a: CellAddr) -> Look {
        let mut out = Look::default();
        if self.cond_formats.is_empty() {
            return out;
        }
        let v = self.get(a);
        for (fi, f) in self.cond_formats.iter().enumerate() {
            if !f.covers(a) {
                continue;
            }
            for (ri, rule) in f.rules.iter().enumerate() {
                let hit = self.rule_applies(fi, ri, f, rule, a, &v, &mut out);
                if hit {
                    merge(&mut out.dxf, &rule.dxf);
                    if rule.stop {
                        return out;
                    }
                }
            }
        }
        out
    }

    fn eval_at(&self, e: &Expr, f: &CondFormat, a: CellAddr) -> Value {
        // Written for the origin: evaluated at the cell, its relative references move.
        let _ = f;
        let v = Ctx::new(self, a.sheet, a.row, a.col).eval(e);
        Ctx::new(self, a.sheet, a.row, a.col).to_scalar(v)
    }

    #[allow(clippy::too_many_arguments)]
    fn rule_applies(&self, fi: usize, ri: usize, f: &CondFormat, rule: &Rule, a: CellAddr, v: &Value, out: &mut Look) -> bool {
        match &rule.kind {
            RuleKind::CellIs { op, a: x, b } => {
                if v.is_empty() && !matches!(op, CmpOp::Equal | CmpOp::NotEqual | CmpOp::Less | CmpOp::LessOrEqual) {
                    return false;
                }
                let x = self.eval_at(x, f, a);
                if matches!(x, Value::Err(_)) || matches!(v, Value::Err(_)) {
                    return false;
                }
                let c = |w: &Value| compare(v, w);
                match op {
                    CmpOp::Equal => c(&x) == Ordering::Equal,
                    CmpOp::NotEqual => c(&x) != Ordering::Equal,
                    CmpOp::Greater => c(&x) == Ordering::Greater,
                    CmpOp::Less => c(&x) == Ordering::Less,
                    CmpOp::GreaterOrEqual => c(&x) != Ordering::Less,
                    CmpOp::LessOrEqual => c(&x) != Ordering::Greater,
                    CmpOp::Between | CmpOp::NotBetween => {
                        let y = b.as_ref().map_or(Value::Empty, |b| self.eval_at(b, f, a));
                        let (lo, hi) = if compare(&x, &y) == Ordering::Greater { (y, x) } else { (x, y) };
                        let inside = c(&lo) != Ordering::Less && c(&hi) != Ordering::Greater;
                        inside == (*op == CmpOp::Between)
                    }
                }
            }
            RuleKind::Expression(e) => {
                let r = self.eval_at(e, f, a);
                matches!(r, Value::Num(_) | Value::Bool(_)) && to_bool(&r).unwrap_or(false)
            }
            RuleKind::Text { op, text } => {
                let s = match v {
                    Value::Err(_) => return false,
                    v => v.display().to_lowercase(),
                };
                let t = text.to_lowercase();
                match op {
                    TextOp::Contains => s.contains(&t),
                    TextOp::NotContains => !s.contains(&t),
                    TextOp::BeginsWith => s.starts_with(&t),
                    TextOp::EndsWith => s.ends_with(&t),
                }
            }
            RuleKind::Blanks(b) => (v.is_empty() || matches!(v, Value::Str(s) if s.trim().is_empty())) == *b,
            RuleKind::Errors(b) => matches!(v, Value::Err(_)) == *b,
            RuleKind::Top { n, percent, bottom } => {
                let Value::Num(x) = v else { return false };
                let st = self.stats(fi, ri, f);
                let len = st.nums.len();
                if len == 0 {
                    return false;
                }
                let k = if *percent { ((len as f64 * *n as f64 / 100.0).floor() as usize).max(1) } else { *n as usize }.clamp(1, len);
                if *bottom {
                    *x <= st.nums[k - 1]
                } else {
                    *x >= st.nums[len - k]
                }
            }
            RuleKind::Average { below, equal, std_dev } => {
                let Value::Num(x) = v else { return false };
                let st = self.stats(fi, ri, f);
                let n = st.nums.len() as f64;
                if n == 0.0 {
                    return false;
                }
                let mean = st.sum / n;
                let line = if *std_dev > 0 {
                    let var = st.nums.iter().map(|y| (y - mean) * (y - mean)).sum::<f64>() / (n - 1.0).max(1.0);
                    let d = var.sqrt() * *std_dev as f64;
                    if *below {
                        mean - d
                    } else {
                        mean + d
                    }
                } else {
                    mean
                };
                match (below, equal) {
                    (false, false) => *x > line,
                    (false, true) => *x >= line,
                    (true, false) => *x < line,
                    (true, true) => *x <= line,
                }
            }
            RuleKind::Duplicates { unique } => {
                let Some(k) = key(v) else { return false };
                let st = self.stats(fi, ri, f);
                let c = st.counts.get(&k).copied().unwrap_or(0);
                if *unique {
                    c == 1
                } else {
                    c > 1
                }
            }
            RuleKind::ColorScale(points) => {
                let Value::Num(x) = v else { return false };
                let st = self.stats(fi, ri, f);
                let ps: Vec<(f64, u32)> = points.iter().map(|(p, c)| (self.cfvo(p, &st, f, a), *c)).collect();
                if ps.len() < 2 {
                    return false;
                }
                out.scale = Some(scale_color(&ps, *x));
                false
            }
            RuleKind::DataBar { min, max, color } => {
                let Value::Num(x) = v else { return false };
                let st = self.stats(fi, ri, f);
                let (lo, hi) = (self.cfvo(min, &st, f, a), self.cfvo(max, &st, f, a));
                let frac = if hi > lo { ((x - lo) / (hi - lo)).clamp(0.0, 1.0) } else { 1.0 };
                // Excel keeps a sliver for the lowest value.
                out.bar = Some(((0.1 + 0.9 * frac) as f32, *color));
                false
            }
            RuleKind::IconSet { set, points, reverse, show_value } => {
                let Value::Num(x) = v else { return false };
                let n = ICON_SETS.get(*set as usize).map_or(0, |s| s.1) as usize;
                if n == 0 || points.len() != n {
                    return false;
                }
                let st = self.stats(fi, ri, f);
                // The highest threshold the value meets; below the first, icon 0.
                let mut k = 0;
                for (i, (p, gte)) in points.iter().enumerate().skip(1) {
                    let t = self.cfvo(p, &st, f, a);
                    if if *gte { *x >= t } else { *x > t } {
                        k = i;
                    }
                }
                let k = if *reverse { n - 1 - k } else { k };
                out.icon = Some((*set, k as u8));
                out.hide_value = !show_value;
                false
            }
        }
    }

    fn cfvo(&self, p: &Cfvo, st: &Stats, f: &CondFormat, a: CellAddr) -> f64 {
        let (lo, hi) = (st.nums.first().copied().unwrap_or(0.0), st.nums.last().copied().unwrap_or(0.0));
        match p {
            Cfvo::Min => lo,
            Cfvo::Max => hi,
            Cfvo::Num(x) => *x,
            Cfvo::Percent(p) => lo + (hi - lo) * p / 100.0,
            Cfvo::Percentile(p) => percentile(&st.nums, p / 100.0),
            Cfvo::Formula(e) => {
                let (r, c) = f.origin();
                crate::eval::to_num(&self.eval_at(e, f, CellAddr::new(a.sheet, r, c))).unwrap_or(0.0)
            }
        }
    }

    /// A range rule's statistics, made once per recalculation.
    fn stats(&self, fi: usize, ri: usize, f: &CondFormat) -> Arc<Stats> {
        let gen = self.recalcs;
        if let Some((g, s)) = self.cond_stats.lock().unwrap().get(&(fi, ri)) {
            if *g == gen {
                return s.clone();
            }
        }
        let mut st = Stats::default();
        let mut budget = MAX_STAT_CELLS;
        for r in &f.ranges {
            let Some(r) = Ctx::new(self, f.sheet, 0, 0).clip(r) else { continue };
            self.each(&r, &mut |_, _, v| {
                if budget == 0 {
                    return;
                }
                budget -= 1;
                if let Value::Num(x) = v {
                    st.nums.push(x);
                    st.sum += x;
                }
                if let Some(k) = key(&v) {
                    *st.counts.entry(k).or_default() += 1;
                }
            });
        }
        st.nums.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
        let st = Arc::new(st);
        self.cond_stats.lock().unwrap().insert((fi, ri), (gen, st.clone()));
        st
    }
}

/// Excel's PERCENTILE (inclusive) of sorted numbers.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let h = (sorted.len() - 1) as f64 * p.clamp(0.0, 1.0);
    let i = h.floor() as usize;
    let f = h - i as f64;
    match sorted.get(i + 1) {
        Some(n) => sorted[i] + f * (n - sorted[i]),
        None => sorted[i],
    }
}

/// The colour at `x` on a scale of (value, 0xRRGGBB) points, mixed in sRGB as Excel does.
pub fn scale_color(points: &[(f64, u32)], x: f64) -> u32 {
    let (first, last) = (points[0], points[points.len() - 1]);
    if x <= first.0 {
        return first.1;
    }
    if x >= last.0 {
        return last.1;
    }
    for w in points.windows(2) {
        let ((a, ca), (b, cb)) = (w[0], w[1]);
        if x <= b {
            let t = if b > a { (x - a) / (b - a) } else { 1.0 };
            let ch = |s: u32| {
                let (p, q) = ((ca >> s) & 0xFF, (cb >> s) & 0xFF);
                ((p as f64 + (q as f64 - p as f64) * t).round() as u32) << s
            };
            return ch(16) | ch(8) | ch(0);
        }
    }
    last.1
}

fn merge(into: &mut Dxf, d: &Dxf) {
    // Earlier rules win: only what is still unset is taken.
    into.color = into.color.or(d.color);
    into.bold = into.bold.or(d.bold);
    into.italic = into.italic.or(d.italic);
    into.underline = into.underline.or(d.underline);
    into.strike = into.strike.or(d.strike);
    into.fill = into.fill.or(d.fill);
    into.num_fmt = into.num_fmt.clone().or(d.num_fmt.clone());
}

// ---------- the native file ----------

fn opt_color(w: &mut Writer, c: Option<u32>) {
    w.u32(c.map_or(u32::MAX, |c| c & 0xFF_FFFF));
}
fn read_color(r: &mut Reader) -> Result<Option<u32>, WireError> {
    Ok(match r.u32()? {
        u32::MAX => None,
        c if c <= 0xFF_FFFF => Some(c),
        _ => return Err(WireError::Invalid("conditional format colour")),
    })
}
fn tri(w: &mut Writer, b: Option<bool>) {
    w.u8(match b {
        None => 2,
        Some(false) => 0,
        Some(true) => 1,
    });
}
fn read_tri(r: &mut Reader) -> Result<Option<bool>, WireError> {
    Ok(match r.u8()? {
        0 => Some(false),
        1 => Some(true),
        2 => None,
        _ => return Err(WireError::Invalid("conditional format flag")),
    })
}

impl CondFormat {
    pub fn encode(&self, w: &mut Writer, wb: &Workbook) {
        let origin = self.origin();
        let ftext = |e: &Expr| wb.cond_formula_text(e, origin);
        w.u16(self.sheet);
        w.var(self.ranges.len() as u64);
        for r in &self.ranges {
            for v in [r.r0, r.c0, r.r1, r.c1] {
                w.var(v as u64);
            }
        }
        w.var(self.rules.len() as u64);
        for rule in &self.rules {
            let d = &rule.dxf;
            opt_color(w, d.color);
            opt_color(w, d.fill);
            for b in [d.bold, d.italic, d.underline, d.strike] {
                tri(w, b);
            }
            w.str(d.num_fmt.as_deref().unwrap_or(""));
            w.u8(rule.stop as u8);
            let cfvo = |w: &mut Writer, p: &Cfvo| match p {
                Cfvo::Min => w.u8(0),
                Cfvo::Max => w.u8(1),
                Cfvo::Num(x) => {
                    w.u8(2);
                    w.f64(*x);
                }
                Cfvo::Percent(x) => {
                    w.u8(3);
                    w.f64(*x);
                }
                Cfvo::Percentile(x) => {
                    w.u8(4);
                    w.f64(*x);
                }
                Cfvo::Formula(e) => {
                    w.u8(5);
                    w.str(&ftext(e));
                }
            };
            match &rule.kind {
                RuleKind::CellIs { op, a, b } => {
                    w.u8(0);
                    w.u8(*op as u8);
                    w.str(&ftext(a));
                    w.str(&b.as_ref().map_or(String::new(), |b| ftext(b)));
                }
                RuleKind::Expression(e) => {
                    w.u8(1);
                    w.str(&ftext(e));
                }
                RuleKind::Text { op, text } => {
                    w.u8(2);
                    w.u8(*op as u8);
                    w.str(text);
                }
                RuleKind::Blanks(b) => {
                    w.u8(3);
                    w.u8(*b as u8);
                }
                RuleKind::Errors(b) => {
                    w.u8(4);
                    w.u8(*b as u8);
                }
                RuleKind::Top { n, percent, bottom } => {
                    w.u8(5);
                    w.var(*n as u64);
                    w.u8(*percent as u8 | (*bottom as u8) << 1);
                }
                RuleKind::Average { below, equal, std_dev } => {
                    w.u8(6);
                    w.u8(*below as u8 | (*equal as u8) << 1);
                    w.u8(*std_dev);
                }
                RuleKind::Duplicates { unique } => {
                    w.u8(7);
                    w.u8(*unique as u8);
                }
                RuleKind::ColorScale(ps) => {
                    w.u8(8);
                    w.var(ps.len() as u64);
                    for (p, c) in ps {
                        cfvo(w, p);
                        w.u32(*c);
                    }
                }
                RuleKind::DataBar { min, max, color } => {
                    w.u8(9);
                    cfvo(w, min);
                    cfvo(w, max);
                    w.u32(*color);
                }
                RuleKind::IconSet { set, points, reverse, show_value } => {
                    w.u8(10);
                    w.u8(*set);
                    w.u8(*reverse as u8 | (*show_value as u8) << 1);
                    w.var(points.len() as u64);
                    for (p, gte) in points {
                        cfvo(w, p);
                        w.u8(*gte as u8);
                    }
                }
            }
        }
    }

    /// Reads one format (its sheets must exist in `wb`, for its formulas).
    pub fn decode(r: &mut Reader, wb: &Workbook) -> Result<CondFormat, WireError> {
        let sheet = r.u16()?;
        if sheet as usize >= wb.sheet_count() {
            return Err(WireError::Invalid("conditional format sheet"));
        }
        let n = r.count("ranges", MAX_RANGES as u64, 4)?;
        let mut ranges = Vec::with_capacity(n);
        for _ in 0..n {
            let mut v = [0u32; 4];
            for (i, x) in v.iter_mut().enumerate() {
                let y = r.var()?;
                if y >= if i % 2 == 0 { MAX_ROWS } else { MAX_COLS } as u64 {
                    return Err(WireError::Invalid("conditional format range"));
                }
                *x = y as u32;
            }
            if v[0] > v[2] || v[1] > v[3] {
                return Err(WireError::Invalid("conditional format range"));
            }
            ranges.push(Rect { sheet, r0: v[0], c0: v[1], r1: v[2], c1: v[3] });
        }
        let origin = ranges.first().map_or((0, 0), |r| (r.r0, r.c0));
        let fx = |r: &mut Reader| -> Result<Arc<Expr>, WireError> {
            let t = r.str("formula", formula::MAX_STORED_FORMULA_LEN as u64)?;
            wb.parse_stored(&format!("={t}"), origin.0, origin.1).map(Arc::new).map_err(|_| WireError::Invalid("conditional format formula"))
        };
        let k = r.count("rules", MAX_RULES as u64, 8)?;
        let mut rules = Vec::with_capacity(k);
        for _ in 0..k {
            let color = read_color(r)?;
            let fill = read_color(r)?;
            let (bold, italic, underline, strike) = (read_tri(r)?, read_tri(r)?, read_tri(r)?, read_tri(r)?);
            let nf = r.str("number format", 255)?;
            let dxf = Dxf { color, bold, italic, underline, strike, fill, num_fmt: (!nf.is_empty()).then(|| Arc::from(nf)) };
            let stop = r.u8()? != 0;
            let cfvo = |r: &mut Reader| -> Result<Cfvo, WireError> {
                Ok(match r.u8()? {
                    0 => Cfvo::Min,
                    1 => Cfvo::Max,
                    2 => Cfvo::Num(finite(r.f64()?)?),
                    3 => Cfvo::Percent(finite(r.f64()?)?),
                    4 => Cfvo::Percentile(finite(r.f64()?)?),
                    5 => Cfvo::Formula(fx(r)?),
                    _ => return Err(WireError::Invalid("conditional format point")),
                })
            };
            let kind = match r.u8()? {
                0 => {
                    let op = *CmpOp::ALL.get(r.u8()? as usize).ok_or(WireError::Invalid("comparison"))?;
                    let a = fx(r)?;
                    let bt = r.str("formula", formula::MAX_STORED_FORMULA_LEN as u64)?;
                    let b = if bt.is_empty() { None } else { Some(wb.parse_stored(&format!("={bt}"), origin.0, origin.1).map(Arc::new).map_err(|_| WireError::Invalid("conditional format formula"))?) };
                    RuleKind::CellIs { op, a, b }
                }
                1 => RuleKind::Expression(fx(r)?),
                2 => {
                    let op = *TextOp::ALL.get(r.u8()? as usize).ok_or(WireError::Invalid("text rule"))?;
                    RuleKind::Text { op, text: Arc::from(r.str("text", 8192)?) }
                }
                3 => RuleKind::Blanks(r.u8()? != 0),
                4 => RuleKind::Errors(r.u8()? != 0),
                5 => {
                    let n = r.var()?.min(u32::MAX as u64) as u32;
                    let b = r.u8()?;
                    RuleKind::Top { n, percent: b & 1 != 0, bottom: b & 2 != 0 }
                }
                6 => {
                    let b = r.u8()?;
                    RuleKind::Average { below: b & 1 != 0, equal: b & 2 != 0, std_dev: r.u8()?.min(3) }
                }
                7 => RuleKind::Duplicates { unique: r.u8()? != 0 },
                8 => {
                    let n = r.count("scale points", 3, 2)?;
                    if n < 2 {
                        return Err(WireError::Invalid("colour scale"));
                    }
                    let mut ps = Vec::with_capacity(n);
                    for _ in 0..n {
                        let p = cfvo(r)?;
                        ps.push((p, r.u32()? & 0xFF_FFFF));
                    }
                    RuleKind::ColorScale(ps)
                }
                9 => {
                    let min = cfvo(r)?;
                    let max = cfvo(r)?;
                    RuleKind::DataBar { min, max, color: r.u32()? & 0xFF_FFFF }
                }
                10 => {
                    let set = r.u8()?;
                    let Some(&(_, n)) = ICON_SETS.get(set as usize) else { return Err(WireError::Invalid("icon set")) };
                    let b = r.u8()?;
                    let k = r.count("icon points", 5, 2)?;
                    if k != n as usize {
                        return Err(WireError::Invalid("icon set points"));
                    }
                    let mut points = Vec::with_capacity(k);
                    for _ in 0..k {
                        let p = cfvo(r)?;
                        points.push((p, r.u8()? != 0));
                    }
                    RuleKind::IconSet { set, points, reverse: b & 1 != 0, show_value: b & 2 != 0 }
                }
                _ => return Err(WireError::Invalid("conditional format rule")),
            };
            rules.push(Rule { kind, dxf, stop });
        }
        if ranges.is_empty() || rules.is_empty() {
            return Err(WireError::Invalid("empty conditional format"));
        }
        Ok(CondFormat { sheet, ranges, rules })
    }
}

fn finite(x: f64) -> Result<f64, WireError> {
    if x.is_finite() {
        Ok(x)
    } else {
        Err(WireError::Invalid("conditional format number"))
    }
}

/// Text comparison for sorting rules' text (kept for callers).
pub fn text_order(a: &str, b: &str) -> Ordering {
    cmp_text(a, b)
}
