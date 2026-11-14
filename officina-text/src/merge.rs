//! Merging reviewed copies (docs/spec/officina.md §16.22, §16.27).
//!
//! Two copies of one original are merged by reading each as pieces against
//! that original (everything rejected): kept text, changes, and paragraph
//! breaks, each with where it is in the copy and in the original. Changes of
//! the other copy that touch nothing this one changed are copied in as they
//! are, under their own revisions, so the result is still a reviewable
//! document whose reject-all is the original. Where both changed the same
//! words (or touching ones), this document keeps its version between two
//! markers and the other version is kept whole in a [`Conflicts`] entry, until
//! someone takes one, the other or both. Nothing is accepted behind anyone's
//! back, and no text of either copy is lost.
//!
//! [`Conflicts`]: crate::model::Conflicts

use std::collections::{HashMap, HashSet};

use officina_core::history::Op;

use crate::model::{AttrId, CharAttrs, CommentId, ConflictId, DelId, DelPiece, Deletion, FmtChange, NoteId, RevId, Revision, TextDoc, OBJECT_CHAR};
use crate::ops::{Fragment, TextOp};
use crate::track::Resolve;

type Pos = (usize, u32);

/// An untracked copy sharing less of its words with the original than this
/// is another document (judged once the original has 20 words or more).
const UNRELATED: f64 = 0.3;

/// Bytes of a conflict marker (U+FFFC).
const MARK: u32 = OBJECT_CHAR.len_utf8() as u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Kept,
    Change,
    Break,
}

/// A piece of a document read against its original: (block, byte) from and
/// to in the document (`x`) and in the original (`o`).
#[derive(Debug, Clone, Copy)]
struct Seg {
    kind: Kind,
    x0: Pos,
    x1: Pos,
    o0: Pos,
    o1: Pos,
}

/// Touching changes of one copy: where they are in the original.
#[derive(Debug, Clone, Copy)]
struct Group {
    theirs: bool,
    o0: Pos,
    o1: Pos,
}

/// Why two documents can't be merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeError {
    /// The other copy's original (its changes rejected) isn't this one's.
    NotSameOriginal,
    /// The other copy has conflicts of its own to settle first.
    HasConflicts,
}

/// A merge as ops on this document, and what it did.
#[derive(Debug, Clone)]
pub struct Merge {
    pub ops: Vec<TextOp>,
    /// Changes of the other copy copied in.
    pub applied: usize,
    /// Changes both copies made alike (kept once).
    pub same: usize,
    /// Places both changed differently.
    pub conflicts: usize,
}

/// What to keep of a conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Take {
    /// This document's version.
    Mine,
    /// The other copy's version (with its tracked changes).
    Theirs,
    /// Both: the other version follows this one, as an insertion.
    Both,
}

/// A conflict in the text: its markers and the other version's revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConflictAt {
    pub id: ConflictId,
    pub start: Pos,
    /// The end marker (its first byte).
    pub end: Pos,
    pub rev: RevId,
}

/// Where original position `p` is in a document read as `segs`: the first
/// and last places (they differ where changes sit at `p`).
fn map(segs: &[Seg], p: Pos, x_end: Pos, o_end: Pos) -> Option<(Pos, Pos)> {
    let (mut lo, mut hi): (Option<Pos>, Option<Pos>) = (None, None);
    let mut see = |x: Pos| {
        lo = Some(lo.map_or(x, |l| l.min(x)));
        hi = Some(hi.map_or(x, |h| h.max(x)));
    };
    if p == (0, 0) {
        see((0, 0));
    }
    if p == o_end {
        see(x_end);
    }
    let from = segs.partition_point(|s| s.o1 < p);
    for s in &segs[from..] {
        if s.o0 > p {
            break;
        }
        if s.kind == Kind::Kept && s.o0.0 == p.0 && s.o0.1 <= p.1 && p.1 <= s.o1.1 {
            see((s.x0.0, s.x0.1 + p.1 - s.o0.1));
        }
        if s.o0 == p {
            see(s.x0);
        }
        if s.o1 == p {
            see(s.x1);
        }
    }
    lo.zip(hi)
}

/// Runs of touching changes.
fn groups(segs: &[Seg], theirs: bool) -> Vec<Group> {
    let mut out: Vec<Group> = Vec::new();
    let mut open = false;
    for s in segs {
        if s.kind != Kind::Change {
            open = false;
            continue;
        }
        match out.last_mut() {
            Some(g) if open => g.o1 = s.o1,
            _ => out.push(Group { theirs, o0: s.o0, o1: s.o1 }),
        }
        open = true;
    }
    out
}

/// Text and looks of what accepting everything shows of `x0..x1`, runs of
/// equal looks joined (for "both made the same change").
fn accepted_view(d: &TextDoc, x0: Pos, x1: Pos) -> Vec<(String, CharAttrs)> {
    let mut out: Vec<(String, CharAttrs)> = Vec::new();
    let mut push = |t: &str, a: CharAttrs| match out.last_mut() {
        Some(l) if l.1 == a => l.0.push_str(t),
        _ => out.push((t.to_string(), a)),
    };
    for b in x0.0..=x1.0 {
        let Some(p) = d.para(b) else { break };
        if b > x0.0 {
            push("\n", CharAttrs::default());
        }
        let (s0, e0) = (if b == x0.0 { x0.1 } else { 0 }, if b == x1.0 { x1.1 } else { p.text.len() as u32 });
        for (s, e, a) in p.spans() {
            let (s, e) = (s.max(s0), e.min(e0));
            let x = d.attrs.get(a);
            if s < e && x.deleted.is_none() && x.conflict.is_none() {
                push(&p.text[s as usize..e as usize], crate::compare::looks(x));
            }
        }
    }
    out
}

