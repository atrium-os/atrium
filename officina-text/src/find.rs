//! Find, replace and word count over the model's text (docs/spec/officina.md
//! §7.4 "Moving around a big document", O3f).
//!
//! Find reads paragraph text, never laid-out pages, so it works before
//! pagination ends. A match lies inside one paragraph. [`Search`] finds every
//! match a slice of blocks at a time, so a caller can show results as they come
//! and stay responsive on a 1,500-page document.

use crate::model::{TextDoc, OBJECT_CHAR};
use crate::ops::{Fragment, TextOp};

/// Longest query, in characters (a longer one is cut).
pub const MAX_QUERY: usize = 1000;
/// Most matches one [`Search`] keeps.
pub const MAX_MATCHES: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Query {
    pub text: String,
    pub match_case: bool,
    pub whole_word: bool,
}

/// A match: bytes `start..end` of paragraph `block`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Match {
    pub block: usize,
    pub start: u32,
    pub end: u32,
}

fn word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The compiled form of a query.
#[derive(Debug, Clone)]
pub struct Matcher {
    folded: Vec<char>,
    match_case: bool,
    whole_word: bool,
}

impl Matcher {
    pub fn new(q: &Query) -> Matcher {
        let match_case = q.match_case;
        let folded = q.text.chars().take(MAX_QUERY).map(|c| fold(c, match_case)).collect();
        Matcher { folded, match_case, whole_word: q.whole_word }
    }

    pub fn is_empty(&self) -> bool {
        self.folded.is_empty()
    }

    /// Whether `text[start..]` begins with a match, and where it ends.
    fn at(&self, text: &str, start: usize) -> Option<usize> {
        let mut it = text[start..].char_indices();
        for &q in &self.folded {
            let (_, c) = it.next()?;
            if fold(c, self.match_case) != q {
                return None;
            }
        }
        let end = it.next().map_or(text.len(), |(i, _)| start + i);
        if self.whole_word {
            let before = text[..start].chars().next_back();
            let after = text[end..].chars().next();
            let first = text[start..].chars().next();
            let last = text[..end].chars().next_back();
            if before.is_some_and(word_char) && first.is_some_and(word_char) || after.is_some_and(word_char) && last.is_some_and(word_char) {
                return None;
            }
        }
        Some(end)
    }

    /// Calls `f(start, end)` for each match in `text` from byte `from`, in order,
    /// not overlapping, until `f` returns false.
    pub fn each(&self, text: &str, from: usize, mut f: impl FnMut(u32, u32) -> bool) {
        if self.folded.is_empty() || from > text.len() || !text.is_char_boundary(from) {
            return;
        }
        let first = self.folded[0];
        let mut i = from;
        while i < text.len() {
            let c = text[i..].chars().next().expect("in range");
            if fold(c, self.match_case) == first {
                if let Some(end) = self.at(text, i) {
                    if !f(i as u32, end as u32) {
                        return;
                    }
                    i = end;
                    continue;
                }
            }
            i += c.len_utf8();
        }
    }

    /// The matches in `text`, in order.
    pub fn all(&self, text: &str) -> Vec<(u32, u32)> {
        let mut v = Vec::new();
        self.each(text, 0, |s, e| {
            v.push((s, e));
            true
        });
        v
    }
}

/// Case folding one char to one char (enough for search: the lowercase of most
/// letters is one char; the rest compare as their first).
fn fold(c: char, match_case: bool) -> char {
    if match_case || c.is_ascii() {
        if match_case {
            c
        } else {
            c.to_ascii_lowercase()
        }
    } else {
        c.to_lowercase().next().unwrap_or(c)
    }
}

impl TextDoc {
    /// The first match after (or before, going back) byte `at` of paragraph
    /// `block`, wrapping round the document once. Going forward a match may
    /// start at `at`; going back it ends at or before `at`.
    pub fn find(&self, q: &Query, block: usize, at: u32, forward: bool) -> Option<Match> {
        let m = Matcher::new(q);
        let n = self.blocks.len();
        if m.is_empty() || n == 0 {
            return None;
        }
        let block = block.min(n - 1);
        if forward {
            for step in 0..=n {
                let b = (block + step) % n;
                let Some(p) = self.para(b) else { continue };
                let from = if step == 0 { at as usize } else { 0 };
                let to = if step == n { at as usize } else { usize::MAX };
                let mut hit = None;
                m.each(&p.text, from, |s, e| {
                    if (s as usize) < to {
                        hit = Some(Match { block: b, start: s, end: e });
                    }
                    false
                });
                if hit.is_some() {
                    return hit;
                }
            }
        } else {
            for step in 0..=n {
                let b = (block + n - step % n) % n;
                let Some(p) = self.para(b) else { continue };
                let mut hit = None;
                m.each(&p.text, 0, |s, e| {
                    let ok = match step {
                        0 => e <= at,
                        s0 if s0 == n => e > at,
                        _ => true,
                    };
                    if ok {
                        hit = Some(Match { block: b, start: s, end: e });
                    }
                    true
                });
                if hit.is_some() {
                    return hit;
                }
            }
        }
        None
    }

