//! Compare (docs/spec/officina.md §16.22, §16.26): two documents in, one out
//! whose proposed text is the second and whose tracked changes, all by one
//! revision, turn the first into it. Accepting everything gives the second
//! document; rejecting everything gives the first.
//!
//! Three passes. Paragraphs are matched by a diff over their text; between two
//! matches, edited paragraphs are paired with the most similar one ahead (by
//! shared words), and what is left over was deleted or inserted (a paragraph
//! deleted here and inserted there with the same text reads as a move, as
//! [`TextDoc::edits`] finds it). Paired paragraphs get a diff over word tokens,
//! so no edit is smaller than a word, and a lone space between two changes
//! joins them. Formatting is compared apart from the text: equal text whose
//! formatting differs carries a formatting change, so a restyle is one
//! formatting edit, never a rewrite.

use std::collections::HashMap;

use crate::model::{AttrId, CharAttrs, DelPiece, Deletion, FmtChange, ParaProps, RevId, Revision, StyleId, TextDoc, OBJECT_CHAR};
use crate::track::Resolve;
use crate::tree::BlockTree;

/// One step of a diff: a run of equal items, or a change (items of the first
/// taken out, items of the second put in; either may be empty).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hunk {
    /// (start in a, start in b, length)
    Equal(usize, usize, usize),
    /// (range in a, range in b)
    Change(usize, usize, usize, usize),
}

/// Past this many differences the middle of a diff is one change (a bound on
/// time and memory; documents that different are rewritten anyway).
const MAX_D: usize = 2048;

/// The diff of `a` and `b` (Myers, after taking off the common ends), as
/// hunks in order.
pub fn diff<T: PartialEq>(a: &[T], b: &[T]) -> Vec<Hunk> {
    let pre = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suf = a[pre..].iter().rev().zip(b[pre..].iter().rev()).take_while(|(x, y)| x == y).count();
    let (am, bm) = (&a[pre..a.len() - suf], &b[pre..b.len() - suf]);
    let mut out = Vec::new();
    if pre > 0 {
        out.push(Hunk::Equal(0, 0, pre));
    }
    for h in myers(am, bm) {
        out.push(match h {
            Hunk::Equal(i, j, n) => Hunk::Equal(i + pre, j + pre, n),
            Hunk::Change(i0, i1, j0, j1) => Hunk::Change(i0 + pre, i1 + pre, j0 + pre, j1 + pre),
        });
    }
    if suf > 0 {
        out.push(Hunk::Equal(a.len() - suf, b.len() - suf, suf));
    }
    out
}

fn myers<T: PartialEq>(a: &[T], b: &[T]) -> Vec<Hunk> {
    let (n, m) = (a.len(), b.len());
    if n == 0 && m == 0 {
        return Vec::new();
    }
    if n == 0 || m == 0 {
        return vec![Hunk::Change(0, n, 0, m)];
    }
    let max = n + m;
    let off = max as isize;
    let mut v = vec![0usize; 2 * max + 2];
    let mut trace: Vec<Vec<usize>> = Vec::new();
    let mut found = None;
    'outer: for d in 0..=max.min(MAX_D) {
        trace.push(v.clone());
        let d = d as isize;
        let mut k = -d;
        while k <= d {
            let i = (k + off) as usize;
            let mut x = if k == -d || (k != d && v[i - 1] < v[i + 1]) { v[i + 1] } else { v[i - 1] + 1 };
            let mut y = (x as isize - k) as usize;
            while x < n && y < m && a[x] == b[y] {
                x += 1;
                y += 1;
            }
            v[i] = x;
            if x >= n && y >= m {
                found = Some(d as usize);
                break 'outer;
            }
            k += 2;
        }
    }
    let Some(_) = found else { return vec![Hunk::Change(0, n, 0, m)] };
    // Walk back through the trace: diagonal steps are equal items, the one
    // step before each diagonal is an edit.
    #[derive(Clone, Copy, PartialEq)]
    enum Step {
        Eq,
        Del,
        Ins,
    }
    let mut steps: Vec<Step> = Vec::new();
    let (mut x, mut y) = (n as isize, m as isize);
    for (d, v) in trace.iter().enumerate().rev() {
        let d = d as isize;
        let k = x - y;
        let at = |k: isize| v[(k + off) as usize] as isize;
        let prev_k = if k == -d || (k != d && at(k - 1) < at(k + 1)) { k + 1 } else { k - 1 };
        let px = at(prev_k);
        let py = px - prev_k;
        while x > px && y > py && x > 0 && y > 0 {
            steps.push(Step::Eq);
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            steps.push(if x == px { Step::Ins } else { Step::Del });
            x = px;
            y = py;
        }
    }
    steps.reverse();
    let mut out: Vec<Hunk> = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    for st in steps {
        match st {
            Step::Eq => {
                match out.last_mut() {
                    Some(Hunk::Equal(_, _, l)) => *l += 1,
                    _ => out.push(Hunk::Equal(i, j, 1)),
                }
                i += 1;
                j += 1;
            }
            Step::Del | Step::Ins => {
                let (di, dj) = if st == Step::Del { (1, 0) } else { (0, 1) };
                match out.last_mut() {
                    Some(Hunk::Change(_, i1, _, j1)) => {
                        *i1 += di;
                        *j1 += dj;
                    }
                    _ => out.push(Hunk::Change(i, i + di, j, j + dj)),
                }
                i += di;
                j += dj;
            }
        }
    }
    out
}