/// Ids handed out while ops are built and applied one by one.
struct Ids {
    del: DelId,
    note: NoteId,
    comment: CommentId,
}

/// Copying the other document's text in: its attributes, revisions,
/// deletions, pictures, notes and comments, as this document's.
struct Copy<'a> {
    b: &'a TextDoc,
    attrs: HashMap<AttrId, AttrId>,
    revs: HashMap<RevId, RevId>,
    sets: HashMap<u32, u32>,
    next_set: u32,
    dels: HashMap<DelId, DelId>,
    images: HashMap<u32, u32>,
    notes: HashMap<NoteId, NoteId>,
    comments: HashMap<CommentId, CommentId>,
    ids: Ids,
    /// The first revision a fragment copied carried.
    first_rev: Option<RevId>,
}

/// Applies `op` to `m` and keeps it.
fn emit(m: &mut TextDoc, ops: &mut Vec<TextOp>, op: TextOp) {
    let o = op.clone();
    o.apply(m);
    ops.push(op);
}

impl<'a> Copy<'a> {
    fn new(m: &TextDoc, b: &'a TextDoc) -> Self {
        let next_set = m.revisions.iter().map(|(_, r)| r.set + 1).max().unwrap_or(0);
        let note = m.notes.iter().map(|(i, _)| i + 1).max().unwrap_or(0);
        let comment = m.comments.iter().map(|(i, _)| i + 1).max().unwrap_or(0);
        Copy {
            b,
            attrs: HashMap::new(),
            revs: HashMap::new(),
            sets: HashMap::new(),
            next_set,
            dels: HashMap::new(),
            images: HashMap::new(),
            notes: HashMap::new(),
            comments: HashMap::new(),
            ids: Ids { del: m.deletions.next_id(), note, comment },
            first_rev: None,
        }
    }

    fn rev(&mut self, m: &mut TextDoc, ops: &mut Vec<TextOp>, r: RevId) -> RevId {
        if let Some(&x) = self.revs.get(&r) {
            self.first_rev.get_or_insert(x);
            return x;
        }
        let v: Revision = self.b.revisions.get(r).map(|v| (**v).clone()).unwrap_or_default();
        // The same revision in both (an earlier pass both copies carry).
        let same = m.revisions.iter().find(|(_, x)| x.author == v.author && x.date == v.date && x.name == v.name).map(|(i, _)| i);
        let id = match same {
            Some(i) => i,
            None => {
                let set = match self.sets.get(&v.set) {
                    Some(&s) => s,
                    None => {
                        let s = self.next_set;
                        self.next_set += 1;
                        self.sets.insert(v.set, s);
                        s
                    }
                };
                let id = m.revisions.next_id();
                emit(m, ops, TextOp::SetRevision { id, rev: Some(Revision { set, ..v }) });
                id
            }
        };
        self.revs.insert(r, id);
        self.first_rev.get_or_insert(id);
        id
    }

    fn comment(&mut self, m: &mut TextDoc, ops: &mut Vec<TextOp>, c: CommentId) -> CommentId {
        if let Some(&x) = self.comments.get(&c) {
            return x;
        }
        let Some(v) = self.b.comments.get(c).map(|v| (**v).clone()) else { return c };
        if m.comments.get(c).is_some_and(|x| **x == v) {
            self.comments.insert(c, c);
            return c;
        }
        let id = self.ids.comment;
        self.ids.comment += 1;
        self.comments.insert(c, id);
        let parent = v.parent.map(|p| self.comment(m, ops, p));
        emit(m, ops, TextOp::SetComment { id, comment: Some(crate::model::Comment { parent, ..v }) });
        // Its replies come too.
        let replies: Vec<CommentId> = self.b.comments.iter().filter(|(_, r)| r.parent == Some(c)).map(|(i, _)| i).collect();
        for r in replies {
            self.comment(m, ops, r);
        }
        id
    }

    fn pieces(&mut self, m: &mut TextDoc, ops: &mut Vec<TextOp>, pieces: &[DelPiece], depth: usize) -> Vec<DelPiece> {
        pieces
            .iter()
            .map(|q| {
                let para = q.para.as_ref().map(|(_, s, p)| {
                    let mut p = p.clone();
                    if let Some(r) = p.rev {
                        p.rev = Some(self.rev(m, ops, r));
                    }
                    (m.new_id(), *s, p)
                });
                let runs = q.runs.iter().map(|&(at, a)| (at, self.attr(m, ops, a, depth + 1))).collect();
                DelPiece { para, text: q.text.clone(), runs }
            })
            .collect()
    }