    /// Ops replacing every match with `with` (last first, so earlier offsets
    /// hold), as one action. The replacement takes the formatting of the
    /// match's first character (a picture's formatting without the picture).
    pub fn replace_all_ops(&mut self, q: &Query, with: &str) -> Vec<TextOp> {
        let m = Matcher::new(q);
        let mut ops = Vec::new();
        if m.is_empty() {
            return ops;
        }
        let mut found = Vec::new();
        for b in 0..self.blocks.len() {
            if let Some(p) = self.para(b) {
                for (s, e) in m.all(&p.text) {
                    found.push((b, s, e, p.attr_at(s)));
                }
            }
        }
        for (b, s, e, a) in found.into_iter().rev() {
            let attr = self.plain_attr(a);
            ops.push(TextOp::Splice { block: b as u32, at: s, del: e - s, ins: Fragment { text: with.to_string(), runs: vec![(0, attr)] } });
        }
        ops
    }

    /// `a` without a picture or note anchor in it.
    pub fn plain_attr(&mut self, a: crate::model::AttrId) -> crate::model::AttrId {
        if self.attrs.get(a).object.is_none() && self.attrs.get(a).note.is_none() {
            return a;
        }
        let mut c = self.attrs.get(a).clone();
        c.object = None;
        c.note = None;
        self.attrs.intern(c)
    }
}

/// Every match in a document, found a slice at a time.
#[derive(Debug, Clone)]
pub struct Search {
    m: Matcher,
    next: usize,
    pub matches: Vec<Match>,
    /// More matches than [`MAX_MATCHES`] were found (the rest aren't kept).
    pub capped: bool,
}

impl Search {
    pub fn new(q: &Query) -> Search {
        Search { m: Matcher::new(q), next: 0, matches: Vec::new(), capped: false }
    }

    pub fn done(&self, doc: &TextDoc) -> bool {
        self.m.is_empty() || self.next >= doc.blocks.len()
    }

    /// Searches up to `blocks` more paragraphs. Returns whether it is done.
    pub fn step(&mut self, doc: &TextDoc, blocks: usize) -> bool {
        if self.m.is_empty() {
            return true;
        }
        let end = (self.next + blocks).min(doc.blocks.len());
        for (b, blk) in doc.blocks.iter_from(self.next).take(end - self.next).enumerate() {
            let b = self.next + b;
            let Some(p) = blk.as_para() else { continue };
            let (ms, capped) = (&mut self.matches, &mut self.capped);
            self.m.each(&p.text, 0, |s, e| {
                if ms.len() >= MAX_MATCHES {
                    *capped = true;
                    return false;
                }
                ms.push(Match { block: b, start: s, end: e });
                true
            });
        }
        self.next = end;
        self.done(doc)
    }
}

/// Word count figures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counts {
    pub words: u64,
    /// Characters, pictures not counted.
    pub chars: u64,
    pub chars_no_spaces: u64,
    /// Paragraphs with text in them.
    pub paragraphs: u64,
}

impl Counts {
    pub fn add(&mut self, o: Counts) {
        self.words += o.words;
        self.chars += o.chars;
        self.chars_no_spaces += o.chars_no_spaces;
        self.paragraphs += o.paragraphs;
    }
}

/// Ideographs and kana count one word each, as other word processors count them.
fn ideographic(c: char) -> bool {
    matches!(c as u32, 0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x20000..=0x2FFFF)
}