/// Word tokens of `s` as byte ranges: a run of letters and digits (with
/// apostrophes inside), a run of spaces, or one other character.
pub fn tokens(s: &str) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    let mut it = s.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        let mut end = i + c.len_utf8();
        let word = |c: char| c.is_alphanumeric() || c == '_';
        if word(c) {
            while let Some(&(j, d)) = it.peek() {
                let apos = (d == '\'' || d == '’') && s[j + d.len_utf8()..].chars().next().is_some_and(word);
                if word(d) || apos {
                    end = j + d.len_utf8();
                    it.next();
                } else {
                    break;
                }
            }
        } else if c == ' ' {
            while let Some(&(j, ' ')) = it.peek() {
                end = j + 1;
                it.next();
            }
        }
        out.push((i as u32, end as u32));
    }
    out
}

/// Hunks with a lone space (or nothing) between two changes joined into one
/// change, so "the quick brown" → "a slow red" is one replacement.
fn join_changes(h: Vec<Hunk>, a: &[&str], _b: &[&str]) -> Vec<Hunk> {
    let mut out: Vec<Hunk> = Vec::new();
    let mut i = 0;
    while i < h.len() {
        let cur = h[i];
        if let (Some(Hunk::Change(a0, _, b0, _)), Hunk::Equal(ea, _, n), Some(Hunk::Change(_, a1, _, b1))) = (out.last().copied(), cur, h.get(i + 1).copied()) {
            if n == 1 && a[ea].chars().all(|c| c == ' ') {
                out.pop();
                out.push(Hunk::Change(a0, a1, b0, b1));
                i += 2;
                continue;
            }
        }
        match (out.last_mut(), cur) {
            (Some(Hunk::Change(_, x1, _, y1)), Hunk::Change(_, a1, _, b1)) => {
                *x1 = a1;
                *y1 = b1;
            }
            _ => out.push(cur),
        }
        i += 1;
    }
    out
}

/// Words of a paragraph's text as a multiset, for pairing edited paragraphs.
fn word_bag(s: &str) -> HashMap<&str, usize> {
    let mut m = HashMap::new();
    for (a, b) in tokens(s) {
        let w = &s[a as usize..b as usize];
        if w.chars().next().is_some_and(|c| c.is_alphanumeric()) {
            *m.entry(w).or_default() += 1;
        }
    }
    m
}

/// How alike two paragraphs are: shared words over the larger count (0 to 1).
pub(crate) fn similarity(a: &str, b: &str) -> f64 {
    let (x, y) = (word_bag(a), word_bag(b));
    let (nx, ny) = (x.values().sum::<usize>(), y.values().sum::<usize>());
    if nx == 0 && ny == 0 {
        return if a == b { 1.0 } else { 0.0 };
    }
    let shared: usize = x.iter().map(|(w, n)| (*n).min(*y.get(w).unwrap_or(&0))).sum();
    shared as f64 / nx.max(ny) as f64
}

/// Paired paragraphs this alike or more are one edited paragraph.
const PAIR: f64 = 0.5;

/// A paragraph of a document being compared: its visible text and the
/// attributes of each run (tombstones left out).
struct Src {
    text: String,
    runs: Vec<(u32, CharAttrs)>,
    style: StyleId,
    props: ParaProps,
}

fn sources(d: &TextDoc) -> Vec<Src> {
    let mut out = Vec::new();
    for b in d.blocks.iter() {
        let Some(p) = b.as_para() else { continue };
        let mut s = Src { text: String::new(), runs: Vec::new(), style: p.style, props: p.props.clone() };
        s.props.rev = None;
        for (a, e, at) in p.spans() {
            if a == e {
                continue;
            }
            let x = d.attrs.get(at);
            if x.deleted.is_some() {
                continue;
            }
            let mut x = x.clone();
            x.rev = None;
            x.fmt = None;
            let at = s.text.len() as u32;
            s.text.push_str(&p.text[a as usize..e as usize]);
            if s.runs.last().map(|r| &r.1) != Some(&x) {
                s.runs.push((at, x));
            }
        }
        if s.runs.is_empty() {
            s.runs.push((0, d.attrs.get(p.attr_at(0)).clone()));
        }
        out.push(s);
    }
    out
}

impl Src {
    fn attr_at(&self, at: u32) -> &CharAttrs {
        let i = self.runs.partition_point(|r| r.0 <= at).saturating_sub(1);
        &self.runs[i].1
    }
}