    /// Attribute `a` of the other document, in `m`.
    fn attr(&mut self, m: &mut TextDoc, ops: &mut Vec<TextOp>, a: AttrId, depth: usize) -> AttrId {
        if let Some(&x) = self.attrs.get(&a) {
            // Note the revision a fragment carries even when the attribute is known.
            if let Some(r) = self.b.attrs.get(a).rev {
                let r = self.rev(m, ops, r);
                self.first_rev.get_or_insert(r);
            }
            return x;
        }
        let mut x = self.b.attrs.get(a).clone();
        x.conflict = None;
        if let Some(r) = x.rev {
            x.rev = Some(self.rev(m, ops, r));
        }
        if let Some(f) = x.fmt {
            let rev = self.rev(m, ops, f.rev);
            let old = if depth < 64 { self.attr(m, ops, f.old, depth + 1) } else { 0 };
            x.fmt = Some(FmtChange { rev, old });
        }
        if let Some(t) = x.deleted.filter(|_| depth < 64) {
            let id = match self.dels.get(&t) {
                Some(&i) => i,
                None => {
                    let id = self.ids.del;
                    self.ids.del += 1;
                    self.dels.insert(t, id);
                    if let Some(d) = self.b.deletions.get(t).map(|d| (**d).clone()) {
                        let rev = self.rev(m, ops, d.rev);
                        let pieces = self.pieces(m, ops, &d.pieces, depth);
                        emit(m, ops, TextOp::SetDeletion { id, del: Some(Deletion { rev, pieces }) });
                    }
                    id
                }
            };
            x.deleted = Some(id);
        }
        if let Some(o) = &mut x.object {
            let theirs = self.b.images.get(o.image).map(|i| i.bytes.clone());
            if m.images.get(o.image).map(|i| i.bytes.clone()) != theirs {
                let img = match self.images.get(&o.image) {
                    Some(&i) => i,
                    None => {
                        let i = self.b.images.get(o.image).map(|i| m.images.add((**i).clone())).unwrap_or(0);
                        self.images.insert(o.image, i);
                        i
                    }
                };
                o.image = img;
            }
        }
        if let Some(n) = x.note {
            x.note = Some(match self.notes.get(&n) {
                Some(&i) => i,
                None => {
                    let id = self.ids.note;
                    self.ids.note += 1;
                    self.notes.insert(n, id);
                    if let Some(note) = self.b.notes.get(n).map(|v| (**v).clone()) {
                        let mut note = note;
                        for p in note.paras.iter_mut() {
                            let runs: Vec<(u32, AttrId)> = p.runs.iter().map(|&(s, a)| (s, self.attr(m, ops, a, depth + 1))).collect();
                            p.runs = runs;
                            p.id = m.new_id();
                        }
                        emit(m, ops, TextOp::SetNote { id, note: Some(note) });
                    }
                    id
                }
            });
        }
        if let Some(cs) = x.comments.clone() {
            let mut v: Vec<CommentId> = cs.iter().map(|&c| self.comment(m, ops, c)).collect();
            v.sort_unstable();
            v.dedup();
            x.comments = Some(v.into());
        }
        let y = m.attrs.intern(x);
        self.attrs.insert(a, y);
        y
    }

    /// The other document's `x0..x1` as pieces in `m` (a piece a paragraph).
    fn fragment(&mut self, m: &mut TextDoc, ops: &mut Vec<TextOp>, x0: Pos, x1: Pos) -> Vec<DelPiece> {
        let b = self.b;
        let mut pieces = vec![DelPiece::default()];
        for blk in x0.0..=x1.0 {
            let Some(p) = b.para(blk) else { break };
            if blk > x0.0 {
                let mut props = p.props.clone();
                if let Some(r) = props.rev {
                    props.rev = Some(self.rev(m, ops, r));
                }
                pieces.push(DelPiece { para: Some((m.new_id(), p.style, props)), text: String::new(), runs: Vec::new() });
            }
            let (s0, e0) = (if blk == x0.0 { x0.1 } else { 0 }, if blk == x1.0 { x1.1 } else { p.text.len() as u32 });
            for (s, e, a) in p.spans() {
                let (s, e) = (s.max(s0), e.min(e0));
                if s >= e {
                    continue;
                }
                let y = self.attr(m, ops, a, 0);
                let last = pieces.last_mut().expect("a piece");
                let at = last.text.len() as u32;
                if last.runs.last().map(|r| r.1) != Some(y) {
                    last.runs.push((at, y));
                }
                last.text.push_str(&p.text[s as usize..e as usize]);
            }
        }
        for p in pieces.iter_mut() {
            if p.runs.is_empty() {
                p.runs.push((0, 0));
            }
        }
        pieces
    }
}

impl TextDoc {
    /// This document with every change accepted, or rejected (conflicts stay
    /// when accepting).
    pub fn resolved_all(&self, how: Resolve) -> TextDoc {
        let mut d = self.clone();
        let ops = d.resolve_ops(how, |_| true);
        for op in ops {
            op.apply(&mut d);
        }
        d
    }

    /// The document read against its original: pieces in order, and where
    /// the original ends.
    fn walk(&self) -> (Vec<Seg>, Pos) {
        let mut scratch = self.clone();
        let mut out = Vec::new();
        let mut o: Pos = (0, 0);
        let mut prev_len = 0u32;
        for b in 0..self.blocks.len() {
            let Some(p) = self.para(b) else { continue };
            if b > 0 {
                let (x0, x1) = ((b - 1, prev_len), (b, 0));
                if p.props.rev.is_some() {
                    out.push(Seg { kind: Kind::Change, x0, x1, o0: o, o1: o });
                } else {
                    let o1 = (o.0 + 1, 0);
                    out.push(Seg { kind: Kind::Break, x0, x1, o0: o, o1 });
                    o = o1;
                }
            }
            for (s, e, a) in p.spans() {
                if s >= e {
                    continue;
                }
                let x = self.attrs.get(a);
                let (x0, x1) = ((b, s), (b, e));
                if let Some(t) = x.deleted {
                    let o1 = match self.deletions.get(t).cloned() {
                        Some(d) => {
                            let ps = scratch.resolve_pieces(&d.pieces, Resolve::Reject, &|_| true, 0);
                            match ps.len() {
                                1 => (o.0, o.1 + ps[0].text.len() as u32),
                                n => (o.0 + n - 1, ps[n - 1].text.len() as u32),
                            }
                        }
                        None => o,
                    };
                    out.push(Seg { kind: Kind::Change, x0, x1, o0: o, o1 });
                    o = o1;
                    continue;
                }
                if x.conflict.is_some() || x.rev.is_some() {
                    out.push(Seg { kind: Kind::Change, x0, x1, o0: o, o1: o });
                    continue;
                }
                let o1 = (o.0, o.1 + e - s);
                out.push(Seg { kind: if x.fmt.is_some() { Kind::Change } else { Kind::Kept }, x0, x1, o0: o, o1 });
                o = o1;
            }
            prev_len = p.text.len() as u32;
        }
        (out, o)
    }

