//! Number formats: Excel's format codes (ECMA-376 Part 1 §18.8.31), as Excel shows
//! them in the en-US locale.
//!
//! A code has up to four sections, `positive;negative;zero;text`, each with optional
//! `[Color]` and `[condition]` brackets. A section is a number pattern (`#,##0.00`,
//! `0%`, `0.00E+00`, `# ?/?`), a date and time pattern (`yyyy-mm-dd h:mm AM/PM`,
//! `[h]:mm:ss`), `General`, or text (`@`), with literals (`"..."`, `\x`), padding
//! (`_x`) and fill (`*x`, shown once: the grid does not repeat characters).
//!
//! [`NumFmt::parse`] never fails: what it cannot read is shown as a literal, and
//! [`NumFmt::is_exact`] says whether anything was guessed. [`NumFmt::format`] gives the
//! text and the section's colour.

use crate::eval::dates;
use crate::value::{format_general, Value};

/// A `[Color]` in a section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FmtColor {
    Black,
    Blue,
    Cyan,
    Green,
    Magenta,
    Red,
    White,
    Yellow,
    /// `[Color1]`..`[Color56]`: the legacy palette index (1-based).
    Indexed(u8),
}

impl FmtColor {
    /// sRGB of the colour (the legacy palette for `ColorN`).
    pub fn rgb(self) -> u32 {
        match self {
            FmtColor::Black => 0x000000,
            FmtColor::Blue => 0x0000FF,
            FmtColor::Cyan => 0x00FFFF,
            FmtColor::Green => 0x00FF00,
            FmtColor::Magenta => 0xFF00FF,
            FmtColor::Red => 0xFF0000,
            FmtColor::White => 0xFFFFFF,
            FmtColor::Yellow => 0xFFFF00,
            FmtColor::Indexed(i) => PALETTE.get(i as usize + 7).copied().unwrap_or(0),
        }
    }
}

/// The 64-entry legacy palette (ECMA-376 §18.8.27 `indexedColors`); `ColorN` is entry
/// N + 7.
pub const PALETTE: [u32; 64] = [
    0x000000, 0xFFFFFF, 0xFF0000, 0x00FF00, 0x0000FF, 0xFFFF00, 0xFF00FF, 0x00FFFF, 0x000000, 0xFFFFFF, 0xFF0000, 0x00FF00, 0x0000FF, 0xFFFF00, 0xFF00FF, 0x00FFFF,
    0x800000, 0x008000, 0x000080, 0x808000, 0x800080, 0x008080, 0xC0C0C0, 0x808080, 0x9999FF, 0x993366, 0xFFFFCC, 0xCCFFFF, 0x660066, 0xFF8080, 0x0066CC, 0xCCCCFF,
    0x000080, 0xFF00FF, 0xFFFF00, 0x00FFFF, 0x800080, 0x800000, 0x008080, 0x0000FF, 0x00CCFF, 0xCCFFFF, 0xCCFFCC, 0xFFFF99, 0x99CCFF, 0xFF99CC, 0xCC99FF, 0xFFCC99,
    0x3366FF, 0x33CCCC, 0x99CC00, 0xFFCC00, 0xFF9900, 0xFF6600, 0x666699, 0x969696, 0x003366, 0x339966, 0x003300, 0x333300, 0x993300, 0x993366, 0x333399, 0x333333,
];

#[derive(Debug, Clone, Copy, PartialEq)]
enum Cmp {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
}

impl Cmp {
    fn test(self, x: f64, k: f64) -> bool {
        match self {
            Cmp::Lt => x < k,
            Cmp::Le => x <= k,
            Cmp::Gt => x > k,
            Cmp::Ge => x >= k,
            Cmp::Eq => x == k,
            Cmp::Ne => x != k,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Lit(String),
    /// `0`, `#`, `?`.
    Digit(u8),
    Point,
    /// A comma between digit placeholders: thousands separators.
    Group,
    Percent,
    /// `E+` / `E-` and the exponent's placeholders.
    Exp { plus: bool, digits: Vec<u8> },
    /// `/` in a fraction; the denominator's placeholders, or a fixed denominator.
    Slash { den: Vec<u8>, fixed: Option<u32> },
    Text,
    General,
    Year(u8),
    Month(u8),
    Day(u8),
    Hour(u8),
    Minute(u8),
    Second(u8),
    /// `.0`, `.00`, `.000` after seconds.
    SubSec(u8),
    /// `AM/PM` (true) or `A/P` (false), with the case written.
    AmPm { long: bool, upper: bool },
    /// `[h]`, `[m]`, `[s]`: elapsed time; the letter and its count.
    Elapsed(char, u8),
}

#[derive(Debug, Clone, PartialEq, Default)]
struct Section {
    toks: Vec<Tok>,
    color: Option<FmtColor>,
    cond: Option<(Cmp, f64)>,
    /// Trailing commas: divide by 1000 each.
    scale: i32,
}

impl Section {
    fn is_date(&self) -> bool {
        self.toks.iter().any(|t| matches!(t, Tok::Year(_) | Tok::Month(_) | Tok::Day(_) | Tok::Hour(_) | Tok::Minute(_) | Tok::Second(_) | Tok::Elapsed(..) | Tok::AmPm { .. }))
    }
    fn has_text(&self) -> bool {
        self.toks.iter().any(|t| matches!(t, Tok::Text))
    }
    fn is_general(&self) -> bool {
        self.toks.iter().any(|t| matches!(t, Tok::General))
    }
}

/// A parsed format code.
#[derive(Debug, Clone, PartialEq)]
pub struct NumFmt {
    code: String,
    sections: Vec<Section>,
    exact: bool,
}

/// A formatted value: the text, the section's colour, and whether it overflowed
/// (a date out of range: Excel shows `#` across the cell).
#[derive(Debug, Clone, PartialEq)]
pub struct Formatted {
    pub text: String,
    pub color: Option<FmtColor>,
    pub hashes: bool,
}

/// The built-in format codes Excel uses for `numFmtId` 0..=49 (en-US), or `None` for
/// an id with no code (Excel's locale-dependent ids show as General here).
pub fn builtin(id: u32) -> Option<&'static str> {
    Some(match id {
        0 => "General",
        1 => "0",
        2 => "0.00",
        3 => "#,##0",
        4 => "#,##0.00",
        5 => "\"$\"#,##0_);\\(\"$\"#,##0\\)",
        6 => "\"$\"#,##0_);[Red]\\(\"$\"#,##0\\)",
        7 => "\"$\"#,##0.00_);\\(\"$\"#,##0.00\\)",
        8 => "\"$\"#,##0.00_);[Red]\\(\"$\"#,##0.00\\)",
        9 => "0%",
        10 => "0.00%",
        11 => "0.00E+00",
        12 => "# ?/?",
        13 => "# ??/??",
        14 => "m/d/yyyy",
        15 => "d-mmm-yy",
        16 => "d-mmm",
        17 => "mmm-yy",
        18 => "h:mm AM/PM",
        19 => "h:mm:ss AM/PM",
        20 => "h:mm",
        21 => "h:mm:ss",
        22 => "m/d/yyyy h:mm",
        37 => "#,##0 ;(#,##0)",
        38 => "#,##0 ;[Red](#,##0)",
        39 => "#,##0.00;(#,##0.00)",
        40 => "#,##0.00;[Red](#,##0.00)",
        41 => "_(* #,##0_);_(* \\(#,##0\\);_(* \"-\"_);_(@_)",
        42 => "_(\"$\"* #,##0_);_(\"$\"* \\(#,##0\\);_(\"$\"* \"-\"_);_(@_)",
        43 => "_(* #,##0.00_);_(* \\(#,##0.00\\);_(* \"-\"??_);_(@_)",
        44 => "_(\"$\"* #,##0.00_);_(\"$\"* \\(#,##0.00\\);_(\"$\"* \"-\"??_);_(@_)",
        45 => "mm:ss",
        46 => "[h]:mm:ss",
        47 => "mmss.0",
        48 => "##0.0E+0",
        49 => "@",
        _ => return None,
    })
}

/// The built-in id of a code, if it is one (for writing `.xlsx`).
pub fn builtin_id(code: &str) -> Option<u32> {
    (0..=49).find(|&i| builtin(i) == Some(code))
}

const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
const DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];

