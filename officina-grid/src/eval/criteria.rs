//! Criteria for SUMIF, COUNTIF, AVERAGEIF and their -IFS forms: `">5"`, `"<>x"`, `"ab*"`,
//! a number, an empty string.

use crate::value::{cmp_text, Value};
use std::cmp::Ordering;

#[derive(Debug, Clone)]
enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// Which side of a number an ordered criterion accepts (see [`Criterion::bound`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Side {
    Lt,
    Le,
    Gt,
    Ge,
}

/// What an equality criterion compares by (see [`Criterion::eq_key`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Key {
    /// An empty cell or empty text.
    Empty,
    Bool(bool),
    Num(u64),
    Text(String),
}

impl Key {
    fn num(x: f64) -> Key {
        Key::Num(if x == 0.0 { 0 } else { x.to_bits() })
    }

    /// The key of a cell's value: numbers and text that reads as a number by
    /// value, other text case-folded; errors have none.
    pub fn of(v: &Value) -> Option<Key> {
        match v {
            Value::Empty => Some(Key::Empty),
            Value::Str(s) if s.is_empty() => Some(Key::Empty),
            Value::Bool(b) => Some(Key::Bool(*b)),
            Value::Num(x) => (!x.is_nan()).then(|| Key::num(*x)),
            Value::Str(s) => match super::parse_number(s) {
                Some(x) if !x.is_nan() => Some(Key::num(x)),
                _ => Some(Key::Text(s.chars().flat_map(char::to_lowercase).collect())),
            },
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Criterion {
    op: Op,
    num: Option<f64>,
    text: String,
    wild: bool,
    boolean: Option<bool>,
}

impl Criterion {
    pub fn new(v: &Value) -> Criterion {
        match v {
            Value::Num(x) => Criterion { op: Op::Eq, num: Some(*x), text: String::new(), wild: false, boolean: None },
            Value::Bool(b) => Criterion { op: Op::Eq, num: None, text: String::new(), wild: false, boolean: Some(*b) },
            Value::Empty => Criterion { op: Op::Eq, num: Some(0.0), text: String::new(), wild: false, boolean: None },
            Value::Err(_) => Criterion { op: Op::Eq, num: None, text: "\u{0}err".into(), wild: false, boolean: None },
            Value::Str(s) => {
                let s: &str = s;
                let (op, rest) = if let Some(r) = s.strip_prefix("<=") {
                    (Op::Le, r)
                } else if let Some(r) = s.strip_prefix(">=") {
                    (Op::Ge, r)
                } else if let Some(r) = s.strip_prefix("<>") {
                    (Op::Ne, r)
                } else if let Some(r) = s.strip_prefix('<') {
                    (Op::Lt, r)
                } else if let Some(r) = s.strip_prefix('>') {
                    (Op::Gt, r)
                } else if let Some(r) = s.strip_prefix('=') {
                    (Op::Eq, r)
                } else {
                    (Op::Eq, s)
                };
                let num = super::parse_number(rest);
                let boolean = if rest.eq_ignore_ascii_case("TRUE") {
                    Some(true)
                } else if rest.eq_ignore_ascii_case("FALSE") {
                    Some(false)
                } else {
                    None
                };
                let wild = matches!(op, Op::Eq | Op::Ne) && rest.contains(['*', '?', '~']);
                Criterion { op, num, text: rest.to_string(), wild, boolean }
            }
        }
    }

    /// The one key a cell must have to match, when this criterion is plain
    /// equality (no wildcards): matching is then
    /// `key(cell) == Some(self.eq_key())`, which lets a range be indexed once.
    pub fn eq_key(&self) -> Option<Key> {
        if !matches!(self.op, Op::Eq) || self.wild {
            return None;
        }
        if let Some(b) = self.boolean {
            return Some(Key::Bool(b));
        }
        if let Some(n) = self.num {
            return (!n.is_nan()).then(|| Key::num(n));
        }
        Some(if self.text.is_empty() { Key::Empty } else { Key::Text(self.text.chars().flat_map(char::to_lowercase).collect()) })
    }

    /// The bound a cell's number must meet, when this criterion is `<`, `<=`,
    /// `>` or `>=` a number: matching is then "the cell holds a number (not
    /// text that reads as one) and it meets the bound", so a range can be
    /// sorted once and every criterion answered by a binary search.
    pub fn bound(&self) -> Option<(Side, f64)> {
        if self.boolean.is_some() {
            return None;
        }
        let x = self.num.filter(|x| !x.is_nan())?;
        let side = match self.op {
            Op::Lt => Side::Lt,
            Op::Le => Side::Le,
            Op::Gt => Side::Gt,
            Op::Ge => Side::Ge,
            _ => return None,
        };
        Some((side, x))
    }

    /// Whether an empty cell can match (so callers walking only stored cells know to
    /// count the empty ones too).
    pub fn matches_empty(&self) -> bool {
        self.test(&Value::Empty)
    }

    pub fn test(&self, v: &Value) -> bool {
        let ord = |o: Ordering| match self.op {
            Op::Eq => o == Ordering::Equal,
            Op::Ne => o != Ordering::Equal,
            Op::Lt => o == Ordering::Less,
            Op::Le => o != Ordering::Greater,
            Op::Gt => o == Ordering::Greater,
            Op::Ge => o != Ordering::Less,
        };
        if let Some(b) = self.boolean {
            return match v {
                Value::Bool(x) => ord(x.cmp(&b)),
                _ => matches!(self.op, Op::Ne),
            };
        }
        if let Some(n) = self.num {
            return match v {
                Value::Num(x) => ord(x.partial_cmp(&n).unwrap_or(Ordering::Equal)),
                Value::Str(s) => match (super::parse_number(s), &self.op) {
                    (Some(x), Op::Eq | Op::Ne) => ord(x.partial_cmp(&n).unwrap_or(Ordering::Equal)),
                    _ => matches!(self.op, Op::Ne),
                },
                _ => matches!(self.op, Op::Ne),
            };
        }
        // Text criteria.
        let t = match v {
            Value::Str(s) => s.to_string(),
            Value::Empty => String::new(),
            Value::Err(_) => return false,
            _ => return matches!(self.op, Op::Ne),
        };
        if self.text.is_empty() {
            // "" or "=" matches empty cells (and empty text); "<>" anything not empty.
            return match self.op {
                Op::Eq => t.is_empty(),
                Op::Ne => !t.is_empty() || !matches!(v, Value::Empty),
                _ => false,
            };
        }
        if matches!(v, Value::Empty) && matches!(self.op, Op::Eq) {
            return false;
        }
        if self.wild {
            let m = wildcard(&self.text, &t);
            return if matches!(self.op, Op::Eq) { m } else { !m };
        }
        ord(cmp_text(&t, &self.text))
    }
}

/// `*` and `?` wildcards with `~` escaping, case-insensitive.
pub fn wildcard(pat: &str, s: &str) -> bool {
    #[derive(Clone, Copy, PartialEq)]
    enum P {
        Ch(char),
        Any,
        One,
    }
    let mut ps = Vec::new();
    let mut it = pat.chars().flat_map(char::to_lowercase).peekable();
    while let Some(c) = it.next() {
        match c {
            '~' => match it.next() {
                Some(n) => ps.push(P::Ch(n)),
                None => ps.push(P::Ch('~')),
            },
            '*' => ps.push(P::Any),
            '?' => ps.push(P::One),
            c => ps.push(P::Ch(c)),
        }
    }
    let s: Vec<char> = s.chars().flat_map(char::to_lowercase).collect();
    // Iterative matcher with one backtrack point (linear for patterns with *).
    let (mut i, mut j) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while i < s.len() {
        if j < ps.len() && (ps[j] == P::One || ps[j] == P::Ch(s[i])) {
            i += 1;
            j += 1;
        } else if j < ps.len() && ps[j] == P::Any {
            star = Some(j);
            mark = i;
            j += 1;
        } else if let Some(st) = star {
            j = st + 1;
            mark += 1;
            i = mark;
        } else {
            return false;
        }
    }
    while j < ps.len() && ps[j] == P::Any {
        j += 1;
    }
    j == ps.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn criteria() {
        let c = |s: &str| Criterion::new(&Value::str(s));
        assert!(c(">5").test(&Value::Num(6.0)));
        assert!(!c(">5").test(&Value::str("6a")));
        assert!(c("<>5").test(&Value::str("x")));
        assert!(c("apple").test(&Value::str("APPLE")));
        assert!(c("a*e").test(&Value::str("apple")));
        assert!(c("?pple").test(&Value::str("Apple")));
        assert!(!c("?pple").test(&Value::str("pple")));
        assert!(c("~*").test(&Value::str("*")));
        assert!(c("").test(&Value::Empty));
        assert!(c("<>").test(&Value::Num(1.0)));
        assert!(!c("<>").test(&Value::Empty));
        assert!(Criterion::new(&Value::Num(3.0)).test(&Value::str("3")));
        assert!(c(">b").test(&Value::str("c")));
        assert!(wildcard("*a*b*c*", "xxaxxbxxcxx"));
    }
}