    /// Where the document ends.
    fn end(&self) -> Pos {
        let n = self.blocks.len().saturating_sub(1);
        (n, self.para(n).map_or(0, |p| p.text.len() as u32))
    }

    /// The conflicts in the text, in order.
    pub fn conflicts(&self) -> Vec<ConflictAt> {
        let mut out = Vec::new();
        if self.conflicts.is_empty() {
            return out;
        }
        let mut open: HashMap<ConflictId, Pos> = HashMap::new();
        for b in 0..self.blocks.len() {
            let Some(p) = self.para(b) else { continue };
            if !p.text.contains(OBJECT_CHAR) {
                continue;
            }
            for (s, e, a) in p.spans() {
                if s >= e {
                    continue;
                }
                let Some(c) = self.attrs.get(a).conflict else { continue };
                match open.remove(&c) {
                    Some(start) => {
                        if let Some(alt) = self.conflicts.get(c) {
                            out.push(ConflictAt { id: c, start, end: (b, s), rev: alt.rev });
                        }
                    }
                    None => {
                        open.insert(c, (b, s));
                    }
                }
            }
        }
        out.sort_by_key(|c| c.start);
        out
    }

    /// Conflicts the document still holds: in the text, and inside deletions
    /// (they come back if the deletion is rejected).
    pub fn live_conflicts(&self) -> Vec<ConflictId> {
        let mut out: HashSet<ConflictId> = self.conflicts().into_iter().map(|c| c.id).collect();
        if self.conflicts.is_empty() {
            return Vec::new();
        }
        for t in self.live_deletions() {
            let Some(d) = self.deletions.get(t) else { continue };
            for p in &d.pieces {
                for &(_, a) in &p.runs {
                    if let Some(c) = self.attrs.get(a).conflict {
                        if self.conflicts.get(c).is_some() {
                            out.insert(c);
                        }
                    }
                }
            }
        }
        let mut v: Vec<ConflictId> = out.into_iter().collect();
        v.sort_unstable();
        v
    }

    /// Ops that merge `other`, another copy of this document's original, into
    /// this one (`author` and `date` name the other's changes if it has none
    /// tracked: it is compared with the original first).
    pub fn merge_ops(&mut self, other: &TextDoc, author: &str, date: &str) -> Result<Merge, MergeError> {
        if !other.live_conflicts().is_empty() {
            return Err(MergeError::HasConflicts);
        }
        let base = self.resolved_all(Resolve::Reject);
        let theirs = if other.changes().is_empty() {
            // An untracked copy shares most of its words with the original.
            let (x, y) = (base.visible_text().join("\n"), other.visible_text().join("\n"));
            if x.split_whitespace().count() >= 20 && crate::compare::similarity(&x, &y) < UNRELATED {
                return Err(MergeError::NotSameOriginal);
            }
            base.compare(other, author, date)
        } else {
            if other.resolved_all(Resolve::Reject).visible_text() != base.visible_text() {
                return Err(MergeError::NotSameOriginal);
            }
            other.clone()
        };
        let (sa, oa) = self.walk();
        let (sb, ob) = theirs.walk();
        if oa != ob {
            return Err(MergeError::NotSameOriginal);
        }
        let (ea, eb) = (self.end(), theirs.end());
        let mut all: Vec<Group> = groups(&sa, false);
        all.extend(groups(&sb, true));
        all.sort_by_key(|g| (g.o0, g.o1, g.theirs));
        // Clusters: groups that overlap or touch in the original.
        let mut clusters: Vec<Vec<Group>> = Vec::new();
        let mut reach: Option<Pos> = None;
        for g in all {
            match reach {
                Some(r) if g.o0 <= r => {
                    clusters.last_mut().expect("open cluster").push(g);
                    reach = Some(r.max(g.o1));
                }
                _ => {
                    clusters.push(vec![g]);
                    reach = Some(g.o1);
                }
            }
        }
        let mut m = self.clone();
        let mut ops = Vec::new();
        let mut copy = Copy::new(&m, &theirs);
        let (mut applied, mut same, mut conflicts) = (0, 0, 0);
        // Comments on text the other kept: onto the same text here.
        for s in sb.iter().filter(|s| s.kind == Kind::Kept) {
            let Some(cs) = theirs.para(s.x0.0).and_then(|p| theirs.attrs.get(p.attr_at(s.x0.1)).comments.clone()) else { continue };
            let (Some((ax0, _)), Some((_, ax1))) = (map(&sa, s.o0, ea, oa), map(&sa, s.o1, ea, oa)) else { continue };
            let ids: Vec<CommentId> = cs.iter().map(|&c| copy.comment(&mut m, &mut ops, c)).collect();
            for blk in ax0.0..=ax1.0 {
                let Some(p) = m.para(blk) else { break };
                let (s0, e0) = (if blk == ax0.0 { ax0.1 } else { 0 }, if blk == ax1.0 { ax1.1 } else { p.text.len() as u32 });
                let spans: Vec<(u32, u32, AttrId)> = p.spans().map(|(s, e, a)| (s.max(s0), e.min(e0), a)).filter(|x| x.0 < x.1).collect();
                let mut runs = Vec::new();
                let mut changed = false;
                for (s, _, a) in spans.iter().copied() {
                    let mut x = m.attrs.get(a).clone();
                    if x.deleted.is_none() && x.conflict.is_none() {
                        let mut v: Vec<CommentId> = x.comments.as_deref().unwrap_or(&[]).to_vec();
                        v.extend(&ids);
                        v.sort_unstable();
                        v.dedup();
                        if x.comments.as_deref() != Some(&v[..]) {
                            x.comments = Some(v.into());
                            changed = true;
                        }
                    }
                    runs.push((s, m.attrs.intern(x)));
                }
                if changed {
                    let (start, end) = (spans[0].0, spans[spans.len() - 1].1);
                    emit(&mut m, &mut ops, TextOp::Restyle { block: blk as u32, start, end, runs });
                }
            }
        }
        // Last place first, so earlier places keep their positions.
        for cl in clusters.iter().rev() {
            let ours = cl.iter().filter(|g| !g.theirs).count();
            let theirs_n = cl.len() - ours;
            if theirs_n == 0 {
                continue;
            }
            let c0 = cl.iter().map(|g| g.o0).min().expect("a group");
            let c1 = cl.iter().map(|g| g.o1).max().expect("a group");
            let (Some((ax0, _)), Some((_, ax1))) = (map(&sa, c0, ea, oa), map(&sa, c1, ea, oa)) else { continue };
            let (Some((bx0, _)), Some((_, bx1))) = (map(&sb, c0, eb, ob), map(&sb, c1, eb, ob)) else { continue };
            if ours == 0 {
                // Only theirs: copy it in over the original text here.
                let pieces = copy.fragment(&mut m, &mut ops, bx0, bx1);
                for op in m.delete_ops(ax0.0, ax0.1, ax1.0, ax1.1) {
                    emit(&mut m, &mut ops, op);
                }
                let mut put = Vec::new();
                m.restore_ops(ax0.0, ax0.1, 0, &pieces, &mut put);
                for op in put {
                    emit(&mut m, &mut ops, op);
                }
                applied += theirs_n;
                continue;
            }
            if ours == 1 && theirs_n == 1 && cl[0].o0 == cl[1].o0 && cl[0].o1 == cl[1].o1 && accepted_view(self, ax0, ax1) == accepted_view(&theirs, bx0, bx1) {
                same += 1;
                continue;
            }
            // A conflict: ours stays between markers, theirs is kept aside.
            copy.first_rev = None;
            let pieces = copy.fragment(&mut m, &mut ops, bx0, bx1);
            let rev = match copy.first_rev {
                Some(r) => r,
                None => {
                    // Theirs is the original text here (a change of ours touched it).
                    let r = theirs.revisions.iter().map(|(i, _)| i).next();
                    match r {
                        Some(r) => copy.rev(&mut m, &mut ops, r),
                        None => continue,
                    }
                }
            };
            let id = m.conflicts.next_id();
            emit(&mut m, &mut ops, TextOp::SetConflict { id, alt: Some(Deletion { rev, pieces }) });
            let mark = m.attrs.intern(CharAttrs { conflict: Some(id), ..Default::default() });
            let marker = || Fragment { text: OBJECT_CHAR.to_string(), runs: vec![(0, mark)] };
            emit(&mut m, &mut ops, TextOp::Splice { block: ax1.0 as u32, at: ax1.1, del: 0, ins: marker() });
            emit(&mut m, &mut ops, TextOp::Splice { block: ax0.0 as u32, at: ax0.1, del: 0, ins: marker() });
            conflicts += 1;
        }
        // What the ops interned and added stays with this document.
        self.attrs = m.attrs.clone();
        self.images = m.images.clone();
        self.raise_watermark(m.new_id());
        Ok(Merge { ops, applied, same, conflicts })
    }

