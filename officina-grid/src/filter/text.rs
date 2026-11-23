//! Custom text and numeric tests shared by spreadsheet formats.

use std::cmp::Ordering;

use officina_core::wire::{Reader, WireError, Writer};
use regex::{Regex, RegexBuilder};

use crate::{eval::funcs::text_key, value::Value};

pub const MAX_TESTS: usize = 16;
const MAX_PATTERN: usize = 4096;
const COMPARISONS: [&str; 6] = ["=", "<>", "<", "<=", ">", ">="];

#[derive(Debug, Clone, PartialEq)]
pub enum Test {
    Number(String, f64),
    Text(TextTest),
}

#[derive(Debug, Clone, PartialEq)]
pub struct TextTest {
    pub op: TextOp,
    pub value: String,
    pub case_sensitive: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Begins,
    NotBegins,
    Ends,
    NotEnds,
    Contains,
    NotContains,
    Wildcard,
    NotWildcard,
    Regex,
    NotRegex,
    PartialRegex,
    NotPartialRegex,
}

impl TextOp {
    const ALL: [Self; 18] = [
        Self::Eq,
        Self::Ne,
        Self::Lt,
        Self::Le,
        Self::Gt,
        Self::Ge,
        Self::Begins,
        Self::NotBegins,
        Self::Ends,
        Self::NotEnds,
        Self::Contains,
        Self::NotContains,
        Self::Wildcard,
        Self::NotWildcard,
        Self::Regex,
        Self::NotRegex,
        Self::PartialRegex,
        Self::NotPartialRegex,
    ];

    pub fn comparison(op: &str) -> Option<Self> {
        COMPARISONS
            .iter()
            .position(|x| *x == op)
            .map(|i| Self::ALL[i])
    }

    pub fn comparison_symbol(self) -> Option<&'static str> {
        COMPARISONS.get(self as usize).copied()
    }
}

/// Interpret only Excel's three defined tilde escapes. Everything else is literal.
pub fn wildcard_regex(value: &str) -> String {
    let mut out = String::from(r"\A(?:");
    let mut chars = value.chars().peekable();
    let mut literal = String::new();
    while let Some(ch) = chars.next() {
        match ch {
            '~' if matches!(chars.peek(), Some('*' | '?' | '~')) => literal.extend(chars.next()),
            '*' | '?' => {
                out.push_str(&regex::escape(&literal));
                literal.clear();
                out.push_str(if ch == '*' { "(?s:.*)" } else { "(?s:.)" });
            }
            _ => literal.push(ch),
        }
    }
    out.push_str(&regex::escape(&literal));
    out.push_str(r")\z");
    out
}

impl TextTest {
    pub fn valid(&self) -> bool {
        self.value.len() <= MAX_PATTERN
            && self
                .pattern()
                .is_none_or(|pattern| self.compile(&pattern).is_some())
    }

    fn pattern(&self) -> Option<String> {
        Some(match self.op {
            TextOp::Wildcard | TextOp::NotWildcard => wildcard_regex(&self.value),
            TextOp::Regex | TextOp::NotRegex => format!(r"\A(?:{})\z", self.value),
            TextOp::PartialRegex | TextOp::NotPartialRegex => self.value.clone(),
            _ => return None,
        })
    }

    fn compile(&self, pattern: &str) -> Option<Regex> {
        RegexBuilder::new(pattern)
            .case_insensitive(!self.case_sensitive)
            .size_limit(256 * 1024)
            .dfa_size_limit(256 * 1024)
            .nest_limit(32)
            .build()
            .ok()
    }
}

impl Test {
    pub(super) fn encode(&self, w: &mut Writer) {
        match self {
            Self::Number(op, x) => {
                w.u8(0);
                w.str(op);
                w.f64(*x);
            }
            Self::Text(text) => {
                w.u8(1);
                w.u8(text.op as u8);
                w.u8(u8::from(text.case_sensitive));
                w.str(&text.value);
            }
        }
    }

    pub(super) fn decode(r: &mut Reader) -> Result<Self, WireError> {
        match r.u8()? {
            0 => {
                let op = r.str("operator", 2)?;
                let x = r.f64()?;
                if !COMPARISONS.contains(&op) || !x.is_finite() {
                    return Err(WireError::Invalid("custom numeric test"));
                }
                Ok(Self::Number(op.to_string(), x))
            }
            1 => {
                let op = *TextOp::ALL
                    .get(r.u8()? as usize)
                    .ok_or(WireError::Invalid("custom text operator"))?;
                let case_sensitive = match r.u8()? {
                    0 => false,
                    1 => true,
                    _ => return Err(WireError::Invalid("custom text case flag")),
                };
                let value = r.str("custom text", MAX_PATTERN as u64)?.to_string();
                let text = TextTest {
                    op,
                    value,
                    case_sensitive,
                };
                if !text.valid() {
                    return Err(WireError::Invalid("custom text pattern"));
                }
                Ok(Self::Text(text))
            }
            _ => Err(WireError::Invalid("custom test kind")),
        }
    }

    pub(super) fn prepare(&self) -> Prepared<'_> {
        match self {
            Self::Number(op, x) => Prepared::Number(op, *x),
            Self::Text(t) => match t.pattern() {
                Some(pattern) => Prepared::Pattern(t.op, t.compile(&pattern)),
                None => Prepared::Text(
                    t.op,
                    if t.case_sensitive {
                        t.value.clone()
                    } else {
                        text_key(&t.value)
                    },
                    t.case_sensitive,
                ),
            },
        }
    }
}

pub(super) enum Prepared<'a> {
    Number(&'a str, f64),
    Text(TextOp, String, bool),
    Pattern(TextOp, Option<Regex>),
}

impl Prepared<'_> {
    pub(super) fn keeps(&self, value: &Value, shown: &str) -> bool {
        use TextOp::*;
        match self {
            Self::Number(op, y) => {
                let Value::Num(x) = value else { return false };
                compare(x.partial_cmp(y), TextOp::comparison(op).unwrap_or(Eq))
            }
            Self::Text(op, needle, case_sensitive) => {
                let folded;
                let shown = if *case_sensitive {
                    shown
                } else {
                    folded = text_key(shown);
                    &folded
                };
                match op {
                    Begins | NotBegins => shown.starts_with(needle) != (*op == NotBegins),
                    Ends | NotEnds => shown.ends_with(needle) != (*op == NotEnds),
                    Contains | NotContains => shown.contains(needle) != (*op == NotContains),
                    _ => compare(Some(shown.cmp(needle)), *op),
                }
            }
            Self::Pattern(op, Some(regex)) => {
                if matches!(op, Wildcard | NotWildcard) && !matches!(value, Value::Str(_)) {
                    return false;
                }
                let text = match (op, value) {
                    (Wildcard | NotWildcard, Value::Str(text)) => text.as_ref(),
                    _ => shown,
                };
                regex.is_match(text) != matches!(op, NotWildcard | NotRegex | NotPartialRegex)
            }
            Self::Pattern(_, None) => false,
        }
    }
}

fn compare(order: Option<Ordering>, op: TextOp) -> bool {
    use Ordering::*;
    use TextOp::*;
    match op {
        Eq => order == Some(Equal),
        Ne => order != Some(Equal),
        Lt => order == Some(Less),
        Le => matches!(order, Some(Less | Equal)),
        Gt => order == Some(Greater),
        Ge => matches!(order, Some(Greater | Equal)),
        _ => false,
    }
}
