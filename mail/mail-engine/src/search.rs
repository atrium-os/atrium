//! Search (mail.md §11.2): a query of plain words and operators, run over
//! the store's index, newest first. Words match whole terms, the last one
//! as a prefix while it is being typed.
//!
//! Operators: `from:` `to:` `subject:` `filename:` (terms in that field),
//! `has:attachment`, `is:unread` `is:read` `is:flagged`, `in:<folder>`,
//! `before:` / `after:` (YYYY-MM-DD, local days), `larger:` / `smaller:` (`10k`,
//! `2M`). A quoted value is several words.

use std::collections::BTreeSet;

use mail_schema::PartKind;
use mail_store::search::{field, words};
use mail_store::{flag, MsgId};

use crate::{Engine, Error};

/// One condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cond {
    /// A word in these fields (`prefix`: any term starting with it).
    Word { word: String, fields: u8, prefix: bool },
    HasAttachment,
    Unread(bool),
    Flagged,
    In(String),
    Before(i64),
    After(i64),
    Larger(u64),
    Smaller(u64),
    /// The quick filters with no other operator (§11.7; `crate::quick`
    /// bits): `is:to-me`, `is:from-people`, `is:vip`, `is:needs-reply`,
    /// `is:back`.
    Quick(u16),
}

/// A parsed query: every condition must hold.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Query {
    pub conds: Vec<Cond>,
}

/// The most results returned.
pub const RESULTS_MAX: usize = 500;

/// Seconds since 1970 of a UTC day "YYYY-MM-DD".
pub fn day(s: &str) -> Option<i64> {
    let mut it = s.split('-');
    let (y, m, d): (i64, i64, i64) = (it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?);
    if it.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Days from civil (Howard Hinnant).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146_097 + doe - 719_468) * 86_400)
}

fn size(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let (n, mul) = match s.chars().last()? {
        'k' => (&s[..s.len() - 1], 1_000),
        'm' => (&s[..s.len() - 1], 1_000_000),
        'g' => (&s[..s.len() - 1], 1_000_000_000),
        _ => (s.as_str(), 1),
    };
    n.parse::<f64>().ok().filter(|v| *v >= 0.0).map(|v| (v * mul as f64) as u64)
}

/// The query's pieces: `key:value` or a word, quotes kept together.
fn pieces(q: &str) -> Vec<(Option<String>, String, bool)> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut was_quoted = false;
    let mut push = |cur: &mut String, q: bool| {
        if !cur.is_empty() {
            match cur.split_once(':') {
                Some((k, v)) if !k.is_empty() && k.chars().all(|c| c.is_ascii_alphabetic()) => out.push((Some(k.to_ascii_lowercase()), v.trim_matches('"').to_string(), q)),
                _ => out.push((None, cur.trim_matches('"').to_string(), q)),
            }
        }
        cur.clear();
    };
    for c in q.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                was_quoted = true;
                cur.push(c);
            }
            c if c.is_whitespace() && !quoted => {
                push(&mut cur, was_quoted);
                was_quoted = false;
            }
            c => cur.push(c),
        }
    }
    push(&mut cur, was_quoted);
    out
}