    /// This document with every conflict settled as its own version, for
    /// files that can't hold conflicts.
    pub fn without_conflicts(&self) -> TextDoc {
        let mut d = self.clone();
        for c in self.conflicts().into_iter().rev() {
            let ops = d.take_ops(c.id, Take::Mine);
            for op in ops {
                op.apply(&mut d);
            }
        }
        d.conflicts = crate::model::Store::new(crate::model::MAX_CONFLICTS);
        d
    }

    /// This document merged with `other` (see [`TextDoc::merge_ops`]).
    pub fn merged(&self, other: &TextDoc, author: &str, date: &str) -> Result<(TextDoc, Merge), MergeError> {
        let mut d = self.clone();
        let mg = d.merge_ops(other, author, date)?;
        for op in mg.ops.clone() {
            op.apply(&mut d);
        }
        Ok((d, mg))
    }

    /// Ops that settle conflict `id`, keeping what `how` says.
    pub fn take_ops(&mut self, id: ConflictId, how: Take) -> Vec<TextOp> {
        let Some(c) = self.conflicts().into_iter().find(|c| c.id == id) else { return Vec::new() };
        let Some(alt) = self.conflicts.get(id).map(|a| (**a).clone()) else { return Vec::new() };
        let mut m = self.clone();
        let mut ops = Vec::new();
        let drop_marker = |at: Pos| TextOp::Splice { block: at.0 as u32, at: at.1, del: MARK, ins: Fragment::default() };
        match how {
            Take::Mine => {
                emit(&mut m, &mut ops, drop_marker(c.end));
                emit(&mut m, &mut ops, drop_marker(c.start));
            }
            Take::Theirs => {
                for op in m.delete_ops(c.start.0, c.start.1, c.end.0, c.end.1 + MARK) {
                    emit(&mut m, &mut ops, op);
                }
                let mut put = Vec::new();
                m.restore_ops(c.start.0, c.start.1, 0, &alt.pieces, &mut put);
                for op in put {
                    emit(&mut m, &mut ops, op);
                }
            }
            Take::Both => {
                // Theirs as it reads, inserted after ours.
                let pieces: Vec<DelPiece> = {
                    let accepted = m.resolve_pieces(&alt.pieces, Resolve::Accept, &|_| true, 0);
                    accepted
                        .into_iter()
                        .map(|q| {
                            let para = q.para.map(|(i, s, mut p)| {
                                p.rev = Some(alt.rev);
                                (i, s, p)
                            });
                            let mut text = String::new();
                            let mut runs = Vec::new();
                            for (k, &(s, a)) in q.runs.iter().enumerate() {
                                let e = q.runs.get(k + 1).map_or(q.text.len() as u32, |r| r.0);
                                let x = m.attrs.get(a).clone();
                                if s >= e || x.deleted.is_some() || x.conflict.is_some() {
                                    continue;
                                }
                                let ya = m.with_rev(a, alt.rev);
                                runs.push((text.len() as u32, ya));
                                text.push_str(&q.text[s as usize..e as usize]);
                            }
                            if runs.is_empty() {
                                runs.push((0, 0));
                            }
                            DelPiece { para, text, runs }
                        })
                        .collect()
                };
                let mut pieces = pieces;
                // Words side by side get a space between.
                let before = m.para(c.end.0).and_then(|p| p.text[..c.end.1 as usize].chars().rev().find(|&ch| ch != OBJECT_CHAR));
                let first = &mut pieces[0];
                if before.is_some_and(char::is_alphanumeric) && first.text.chars().next().is_some_and(char::is_alphanumeric) {
                    first.text.insert(0, ' ');
                    for r in first.runs.iter_mut().skip(1) {
                        r.0 += 1;
                    }
                }
                emit(&mut m, &mut ops, drop_marker(c.end));
                let mut put = Vec::new();
                m.restore_ops(c.end.0, c.end.1, 0, &pieces, &mut put);
                for op in put {
                    emit(&mut m, &mut ops, op);
                }
                emit(&mut m, &mut ops, drop_marker(c.start));
            }
        }
        emit(&mut m, &mut ops, TextOp::SetConflict { id, alt: None });
        self.attrs = m.attrs.clone();
        self.raise_watermark(m.new_id());
        ops
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::{looks, tokens};

    fn doc(paras: &[&str]) -> TextDoc {
        let mut d = TextDoc::new();
        d.blocks.remove(0);
        for p in paras {
            d.push_para(p, 0, vec![]);
        }
        d
    }

    /// Visible text with formatting, run by run (marks left out).
    fn view(d: &TextDoc) -> Vec<Vec<(String, CharAttrs)>> {
        let mut out = Vec::new();
        for b in d.blocks.iter() {
            let p = b.as_para().unwrap();
            let mut v: Vec<(String, CharAttrs)> = Vec::new();
            for (s, e, a) in p.spans() {
                let x = d.attrs.get(a);
                if s == e || x.deleted.is_some() || x.conflict.is_some() {
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

    fn apply(d: &mut TextDoc, ops: Vec<TextOp>) {
        for op in ops {
            op.apply(d);
        }
    }

    const DATE: &str = "2026-11-15T10:00:00Z";

    #[test]
    fn edits_in_different_places_both_go_in() {
        let o = doc(&["The buyer shall pay within sixty days.", "Delivery takes two weeks.", "The warranty runs twelve months."]);
        let a = o.compare(&doc(&["The buyer shall pay within thirty days.", "Delivery takes two weeks.", "The warranty runs twelve months."]), "Ann", DATE);
        let b = o.compare(&doc(&["The buyer shall pay within sixty days.", "Delivery takes two weeks.", "The warranty runs twenty-four months."]), "Ben", DATE);
        let (m, mg) = a.merged(&b, "Ben", DATE).unwrap();
        assert_eq!((mg.applied, mg.conflicts, mg.same), (1, 0, 0));
        assert_eq!(m.visible_text(), vec!["The buyer shall pay within thirty days.", "Delivery takes two weeks.", "The warranty runs twenty-four months."]);
        assert_eq!(m.resolved_all(Resolve::Reject).visible_text(), o.visible_text());
        let who: Vec<String> = m.change_sets().iter().map(|s| s.author.to_string()).collect();
        assert_eq!(who, vec!["Ann", "Ben"]);
        // An untracked copy is compared with the original first.
        let (m2, _) = a.merged(&doc(&["The buyer shall pay within sixty days.", "Delivery takes two weeks.", "The warranty runs twenty-four months."]), "Ben", DATE).unwrap();
        assert_eq!(m2.visible_text(), m.visible_text());
        // The same change in both is kept once.
        let (m3, mg3) = a.merged(&a, "Ann", DATE).unwrap();
        assert_eq!((mg3.applied, mg3.conflicts, mg3.same), (0, 0, 1));
        assert_eq!(view(&m3), view(&a));
    }

    #[test]
    fn the_same_words_changed_differently_is_one_conflict() {
        let o = doc(&["The buyer shall pay within sixty days."]);
        let a = o.compare(&doc(&["The buyer shall pay within thirty days."]), "Ann", DATE);
        let b = o.compare(&doc(&["The buyer shall pay within ninety days."]), "Ben", DATE);
        let (m, mg) = a.merged(&b, "Ben", DATE).unwrap();
        assert_eq!(mg.conflicts, 1);
        // Reads as ours until someone picks; theirs is kept.
        assert_eq!(m.visible_text(), vec!["The buyer shall pay within thirty days."]);
        let c = m.conflicts();
        assert_eq!(c.len(), 1);
        let e = m.edits();
        assert_eq!(e.len(), 1, "{e:?}");
        assert_eq!(e[0].summary, "Ann: “thirty” · Ben: “ninety”");
        let back = crate::native::load(&crate::native::save_to_vec(&m, &[])).unwrap().doc;
        assert_eq!(back.conflicts().len(), 1);
        assert_eq!(back.resolved_all(Resolve::Reject).visible_text(), o.visible_text());
        // Accepting everything leaves the conflict to settle.
        assert_eq!(m.resolved_all(Resolve::Accept).conflicts().len(), 1);
        let take = |how: Take| {
            let mut x = m.clone();
            let ops = x.take_ops(c[0].id, how);
            apply(&mut x, ops);
            assert!(x.conflicts().is_empty());
            assert_eq!(x.resolved_all(Resolve::Reject).visible_text(), o.visible_text(), "{how:?}");
            x.resolved_all(Resolve::Accept).visible_text()
        };
        assert_eq!(take(Take::Mine), vec!["The buyer shall pay within thirty days."]);
        assert_eq!(take(Take::Theirs), vec!["The buyer shall pay within ninety days."]);
        assert_eq!(take(Take::Both), vec!["The buyer shall pay within thirty ninety days."]);
        // Undo puts it back.
        let mut x = m.clone();
        let mut h: officina_core::history::History<TextOp> = officina_core::history::History::new();
        let ops = x.take_ops(c[0].id, Take::Theirs);
        h.run(&mut x, "take", ops);
        h.undo(&mut x);
        assert_eq!(view(&x), view(&m));
        assert_eq!(x.conflicts().len(), 1);
    }

    #[test]
    fn a_copy_compared_in_place_merges_an_untracked_one() {
        let o = doc(&["Pay within sixty days.", "Deliver in two weeks."]);
        let mut a = doc(&["Pay within thirty days.", "Deliver in two weeks."]);
        let ops = a.compare_ops(&o, "Ann", DATE).unwrap();
        apply(&mut a, ops);
        let b = doc(&["Pay within ninety days.", "Deliver in five weeks."]);
        let (m, mg) = a.merged(&b, "Ben", DATE).unwrap();
        assert_eq!((mg.applied, mg.conflicts), (1, 1));
        assert_eq!(m.visible_text(), vec!["Pay within thirty days.", "Deliver in five weeks."]);
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

    /// A paragraph that no other has (its first word is its number).
    fn para(r: &mut Rng, k: usize) -> String {
        let mut s = format!("p{k}");
        for _ in 0..1 + r.below(10) {
            s.push_str([" ", " ", ", "][r.below(3)]);
            s.push_str(WORDS[r.below(WORDS.len())]);
        }
        s
    }

    type Paras = Vec<(String, Vec<(u32, CharAttrs)>)>;

    /// One copy's edit of paragraph `k`'s text: what replaces it.
    fn edit(r: &mut Rng, p: &(String, Vec<(u32, CharAttrs)>), fresh: &mut usize) -> Paras {
        let t = &p.0;
        let toks = tokens(t);
        match r.below(6) {
            // A word replaced, put in or taken out (never the number).
            0 | 1 if toks.len() > 1 => {
                let (s, e) = toks[1 + r.below(toks.len() - 1)];
                let w = match r.below(3) {
                    0 => String::new(),
                    1 => format!("{} {}", WORDS[r.below(WORDS.len())], &t[s as usize..e as usize]),
                    _ => format!("new{}", r.below(100)),
                };
                let mut t2 = t.clone();
                t2.replace_range(s as usize..e as usize, &w);
                vec![(t2, vec![(0, CharAttrs::default())])]
            }
            // Taken out.
            2 => vec![],
            // A new paragraph after it.
            3 => {
                *fresh += 1;
                vec![p.clone(), (para(r, 1000 + *fresh), vec![(0, CharAttrs::default())])]
            }
            // Made bold.
            4 => vec![(t.clone(), vec![(0, CharAttrs { bold: Some(true), ..Default::default() })])],
            _ => {
                let mut t2 = t.clone();
                t2.push_str(" end");
                vec![(t2, vec![(0, CharAttrs::default())])]
            }
        }
    }

    fn build(paras: &Paras) -> TextDoc {
        let mut d = TextDoc::new();
        d.blocks.remove(0);
        for (t, runs) in paras {
            let runs = runs.iter().filter(|r| (r.0 as usize) < t.len().max(1)).map(|(s, x)| (*s, d.attrs.intern(x.clone()))).collect();
            d.push_para(t, 0, runs);
        }
        if d.blocks.is_empty() {
            d.push_para("", 0, vec![]);
        }
        d
    }

    #[test]
    fn random_merges_of_separate_edits_equal_applying_both() {
        let mut word_pairs = 0;
        for seed in 1..=10_000u64 {
            let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let n = 3 + r.below(10);
            let orig: Paras = (0..n).map(|k| (para(&mut r, k), vec![(0, CharAttrs::default())])).collect();
            // Who edits each paragraph: nobody, A or B, with untouched ones between.
            let mut who = vec![0u8; n];
            let mut last: Option<(usize, u8)> = None;
            for k in 0..n {
                let w = [0u8, 0, 1, 2, 3][r.below(5)];
                if w == 0 {
                    continue;
                }
                if let Some((j, lw)) = last {
                    if (lw != w || w == 3) && k <= j + 1 {
                        continue;
                    }
                }
                who[k] = w;
                last = Some((k, w));
            }
            let mut fresh = 0;
            let (mut ta, mut tb, mut both): (Paras, Paras, Paras) = (vec![], vec![], vec![]);
            for k in 0..n {
                let p = &orig[k];
                match who[k] {
                    1 => {
                        let e = edit(&mut r, p, &mut fresh);
                        ta.extend(e.clone());
                        tb.push(p.clone());
                        both.extend(e);
                    }
                    2 => {
                        let e = edit(&mut r, p, &mut fresh);
                        tb.extend(e.clone());
                        ta.push(p.clone());
                        both.extend(e);
                    }
                    // Both, words apart: A one word, B a later one.
                    3 => {
                        let words: Vec<(u32, u32)> = tokens(&p.0).into_iter().filter(|&(s, _)| p.0[s as usize..].starts_with(|c: char| c.is_alphanumeric())).collect();
                        if words.len() < 5 {
                            ta.push(p.clone());
                            tb.push(p.clone());
                            both.push(p.clone());
                            continue;
                        }
                        let i = 1 + r.below(words.len() - 4);
                        let j = i + 3 + r.below(words.len() - i - 3);
                        let (wa, wb) = (format!("ann{}", r.below(50)), format!("ben{}", r.below(50)));
                        let put = |t: &str, k: usize, w: &str| {
                            let mut t = t.to_string();
                            t.replace_range(words[k].0 as usize..words[k].1 as usize, w);
                            t
                        };
                        let plain = vec![(0, CharAttrs::default())];
                        ta.push((put(&p.0, i, &wa), plain.clone()));
                        tb.push((put(&p.0, j, &wb), plain.clone()));
                        both.push((put(&put(&p.0, j, &wb), i, &wa), plain));
                        word_pairs += 1;
                    }
                    _ => {
                        ta.push(p.clone());
                        tb.push(p.clone());
                        both.push(p.clone());
                    }
                }
            }
            let o = build(&orig);
            let a = o.compare(&build(&ta), "Ann", DATE);
            let b = o.compare(&build(&tb), "Ben", DATE);
            let (m, mg) = a.merged(&b, "Ben", DATE).unwrap_or_else(|e| panic!("seed {seed}: {e:?}"));
            assert_eq!(mg.conflicts, 0, "seed {seed}: no conflicts\n{ta:?}\n{tb:?}");
            assert_eq!(view(&m.resolved_all(Resolve::Accept)), view(&build(&both)), "seed {seed}: accept all is both");
            assert_eq!(view(&m.resolved_all(Resolve::Reject)), view(&o), "seed {seed}: reject all is the original");
            // Ben's copy edited without tracking merges the same.
            let (m2, _) = a.merged(&build(&tb), "Ben", DATE).unwrap();
            assert_eq!(view(&m2.resolved_all(Resolve::Accept)), view(&build(&both)), "seed {seed}: untracked copy");
            // Merging the other way round reads the same.
            let (m3, mg3) = b.merged(&a, "Ann", DATE).unwrap();
            assert_eq!(mg3.conflicts, 0);
            assert_eq!(view(&m3.resolved_all(Resolve::Accept)), view(&build(&both)), "seed {seed}: other way");
            // The native file keeps it; the ops undo to the copy.
            let back = crate::native::load(&crate::native::save_to_vec(&m, &[])).unwrap().doc;
            assert_eq!(view(&back.resolved_all(Resolve::Reject)), view(&o), "seed {seed}: file");
            let mut a2 = a.clone();
            let mg = a2.merge_ops(&b, "Ben", DATE).unwrap();
            let mut h: officina_core::history::History<TextOp> = officina_core::history::History::new();
            h.run(&mut a2, "merge", mg.ops);
            assert_eq!(view(&a2), view(&m));
            h.undo(&mut a2);
            assert_eq!(view(&a2), view(&a), "seed {seed}: undo");
            assert_eq!(a2.edits().len(), a.edits().len());
        }
        // Both copies changed words of one paragraph this often.
        assert!(word_pairs > 1000, "{word_pairs}");
    }

    #[test]
    fn random_overlapping_edits_become_conflicts_and_lose_nothing() {
        for seed in 1..=10_000u64 {
            let mut r = Rng(seed.wrapping_mul(0x2545_F491_4F6C_DD1D) | 1);
            let n = 1 + r.below(6);
            let orig: Paras = (0..n).map(|k| (para(&mut r, k), vec![(0, CharAttrs::default())])).collect();
            // Both replace the same word of one paragraph, differently.
            let k = r.below(n);
            let toks = tokens(&orig[k].0);
            if toks.len() < 2 {
                continue;
            }
            let (s, e) = toks[1 + r.below(toks.len() - 1)];
            let mut ta = orig.clone();
            let mut tb = orig.clone();
            ta[k].0.replace_range(s as usize..e as usize, &format!("ann{}", r.below(50)));
            tb[k].0.replace_range(s as usize..e as usize, &format!("ben{}", r.below(50)));
            // And sometimes more edits of their own around it.
            if r.below(2) == 0 {
                let t = ta[k].0.clone();
                ta[k].0 = format!("{t} more");
            }
            let o = build(&orig);
            let a = o.compare(&build(&ta), "Ann", DATE);
            let b = o.compare(&build(&tb), "Ben", DATE);
            let (m, mg) = a.merged(&b, "Ben", DATE).unwrap();
            assert!(mg.conflicts >= 1, "seed {seed}: a conflict");
            assert_eq!(view(&m.resolved_all(Resolve::Reject)), view(&o), "seed {seed}: reject all is the original");
            // Ours reads; theirs is in the conflict, whole.
            assert_eq!(view(&m), view(&a.resolved_all(Resolve::Accept)), "seed {seed}: reads as ours");
            let mut all_theirs = m.clone();
            for c in m.conflicts().into_iter().rev() {
                let ops = all_theirs.take_ops(c.id, Take::Theirs);
                apply(&mut all_theirs, ops);
            }
            assert_eq!(all_theirs.visible_text()[k].contains("ben"), true, "seed {seed}: theirs kept");
            assert_eq!(view(&all_theirs.resolved_all(Resolve::Reject)), view(&o), "seed {seed}: theirs rejects to the original");
            let mut all_mine = m.clone();
            for c in m.conflicts().into_iter().rev() {
                let ops = all_mine.take_ops(c.id, Take::Mine);
                apply(&mut all_mine, ops);
            }
            assert_eq!(view(&all_mine), view(&a.resolved_all(Resolve::Accept)), "seed {seed}: mine");
        }
    }
}