/// The formatting of attributes, what a formatting change is about.
fn looks(a: &CharAttrs) -> CharAttrs {
    let mut x = a.clone();
    x.comments = None;
    x.note = None;
    x.rev = None;
    x.deleted = None;
    x.fmt = None;
    if let Some(o) = &mut x.object {
        o.image = 0;
    }
    x
}

/// Building the compared document.
struct Out<'a> {
    a: &'a TextDoc,
    d: TextDoc,
    rev: RevId,
    styles: HashMap<StyleId, StyleId>,
    images: HashMap<u32, u32>,
    /// Paragraphs made: text, runs, style, props.
    paras: Vec<(String, Vec<(u32, AttrId)>, StyleId, ParaProps)>,
    /// Paragraphs of the first document deleted before any paragraph was made.
    lead: Vec<DelPiece>,
    /// The next paragraph made starts with an inserted break.
    break_next: bool,
}

impl Out<'_> {
    /// Attributes of the first document, in the result's tables.
    fn from_a(&mut self, x: &CharAttrs) -> AttrId {
        let mut y = x.clone();
        y.note = None;
        y.comments = None;
        y.rev = None;
        y.deleted = None;
        y.fmt = None;
        y.style = y.style.map(|s| self.style(s));
        if let Some(o) = &mut y.object {
            let img = match self.images.get(&o.image) {
                Some(&i) => i,
                None => {
                    let i = self.a.images.get(o.image).map(|d| self.d.images.add((**d).clone())).unwrap_or(0);
                    self.images.insert(o.image, i);
                    i
                }
            };
            o.image = img;
        }
        self.d.attrs.intern(y)
    }

    fn style(&mut self, s: StyleId) -> StyleId {
        if let Some(&t) = self.styles.get(&s) {
            return t;
        }
        let t = self.a.styles.styles.get(s as usize).and_then(|st| self.d.styles.by_name(&st.name)).unwrap_or(0);
        self.styles.insert(s, t);
        t
    }

    /// Text of the first document `src` from `s` to `e` as deletion runs.
    fn del_runs(&mut self, src: &Src, s: u32, e: u32) -> (String, Vec<(u32, AttrId)>) {
        let mut runs = Vec::new();
        for (i, (at, x)) in src.runs.iter().enumerate() {
            let end = src.runs.get(i + 1).map_or(src.text.len() as u32, |r| r.0);
            let (rs, re) = ((*at).max(s), end.min(e));
            if rs < re {
                let id = self.from_a(x);
                if runs.last().map(|r: &(u32, AttrId)| r.1) != Some(id) {
                    runs.push((rs - s, id));
                }
            }
        }
        let text = src.text[s as usize..e as usize].to_string();
        if runs.is_empty() {
            runs.push((0, 0));
        }
        (text, runs)
    }

    fn tombstone(&mut self, pieces: Vec<DelPiece>) -> Option<AttrId> {
        let id = self.d.deletions.add(Deletion { rev: self.rev, pieces })?;
        Some(self.d.attrs.intern(CharAttrs { deleted: Some(id), ..Default::default() }))
    }

    /// Paragraphs of the first document deleted whole, after the last
    /// paragraph made (or before the first).
    fn delete_paras(&mut self, srcs: &[&Src]) {
        let mut pieces = Vec::new();
        for s in srcs {
            let (text, runs) = self.del_runs(s, 0, s.text.len() as u32);
            let style = self.style(s.style);
            let id = self.d.new_id();
            pieces.push(DelPiece { para: Some((id, style, s.props.clone())), text, runs });
        }
        if self.paras.is_empty() {
            self.lead.extend(pieces);
            return;
        }
        pieces.insert(0, DelPiece { para: None, text: String::new(), runs: vec![(0, 0)] });
        if let Some(t) = self.tombstone(pieces) {
            let last = self.paras.last_mut().expect("a paragraph");
            let at = last.0.len() as u32;
            last.0.push(OBJECT_CHAR);
            last.1.push((at, t));
        }
    }

    /// A paragraph of the second document, inserted whole.
    fn insert_para(&mut self, b: &Src) {
        let runs = b.runs.iter().map(|(at, x)| (*at, self.mark(x, None))).collect();
        self.push(b.text.clone(), runs, b.style, b.props.clone(), true);
    }

    /// `x` of the second document as inserted (`old` none) or as kept text
    /// formatted like `old` before.
    fn mark(&mut self, x: &CharAttrs, old: Option<&CharAttrs>) -> AttrId {
        let mut y = x.clone();
        match old {
            None => y.rev = Some(self.rev),
            Some(o) if looks(o) != looks(x) => {
                let mut was = o.clone();
                was.comments = x.comments.clone();
                was.note = x.note;
                if let (Some(w), Some(n)) = (&mut was.object, &x.object) {
                    w.image = n.image;
                }
                let mut was_id = self.from_a(&was);
                // Comments and notes are the second document's.
                let mut w = self.d.attrs.get(was_id).clone();
                w.comments = x.comments.clone();
                w.note = x.note;
                if let (Some(wo), Some(n)) = (&mut w.object, &x.object) {
                    wo.image = n.image;
                }
                was_id = self.d.attrs.intern(w);
                y.fmt = Some(FmtChange { rev: self.rev, old: was_id });
            }
            Some(_) => {}
        }
        self.d.attrs.intern(y)
    }

    fn push(&mut self, text: String, runs: Vec<(u32, AttrId)>, style: StyleId, mut props: ParaProps, inserted: bool) {
        let mut text = text;
        let mut runs = runs;
        if inserted && !self.paras.is_empty() {
            props.rev = Some(self.rev);
        }
        if inserted && self.paras.is_empty() && self.lead.is_empty() {
            // Paragraphs inserted before the first kept one: the break before
            // that one is the new one (as many breaks as paragraphs).
            self.break_next = true;
        } else if self.break_next && !inserted {
            props.rev = Some(self.rev);
            self.break_next = false;
        }
        // Paragraphs deleted before the first one made: a tombstone at its start.
        if !self.lead.is_empty() {
            let mut pieces = std::mem::take(&mut self.lead);
            // The first deleted paragraph takes this one's place; this one
            // starts after the last of them.
            let first = pieces.remove(0);
            pieces.insert(0, DelPiece { para: None, text: first.text, runs: first.runs });
            // A kept paragraph goes on after them; an inserted one's place
            // is the last of them.
            if !inserted {
                let id = self.d.new_id();
                pieces.push(DelPiece { para: Some((id, style, props.clone())), text: String::new(), runs: vec![(0, 0)] });
            }
            if let Some(t) = self.tombstone(pieces) {
                let n = OBJECT_CHAR.len_utf8() as u32;
                text.insert(0, OBJECT_CHAR);
                runs = std::iter::once((0, t)).chain(runs.into_iter().map(|(at, a)| (at + n, a))).collect();
            }
        }
        if runs.is_empty() {
            runs.push((0, 0));
        }
        self.paras.push((text, runs, style, props));
    }

    /// A paragraph of the first document edited into one of the second: a
    /// word diff, deletions before insertions, formatting compared on what
    /// stayed.
    fn edit_para(&mut self, a: &Src, b: &Src) {
        let ta = tokens(&a.text);
        let tb = tokens(&b.text);
        let wa: Vec<&str> = ta.iter().map(|&(s, e)| &a.text[s as usize..e as usize]).collect();
        let wb: Vec<&str> = tb.iter().map(|&(s, e)| &b.text[s as usize..e as usize]).collect();
        let hunks = join_changes(diff(&wa, &wb), &wa, &wb);
        let mut text = String::new();
        let mut runs: Vec<(u32, AttrId)> = Vec::new();
        let put = |out: &mut Self, text: &mut String, runs: &mut Vec<(u32, AttrId)>, s: &str, at: AttrId| {
            let _ = out;
            if s.is_empty() {
                return;
            }
            let p = text.len() as u32;
            if runs.last().map(|r| r.1) != Some(at) {
                runs.push((p, at));
            }
            text.push_str(s);
        };
        let span = |t: &[(u32, u32)], i: usize, j: usize, len: usize| -> (u32, u32) {
            if i >= j {
                let at = t.get(i).map_or(len as u32, |x| x.0);
                return (at, at);
            }
            (t[i].0, t[j - 1].1)
        };
        for h in hunks {
            match h {
                Hunk::Equal(i, j, n) => {
                    let (sa, _) = span(&ta, i, i + n, a.text.len());
                    let (sb, eb) = span(&tb, j, j + n, b.text.len());
                    // Kept text, run by run of the second, formatting against the first.
                    let mut k = sb;
                    while k < eb {
                        let ib = b.runs.partition_point(|r| r.0 <= k).saturating_sub(1);
                        let end = b.runs.get(ib + 1).map_or(b.text.len() as u32, |r| r.0).min(eb);
                        // Cut where the first document's runs change, too.
                        let ka = sa + (k - sb);
                        let ia = a.runs.partition_point(|r| r.0 <= ka).saturating_sub(1);
                        let end_a = a.runs.get(ia + 1).map_or(a.text.len() as u32, |r| r.0);
                        let end = end.min(sb + (end_a - sa)).max(k + 1);
                        let mut end = end.min(eb);
                        while !b.text.is_char_boundary(end as usize) {
                            end += 1;
                        }
                        let at = self.mark(&b.runs[ib].1, Some(a.attr_at(ka)));
                        put(self, &mut text, &mut runs, &b.text[k as usize..end as usize], at);
                        k = end;
                    }
                }
                Hunk::Change(i0, i1, j0, j1) => {
                    let (sa, ea) = span(&ta, i0, i1, a.text.len());
                    if sa < ea {
                        let (dt, dr) = self.del_runs(a, sa, ea);
                        if let Some(t) = self.tombstone(vec![DelPiece { para: None, text: dt, runs: dr }]) {
                            put(self, &mut text, &mut runs, "\u{FFFC}", t);
                        }
                    }
                    let (sb, eb) = span(&tb, j0, j1, b.text.len());
                    let mut k = sb;
                    while k < eb {
                        let ib = b.runs.partition_point(|r| r.0 <= k).saturating_sub(1);
                        let end = b.runs.get(ib + 1).map_or(b.text.len() as u32, |r| r.0).min(eb);
                        let at = self.mark(&b.runs[ib].1, None);
                        put(self, &mut text, &mut runs, &b.text[k as usize..end as usize], at);
                        k = end;
                    }
                }
            }
        }
        if runs.is_empty() {
            let at = self.mark(&b.runs[0].1, Some(&a.runs[0].1));
            runs.push((0, at));
        }
        self.push(text, runs, b.style, b.props.clone(), false);
    }
}