/// Counts in one piece of text. A word is a run of characters between spaces
/// with a letter or digit in it; each ideograph is a word of its own.
pub fn count(text: &str) -> Counts {
    let mut c = Counts::default();
    let mut in_word = false;
    let mut has_alnum = false;
    let mut any = false;
    for ch in text.chars() {
        if ch == OBJECT_CHAR {
            continue;
        }
        any = true;
        c.chars += 1;
        if ch.is_whitespace() {
            if in_word && has_alnum {
                c.words += 1;
            }
            in_word = false;
            has_alnum = false;
            continue;
        }
        c.chars_no_spaces += 1;
        if ideographic(ch) {
            if in_word && has_alnum {
                c.words += 1;
            }
            c.words += 1;
            in_word = false;
            has_alnum = false;
            continue;
        }
        in_word = true;
        has_alnum |= ch.is_alphanumeric();
    }
    if in_word && has_alnum {
        c.words += 1;
    }
    c.paragraphs = any as u64;
    c
}

impl TextDoc {
    /// Counts over the body (headers and footers aside).
    pub fn counts(&self) -> Counts {
        let mut c = Counts::default();
        for b in self.blocks.iter() {
            if let Some(p) = b.as_para() {
                c.add(count(&p.text));
            }
        }
        c
    }
}

/// Word count kept current through edits: counts per paragraph, so an edit
/// recounts only what it changed.
#[derive(Debug, Clone, Default)]
pub struct WordCounter {
    per: Vec<Counts>,
    total: Counts,
}

fn para_counts(doc: &TextDoc, i: usize) -> Counts {
    doc.para(i).map(|p| count(&p.text)).unwrap_or_default()
}

impl WordCounter {
    pub fn new(doc: &TextDoc) -> WordCounter {
        let per: Vec<Counts> = doc.blocks.iter().map(|b| b.as_para().map(|p| count(&p.text)).unwrap_or_default()).collect();
        let mut total = Counts::default();
        per.iter().for_each(|c| total.add(*c));
        WordCounter { per, total }
    }

    pub fn total(&self) -> Counts {
        self.total
    }

    /// Follows an edit (as [`TextDoc::take_changes`] gives it).
    pub fn edit(&mut self, doc: &TextDoc, e: crate::model::Edit) {
        let old_last = (e.last as isize - e.delta).max(e.first as isize) as usize;
        if old_last > self.per.len() || e.last > doc.blocks.len() || self.per.len() as isize + e.delta != doc.blocks.len() as isize {
            *self = WordCounter::new(doc);
            return;
        }
        let new: Vec<Counts> = (e.first..e.last).map(|i| para_counts(doc, i)).collect();
        for c in &self.per[e.first..old_last] {
            self.total.words -= c.words;
            self.total.chars -= c.chars;
            self.total.chars_no_spaces -= c.chars_no_spaces;
            self.total.paragraphs -= c.paragraphs;
        }
        new.iter().for_each(|c| self.total.add(*c));
        self.per.splice(e.first..old_last, new);
    }

    /// Counts over blocks `range` only.
    pub fn over(&self, range: std::ops::Range<usize>) -> Counts {
        let mut c = Counts::default();
        self.per.get(range).unwrap_or_default().iter().for_each(|x| c.add(*x));
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CharAttrs, ObjectRef};
    use officina_core::history::Op;

    fn doc(paras: &[&str]) -> TextDoc {
        let mut d = TextDoc::new();
        d.blocks.remove(0);
        for p in paras {
            d.push_para(p, 0, vec![]);
        }
        d
    }

    fn q(t: &str) -> Query {
        Query { text: t.into(), ..Default::default() }
    }

    #[test]
    fn matcher_cases() {
        let m = Matcher::new(&q("ab"));
        assert_eq!(m.all("xAbab aB"), vec![(1, 3), (3, 5), (6, 8)]);
        let m = Matcher::new(&Query { text: "ab".into(), match_case: true, ..Default::default() });
        assert_eq!(m.all("xAbab aB"), vec![(3, 5)]);
        let m = Matcher::new(&Query { text: "ab".into(), whole_word: true, ..Default::default() });
        assert_eq!(m.all("ab abc cab ab, (ab)"), vec![(0, 2), (11, 13), (16, 18)]);
        let m = Matcher::new(&q("aa"));
        assert_eq!(m.all("aaaa"), vec![(0, 2), (2, 4)]);
        let m = Matcher::new(&q("straße"));
        assert_eq!(m.all("STRASSE Straße"), vec![(8, 15)]);
        let m = Matcher::new(&q("ÉTÉ"));
        assert_eq!(m.all("un été"), vec![(3, 8)]);
        assert!(Matcher::new(&q("")).all("abc").is_empty());
    }