impl NumFmt {
    pub fn general() -> Self {
        NumFmt::parse("General")
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    /// Everything in the code was understood.
    pub fn is_exact(&self) -> bool {
        self.exact
    }

    pub fn is_general(&self) -> bool {
        self.sections.len() == 1 && self.sections[0].toks == [Tok::General]
    }

    /// A date or time format (its first section shows a date or a time).
    pub fn is_date(&self) -> bool {
        self.sections.first().is_some_and(|s| s.is_date())
    }

    pub fn parse(code: &str) -> Self {
        let mut exact = true;
        let mut sections = Vec::new();
        for raw in split_sections(code) {
            let (s, ok) = parse_section(&raw);
            exact &= ok;
            sections.push(s);
            if sections.len() == 4 {
                break;
            }
        }
        if sections.is_empty() {
            sections.push(Section { toks: vec![Tok::General], ..Section::default() });
        }
        NumFmt { code: code.to_string(), sections, exact }
    }

    /// How `v` shows in a cell with this format. Booleans and errors are not formatted.
    pub fn format(&self, v: &Value) -> Formatted {
        let plain = |text: String| Formatted { text, color: None, hashes: false };
        match v {
            Value::Empty => plain(String::new()),
            Value::Bool(_) | Value::Err(_) => plain(v.display()),
            Value::Str(s) => {
                // The fourth section, or a lone section with `@`.
                let sec = match self.sections.len() {
                    4 => Some(&self.sections[3]),
                    _ => self.sections.iter().find(|s| s.has_text()),
                };
                match sec {
                    Some(sec) => Formatted { text: text_section(sec, s), color: sec.color, hashes: false },
                    None => plain(s.to_string()),
                }
            }
            Value::Num(x) => self.format_num(*x),
        }
    }

    fn format_num(&self, x: f64) -> Formatted {
        let (sec, neg_sign) = self.pick(x);
        let Some(sec) = sec else {
            return Formatted { text: format_general(x), color: None, hashes: false };
        };
        // A section for negatives shows the magnitude; its own literals say "negative".
        let v = if neg_sign { x } else { x.abs() };
        let (text, hashes) = if sec.is_date() {
            match format_date(sec, v) {
                Some(t) => (t, false),
                None => ("#".repeat(11), true),
            }
        } else if sec.is_general() {
            let g = format_general(v);
            (sec.toks.iter().map(|t| if let Tok::General = t { g.clone() } else { lit_of(t) }).collect(), false)
        } else if sec.has_text() && !sec.toks.iter().any(|t| matches!(t, Tok::Digit(_))) {
            // A text-only section for a number: the number as General.
            (text_section(sec, &format_general(v)), false)
        } else {
            (format_number(sec, v), false)
        };
        Formatted { text, color: sec.color, hashes }
    }

    /// The section for `x`, and whether the number keeps its own minus sign.
    fn pick(&self, x: f64) -> (Option<&Section>, bool) {
        let s = &self.sections;
        let num: Vec<&Section> = s.iter().take(3).filter(|s| !(s.has_text() && !s.toks.iter().any(|t| matches!(t, Tok::Digit(_) | Tok::General)) && !s.is_date())).collect();
        let num = if num.is_empty() { s.iter().take(1).collect() } else { num };
        let conds = s.iter().any(|s| s.cond.is_some());
        if conds {
            // Conditions: the first two sections are tried in order; the third (or the
            // second, if it has none) takes the rest.
            for (i, sec) in s.iter().take(2).enumerate() {
                if let Some((c, k)) = sec.cond {
                    if c.test(x, k) {
                        // A section for "less than" a non-positive bound shows the
                        // magnitude, as the default negative section does.
                        let own_sign = !(matches!(c, Cmp::Lt | Cmp::Le) && k <= 0.0) || i == 0 && s.len() == 1;
                        return (Some(sec), own_sign);
                    }
                } else if i > 0 {
                    return (Some(sec), true);
                }
            }
            return match s.get(2).or(s.get(1)).filter(|t| t.cond.is_none()) {
                Some(sec) => (Some(sec), true),
                None => (None, true),
            };
        }
        match num.len() {
            1 => (Some(num[0]), true),
            2 => {
                if x < 0.0 {
                    (Some(num[1]), false)
                } else {
                    (Some(num[0]), true)
                }
            }
            _ => {
                if x > 0.0 {
                    (Some(num[0]), true)
                } else if x < 0.0 {
                    (Some(num[1]), false)
                } else {
                    (Some(num[2]), true)
                }
            }
        }
    }
}

/// Split at `;` outside quotes, brackets and escapes.
fn split_sections(code: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut it = code.chars().peekable();
    while let Some(c) = it.next() {
        let cur = out.last_mut().unwrap();
        match c {
            '"' => {
                cur.push(c);
                for d in it.by_ref() {
                    cur.push(d);
                    if d == '"' {
                        break;
                    }
                }
            }
            '\\' | '_' | '*' => {
                cur.push(c);
                if let Some(d) = it.next() {
                    cur.push(d);
                }
            }
            '[' => {
                cur.push(c);
                for d in it.by_ref() {
                    cur.push(d);
                    if d == ']' {
                        break;
                    }
                }
            }
            ';' => out.push(String::new()),
            _ => cur.push(c),
        }
    }
    out
}

fn color_named(s: &str) -> Option<FmtColor> {
    let l = s.to_ascii_lowercase();
    Some(match l.as_str() {
        "black" => FmtColor::Black,
        "blue" => FmtColor::Blue,
        "cyan" => FmtColor::Cyan,
        "green" => FmtColor::Green,
        "magenta" => FmtColor::Magenta,
        "red" => FmtColor::Red,
        "white" => FmtColor::White,
        "yellow" => FmtColor::Yellow,
        _ => {
            let n: u8 = l.strip_prefix("color")?.parse().ok()?;
            if !(1..=56).contains(&n) {
                return None;
            }
            FmtColor::Indexed(n)
        }
    })
}

fn parse_section(src: &str) -> (Section, bool) {
    let mut sec = Section::default();
    let mut ok = true;
    let ch: Vec<char> = src.chars().collect();
    let mut toks: Vec<Tok> = Vec::new();
    let lit = |toks: &mut Vec<Tok>, s: &str| match toks.last_mut() {
        Some(Tok::Lit(l)) => l.push_str(s),
        _ => toks.push(Tok::Lit(s.to_string())),
    };
    let mut i = 0;
    let lower = |c: char| c.to_ascii_lowercase();
    let run = |i: usize, c: char| ch[i..].iter().take_while(|&&d| lower(d) == c).count();
    while i < ch.len() {
        let c = ch[i];
        match c {
            '"' => {
                let mut s = String::new();
                i += 1;
                while i < ch.len() && ch[i] != '"' {
                    s.push(ch[i]);
                    i += 1;
                }
                i += 1;
                lit(&mut toks, &s);
            }
            '\\' => {
                if let Some(&d) = ch.get(i + 1) {
                    lit(&mut toks, &d.to_string());
                }
                i += 2;
            }
            // Padding the width of a character: a space here.
            '_' => {
                lit(&mut toks, " ");
                i += 2;
            }
            // Fill: Excel repeats it across the cell; the grid shows it once.
            '*' => {
                if let Some(&d) = ch.get(i + 1) {
                    lit(&mut toks, &d.to_string());
                }
                i += 2;
            }
            '[' => {
                let end = ch[i..].iter().position(|&d| d == ']').map(|p| i + p).unwrap_or(ch.len());
                let inner: String = ch[i + 1..end.min(ch.len())].iter().collect();
                i = end + 1;
                let il = inner.to_ascii_lowercase();
                if let Some(col) = color_named(&inner) {
                    sec.color = Some(col);
                } else if let Some(rest) = inner.strip_prefix('$') {
                    // [$€-407]: currency text, then a locale.
                    let sym = rest.split('-').next().unwrap_or("");
                    lit(&mut toks, sym);
                } else if !il.is_empty() && il.chars().all(|d| d == 'h') {
                    toks.push(Tok::Elapsed('h', il.len() as u8));
                } else if !il.is_empty() && il.chars().all(|d| d == 'm') {
                    toks.push(Tok::Elapsed('m', il.len() as u8));
                } else if !il.is_empty() && il.chars().all(|d| d == 's') {
                    toks.push(Tok::Elapsed('s', il.len() as u8));
                } else if let Some(c) = parse_cond(&inner) {
                    sec.cond = Some(c);
                } else {
                    // [DBNum1], [natnum], calendars: not shown differently here.
                    ok = false;
                }
            }
            '0' | '#' | '?' => {
                toks.push(Tok::Digit(c as u8));
                i += 1;
            }
            '.' => {
                // `.0` after seconds: fractions of a second.
                let after_sec = toks.iter().rev().find(|t| !matches!(t, Tok::Lit(_))).is_some_and(|t| matches!(t, Tok::Second(_) | Tok::Elapsed('s', _)));
                let zeros = ch[i + 1..].iter().take_while(|&&d| d == '0').count();
                if after_sec && zeros > 0 {
                    toks.push(Tok::SubSec(zeros.min(3) as u8));
                    i += 1 + zeros;
                } else {
                    toks.push(Tok::Point);
                    i += 1;
                }
            }
            ',' => {
                toks.push(Tok::Group);
                i += 1;
            }
            '%' => {
                toks.push(Tok::Percent);
                i += 1;
            }
            '@' => {
                toks.push(Tok::Text);
                i += 1;
            }
            'E' | 'e' if matches!(ch.get(i + 1), Some('+') | Some('-')) => {
                let plus = ch[i + 1] == '+';
                i += 2;
                let mut digits = Vec::new();
                while i < ch.len() && matches!(ch[i], '0' | '#' | '?') {
                    digits.push(ch[i] as u8);
                    i += 1;
                }
                toks.push(Tok::Exp { plus, digits });
            }
            '/' if toks.iter().any(|t| matches!(t, Tok::Digit(_))) && !toks.iter().any(|t| matches!(t, Tok::Slash { .. })) && (matches!(ch.get(i + 1), Some('0'..='9' | '#' | '?'))) => {
                i += 1;
                let mut den = Vec::new();
                let mut fixed = String::new();
                while i < ch.len() && matches!(ch[i], '0'..='9' | '#' | '?') {
                    if ch[i].is_ascii_digit() && ch[i] != '0' || !fixed.is_empty() && ch[i].is_ascii_digit() {
                        fixed.push(ch[i]);
                    } else {
                        den.push(ch[i] as u8);
                    }
                    i += 1;
                }
                let fixed = if fixed.is_empty() { None } else { fixed.parse().ok().filter(|&d: &u32| d > 0) };
                toks.push(Tok::Slash { den, fixed });
            }
            _ => {
                let l = lower(c);
                if src[byte_at(&ch, i)..].len() >= 7 && ch[i..i + 7].iter().collect::<String>().eq_ignore_ascii_case("general") {
                    toks.push(Tok::General);
                    i += 7;
                } else if ch.len() >= i + 5 && ch[i..i + 5].iter().collect::<String>().eq_ignore_ascii_case("am/pm") {
                    toks.push(Tok::AmPm { long: true, upper: c == 'A' });
                    i += 5;
                } else if ch.len() >= i + 3 && ch[i..i + 3].iter().collect::<String>().eq_ignore_ascii_case("a/p") {
                    toks.push(Tok::AmPm { long: false, upper: c == 'A' });
                    i += 3;
                } else if matches!(l, 'y' | 'm' | 'd' | 'h' | 's') {
                    let n = run(i, l);
                    i += n;
                    let n = n.min(255) as u8;
                    toks.push(match l {
                        'y' => Tok::Year(if n <= 2 { 2 } else { 4 }),
                        'm' => Tok::Month(n.min(5)),
                        'd' => Tok::Day(n.min(4)),
                        'h' => Tok::Hour(n.min(2)),
                        _ => Tok::Second(n.min(2)),
                    });
                } else if l == 'b' && matches!(ch.get(i + 1), Some('1') | Some('2')) {
                    // b1/b2: Buddhist years; shown Gregorian.
                    ok = false;
                    i += 2;
                } else {
                    if c.is_ascii_alphabetic() && !matches!(c, 'E' | 'e') {
                        // Excel takes a few letters bare; anything else it would refuse.
                        ok &= " $-+/():!^&'~{}<>=".contains(c);
                    }
                    lit(&mut toks, &c.to_string());
                    i += 1;
                }
            }
        }
    }
    // `m` next to an hour or before seconds is minutes.
    for k in 0..toks.len() {
        if let Tok::Month(n) = toks[k] {
            if n > 2 {
                continue;
            }
            let prev = toks[..k].iter().rev().find(|t| !matches!(t, Tok::Lit(_)));
            let next = toks[k + 1..].iter().find(|t| !matches!(t, Tok::Lit(_)));
            if matches!(prev, Some(Tok::Hour(_) | Tok::Elapsed('h', _))) || matches!(next, Some(Tok::Second(_) | Tok::Elapsed('s', _))) {
                toks[k] = Tok::Minute(n);
            }
        }
    }
    // Commas: between digit placeholders they group thousands; after the last digit
    // placeholder (before the point or the end) each divides by 1000.
    let is_date = toks.iter().any(|t| matches!(t, Tok::Year(_) | Tok::Month(_) | Tok::Day(_) | Tok::Hour(_) | Tok::Minute(_) | Tok::Second(_) | Tok::Elapsed(..)));
    if is_date {
        for t in toks.iter_mut() {
            match t {
                Tok::Group => *t = Tok::Lit(",".into()),
                Tok::Point => *t = Tok::Lit(".".into()),
                Tok::Digit(d) => *t = Tok::Lit((*d as char).to_string()),
                Tok::Percent => *t = Tok::Lit("%".into()),
                _ => {}
            }
        }
    } else {
        let mut grouped = false;
        let mut k = 0;
        while k < toks.len() {
            if toks[k] == Tok::Group {
                let before = toks[..k].iter().any(|t| matches!(t, Tok::Digit(_)));
                let next_digit = toks[k + 1..].iter().find(|t| !matches!(t, Tok::Group)).is_some_and(|t| matches!(t, Tok::Digit(_)));
                let in_int = !toks[..k].iter().any(|t| matches!(t, Tok::Point | Tok::Exp { .. } | Tok::Slash { .. }));
                if before && next_digit && in_int {
                    grouped = true;
                    toks.remove(k);
                    continue;
                } else if before {
                    sec.scale += 1;
                    toks.remove(k);
                    continue;
                } else {
                    toks[k] = Tok::Lit(",".into());
                }
            }
            k += 1;
        }
        if grouped {
            // A marker at the front: the integer part groups thousands.
            toks.insert(0, Tok::Group);
        }
    }
    // Merge adjacent literals.
    let mut merged: Vec<Tok> = Vec::with_capacity(toks.len());
    for t in toks {
        match (merged.last_mut(), t) {
            (Some(Tok::Lit(a)), Tok::Lit(b)) => a.push_str(&b),
            (_, t) => merged.push(t),
        }
    }
    sec.toks = merged;
    (sec, ok)
}

fn byte_at(ch: &[char], i: usize) -> usize {
    ch[..i].iter().map(|c| c.len_utf8()).sum()
}

fn parse_cond(s: &str) -> Option<(Cmp, f64)> {
    let (c, rest) = if let Some(r) = s.strip_prefix("<=") {
        (Cmp::Le, r)
    } else if let Some(r) = s.strip_prefix(">=") {
        (Cmp::Ge, r)
    } else if let Some(r) = s.strip_prefix("<>") {
        (Cmp::Ne, r)
    } else if let Some(r) = s.strip_prefix('<') {
        (Cmp::Lt, r)
    } else if let Some(r) = s.strip_prefix('>') {
        (Cmp::Gt, r)
    } else if let Some(r) = s.strip_prefix('=') {
        (Cmp::Eq, r)
    } else {
        return None;
    };
    Some((c, rest.trim().parse().ok()?))
}

fn lit_of(t: &Tok) -> String {
    match t {
        Tok::Lit(s) => s.clone(),
        Tok::Percent => "%".into(),
        Tok::Point => ".".into(),
        _ => String::new(),
    }
}

fn text_section(sec: &Section, s: &str) -> String {
    let mut out = String::new();
    for t in &sec.toks {
        match t {
            Tok::Text => out.push_str(s),
            Tok::Lit(l) => out.push_str(l),
            Tok::Percent => out.push('%'),
            Tok::Point => out.push('.'),
            _ => {}
        }
    }
    out
}

/// `x` rounded to `d` decimals, half away from zero, from its 15 significant digits
/// (as Excel rounds what it shows), as the digits before and after the point.
fn round_digits(x: f64, d: usize) -> (String, String) {
    let x = crate::value::to_15_digits(x.abs());
    // Decimal arithmetic on the shortest representation, so 0.125 → 0.13 and
    // 2.675 → 2.68 as Excel shows them.
    let s = shortest_plain(x);
    let (ip, fp) = s.split_once('.').unwrap_or((&s, ""));
    let mut digits: Vec<u8> = ip.bytes().chain(fp.bytes().chain(std::iter::repeat(b'0')).take(d)).map(|b| b - b'0').collect();
    let round_up = fp.as_bytes().get(d).is_some_and(|&b| b >= b'5');
    if round_up {
        let mut k = digits.len();
        loop {
            if k == 0 {
                digits.insert(0, 1);
                break;
            }
            k -= 1;
            if digits[k] == 9 {
                digits[k] = 0;
            } else {
                digits[k] += 1;
                break;
            }
        }
    }
    let n_int = digits.len() - d;
    let int: String = digits[..n_int].iter().map(|&b| (b + b'0') as char).collect();
    let frac: String = digits[n_int..].iter().map(|&b| (b + b'0') as char).collect();
    let int = int.trim_start_matches('0').to_string();
    (int, frac)
}

/// A non-negative finite number in plain decimal (no exponent), shortest round-trip.
fn shortest_plain(x: f64) -> String {
    let s = format!("{x:e}");
    let (m, e) = s.split_once('e').unwrap();
    let e: i32 = e.parse().unwrap();
    let digits: String = m.chars().filter(|c| c.is_ascii_digit()).collect();
    let point = 1 + e; // digits before the point
    if point <= 0 {
        format!("0.{}{}", "0".repeat((-point) as usize), digits)
    } else if point as usize >= digits.len() {
        format!("{}{}", digits, "0".repeat(point as usize - digits.len()))
    } else {
        format!("{}.{}", &digits[..point as usize], &digits[point as usize..])
    }
}

/// Fill integer placeholders (and the literals among them) right to left with `int`.
fn fill_int(toks: &[Tok], int: &str, group: bool) -> String {
    let n_ph = toks.iter().filter(|t| matches!(t, Tok::Digit(_))).count();
    let digits: Vec<char> = int.chars().collect();
    // Built right to left; leading digits beyond the placeholders go with the first.
    let mut out: Vec<char> = Vec::with_capacity(digits.len() + toks.len() + digits.len() / 3);
    let mut di = digits.len() as isize - 1;
    let mut seen_ph = 0;
    let mut placed = 0; // digits written so far, for thousands separators
    let put = |out: &mut Vec<char>, c: char, placed: &mut usize| {
        if group && *placed > 0 && *placed % 3 == 0 {
            out.push(',');
        }
        out.push(c);
        *placed += 1;
    };
    for t in toks.iter().rev() {
        match t {
            Tok::Digit(d) => {
                seen_ph += 1;
                if di >= 0 {
                    let stop = if seen_ph == n_ph { 0 } else { di };
                    while di >= stop {
                        put(&mut out, digits[di as usize], &mut placed);
                        di -= 1;
                    }
                } else {
                    match d {
                        b'0' => put(&mut out, '0', &mut placed),
                        b'?' => out.push(' '),
                        _ => {}
                    }
                }
            }
            other => out.extend(lit_of(other).chars().rev()),
        }
    }
    out.iter().rev().collect()
}

fn fill_frac(toks: &[Tok], frac: &str) -> String {
    let fd: Vec<char> = frac.chars().collect();
    // Trailing zeros under `#` vanish, under `?` become spaces.
    let ph: Vec<u8> = toks.iter().filter_map(|t| if let Tok::Digit(d) = t { Some(*d) } else { None }).collect();
    let mut keep = vec![true; ph.len()];
    let mut space = vec![false; ph.len()];
    for k in (0..ph.len()).rev() {
        if fd.get(k).copied().unwrap_or('0') != '0' {
            break;
        }
        match ph[k] {
            b'#' => keep[k] = false,
            b'?' => space[k] = true,
            _ => break,
        }
    }
    let mut out = String::new();
    let mut k = 0;
    for t in toks {
        match t {
            Tok::Digit(_) => {
                if space[k] {
                    out.push(' ');
                } else if keep[k] {
                    out.push(fd.get(k).copied().unwrap_or('0'));
                }
                k += 1;
            }
            other => out.push_str(&lit_of(other)),
        }
    }
    out
}

fn format_number(sec: &Section, v: f64) -> String {
    let toks = &sec.toks;
    let group = toks.first() == Some(&Tok::Group);
    let toks: &[Tok] = if group { &toks[1..] } else { toks };
    let pct = toks.iter().filter(|t| matches!(t, Tok::Percent)).count() as i32;
    let x = v * 10f64.powi(2 * pct) / 1000f64.powi(sec.scale);
    if !x.is_finite() || !crate::value::to_15_digits(x.abs()).is_finite() {
        return format_general(v);
    }
    let neg = x < 0.0;
    let sign = |s: String| if neg && s.chars().any(|c| c.is_ascii_digit() && c != '0') { format!("-{s}") } else { s };
    if let Some(si) = toks.iter().position(|t| matches!(t, Tok::Slash { .. })) {
        return sign(format_fraction(toks, si, x.abs()));
    }
    let exp_at = toks.iter().position(|t| matches!(t, Tok::Exp { .. }));
    let point = toks.iter().position(|t| *t == Tok::Point);
    let int_end = point.or(exp_at).unwrap_or(toks.len());
    let int_toks = &toks[..int_end];
    let frac_end = exp_at.unwrap_or(toks.len());
    let frac_toks: &[Tok] = match point {
        Some(p) => &toks[p + 1..frac_end.max(p + 1)],
        None => &[],
    };
    let n_frac = frac_toks.iter().filter(|t| matches!(t, Tok::Digit(_))).count();
    // Literals before the first placeholder stay in front, after the last at the end.
    let first_ph = int_toks.iter().position(|t| matches!(t, Tok::Digit(_)));
    let (prefix, int_core) = match first_ph {
        Some(f) => (&int_toks[..f], &int_toks[f..]),
        None => (int_toks, &int_toks[int_toks.len()..]),
    };
    let pre: String = prefix.iter().map(lit_of).collect();
    if let Some(ei) = exp_at {
        let Tok::Exp { plus, digits } = &toks[ei] else { unreachable!() };
        let n_int = int_core.iter().filter(|t| matches!(t, Tok::Digit(_))).count() as i32;
        let a = x.abs();
        let mut e = if a == 0.0 { 0 } else { a.log10().floor() as i32 };
        // Engineering form (`##0.0E+0`): exponents in steps of the placeholders.
        let step = if n_int > 1 && int_core.iter().any(|t| *t == Tok::Digit(b'#')) { n_int } else { 1 };
        let int_digits = if step > 1 { 1 } else { n_int.max(1) };
        if step > 1 {
            e = e.div_euclid(step) * step;
        } else {
            e -= int_digits - 1;
        }
        let mut m = a / 10f64.powi(e);
        let (mut ip, mut fp) = round_digits(m, n_frac);
        // Rounding up to the next power of ten: renormalize.
        let limit = if step > 1 { step } else { int_digits };
        if ip.len() as i32 > limit && a != 0.0 {
            e += if step > 1 { step } else { 1 };
            m = a / 10f64.powi(e);
            let r = round_digits(m, n_frac);
            ip = r.0;
            fp = r.1;
        }
        let ex_digits: Vec<Tok> = digits.iter().map(|&d| Tok::Digit(d)).collect();
        let es = fill_int(&ex_digits, &e.abs().to_string(), false);
        let esign = if e < 0 { "-" } else if *plus { "+" } else { "" };
        let mut s = pre;
        s.push_str(&fill_int(int_core, &ip, group));
        if point.is_some() {
            s.push('.');
            s.push_str(&fill_frac(frac_toks, &fp));
        }
        s.push('E');
        s.push_str(esign);
        s.push_str(es.trim());
        for t in &toks[ei + 1..] {
            s.push_str(&lit_of(t));
        }
        return sign(s);
    }
    let (ip, fp) = round_digits(x, n_frac);
    let mut s = pre;
    s.push_str(&fill_int(int_core, &ip, group));
    if point.is_some() {
        s.push('.');
        s.push_str(&fill_frac(frac_toks, &fp));
    }
    sign(s)
}

/// `# ?/?`, `?/8`, `0 ??/100`: the best fraction with the denominator's digits.
fn format_fraction(toks: &[Tok], si: usize, a: f64) -> String {
    let Tok::Slash { den, fixed } = &toks[si] else { unreachable!() };
    // Numerator placeholders: the digit run just before the slash.
    let mut ns = si;
    while ns > 0 && matches!(toks[ns - 1], Tok::Digit(_)) {
        ns -= 1;
    }
    let whole_toks = &toks[..ns];
    let has_whole = whole_toks.iter().any(|t| matches!(t, Tok::Digit(_)));
    let num_toks = &toks[ns..si];
    let (w, f) = if has_whole { (a.trunc(), a.fract()) } else { (0.0, a) };
    let (n, d) = match fixed {
        Some(d) => ((f * *d as f64).round() as u64, *d as u64),
        None => best_fraction(f, 10u64.pow(den.len().clamp(1, 7) as u32) - 1),
    };
    let (mut w, mut n) = (w as u64, n);
    if has_whole && n == d && d != 0 {
        w += 1;
        n = 0;
    }
    let mut out = String::new();
    let first_ph = whole_toks.iter().position(|t| matches!(t, Tok::Digit(_)));
    if has_whole {
        let f = first_ph.unwrap();
        out.push_str(&whole_toks[..f].iter().map(lit_of).collect::<String>());
        let core = &whole_toks[f..];
        let wi = if w == 0 { String::new() } else { w.to_string() };
        // With a zero fraction the whole number stands alone (`5`, or `0`), the
        // fraction's `?` places kept as spaces.
        if n == 0 {
            let mut s = fill_int(core, if w == 0 { "0" } else { &wi }, false);
            let q = |ph: &[u8]| ph.iter().filter(|&&c| c == b'?').count();
            let nq = num_toks.iter().filter(|t| **t == Tok::Digit(b'?')).count();
            let dq = q(den);
            s.push_str(&" ".repeat(nq + dq + usize::from(nq + dq > 0)));
            for t in &toks[si + 1..] {
                s.push_str(&lit_of(t));
            }
            return s;
        }
        out.push_str(&fill_int(core, &wi, false));
    } else {
        out.push_str(&whole_toks.iter().map(lit_of).collect::<String>());
    }
    let numer: Vec<Tok> = num_toks.to_vec();
    out.push_str(&fill_int(&numer, &n.to_string(), false));
    out.push('/');
    match fixed {
        Some(d) => out.push_str(&d.to_string()),
        None => {
            let ds = d.to_string();
            out.push_str(&ds);
            // `?` pads the denominator on the right.
            let q = den.iter().filter(|&&c| c == b'?').count();
            if ds.len() < q {
                out.push_str(&" ".repeat(q - ds.len()));
            }
        }
    }
    for t in &toks[si + 1..] {
        out.push_str(&lit_of(t));
    }
    out
}

/// The closest n/d to `x` (0 ≤ x < 1 usually) with d ≤ `max_d` (continued fractions,
/// then the best semiconvergent).
fn best_fraction(x: f64, max_d: u64) -> (u64, u64) {
    let (mut p0, mut q0, mut p1, mut q1) = (0u64, 1u64, 1u64, 0u64);
    let mut v = x;
    loop {
        let a = v.floor();
        if a > 1e12 {
            break;
        }
        let a = a as u64;
        let q2 = a.saturating_mul(q1).saturating_add(q0);
        if q2 > max_d {
            // Best semiconvergent within the bound.
            let k = (max_d - q0) / q1.max(1);
            let (ps, qs) = (k * p1 + p0, k * q1 + q0);
            if qs > 0 && (ps as f64 / qs as f64 - x).abs() < (p1 as f64 / q1.max(1) as f64 - x).abs() {
                return (ps, qs);
            }
            break;
        }
        let p2 = a * p1 + p0;
        (p0, q0, p1, q1) = (p1, q1, p2, q2);
        let r = v - a as f64;
        if r < 1e-12 || (p1 as f64 / q1 as f64 - x).abs() < 1e-15 {
            break;
        }
        v = 1.0 / r;
    }
    if q1 == 0 {
        return (0, 1);
    }
    (p1, q1)
}

fn format_date(sec: &Section, v: f64) -> Option<String> {
    if !(0.0..2958466.0).contains(&v) {
        return None;
    }
    // Round to the precision shown (whole seconds, or the sub-second digits).
    let sub = sec.toks.iter().filter_map(|t| if let Tok::SubSec(n) = t { Some(*n) } else { None }).max().unwrap_or(0);
    let unit = 86400.0 * 10f64.powi(sub as i32);
    let ticks = (v * unit + 0.5).floor();
    let per_day = unit as i64;
    let ticks = ticks as i64;
    let day = ticks / per_day;
    let in_day = ticks % per_day;
    let scale = 10i64.pow(sub as u32);
    let secs = in_day / scale;
    let frac = in_day % scale;
    let (h, mi, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    let (y, mo, d) = dates::ymd(day as f64)?;
    let ampm = sec.toks.iter().any(|t| matches!(t, Tok::AmPm { .. }));
    let total_secs = ticks / scale;
    let mut out = String::new();
    // Elapsed tokens: the first one carries the total, later fields wrap.
    for t in &sec.toks {
        match t {
            Tok::Lit(l) => out.push_str(l),
            Tok::Year(2) => out.push_str(&format!("{:02}", y % 100)),
            Tok::Year(_) => out.push_str(&format!("{y}")),
            Tok::Month(1) => out.push_str(&mo.to_string()),
            Tok::Month(2) => out.push_str(&format!("{mo:02}")),
            Tok::Month(3) => out.push_str(&MONTHS[(mo - 1) as usize][..3]),
            Tok::Month(4) => out.push_str(MONTHS[(mo - 1) as usize]),
            Tok::Month(_) => out.push_str(&MONTHS[(mo - 1) as usize][..1]),
            Tok::Day(1) => out.push_str(&d.to_string()),
            Tok::Day(2) => out.push_str(&format!("{d:02}")),
            Tok::Day(n) => {
                // Serial 0 is Saturday 1900-01-00 in Excel; its weekdays follow on
                // (including its 29 February 1900).
                let wd = ((day + 6) % 7) as usize;
                let name = DAYS[wd];
                out.push_str(if *n == 3 { &name[..3] } else { name });
            }
            Tok::Hour(n) => {
                let hh = if ampm { (h + 11) % 12 + 1 } else { h };
                if *n >= 2 {
                    out.push_str(&format!("{hh:02}"));
                } else {
                    out.push_str(&hh.to_string());
                }
            }
            Tok::Minute(n) => {
                if *n >= 2 {
                    out.push_str(&format!("{mi:02}"));
                } else {
                    out.push_str(&mi.to_string());
                }
            }
            Tok::Second(n) => {
                if *n >= 2 {
                    out.push_str(&format!("{s:02}"));
                } else {
                    out.push_str(&s.to_string());
                }
            }
            Tok::SubSec(n) => {
                let f = frac * 10i64.pow(3) / scale.max(1); // milliseconds
                let ms = format!("{f:03}");
                out.push('.');
                out.push_str(&ms[..*n as usize]);
            }
            Tok::AmPm { long, upper } => {
                let pm = h >= 12;
                let s = match (long, pm) {
                    (true, false) => "AM",
                    (true, true) => "PM",
                    (false, false) => "A",
                    (false, true) => "P",
                };
                out.push_str(&if *upper { s.to_string() } else { s.to_ascii_lowercase() });
            }
            Tok::Elapsed(c, n) => {
                let val = match c {
                    'h' => total_secs / 3600,
                    'm' => total_secs / 60,
                    _ => total_secs,
                };
                out.push_str(&format!("{val:0w$}", w = *n as usize));
            }
            Tok::Digit(_) | Tok::Point | Tok::Group | Tok::Percent | Tok::Exp { .. } | Tok::Slash { .. } | Tok::General => {}
            Tok::Text => {}
        }
    }
    // After an elapsed hour, minutes and seconds are within the hour (as fields above).
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(code: &str, x: f64) -> String {
        NumFmt::parse(code).format(&Value::Num(x)).text
    }

    #[test]
    fn numbers() {
        assert_eq!(f("0", 1234.5), "1235");
        assert_eq!(f("0.00", 2.675), "2.68");
        assert_eq!(f("0.00", 0.125), "0.13");
        assert_eq!(f("#,##0", 1234567.0), "1,234,567");
        assert_eq!(f("#,##0.00", -1234.5), "-1,234.50");
        assert_eq!(f("#,##0", 12.0), "12");
        assert_eq!(f("#.##", 0.5), ".5");
        assert_eq!(f("#.##", 3.0), "3.");
        assert_eq!(f("0.0#", 3.0), "3.0");
        assert_eq!(f("0.??", 3.5), "3.5 ");
        assert_eq!(f("000-0000", 5551234.0), "555-1234");
        assert_eq!(f("00000", 42.0), "00042");
        assert_eq!(f("0%", 0.256), "26%");
        assert_eq!(f("0.00%", -0.01), "-1.00%");
        assert_eq!(f("#,##0,", 1234567.0), "1,235");
        assert_eq!(f("0.0,,\"M\"", 1234567.0), "1.2M");
        assert_eq!(f("\"$\"#,##0.00", 1234.5), "$1,234.50");
        assert_eq!(f("[$€-407] #,##0.00", 3.0), "€ 3.00");
        assert_eq!(f("0;(0)", -5.0), "(5)");
        assert_eq!(f("0;-0;\"zero\"", 0.0), "zero");
        assert_eq!(f("0", -0.4), "0");
        assert_eq!(f("0.0", -0.04), "0.0");
        assert_eq!(f("General", 1.0 / 3.0), "0.333333333333333");
        assert_eq!(f("[Red]General", -2.0), "-2");
        assert_eq!(f("#,##0_);[Red]\\(#,##0\\)", -1234.0), "(1,234)");
        assert_eq!(f("#,##0_);[Red]\\(#,##0\\)", 1234.0), "1,234 ");
        assert_eq!(NumFmt::parse("#,##0_);[Red]\\(#,##0\\)").format(&Value::Num(-1.0)).color, Some(FmtColor::Red));
        assert_eq!(f(builtin(44).unwrap(), 0.0), " $ -   ");
    }

    #[test]
    fn scientific() {
        assert_eq!(f("0.00E+00", 12345.0), "1.23E+04");
        assert_eq!(f("0.00E+00", 0.000123), "1.23E-04");
        assert_eq!(f("0.00E+00", 0.0), "0.00E+00");
        assert_eq!(f("0.00E+00", 9.999), "1.00E+01");
        assert_eq!(f("##0.0E+0", 12345.0), "12.3E+3");
        assert_eq!(f("##0.0E+0", 1234567.0), "1.2E+6");
        assert_eq!(f("0E+0", -5000.0), "-5E+3");
    }

    #[test]
    fn fractions() {
        assert_eq!(f("# ?/?", 1.5), "1 1/2");
        assert_eq!(f("# ??/??", 3.14159), "3 14/99");
        assert_eq!(f("?/?", 0.75), "3/4");
        assert_eq!(f("# ?/8", 2.3), "2 2/8");
        assert_eq!(f("# ?/?", 5.0), "5    ");
        assert_eq!(f("# ?/?", -0.25), "- 1/4");
        assert_eq!(f("# ?/?", 0.125), " 1/8");
    }

    #[test]
    fn dates_and_times() {
        // 2026-11-14 is serial 46340; 15:30:45 is 0.646354...
        let d = 46340.0 + (15.0 * 3600.0 + 30.0 * 60.0 + 45.0) / 86400.0;
        assert_eq!(f("yyyy-mm-dd", d), "2026-11-14");
        assert_eq!(f("m/d/yyyy", d), "11/14/2026");
        assert_eq!(f("d-mmm-yy", d), "14-Nov-26");
        assert_eq!(f("dddd, mmmm d, yyyy", d), "Saturday, November 14, 2026");
        assert_eq!(f("ddd mmmmm", d), "Sat N");
        assert_eq!(f("h:mm AM/PM", d), "3:30 PM");
        assert_eq!(f("hh:mm:ss", d), "15:30:45");
        assert_eq!(f("h:mm a/p", 0.25), "6:00 a");
        assert_eq!(f("mm:ss.0", 1.5 / 86400.0), "00:01.5");
        assert_eq!(f("[h]:mm:ss", 1.5), "36:00:00");
        assert_eq!(f("[mm]:ss", 0.5 / 24.0), "30:00");
        assert_eq!(f("m/d/yyyy h:mm", 1.0), "1/1/1900 0:00");
        assert_eq!(f("yyyy-mm-dd", 60.0), "1900-02-29");
        assert_eq!(f("dddd", 1.0), "Sunday");
        assert_eq!(f("dddd", 61.0), "Thursday");
        // Seconds round: 59.6 s shows as the next minute.
        assert_eq!(f("h:mm:ss", (59.6) / 86400.0), "0:01:00");
        assert!(NumFmt::parse("yyyy").format(&Value::Num(-1.0)).hashes);
        assert!(NumFmt::parse("d-mmm").is_date());
        assert!(!NumFmt::parse("0.00").is_date());
    }

    #[test]
    fn conditions_and_text() {
        assert_eq!(f("[<1000]0;0.0,\"k\"", 999.0), "999");
        assert_eq!(f("[<1000]0;0.0,\"k\"", 12345.0), "12.3k");
        assert_eq!(f("[>=100][Blue]0;[<0][Red]0;0.00", 5.0), "5.00");
        assert_eq!(f("[>=100][Blue]0;[<0][Red]0;0.00", -5.0), "5");
        let t = NumFmt::parse("0;-0;0;\"<\"@\">\"");
        assert_eq!(t.format(&Value::str("hi")).text, "<hi>");
        assert_eq!(NumFmt::parse("@").format(&Value::Num(12.5)).text, "12.5");
        assert_eq!(NumFmt::parse("0.00").format(&Value::str("x")).text, "x");
        assert_eq!(NumFmt::parse("0.00").format(&Value::Bool(true)).text, "TRUE");
    }

    #[test]
    fn builtins_parse() {
        for id in 0..=49 {
            if let Some(c) = builtin(id) {
                let n = NumFmt::parse(c);
                assert!(n.is_exact(), "{id} {c}");
                assert_eq!(builtin_id(c), Some(id));
            }
        }
        assert!(NumFmt::parse("General").is_general());
    }

    #[test]
    fn odd_codes_never_panic() {
        for code in ["", ";", ";;;", "[", "\"", "\\", "_", "*", "0/", "/0", "E+", "0E+", "[h", "[>]0", "#/#/#", "0.0.0", "yyyyyyyy", "mmmmmmm", "0,,,,,", "?/0", "# ?/0", "[Color99]0", "[$-409]h:mm", "0.000000000000000000000000000000"] {
            let n = NumFmt::parse(code);
            for x in [0.0, -1.5, 1e300, -1e-300, 1e15, 0.999999999, f64::MAX] {
                let _ = n.format(&Value::Num(x));
            }
            let _ = n.format(&Value::str("t"));
        }
    }
}