impl TextDoc {
    /// This document (the first) compared with `other` (the second): a
    /// document reading as `other`, with tracked changes by `author` at `date`
    /// that turn this one into it. Tracked changes already in either are
    /// accepted first.
    pub fn compare(&self, other: &TextDoc, author: &str, date: &str) -> TextDoc {
        use officina_core::history::Op;
        let accepted = |d: &TextDoc| {
            let mut x = d.clone();
            if !x.revisions.is_empty() {
                let ops = x.resolve_ops(Resolve::Accept, |_| true);
                for op in ops {
                    op.apply(&mut x);
                }
            }
            x
        };
        let a = accepted(self);
        let b = accepted(other);
        let (sa, sb) = (sources(&a), sources(&b));
        let mut d = b.clone();
        d.blocks = BlockTree::default();
        d.deletions = crate::model::Store::new(crate::model::MAX_DELETIONS);
        let set = d.revisions.iter().map(|(_, r)| r.set + 1).max().unwrap_or(0);
        let rev = d.revisions.add(Revision { author: author.into(), date: date.into(), set, name: "Compare".into() }).unwrap_or(0);
        let mut o = Out { a: &a, d, rev, styles: HashMap::new(), images: HashMap::new(), paras: Vec::new(), lead: Vec::new(), break_next: false };
        let ka: Vec<&str> = sa.iter().map(|s| s.text.as_str()).collect();
        let kb: Vec<&str> = sb.iter().map(|s| s.text.as_str()).collect();
        for h in diff(&ka, &kb) {
            match h {
                Hunk::Equal(i, j, n) => {
                    for k in 0..n {
                        o.edit_para(&sa[i + k], &sb[j + k]);
                    }
                }
                Hunk::Change(i0, i1, j0, j1) => {
                    // Pair edited paragraphs in order, each with the most alike ahead.
                    let mut j = j0;
                    let mut i = i0;
                    while i < i1 || j < j1 {
                        let best = (i < i1).then(|| (j..j1).take(64).map(|q| (q, similarity(&sa[i].text, &sb[q].text))).filter(|x| x.1 >= PAIR).max_by(|x, y| x.1.total_cmp(&y.1).then(y.0.cmp(&x.0)))).flatten();
                        match best {
                            Some((q, _)) => {
                                for k in j..q {
                                    o.insert_para(&sb[k]);
                                }
                                o.edit_para(&sa[i], &sb[q]);
                                i += 1;
                                j = q + 1;
                            }
                            None if i < i1 => {
                                // Deleted, unless a later one pairs with what's next in b.
                                let mut run = vec![&sa[i]];
                                i += 1;
                                while i < i1 && !(j..j1).take(64).any(|q| similarity(&sa[i].text, &sb[q].text) >= PAIR) {
                                    run.push(&sa[i]);
                                    i += 1;
                                }
                                o.delete_paras(&run);
                            }
                            None => {
                                o.insert_para(&sb[j]);
                                j += 1;
                            }
                        }
                    }
                }
            }
        }
        // Everything was deleted: one empty paragraph holds it.
        if !o.lead.is_empty() || o.paras.is_empty() {
            // Its place is the last deleted paragraph's.
            o.push(String::new(), Vec::new(), 0, ParaProps::default(), true);
        }
        let Out { mut d, paras, .. } = o;
        for (text, runs, style, props) in paras {
            let k = d.push_para(&text, style, runs);
            d.blocks.update(k, |b| {
                if let Some(p) = b.as_para_mut() {
                    p.props = props;
                }
            });
        }
        d.take_changes();
        d
    }
}