/// Parse `q`. `typing`: the last plain word is a prefix (results as you
/// type, when the query doesn't end in a space).
pub fn parse(q: &str, typing: bool) -> Query {
    let ends_open = typing && !q.ends_with(char::is_whitespace);
    let ps = pieces(q);
    let n = ps.len();
    let mut conds = Vec::new();
    for (i, (k, v, quoted)) in ps.into_iter().enumerate() {
        let last = i + 1 == n && ends_open && !quoted;
        let fields = match k.as_deref() {
            None => Some(field::ANY),
            Some("from") => Some(field::FROM),
            Some("to") => Some(field::TO),
            Some("subject") => Some(field::SUBJECT),
            Some("filename") => Some(field::FILENAME),
            _ => None,
        };
        if let Some(fields) = fields {
            // An address or file name is one term too; else its words.
            let lower = v.to_lowercase();
            if (fields == field::FROM || fields == field::TO) && lower.contains('@') || fields == field::FILENAME && lower.contains('.') {
                conds.push(Cond::Word { word: lower, fields, prefix: last });
                continue;
            }
            let ws: Vec<String> = words(&v).collect();
            let m = ws.len();
            for (j, w) in ws.into_iter().enumerate() {
                conds.push(Cond::Word { word: w, fields, prefix: last && j + 1 == m });
            }
            continue;
        }
        let c = match (k.as_deref().unwrap_or(""), v.to_ascii_lowercase().as_str()) {
            ("has", "attachment" | "attachments" | "file" | "files") => Some(Cond::HasAttachment),
            ("is", "unread") => Some(Cond::Unread(true)),
            ("is", "read") => Some(Cond::Unread(false)),
            ("is", "flagged" | "starred") => Some(Cond::Flagged),
            ("is", "to-me" | "tome") => Some(Cond::Quick(crate::quick::TO_ME)),
            ("is", "from-people" | "people") => Some(Cond::Quick(crate::quick::FROM_PEOPLE)),
            ("is", "vip" | "vips") => Some(Cond::Quick(crate::quick::VIPS)),
            ("is", "needs-reply") => Some(Cond::Quick(crate::quick::NEEDS_REPLY)),
            ("is", "back" | "snoozed-back") => Some(Cond::Quick(crate::quick::BACK)),
            ("in", f) if !f.is_empty() => Some(Cond::In(f.to_string())),
            // Days are the person's local days.
            ("before", d) => day(d).map(|t| Cond::Before(crate::clock::from_local(t))),
            ("after", d) => day(d).map(|t| Cond::After(crate::clock::from_local(t + 86_400))),
            ("larger", s) => size(s).map(Cond::Larger),
            ("smaller", s) => size(s).map(Cond::Smaller),
            _ => None,
        };
        match c {
            Some(c) => conds.push(c),
            // An unknown operator is searched as words.
            None => {
                for w in words(&format!("{} {v}", k.unwrap_or_default())) {
                    conds.push(Cond::Word { word: w, fields: field::ANY, prefix: false });
                }
            }
        }
    }
    Query { conds }
}

/// One result.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub msg: MsgId,
    pub date: i64,
    pub from: String,
    pub subject: String,
    pub preview: String,
    pub unread: bool,
}

impl Engine {
    /// A message as a result row (for views that aren't searches).
    pub fn hit(&self, id: MsgId) -> Option<Hit> {
        let m = self.store.message(id).ok()?;
        let at = m.locations.iter().chain(&m.held);
        let flags: Vec<u32> = at.filter_map(|(f, u)| self.store.at(*f, *u).ok().flatten().map(|(_, fl)| fl)).collect();
        let unread = !flags.is_empty() && flags.iter().all(|f| f & flag::SEEN == 0);
        let from = m.summary.from.first().map(|a| if a.name.is_empty() { a.addr.clone() } else { a.name.clone() }).unwrap_or_default();
        Some(Hit { msg: id, date: m.date, from, subject: m.summary.subject.clone(), preview: m.summary.preview.clone(), unread })
    }

