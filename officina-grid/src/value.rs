//! Cell values, errors, and Excel's display of numbers.

use std::cmp::Ordering;
use std::fmt;
use std::sync::Arc;

/// Excel's error values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ErrorKind {
    Null = 1,
    Div0 = 2,
    Value = 3,
    Ref = 4,
    Name = 5,
    Num = 6,
    NA = 7,
    /// A circular reference (Excel shows a warning and 0; LibreOffice Err:522). Officina
    /// shows it in the cell so the user sees where the cycle is.
    Circ = 8,
    /// A formula or array over Officina's limits (too deep, too big).
    Limit = 9,
}

impl ErrorKind {
    pub fn text(self) -> &'static str {
        match self {
            ErrorKind::Null => "#NULL!",
            ErrorKind::Div0 => "#DIV/0!",
            ErrorKind::Value => "#VALUE!",
            ErrorKind::Ref => "#REF!",
            ErrorKind::Name => "#NAME?",
            ErrorKind::Num => "#NUM!",
            ErrorKind::NA => "#N/A",
            ErrorKind::Circ => "#CIRC!",
            ErrorKind::Limit => "#LIMIT!",
        }
    }
    pub fn from_text(s: &str) -> Option<Self> {
        let u = s.to_ascii_uppercase();
        Some(match u.as_str() {
            "#NULL!" => ErrorKind::Null,
            "#DIV/0!" => ErrorKind::Div0,
            "#VALUE!" => ErrorKind::Value,
            "#REF!" => ErrorKind::Ref,
            "#NAME?" => ErrorKind::Name,
            "#NUM!" => ErrorKind::Num,
            "#N/A" => ErrorKind::NA,
            "#CIRC!" => ErrorKind::Circ,
            "#LIMIT!" => ErrorKind::Limit,
            _ => return None,
        })
    }
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => ErrorKind::Null,
            2 => ErrorKind::Div0,
            3 => ErrorKind::Value,
            4 => ErrorKind::Ref,
            5 => ErrorKind::Name,
            6 => ErrorKind::Num,
            7 => ErrorKind::NA,
            8 => ErrorKind::Circ,
            9 => ErrorKind::Limit,
            _ => return None,
        })
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.text())
    }
}

/// A value as formulas see it.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Empty,
    Num(f64),
    Str(Arc<str>),
    Bool(bool),
    Err(ErrorKind),
}

impl Value {
    pub fn str(s: &str) -> Value {
        Value::Str(Arc::from(s))
    }
    pub fn is_empty(&self) -> bool {
        matches!(self, Value::Empty)
    }
    /// A number result, or `#NUM!` for infinities and NaN, as Excel never stores them.
    pub fn num(x: f64) -> Value {
        if x.is_finite() {
            Value::Num(if x == 0.0 { 0.0 } else { x })
        } else {
            Value::Err(ErrorKind::Num)
        }
    }
    /// How the cell reads in General format.
    pub fn display(&self) -> String {
        match self {
            Value::Empty => String::new(),
            Value::Num(x) => format_general(*x),
            Value::Str(s) => s.to_string(),
            Value::Bool(true) => "TRUE".into(),
            Value::Bool(false) => "FALSE".into(),
            Value::Err(e) => e.text().into(),
        }
    }
}

/// Excel's ordering for sorting and comparisons: numbers < text < booleans; errors are
/// handled by the caller. Text compares case-insensitively.
pub fn compare(a: &Value, b: &Value) -> Ordering {
    fn rank(v: &Value) -> u8 {
        match v {
            Value::Empty | Value::Num(_) => 0,
            Value::Str(_) => 1,
            Value::Bool(_) => 2,
            Value::Err(_) => 3,
        }
    }
    match (a, b) {
        // Empty against text is "", against anything else 0 / FALSE.
        (Value::Empty, Value::Str(s)) => cmp_text("", s),
        (Value::Str(s), Value::Empty) => cmp_text(s, ""),
        (Value::Empty, Value::Bool(x)) => false.cmp(x),
        (Value::Bool(x), Value::Empty) => x.cmp(&false),
        _ => match rank(a).cmp(&rank(b)) {
            Ordering::Equal => match (a, b) {
                (Value::Num(x), Value::Num(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
                (Value::Num(x), Value::Empty) => x.partial_cmp(&0.0).unwrap_or(Ordering::Equal),
                (Value::Empty, Value::Num(y)) => 0.0f64.partial_cmp(y).unwrap_or(Ordering::Equal),
                (Value::Str(x), Value::Str(y)) => cmp_text(x, y),
                (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
                _ => Ordering::Equal,
            },
            o => o,
        },
    }
}

/// Case-insensitive text comparison (Unicode lowercase, then code points).
pub fn cmp_text(a: &str, b: &str) -> Ordering {
    let mut x = a.chars().flat_map(char::to_lowercase);
    let mut y = b.chars().flat_map(char::to_lowercase);
    loop {
        match (x.next(), y.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(p), Some(q)) if p != q => return p.cmp(&q),
            _ => {}
        }
    }
}

/// `x` rounded to 15 significant digits, the precision Excel shows and compares at.
pub fn to_15_digits(x: f64) -> f64 {
    if x == 0.0 || !x.is_finite() {
        return x;
    }
    format!("{:.14e}", x).parse().unwrap_or(x)
}

/// A number in Excel's General format: at most 15 significant digits, no trailing zeros,
/// scientific notation for very large and very small magnitudes. (Excel also narrows to
/// the column's width; that is the view's job.)
pub fn format_general(x: f64) -> String {
    if x == 0.0 {
        return "0".into();
    }
    let a = x.abs();
    if !(1e-9..1e15).contains(&a) {
        // d.dddddE+XX with up to 14 decimals, zeros trimmed
        let s = format!("{:.14e}", x);
        let (m, e) = s.split_once('e').unwrap();
        let m = trim_zeros(m);
        let e: i32 = e.parse().unwrap();
        return format!("{}E{}{:02}", m, if e < 0 { '-' } else { '+' }, e.abs());
    }
    let r = to_15_digits(x);
    // Decimal exponent from the 15-digit form, not log10 (which can be off by one near
    // powers of ten).
    let sci = format!("{:.14e}", r);
    let e: i32 = sci.split_once('e').unwrap().1.parse().unwrap();
    let decimals = (14 - e).clamp(0, 24) as usize;
    let s = format!("{:.*}", decimals, r);
    let s = trim_zeros(&s);
    if s == "-0" { "0".into() } else { s }
}

fn trim_zeros(s: &str) -> String {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn general_format() {
        assert_eq!(format_general(0.1 + 0.2), "0.3");
        assert_eq!(format_general(1.0 / 3.0), "0.333333333333333");
        assert_eq!(format_general(123456789012345.0), "123456789012345");
        assert_eq!(format_general(1e15), "1E+15");
        assert_eq!(format_general(-2.5), "-2.5");
        assert_eq!(format_general(1e-10), "1E-10");
        assert_eq!(format_general(100.0), "100");
        assert_eq!(format_general(2.0f64.sqrt()), "1.4142135623731");
    }

    #[test]
    fn ordering() {
        use Ordering::*;
        assert_eq!(compare(&Value::Num(5.0), &Value::str("a")), Less);
        assert_eq!(compare(&Value::str("Z"), &Value::Bool(false)), Less);
        assert_eq!(compare(&Value::str("abc"), &Value::str("ABC")), Equal);
        assert_eq!(compare(&Value::Empty, &Value::Num(0.0)), Equal);
        assert_eq!(compare(&Value::Empty, &Value::str("")), Equal);
    }
}