impl TextDoc {
    /// Ops that make this document (the second) the comparison of `original`
    /// with it: tombstones where text was taken out, marks on what was put in
    /// or reformatted, all one new revision by `author`. The text stays as it
    /// is. None when this document has tracked changes of its own.
    pub fn compare_ops(&mut self, original: &TextDoc, author: &str, date: &str) -> Option<Vec<crate::ops::TextOp>> {
        use crate::ops::{Fragment, TextOp};
        if !self.changes().is_empty() {
            return None;
        }
        let c = original.compare(self, author, date);
        if c.blocks.len() != self.blocks.len() {
            return None;
        }
        let (crev, r) = c.revisions.iter().last().map(|(i, r)| (i, (**r).clone()))?;
        let (rev, mut ops) = self.add_revision_ops(r);
        // Attributes and deletions of the comparison, in this document's tables.
        struct Map {
            attrs: HashMap<AttrId, AttrId>,
            dels: HashMap<u32, u32>,
            images: HashMap<u32, u32>,
            next_del: u32,
        }
        fn attr(m: &mut Map, d: &mut TextDoc, c: &TextDoc, a: AttrId, crev: RevId, rev: RevId, ops: &mut Vec<TextOp>, depth: usize) -> AttrId {
            if let Some(&x) = m.attrs.get(&a) {
                return x;
            }
            let mut x = c.attrs.get(a).clone();
            if x.rev == Some(crev) {
                x.rev = Some(rev);
            }
            if let Some(f) = x.fmt {
                let old = attr(m, d, c, f.old, crev, rev, ops, depth + 1);
                x.fmt = Some(FmtChange { rev, old });
            }
            if let Some(o) = &mut x.object {
                // Pictures only the original had.
                if d.images.get(o.image).map(|i| i.bytes.clone()) != c.images.get(o.image).map(|i| i.bytes.clone()) {
                    let img = match m.images.get(&o.image) {
                        Some(&i) => i,
                        None => {
                            let i = c.images.get(o.image).map(|i| d.images.add((**i).clone())).unwrap_or(0);
                            m.images.insert(o.image, i);
                            i
                        }
                    };
                    o.image = img;
                }
            }
            if let Some(t) = x.deleted.filter(|_| depth < 64) {
                let id = match m.dels.get(&t) {
                    Some(&i) => i,
                    None => {
                        let id = m.next_del;
                        m.next_del += 1;
                        m.dels.insert(t, id);
                        if let Some(del) = c.deletions.get(t) {
                            let pieces = del
                                .pieces
                                .iter()
                                .map(|q| DelPiece {
                                    para: q.para.as_ref().map(|(_, s, p)| (d.new_id(), *s, p.clone())),
                                    text: q.text.clone(),
                                    runs: q.runs.iter().map(|&(at, a)| (at, attr(m, d, c, a, crev, rev, ops, depth + 1))).collect(),
                                })
                                .collect();
                            ops.push(TextOp::SetDeletion { id, del: Some(Deletion { rev, pieces }) });
                        }
                        id
                    }
                };
                x.deleted = Some(id);
            }
            let y = d.attrs.intern(x);
            m.attrs.insert(a, y);
            y
        }
        let mut m = Map { attrs: HashMap::new(), dels: HashMap::new(), images: HashMap::new(), next_del: self.deletions.next_id() };
        for k in 0..c.blocks.len() {
            let (Some(q), Some(p)) = (c.para(k), self.para(k)) else { continue };
            let q = q.clone();
            let (style, mut props) = (p.style, p.props.clone());
            // Tombstones, left to right (each shifts what follows).
            let mut runs: Vec<(u32, AttrId)> = Vec::new();
            let mut changed = false;
            for (s, e, a) in q.spans() {
                if s == e {
                    continue;
                }
                let y = attr(&mut m, self, &c, a, crev, rev, &mut ops, 0);
                if c.attrs.get(a).deleted.is_some() {
                    ops.push(TextOp::Splice { block: k as u32, at: s, del: 0, ins: Fragment { text: q.text[s as usize..e as usize].to_string(), runs: vec![(0, y)] } });
                }
                runs.push((s, y));
                changed |= c.attrs.get(a).rev.is_some() || c.attrs.get(a).fmt.is_some() || c.attrs.get(a).deleted.is_some();
            }
            if changed {
                ops.push(TextOp::Restyle { block: k as u32, start: 0, end: q.text.len() as u32, runs });
            }
            if q.props.rev.is_some() {
                props.rev = Some(rev);
                ops.push(TextOp::SetPara { block: k as u32, style, props });
            }
        }
        Some(ops)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use officina_core::history::Op;

    fn doc(paras: &[&str]) -> TextDoc {
        let mut d = TextDoc::new();
        d.blocks.remove(0);
        for p in paras {
            d.push_para(p, 0, vec![]);
        }
        d
    }

    fn resolved(d: &TextDoc, how: Resolve) -> Vec<String> {
        let mut x = d.clone();
        let ops = x.resolve_ops(how, |_| true);
        for op in ops {
            op.apply(&mut x);
        }
        x.visible_text()
    }

    /// Visible text with its formatting, run by run (marks left out).
    fn view(d: &TextDoc) -> Vec<Vec<(String, CharAttrs)>> {
        let mut out = Vec::new();
        for b in d.blocks.iter() {
            let p = b.as_para().unwrap();
            let mut v: Vec<(String, CharAttrs)> = Vec::new();
            for (s, e, a) in p.spans() {
                let x = d.attrs.get(a);
                if s == e || x.deleted.is_some() {
                    continue;
                }
                let l = looks(x);
                match v.last_mut() {
                    Some(r) if r.1 == l => r.0.push_str(&p.text[s as usize..e as usize]),
                    _ => v.push((p.text[s as usize..e as usize].to_string(), l)),
                }
            }
            out.push(v);
        }
        out
    }

    fn resolved_doc(d: &TextDoc, how: Resolve) -> TextDoc {
        let mut x = d.clone();
        let ops = x.resolve_ops(how, |_| true);
        for op in ops {
            op.apply(&mut x);
        }
        x
    }

    struct Rng(u64);
    impl Rng {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % n.max(1) as u64) as usize
        }
    }

    const WORDS: [&str; 12] = ["alpha", "beta", "gamma", "delta", "the", "a", "of", "and", "river", "stone", "light", "x"];

    fn random_para(r: &mut Rng) -> String {
        let n = 1 + r.below(10);
        let mut s = String::new();
        for i in 0..n {
            if i > 0 {
                s.push_str([" ", " ", ", ", "  "][r.below(4)]);
            }
            s.push_str(WORDS[r.below(WORDS.len())]);
        }
        if r.below(3) == 0 {
            s.push('.');
        }
        s
    }

    /// A paragraph's text and formatting runs (bold or italic on some words).
    fn styled(d: &mut TextDoc, text: &str, r: &mut Rng) -> Vec<(u32, AttrId)> {
        let mut runs = Vec::new();
        for (s, _) in tokens(text) {
            let a = match r.below(6) {
                0 => d.attrs.intern(CharAttrs { bold: Some(true), ..Default::default() }),
                1 => d.attrs.intern(CharAttrs { italic: Some(true), ..Default::default() }),
                _ => 0,
            };
            runs.push((s, a));
        }
        runs
    }

    #[test]
    fn random_compares_accept_to_the_second_and_reject_to_the_first() {
        for seed in 1..=10_000u64 {
            let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let mut a = TextDoc::new();
            a.blocks.remove(0);
            let n = 1 + r.below(8);
            for _ in 0..n {
                let t = random_para(&mut r);
                let runs = styled(&mut a, &t, &mut r);
                a.push_para(&t, 0, runs);
            }
            // The second: edits of the first, as (text, runs) per paragraph.
            let mut paras: Vec<(String, Vec<(u32, CharAttrs)>)> = a.blocks.iter().map(|b| {
                let p = b.as_para().unwrap();
                (p.text.clone(), p.runs.iter().map(|&(s, x)| (s, a.attrs.get(x).clone())).collect())
            }).collect();
            for _ in 0..1 + r.below(6) {
                let k = r.below(paras.len());
                match r.below(7) {
                    // Words: replace, insert, delete, in one paragraph.
                    0 | 1 if !paras[k].0.is_empty() => {
                        let toks = tokens(&paras[k].0);
                        let (s, e) = toks[r.below(toks.len())];
                        let w = if r.below(3) == 0 { String::new() } else { WORDS[r.below(WORDS.len())].to_string() };
                        let mut t = paras[k].0.clone();
                        t.replace_range(s as usize..e as usize, &w);
                        paras[k] = (t, vec![(0, CharAttrs::default())]);
                    }
                    // A new paragraph.
                    2 => {
                        let t = random_para(&mut r);
                        paras.insert(r.below(paras.len() + 1), (t, vec![(0, CharAttrs::default())]));
                    }
                    // A paragraph deleted (one stays).
                    3 if paras.len() > 1 => {
                        paras.remove(k);
                    }
                    // A paragraph moved.
                    4 if paras.len() > 1 => {
                        let p = paras.remove(k);
                        paras.insert(r.below(paras.len() + 1), p);
                    }
                    // Formatting: one paragraph made bold.
                    5 => {
                        paras[k].1 = vec![(0, CharAttrs { bold: Some(true), ..Default::default() })];
                    }
                    // Two paragraphs joined.
                    _ if k + 1 < paras.len() => {
                        let next = paras.remove(k + 1);
                        let at = paras[k].0.len() as u32;
                        paras[k].0.push_str(&next.0);
                        paras[k].1.extend(next.1.into_iter().map(|(s, x)| (s + at, x)));
                    }
                    _ => {}
                }
            }
            let mut b = TextDoc::new();
            b.blocks.remove(0);
            for (t, runs) in &paras {
                let runs = runs.iter().filter(|r| (r.0 as usize) < t.len().max(1)).map(|(s, x)| (*s, b.attrs.intern(x.clone()))).collect();
                b.push_para(t, 0, runs);
            }
            let c = a.compare(&b, "Cy", "2026-11-15T10:00:00Z");
            if std::env::var("CMP_SEED").ok() == Some(seed.to_string()) {
                eprintln!("A {:?}\nB {:?}", a.visible_text(), b.visible_text());
                for bl in c.blocks.iter() {
                    let p = bl.as_para().unwrap();
                    eprintln!("C {:?} rev={:?} runs={:?}", p.text, p.props.rev, p.runs.iter().map(|r| (r.0, c.attrs.get(r.1).rev, c.attrs.get(r.1).deleted.map(|t| c.deletions.get(t).unwrap().pieces.iter().map(|q| (q.para.is_some(), q.text.clone())).collect::<Vec<_>>()))).collect::<Vec<_>>());
                }
            }
            assert_eq!(c.visible_text(), b.visible_text(), "seed {seed}: reads as the second");
            assert_eq!(view(&resolved_doc(&c, Resolve::Accept)), view(&b), "seed {seed}: accept all");
            let rej = resolved_doc(&c, Resolve::Reject);
            assert_eq!(view(&rej), view(&a), "seed {seed}: reject all");
            let back = crate::native::load(&crate::native::save_to_vec(&c, &[])).unwrap().doc;
            assert_eq!(view(&resolved_doc(&back, Resolve::Reject)), view(&a), "seed {seed}: native file");
            // The same as ops on the second document: undoable, the text kept.
            let mut b2 = b.clone();
            let ops = b2.compare_ops(&a, "Cy", "2026-11-15T10:00:00Z").unwrap();
            let mut h: officina_core::history::History<crate::ops::TextOp> = officina_core::history::History::new();
            h.run(&mut b2, "compare", ops);
            assert_eq!(view(&b2), view(&b), "seed {seed}: ops keep the text");
            assert_eq!(view(&resolved_doc(&b2, Resolve::Reject)), view(&a), "seed {seed}: ops reject to the first");
            assert_eq!(b2.edits().len(), c.edits().len(), "seed {seed}: ops give the same edits");
            h.undo(&mut b2);
            assert_eq!(view(&b2), view(&b));
            assert!(b2.changes().is_empty(), "seed {seed}: undone");
            // Comparing a document with itself: no edits.
            assert!(a.compare(&a, "Cy", "").edits().is_empty(), "seed {seed}: self");
        }
    }

    #[test]
    fn a_long_document_compares_quickly() {
        let mut r = Rng(7);
        let paras: Vec<String> = (0..3000).map(|_| random_para(&mut r) + " " + &random_para(&mut r)).collect();
        let a = doc(&paras.iter().map(|s| s.as_str()).collect::<Vec<_>>());
        let mut edited = paras.clone();
        for k in (0..edited.len()).step_by(37) {
            edited[k] = edited[k].replacen("the", "a", 1) + " more";
        }
        edited.remove(100);
        edited.insert(2000, "An inserted paragraph here.".into());
        let b = doc(&edited.iter().map(|s| s.as_str()).collect::<Vec<_>>());
        let t = std::time::Instant::now();
        let c = a.compare(&b, "Cy", "");
        let took = t.elapsed();
        assert_eq!(c.visible_text(), b.visible_text());
        eprintln!("3,000 paragraphs, {} edits, compared in {took:?}", c.edits().len());
        assert!(took < std::time::Duration::from_secs(if cfg!(debug_assertions) { 30 } else { 3 }));
    }

    #[test]
    fn diff_is_minimal_and_ordered() {
        let a: Vec<char> = "ABCABBA".chars().collect();
        let b: Vec<char> = "CBABAC".chars().collect();
        let h = diff(&a, &b);
        let eq: usize = h.iter().map(|h| if let Hunk::Equal(_, _, n) = h { *n } else { 0 }).sum();
        assert_eq!(eq, 4, "{h:?}");
        assert_eq!(tokens("don't stop  now, ok").len(), 8);
    }

    #[test]
    fn compare_reads_as_the_second_and_rejects_to_the_first() {
        let a = doc(&["The quick brown fox jumps over the lazy dog.", "A paragraph that will move down.", "Unchanged text stays.", "This one is deleted entirely."]);
        let mut b = doc(&["The slow brown fox leaps over the lazy dog.", "Unchanged text stays.", "A paragraph that will move down.", "A new paragraph at the end."]);
        // "Unchanged" bold in the second: a formatting change.
        let bold = b.attrs.intern(CharAttrs { bold: Some(true), ..Default::default() });
        b.blocks.update(1, |x| x.as_para_mut().unwrap().runs = vec![(0, bold), (9, 0)]);
        let c = a.compare(&b, "Cy", "2026-11-15T10:00:00Z");
        assert_eq!(c.visible_text(), b.visible_text());
        assert_eq!(resolved(&c, Resolve::Accept), b.visible_text());
        assert_eq!(resolved(&c, Resolve::Reject), a.visible_text());
        let sums: Vec<String> = c.edits().into_iter().map(|e| e.summary).collect();
        assert!(sums.contains(&"“quick” → “slow”".to_string()), "{sums:?}");
        assert!(sums.contains(&"“jumps” → “leaps”".to_string()), "{sums:?}");
        assert!(sums.iter().any(|s| s.starts_with("moved")), "{sums:?}");
        assert!(sums.iter().any(|s| s.starts_with("bold")), "{sums:?}");
    }
}