    /// Run `q` (as [`parse`]d) over every account's mail; newest first, at
    /// most [`RESULTS_MAX`].
    pub fn search(&self, q: &Query) -> Result<Vec<Hit>, Error> {
        let mut set: Option<BTreeSet<MsgId>> = None;
        for c in &q.conds {
            if let Cond::Word { word, fields, prefix } = c {
                let got = self.store.term(word, *fields, *prefix)?;
                set = Some(match set {
                    None => got,
                    Some(s) => s.intersection(&got).copied().collect(),
                });
                if set.as_ref().is_some_and(|s| s.is_empty()) {
                    return Ok(Vec::new());
                }
            }
        }
        // Only operators: every message is a candidate.
        let candidates: Vec<MsgId> = match set {
            Some(s) => s.into_iter().collect(),
            None if q.conds.is_empty() => return Ok(Vec::new()),
            None => self.store.all_messages()?,
        };
        let mut folder_names = std::collections::HashMap::new();
        let bits = q.conds.iter().fold(0, |b, c| if let Cond::Quick(x) = c { b | x } else { b });
        let mut quick = (bits != 0).then(|| self.quick(bits));
        let mut hits = Vec::new();
        for id in candidates {
            let Ok(m) = self.store.message(id) else { continue };
            let flags: Vec<u32> = m.locations.iter().filter_map(|(f, u)| self.store.at(*f, *u).ok().flatten().map(|(_, fl)| fl)).collect();
            let unread = !flags.is_empty() && flags.iter().all(|f| f & flag::SEEN == 0);
            let ok = q.conds.iter().all(|c| match c {
                Cond::Word { .. } => true,
                Cond::HasAttachment => m.summary.parts.iter().any(|p| p.kind == PartKind::Attachment),
                Cond::Unread(u) => unread == *u,
                Cond::Flagged => flags.iter().any(|f| f & flag::FLAGGED != 0),
                Cond::In(name) => m.locations.iter().any(|(f, _)| {
                    let n = folder_names.entry(*f).or_insert_with(|| self.store.folder(*f).map(|x| x.name.to_lowercase()).unwrap_or_default());
                    n == name || n.rsplit(['/', '.']).next() == Some(name.as_str()) || (name == "inbox" && n == "inbox")
                }),
                Cond::Before(t) => m.date < *t,
                Cond::After(t) => m.date >= *t,
                Cond::Larger(n) => m.size > *n,
                Cond::Smaller(n) => m.size < *n,
                Cond::Quick(_) => true,
            });
            let ok = ok && quick.as_mut().is_none_or(|qk| qk.passes(id, None, unread, flags.iter().any(|f| f & flag::FLAGGED != 0)));
            if ok {
                let from = m.summary.from.first().map(|a| if a.name.is_empty() { a.addr.clone() } else { a.name.clone() }).unwrap_or_default();
                hits.push(Hit { msg: id, date: m.date, from, subject: m.summary.subject.clone(), preview: m.summary.preview.clone(), unread });
            }
        }
        hits.sort_by(|a, b| b.date.cmp(&a.date).then(b.msg.cmp(&a.msg)));
        hits.truncate(RESULTS_MAX);
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_filters_as_operators() {
        use crate::quick::*;
        let all = UNREAD | FLAGGED | TO_ME | FROM_PEOPLE | VIPS | ATTACHMENT | NEEDS_REPLY | BACK;
        let q = query_of(all);
        assert_eq!(q, "is:unread is:flagged is:to-me is:from-people is:vip has:attachment is:needs-reply is:back");
        let c = parse(&q, false).conds;
        assert_eq!(c.len(), 8);
        let bits = c.iter().fold(0, |b, c| match c {
            Cond::Unread(true) => b | UNREAD,
            Cond::Flagged => b | FLAGGED,
            Cond::HasAttachment => b | ATTACHMENT,
            Cond::Quick(x) => b | x,
            _ => b,
        });
        assert_eq!(bits, all);
    }

    #[test]
    fn queries() {
        let q = parse(r#"from:priya@example.com subject:"q4 numbers" has:attachment before:2026-11-20 nort"#, true);
        assert_eq!(q.conds[0], Cond::Word { word: "priya@example.com".into(), fields: field::FROM, prefix: false });
        assert_eq!(q.conds[1], Cond::Word { word: "q4".into(), fields: field::SUBJECT, prefix: false });
        assert_eq!(q.conds[2], Cond::Word { word: "numbers".into(), fields: field::SUBJECT, prefix: false });
        assert_eq!(q.conds[3], Cond::HasAttachment);
        assert_eq!(q.conds[4], Cond::Before(crate::clock::from_local(day("2026-11-20").unwrap())));
        assert_eq!(q.conds[5], Cond::Word { word: "nort".into(), fields: field::ANY, prefix: true });
        assert_eq!(day("1970-01-02"), Some(86_400));
        assert_eq!(day("2026-11-15"), Some(1_763_164_800 + 365 * 86_400));
        assert_eq!(parse("larger:2M is:unread", false).conds, [Cond::Larger(2_000_000), Cond::Unread(true)]);
        assert!(matches!(parse("north ", true).conds[0], Cond::Word { prefix: false, .. }));
    }
}