    #[test]
    fn find_wraps_both_ways() {
        let d = doc(&["one cat", "no", "cat two cat"]);
        let f = |b, at, fw| d.find(&q("cat"), b, at, fw).map(|m| (m.block, m.start));
        assert_eq!(f(0, 0, true), Some((0, 4)));
        assert_eq!(f(0, 5, true), Some((2, 0)));
        assert_eq!(f(2, 1, true), Some((2, 8)));
        assert_eq!(f(2, 9, true), Some((0, 4)));
        assert_eq!(f(2, 8, false), Some((2, 0)));
        assert_eq!(f(2, 3, false), Some((2, 0)));
        assert_eq!(f(2, 2, false), Some((0, 4)));
        assert_eq!(f(0, 4, false), Some((2, 8)));
        // The only match, found again from inside it.
        let d = doc(&["x cat x"]);
        assert_eq!(d.find(&q("cat"), 0, 3, true).map(|m| m.start), Some(2));
        assert_eq!(d.find(&q("cat"), 0, 3, false).map(|m| m.start), Some(2));
        assert_eq!(d.find(&q("dog"), 0, 0, true), None);
    }

    #[test]
    fn replace_all_is_one_undoable_action() {
        let mut d = doc(&["a cat and a cat", "cat"]);
        let pic = d.attrs.intern(CharAttrs { object: Some(ObjectRef { image: 0, width: 10, height: 10 }), ..Default::default() });
        d.push_para("\u{FFFC}cat", 0, vec![(0, pic), (3, 0)]);
        let before = d.plain_text();
        let ops = d.replace_all_ops(&q("cat"), "dog");
        assert_eq!(ops.len(), 4);
        let mut h = officina_core::history::History::new();
        h.run(&mut d, "Replace All", ops);
        assert_eq!(d.plain_text(), "a dog and a dog\ndog\n\u{FFFC}dog");
        assert!(d.attrs.get(d.para(2).unwrap().attr_at(3)).object.is_none());
        h.undo(&mut d);
        assert_eq!(d.plain_text(), before);
        // A replacement containing the query doesn't loop.
        let ops = d.replace_all_ops(&q("cat"), "catcat");
        for o in ops {
            o.apply(&mut d);
        }
        assert_eq!(d.plain_text(), "a catcat and a catcat\ncatcat\n\u{FFFC}catcat");
    }

    #[test]
    fn search_in_slices() {
        let d = doc(&["cat", "x", "cat cat", "", "cat"]);
        let mut s = Search::new(&q("cat"));
        assert!(!s.step(&d, 2));
        assert_eq!(s.matches.len(), 1);
        assert!(s.step(&d, 10));
        assert_eq!(s.matches.iter().map(|m| (m.block, m.start)).collect::<Vec<_>>(), vec![(0, 0), (2, 0), (2, 4), (4, 0)]);
    }

    #[test]
    fn counter_follows_edits() {
        use crate::ops::TextOp;
        let mut d = doc(&["one two", "three", "four five six"]);
        let mut w = WordCounter::new(&d);
        assert_eq!(w.total().words, 6);
        let mut x: u64 = 77;
        for _ in 0..300 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let n = d.blocks.len();
            let b = (x % n as u64) as u32;
            let len = d.para(b as usize).unwrap().text.len() as u32;
            let at = if len == 0 { 0 } else { ((x >> 8) % (len as u64 + 1)) as u32 };
            let at = if d.para(b as usize).unwrap().text.is_char_boundary(at as usize) { at } else { 0 };
            let op = match (x >> 20) % 4 {
                0 => TextOp::Splice { block: b, at, del: 0, ins: Fragment { text: " w x".into(), runs: vec![(0, 0)] } },
                1 => TextOp::Split { block: b, at, id: d.new_id(), style: 0, props: Default::default() },
                2 if (b as usize) + 1 < n => TextOp::Join { block: b },
                _ => TextOp::Splice { block: b, at: 0, del: at, ins: Fragment::default() },
            };
            op.apply(&mut d);
            if let Some(e) = d.take_changes() {
                w.edit(&d, e);
            }
            assert_eq!(w.total(), d.counts());
        }
    }

    #[test]
    fn word_counts() {
        assert_eq!(count("Plain, bold, big, link.").words, 4);
        assert_eq!(count("  e-mail — it's 3.5 km ").words, 4);
        assert_eq!(count("日本語 text").words, 4);
        let c = count("a b\u{FFFC}");
        assert_eq!((c.words, c.chars, c.chars_no_spaces), (2, 3, 2));
        let d = doc(&["One two.", "", "three"]);
        let c = d.counts();
        assert_eq!((c.words, c.paragraphs), (3, 2));
    }
}
