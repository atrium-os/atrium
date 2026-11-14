//! Tracked changes (docs/spec/officina.md §16.22).
//!
//! The text is always the proposed text. An insertion is text whose attributes
//! carry `rev`; a paragraph break inserted is a paragraph whose `props.rev` is
//! set. A deletion takes the text out and leaves a tombstone, a U+FFFC whose
//! `deleted` names an entry of [`TextDoc::deletions`] holding what went out
//! (runs, and paragraph breaks with the following paragraph's id, style and
//! properties). A formatting change keeps the attributes the text had in
//! `fmt`. Everything here builds ordinary [`TextOp`]s, so undo, the journal
//! and the file need nothing else.
//!
//! Your own pending text is just text: deleting it removes it outright, and a
//! deletion next to one of yours of the same revision joins it, so pressing
//! Backspace ten times leaves one tombstone, not ten.

use crate::model::{AttrId, BlockId, CharAttrs, DelId, DelPiece, Deletion, FmtChange, ParaProps, RevId, Revision, StyleId, TextDoc, OBJECT_CHAR};
use crate::ops::{Fragment, TextOp};

/// Bytes of the tombstone character.
const TOMB: u32 = OBJECT_CHAR.len_utf8() as u32;

/// What to do with the revisions a filter picks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolve {
    Accept,
    Reject,
}

/// A tracked change in the text, as it stands: what it is and where.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub kind: ChangeKind,
    pub rev: RevId,
    /// (block, byte) from, and (block, byte) to (exclusive).
    pub from: (usize, u32),
    pub to: (usize, u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Insert,
    /// A paragraph break inserted (before block `from.0`).
    InsertBreak,
    Delete(DelId),
    Format,
}

/// A document with every deletion's text back in place, for formats that keep
/// deleted text inline (`.docx`): deleted text keeps `deleted` naming its
/// deletion (it is no longer a tombstone), and a paragraph whose break before
/// it was deleted is in `deleted_breaks` by id, with the revision.
pub struct Expanded {
    pub doc: TextDoc,
    pub deleted_breaks: std::collections::HashMap<BlockId, RevId>,
}

impl TextDoc {
    /// Ops that put revision `rev` in the store; its id.
    pub fn add_revision_ops(&self, rev: Revision) -> (RevId, Vec<TextOp>) {
        let id = self.revisions.next_id();
        (id, vec![TextOp::SetRevision { id, rev: Some(rev) }])
    }

    /// Whether revisions `a` and `b` are one author's (own text is just text).
    fn same_author(&self, a: RevId, b: RevId) -> bool {
        a == b || matches!((self.revisions.get(a), self.revisions.get(b)), (Some(x), Some(y)) if x.author == y.author)
    }

    /// `a` with insertion revision `rev`, interned: what typed text takes.
    pub fn with_rev(&mut self, a: AttrId, rev: RevId) -> AttrId {
        let mut x = self.attrs.get(a).clone();
        x.rev = Some(rev);
        x.deleted = None;
        x.fmt = None;
        self.attrs.intern(x)
    }

    /// The deletions the document still holds: tombstones in the text, and those
    /// inside them.
    pub fn live_deletions(&self) -> Vec<DelId> {
        let mut out = Vec::new();
        if self.deletions.is_empty() {
            return out;
        }
        let mut seen = std::collections::HashSet::new();
        let mut stack: Vec<DelId> = Vec::new();
        for b in self.blocks.iter() {
            let Some(p) = b.as_para() else { continue };
            if !p.text.contains(OBJECT_CHAR) {
                continue;
            }
            for (s, e, a) in p.spans() {
                if s < e {
                    if let Some(t) = self.attrs.get(a).deleted {
                        stack.push(t);
                    }
                }
            }
        }
        while let Some(t) = stack.pop() {
            if !seen.insert(t) {
                continue;
            }
            let Some(d) = self.deletions.get(t) else { continue };
            out.push(t);
            for p in &d.pieces {
                for (i, &(s, a)) in p.runs.iter().enumerate() {
                    let e = p.runs.get(i + 1).map_or(p.text.len() as u32, |r| r.0);
                    if s < e {
                        if let Some(n) = self.attrs.get(a).deleted {
                            stack.push(n);
                        }
                    }
                }
            }
        }
        out.sort_unstable();
        out
    }

    /// This document with its deletions put back as marked text
    /// ([`Expanded`]).
    pub fn expanded(&self) -> Expanded {
        use officina_core::history::Op;
        let mut d = self.clone();
        let mut breaks = std::collections::HashMap::new();
        let mut done = std::collections::HashSet::new();
        let mut from = d.blocks.len();
        while !d.deletions.is_empty() {
            // The last tombstone not yet put back (nested ones come back with
            // their deletion, after it).
            let mut found = None;
            'scan: for b in (0..from.min(d.blocks.len())).rev() {
                let Some(p) = d.para(b) else { continue };
                if !p.text.contains(OBJECT_CHAR) {
                    continue;
                }
                let spans: Vec<(u32, u32, AttrId)> = p.spans().collect();
                for &(s, e, a) in spans.iter().rev() {
                    if e - s == TOMB && &p.text[s as usize..e as usize] == "\u{FFFC}" {
                        if let Some(t) = d.attrs.get(a).deleted.filter(|t| !done.contains(t)) {
                            if d.attrs.get(a).object.is_none() && d.attrs.get(a).note.is_none() {
                                found = Some((b, s, t));
                                break 'scan;
                            }
                        }
                    }
                }
            }
            let Some((b, at, t)) = found else { break };
            done.insert(t);
            let Some(del) = d.deletions.get(t).cloned() else { continue };
            let pieces: Vec<DelPiece> = del
                .pieces
                .iter()
                .map(|q| {
                    let runs = q
                        .runs
                        .iter()
                        .map(|&(s, a)| {
                            let x = d.attrs.get(a).clone();
                            let tomb = x.deleted.is_some() && x.object.is_none() && x.note.is_none();
                            if tomb {
                                (s, a)
                            } else {
                                let mut y = x;
                                y.deleted = Some(t);
                                (s, d.attrs.intern(y))
                            }
                        })
                        .collect();
                    DelPiece { para: q.para.clone(), text: q.text.clone(), runs }
                })
                .collect();
            for q in &pieces[1..] {
                if let Some((id, _, _)) = &q.para {
                    breaks.insert(*id, del.rev);
                }
            }
            let mut ops = Vec::new();
            d.restore_ops(b, at, TOMB, &pieces, &mut ops);
            for op in ops {
                op.apply(&mut d);
            }
            from = b + pieces.len();
        }
        Expanded { doc: d, deleted_breaks: breaks }
    }

    /// Turns text read as deleted into tombstones, for readers of formats that
    /// keep deleted text inline: each range is (from, to, revision), a deleted
    /// paragraph break being (end of one, start of the next); ranges of one
    /// revision that touch become one.
    pub fn make_deletions(&mut self, mut ranges: Vec<((usize, u32), (usize, u32), RevId)>) {
        use officina_core::history::Op;
        ranges.retain(|r| r.0 < r.1);
        ranges.sort_by_key(|r| (r.0, r.1));
        let mut merged: Vec<((usize, u32), (usize, u32), RevId)> = Vec::new();
        for r in ranges {
            if let Some(l) = merged.last_mut() {
                // A deleted break is a range of its own, from the end of one
                // paragraph to the start of the next, so touching is enough.
                if l.2 == r.2 && l.1 >= r.0 {
                    l.1 = l.1.max(r.1);
                    continue;
                }
                if l.1 > r.0 {
                    // Overlapping deletions by two people: the first keeps it.
                    let rest = (l.1, r.1, r.2);
                    if rest.1 > rest.0 {
                        merged.push(rest);
                    }
                    continue;
                }
            }
            merged.push(r);
        }
        for (from, to, rev) in merged.into_iter().rev() {
            let id = self.deletions.next_id();
            let (ops, _) = self.tracked_delete_ops(from.0, from.1, to.0, to.1, rev, id);
            for op in ops {
                op.apply(self);
            }
        }
    }

    /// The tracked changes in the text, in document order.
    pub fn changes(&self) -> Vec<Change> {
        let mut out: Vec<Change> = Vec::new();
        if self.revisions.is_empty() {
            return out;
        }
        for (b, bl) in self.blocks.iter().enumerate() {
            let Some(p) = bl.as_para() else { continue };
            if let Some(r) = p.props.rev {
                out.push(Change { kind: ChangeKind::InsertBreak, rev: r, from: (b, 0), to: (b, 0) });
            }
            for (s, e, a) in p.spans() {
                if s == e {
                    continue;
                }
                let x = self.attrs.get(a);
                let mut push = |kind: ChangeKind, rev: RevId| match out.last_mut() {
                    Some(l) if l.kind == kind && l.rev == rev && l.to == (b, s) && !matches!(kind, ChangeKind::Delete(_)) => l.to = (b, e),
                    _ => out.push(Change { kind, rev, from: (b, s), to: (b, e) }),
                };
                if let Some(t) = x.deleted {
                    if let Some(d) = self.deletions.get(t) {
                        push(ChangeKind::Delete(t), d.rev);
                    }
                    continue;
                }
                if let Some(r) = x.rev {
                    push(ChangeKind::Insert, r);
                }
                if let Some(f) = x.fmt {
                    push(ChangeKind::Format, f.rev);
                }
            }
        }
        out
    }

    /// The text of paragraphs `b0..=b1` between the offsets as deletion pieces,
    /// leaving out text of `rev`'s author (own pending text just goes) and
    /// inlining tombstones of `rev` itself (so neighbouring deletions join).
    /// Also the tombstones inlined.
    fn deleted_pieces(&self, b0: usize, o0: u32, b1: usize, o1: u32, rev: RevId) -> (Vec<DelPiece>, Vec<DelId>) {
        let mut pieces = vec![DelPiece::default()];
        let mut inlined = Vec::new();
        let push = |pieces: &mut Vec<DelPiece>, text: &str, a: AttrId| {
            let last = pieces.last_mut().expect("one piece at least");
            if text.is_empty() {
                return;
            }
            let at = last.text.len() as u32;
            if last.runs.last().map(|r| r.1) != Some(a) {
                last.runs.push((at, a));
            }
            last.text.push_str(text);
        };
        for b in b0..=b1 {
            let Some(p) = self.para(b) else { continue };
            if b > b0 {
                // This paragraph's break: own inserted ones just go.
                if !p.props.rev.is_some_and(|r| self.same_author(r, rev)) {
                    pieces.push(DelPiece { para: Some((p.id, p.style, p.props.clone())), text: String::new(), runs: Vec::new() });
                }
            }
            let s0 = if b == b0 { o0 } else { 0 };
            let e0 = if b == b1 { o1 } else { p.text.len() as u32 };
            for (s, e, a) in p.spans() {
                let (s, e) = (s.max(s0), e.min(e0));
                if s >= e {
                    continue;
                }
                let x = self.attrs.get(a);
                if let Some(t) = x.deleted {
                    if let Some(d) = self.deletions.get(t).filter(|d| d.rev == rev) {
                        // Inline it: its first piece continues ours, the rest start paragraphs.
                        inlined.push(t);
                        for (k, q) in d.pieces.iter().enumerate() {
                            if k > 0 {
                                pieces.push(DelPiece { para: q.para.clone(), text: String::new(), runs: Vec::new() });
                            }
                            for (i, &(qs, qa)) in q.runs.iter().enumerate() {
                                let qe = q.runs.get(i + 1).map_or(q.text.len() as u32, |r| r.0);
                                push(&mut pieces, &q.text[qs as usize..qe as usize], qa);
                            }
                        }
                        continue;
                    }
                }
                if x.deleted.is_none() && x.rev.is_some_and(|r| self.same_author(r, rev)) {
                    continue;
                }
                push(&mut pieces, &p.text[s as usize..e as usize], a);
            }
        }
        // A piece with no text keeps its run list well formed.
        for p in pieces.iter_mut() {
            if p.runs.is_empty() {
                p.runs.push((0, 0));
            }
        }
        (pieces, inlined)
    }

    /// Ops that delete (`b0`, `o0`)..(`b1`, `o1`) as revision `rev`: what is
    /// not the author's own pending text goes into one tombstone at the start.
    /// The tombstone takes deletion id `id` (the store's next id, or past it
    /// when several deletions are built before any is applied). Returns the
    /// ops and where the tombstone is, if one was left.
    pub fn tracked_delete_ops(&mut self, b0: usize, mut o0: u32, b1: usize, mut o1: u32, rev: RevId, id: DelId) -> (Vec<TextOp>, Option<(usize, u32)>) {
        if (b1, o1) <= (b0, o0) || self.para(b0).is_none() || self.para(b1).is_none() {
            return (Vec::new(), None);
        }
        // Take in tombstones of the same revision either side, so deletions join.
        let tomb_of = |d: &TextDoc, b: usize, at: u32| -> bool {
            d.para(b).is_some_and(|p| p.text[at as usize..].starts_with(OBJECT_CHAR) && d.attrs.get(p.attr_at(at)).deleted.and_then(|t| d.deletions.get(t)).is_some_and(|x| x.rev == rev))
        };
        while o0 >= TOMB && self.para(b0).is_some_and(|p| p.text.is_char_boundary((o0 - TOMB) as usize)) && tomb_of(self, b0, o0 - TOMB) {
            o0 -= TOMB;
        }
        while self.para(b1).is_some_and(|p| (o1 as usize) < p.text.len()) && tomb_of(self, b1, o1) {
            o1 += TOMB;
        }
        let (pieces, inlined) = self.deleted_pieces(b0, o0, b1, o1, rev);
        let kept = pieces.len() > 1 || !pieces[0].text.is_empty();
        let mut ops = Vec::new();
        let mut tomb = None;
        if kept {
            ops.push(TextOp::SetDeletion { id, del: Some(Deletion { rev, pieces }) });
        }
        for t in inlined {
            ops.push(TextOp::SetDeletion { id: t, del: None });
        }
        ops.extend(self.delete_ops(b0, o0, b1, o1));
        if kept {
            let a = self.attrs.intern(CharAttrs { deleted: Some(id), ..Default::default() });
            ops.push(TextOp::Splice { block: b0 as u32, at: o0, del: 0, ins: Fragment { text: OBJECT_CHAR.to_string(), runs: vec![(0, a)] } });
            tomb = Some((b0, o0));
        }
        (ops, tomb)
    }

    /// Ops that apply `f` to the formatting of `start..end` of paragraph
    /// `block` as revision `rev`: text that isn't the author's own pending text
    /// keeps what it had in `fmt`; formatting put back as it was is no change.
    pub fn tracked_format_ops(&mut self, block: usize, start: u32, end: u32, rev: RevId, f: impl Fn(&mut CharAttrs)) -> Vec<TextOp> {
        let Some(p) = self.para(block) else { return Vec::new() };
        let mut spans: Vec<(u32, u32, AttrId)> = Vec::new();
        for (s, e, a) in p.spans() {
            let (s, e) = (s.max(start), e.min(end));
            if s < e {
                spans.push((s, e, a));
            }
        }
        if spans.is_empty() {
            return Vec::new();
        }
        let mut runs = Vec::new();
        for (s, _, a) in spans {
            let x = self.attrs.get(a).clone();
            if x.deleted.is_some() {
                runs.push((s, a));
                continue;
            }
            let own = x.rev.is_some_and(|r| self.same_author(r, rev));
            let mut y = x.clone();
            y.fmt = None;
            f(&mut y);
            if own {
                y.fmt = x.fmt;
                runs.push((s, self.attrs.intern(y)));
                continue;
            }
            let base = x.fmt.map_or(a, |m| m.old);
            let mut was = self.attrs.get(base).clone();
            // Comments come and go apart from formatting.
            was.comments = y.comments.clone();
            if y == was {
                runs.push((s, self.attrs.intern(was)));
            } else {
                y.fmt = Some(FmtChange { rev: x.fmt.map_or(rev, |m| m.rev), old: base });
                runs.push((s, self.attrs.intern(y)));
            }
        }
        vec![TextOp::Restyle { block: block as u32, start, end, runs }]
    }

    /// Ops that accept or reject every change whose revision `pick` takes, last
    /// paragraph first so each op's positions are still those it was built on.
    pub fn resolve_ops(&mut self, how: Resolve, pick: impl Fn(RevId) -> bool) -> Vec<TextOp> {
        self.resolve_range_ops(how, pick, (0, 0), (usize::MAX, u32::MAX))
    }

    /// [`TextDoc::resolve_ops`] for the changes inside (`from`)..=(`to`) only: a
    /// span counts if it lies inside, a paragraph break if its paragraph's start
    /// does.
    pub fn resolve_range_ops(&mut self, how: Resolve, pick: impl Fn(RevId) -> bool, from: (usize, u32), to: (usize, u32)) -> Vec<TextOp> {
        let mut ops = Vec::new();
        let n = self.blocks.len();
        let inside = |b: usize, s: u32, e: u32| from <= (b, s) && (b, e) <= to;
        for b in (from.0..n.min(to.0.saturating_add(1))).rev() {
            let Some(p) = self.para(b) else { continue };
            let (id, style, props, text) = (p.id, p.style, p.props.clone(), p.text.clone());
            let _ = id;
            let spans: Vec<(u32, u32, AttrId)> = p.spans().filter(|s| s.0 < s.1).collect();
            let break_picked = props.rev.is_some_and(&pick) && inside(b, 0, 0);
            if break_picked && how == Resolve::Accept {
                let mut q = props.clone();
                q.rev = None;
                ops.push(TextOp::SetPara { block: b as u32, style, props: q });
            }
            for &(s, e, a) in spans.iter().rev() {
                if !inside(b, s, e) {
                    continue;
                }
                let x = self.attrs.get(a).clone();
                if let Some(t) = x.deleted {
                    let Some(d) = self.deletions.get(t).cloned() else { continue };
                    if !pick(d.rev) {
                        continue;
                    }
                    match how {
                        Resolve::Accept => ops.push(TextOp::Splice { block: b as u32, at: s, del: e - s, ins: Fragment::default() }),
                        Resolve::Reject => {
                            // What comes back is resolved too: it is already behind us.
                            let pieces = self.resolve_pieces(&d.pieces, how, &pick, 0);
                            self.restore_ops(b, s, e - s, &pieces, &mut ops);
                        }
                    }
                    ops.push(TextOp::SetDeletion { id: t, del: None });
                    continue;
                }
                if how == Resolve::Reject && x.rev.is_some_and(&pick) {
                    ops.push(TextOp::Splice { block: b as u32, at: s, del: e - s, ins: Fragment::default() });
                    continue;
                }
                let mut y = x.clone();
                if let Some(m) = x.fmt.filter(|m| pick(m.rev)) {
                    match how {
                        Resolve::Accept => y.fmt = None,
                        Resolve::Reject => {
                            let mut old = self.attrs.get(m.old).clone();
                            old.comments = x.comments.clone();
                            old.rev = x.rev;
                            y = old;
                        }
                    }
                }
                if how == Resolve::Accept && x.rev.is_some_and(&pick) {
                    y.rev = None;
                }
                if y != x {
                    let ya = self.attrs.intern(y);
                    ops.push(TextOp::Restyle { block: b as u32, start: s, end: e, runs: vec![(s, ya)] });
                }
            }
            if break_picked && how == Resolve::Reject && b > 0 {
                ops.push(TextOp::Join { block: b as u32 - 1 });
            }
            let _ = text;
        }
        ops
    }

    /// Ops that put deletion `d` back where its tombstone (`at`, `len` bytes)
    /// is in paragraph `b`, splitting paragraphs at its breaks.
    fn restore_ops(&self, b: usize, at: u32, len: u32, pieces: &[DelPiece], ops: &mut Vec<TextOp>) {
        let frag = |p: &DelPiece| Fragment { text: p.text.clone(), runs: if p.text.is_empty() { Vec::new() } else { p.runs.clone() } };
        ops.push(TextOp::Splice { block: b as u32, at, del: len, ins: frag(&pieces[0]) });
        let mut block = b as u32;
        let mut pos = at + pieces[0].text.len() as u32;
        for p in &pieces[1..] {
            let (id, style, props): (BlockId, StyleId, ParaProps) = p.para.clone().unwrap_or((0, 0, ParaProps::default()));
            ops.push(TextOp::Split { block, at: pos, id, style, props });
            block += 1;
            if !p.text.is_empty() {
                ops.push(TextOp::Splice { block, at: 0, del: 0, ins: frag(p) });
            }
            pos = p.text.len() as u32;
        }
    }

    /// Deleted pieces with the picked changes in them resolved: what a rejected
    /// deletion puts back. Picked tombstones inside are put back (rejecting) or
    /// left out (accepting), picked insertions dropped or kept, picked breaks
    /// dropped or kept, picked formatting undone or kept.
    fn resolve_pieces(&mut self, pieces: &[DelPiece], how: Resolve, pick: &impl Fn(RevId) -> bool, depth: usize) -> Vec<DelPiece> {
        let mut out = vec![DelPiece::default()];
        let push = |out: &mut Vec<DelPiece>, text: &str, a: AttrId| {
            let last = out.last_mut().expect("one piece at least");
            if text.is_empty() {
                return;
            }
            let at = last.text.len() as u32;
            if last.runs.last().map(|r| r.1) != Some(a) {
                last.runs.push((at, a));
            }
            last.text.push_str(text);
        };
        for (k, q) in pieces.iter().enumerate() {
            if k > 0 {
                let (id, style, mut props) = q.para.clone().unwrap_or((0, 0, ParaProps::default()));
                match props.rev {
                    Some(r) if pick(r) && how == Resolve::Reject => {}
                    _ => {
                        if props.rev.is_some_and(pick) {
                            props.rev = None;
                        }
                        out.push(DelPiece { para: Some((id, style, props)), text: String::new(), runs: Vec::new() });
                    }
                }
            }
            for (i, &(qs, qa)) in q.runs.iter().enumerate() {
                let qe = q.runs.get(i + 1).map_or(q.text.len() as u32, |r| r.0);
                if qs >= qe {
                    continue;
                }
                let text = &q.text[qs as usize..qe as usize];
                let x = self.attrs.get(qa).clone();
                if let Some(t) = x.deleted {
                    match self.deletions.get(t).cloned() {
                        Some(d) if pick(d.rev) && depth < 64 => {
                            if how == Resolve::Reject {
                                let inner = self.resolve_pieces(&d.pieces, how, pick, depth + 1);
                                for (j, ip) in inner.into_iter().enumerate() {
                                    if j > 0 {
                                        out.push(DelPiece { para: ip.para.clone(), text: String::new(), runs: Vec::new() });
                                    }
                                    for (m, &(is, ia)) in ip.runs.iter().enumerate() {
                                        let ie = ip.runs.get(m + 1).map_or(ip.text.len() as u32, |r| r.0);
                                        if is < ie {
                                            push(&mut out, &ip.text[is as usize..ie as usize], ia);
                                        }
                                    }
                                }
                            }
                        }
                        _ => push(&mut out, text, qa),
                    }
                    continue;
                }
                if how == Resolve::Reject && x.rev.is_some_and(pick) {
                    continue;
                }
                let mut y = x.clone();
                if let Some(m) = x.fmt.filter(|m| pick(m.rev)) {
                    match how {
                        Resolve::Accept => y.fmt = None,
                        Resolve::Reject => {
                            let mut old = self.attrs.get(m.old).clone();
                            old.comments = x.comments.clone();
                            old.rev = x.rev;
                            y = old;
                        }
                    }
                }
                if how == Resolve::Accept && x.rev.is_some_and(pick) {
                    y.rev = None;
                }
                let ya = if y == x { qa } else { self.attrs.intern(y) };
                push(&mut out, text, ya);
            }
        }
        for p in out.iter_mut() {
            if p.runs.is_empty() {
                p.runs.push((0, 0));
            }
        }
        out
    }

    /// The proposed text with tracked marks taken off (what accepting everything
    /// shows), for comparisons: each paragraph's text without tombstones.
    pub fn visible_text(&self) -> Vec<String> {
        self.blocks
            .iter()
            .filter_map(|b| b.as_para())
            .map(|p| {
                let mut t = String::new();
                for (s, e, a) in p.spans() {
                    if s < e && self.attrs.get(a).deleted.is_none() {
                        t.push_str(&p.text[s as usize..e as usize]);
                    }
                }
                t
            })
            .collect()
    }
}

/// What an edit did, as people read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKind {
    Insert,
    Delete,
    /// Text taken out and other text put in its place.
    Replace,
    /// The same text taken out in one place and put in another.
    Move,
    /// Paragraph breaks put in (a new paragraph).
    Break,
    /// Formatting changed (one entry for all the places of one change).
    Format,
}

/// An edit: tracked changes grouped the way people think of them. Never
/// stored; derived from the marks each time.
#[derive(Debug, Clone, PartialEq)]
pub struct Edit {
    pub kind: EditKind,
    pub rev: RevId,
    /// Where it is: (block, byte) from and to. A move has the place it left
    /// first; a formatting edit has every place.
    pub ranges: Vec<((usize, u32), (usize, u32))>,
    /// The words around the change before and after (whole words).
    pub old: String,
    pub new: String,
    /// Only spaces or punctuation changed.
    pub minor: bool,
    /// "replaced “quick” with “fast”".
    pub summary: String,
}

/// A change set: one author's pass, its edits in document order.
#[derive(Debug, Clone, PartialEq)]
pub struct ChangeSet {
    pub set: u32,
    pub author: String,
    pub name: String,
    pub edits: Vec<Edit>,
}

/// Longest text an edit's summary quotes.
const QUOTE: usize = 40;

/// `old` and `new` without the whole words they share at either end ("two
/// percent" → "five percent" reads "two" → "five").
fn differing_words<'a>(old: &'a str, new: &'a str) -> (&'a str, &'a str) {
    let (ow, nw): (Vec<&str>, Vec<&str>) = (old.split(' ').collect(), new.split(' ').collect());
    let mut head = ow.iter().zip(&nw).take_while(|(a, b)| a == b).count();
    head = head.min(ow.len() - 1).min(nw.len() - 1);
    let mut tail = ow[head..].iter().rev().zip(nw[head..].iter().rev()).take_while(|(a, b)| a == b).count();
    tail = tail.min(ow.len() - head - 1).min(nw.len() - head - 1);
    if head == 0 && tail == 0 {
        return (old, new);
    }
    let span = |s: &'a str, w: &[&str]| {
        let start: usize = w[..head].iter().map(|x| x.len() + 1).sum();
        let end = s.len() - w[w.len() - tail..].iter().map(|x| x.len() + 1).sum::<usize>();
        &s[start..end]
    };
    (span(old, &ow), span(new, &nw))
}

fn quote(s: &str) -> String {
    let t: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() > QUOTE {
        format!("“{}…”", t.chars().take(QUOTE).collect::<String>())
    } else {
        format!("“{t}”")
    }
}

/// The formatting difference between two attribute sets, for a summary.
fn fmt_diff(old: &CharAttrs, new: &CharAttrs) -> String {
    let mut v: Vec<String> = Vec::new();
    let flag = |v: &mut Vec<String>, name: &str, a: Option<bool>, b: Option<bool>| {
        if a.unwrap_or(false) != b.unwrap_or(false) {
            v.push(if b.unwrap_or(false) { name.to_string() } else { format!("not {name}") });
        }
    };
    flag(&mut v, "bold", old.bold, new.bold);
    flag(&mut v, "italic", old.italic, new.italic);
    flag(&mut v, "underlined", old.underline, new.underline);
    flag(&mut v, "struck through", old.strike, new.strike);
    if old.size != new.size {
        v.push(match new.size {
            Some(s) => format!("{} pt", s as f32 / 2.0),
            None => "default size".into(),
        });
    }
    if old.color != new.color {
        v.push("colour".into());
    }
    if old.highlight != new.highlight {
        v.push(if new.highlight.is_some() { "highlighted".into() } else { "not highlighted".into() });
    }
    if old.family != new.family {
        v.push(new.family.as_deref().map_or("default font".to_string(), |f| f.to_string()));
    }
    if old.style != new.style {
        v.push("character style".into());
    }
    if old.vert != new.vert {
        v.push("raised or lowered".into());
    }
    if old.link != new.link {
        v.push("link".into());
    }
    if v.is_empty() {
        "formatting".into()
    } else {
        v.join(", ")
    }
}

impl TextDoc {
    /// Byte where the word around (`b`, `at`) starts, or ends, in paragraph `b`.
    fn word_edge(&self, b: usize, at: u32, forward: bool) -> u32 {
        let Some(p) = self.para(b) else { return at };
        let t = &p.text;
        let word = |c: char| c.is_alphanumeric() || c == '\'' || c == '’' || c == OBJECT_CHAR;
        let mut i = at as usize;
        if forward {
            while let Some(c) = t[i..].chars().next() {
                if !word(c) {
                    break;
                }
                i += c.len_utf8();
            }
        } else {
            while let Some(c) = t[..i].chars().next_back() {
                if !word(c) {
                    break;
                }
                i -= c.len_utf8();
            }
        }
        i as u32
    }

    /// The text of (`from`)..(`to`) as it was (`old`) and as it is (`new`):
    /// tombstones give their text to the old, insertions theirs to the new.
    fn old_new(&self, from: (usize, u32), to: (usize, u32), rev: RevId) -> (String, String) {
        let (mut old, mut new) = (String::new(), String::new());
        for b in from.0..=to.0 {
            let Some(p) = self.para(b) else { break };
            if b > from.0 {
                new.push('\n');
                if !p.props.rev.is_some_and(|r| r == rev) {
                    old.push('\n');
                }
            }
            let (s0, e0) = (if b == from.0 { from.1 } else { 0 }, if b == to.0 { to.1 } else { p.text.len() as u32 });
            for (s, e, a) in p.spans() {
                let (s, e) = (s.max(s0), e.min(e0));
                if s >= e {
                    continue;
                }
                let x = self.attrs.get(a);
                let t = &p.text[s as usize..e as usize];
                match (x.deleted, x.rev) {
                    (Some(d), _) => {
                        match self.deletions.get(d) {
                            Some(del) if del.rev == rev => old.push_str(&del.text()),
                            // Someone else's deletion: gone in both.
                            _ => {}
                        }
                    }
                    (None, Some(r)) if r == rev => new.push_str(t),
                    _ => {
                        old.push_str(t);
                        new.push_str(t);
                    }
                }
            }
        }
        (old, new)
    }

    /// The tracked changes grouped into edits, in document order (formatting
    /// edits after the text ones they share a place with).
    pub fn edits(&self) -> Vec<Edit> {
        let changes = self.changes();
        // Text changes of one revision that touch are one edit.
        let mut groups: Vec<(RevId, (usize, u32), (usize, u32), bool, bool, bool)> = Vec::new(); // rev, from, to, ins, del, break
        let mut formats: Vec<Change> = Vec::new();
        for c in changes {
            if c.kind == ChangeKind::Format {
                formats.push(c);
                continue;
            }
            let (ins, del, brk) = (c.kind == ChangeKind::Insert, matches!(c.kind, ChangeKind::Delete(_)), c.kind == ChangeKind::InsertBreak);
            let touches = |g: &(RevId, (usize, u32), (usize, u32), bool, bool, bool)| {
                g.0 == c.rev && (g.2 == c.from || (brk && c.from.0 == g.2 .0 + 1 && self.para(g.2 .0).is_some_and(|p| p.text.len() as u32 == g.2 .1)))
            };
            match groups.last_mut() {
                Some(g) if touches(g) => {
                    g.2 = c.to;
                    g.3 |= ins;
                    g.4 |= del;
                    g.5 |= brk;
                }
                _ => groups.push((c.rev, c.from, c.to, ins, del, brk)),
            }
        }
        let mut out: Vec<Edit> = Vec::new();
        for (rev, from, to, ins, del, brk) in groups {
            // Whole words around it, in its paragraphs.
            let wf = (from.0, self.word_edge(from.0, from.1, false));
            let wt = (to.0, self.word_edge(to.0, to.1, true));
            let (old, new) = self.old_new(wf, wt, rev);
            let kind = match (ins || brk, del) {
                (true, true) => EditKind::Replace,
                (false, true) => EditKind::Delete,
                (true, false) if !ins => EditKind::Break,
                _ => EditKind::Insert,
            };
            let changed = |s: &str| s.chars().filter(|c| c.is_alphanumeric()).collect::<String>();
            let minor = changed(&old) == changed(&new) && kind != EditKind::Break;
            let (o, n) = self.old_new(from, to, rev);
            let summary = match kind {
                EditKind::Insert => format!("inserted {}", quote(&n)),
                EditKind::Delete => format!("deleted {}", quote(&o)),
                EditKind::Replace if !old.contains('\n') && !new.contains('\n') => {
                    let (a, b) = differing_words(&old, &new);
                    format!("{} → {}", quote(a), quote(b))
                }
                EditKind::Replace => format!("replaced {} with {}", quote(&o), quote(&n)),
                EditKind::Break => "new paragraph".into(),
                _ => String::new(),
            };
            out.push(Edit { kind, rev, ranges: vec![(from, to)], old, new, minor, summary });
        }
        // Moves: a deletion and an insertion of the same text by one author.
        let author = |r: RevId| self.revisions.get(r).map(|v| v.author.clone());
        let mut k = 0;
        while k < out.len() {
            if out[k].kind == EditKind::Delete {
                let (o, _) = self.old_new(out[k].ranges[0].0, out[k].ranges[0].1, out[k].rev);
                let t = o.trim().to_string();
                if t.chars().count() >= 8 {
                    if let Some(j) = out.iter().position(|e| e.kind == EditKind::Insert && author(e.rev) == author(out[k].rev) && self.old_new(e.ranges[0].0, e.ranges[0].1, e.rev).1.trim() == t) {
                        let to = out[j].ranges[0];
                        out[k].kind = EditKind::Move;
                        out[k].ranges.push(to);
                        out[k].summary = format!("moved {}", quote(&t));
                        out[k].minor = false;
                        out.remove(j);
                        if j < k {
                            k -= 1;
                        }
                    }
                }
            }
            k += 1;
        }
        // Formatting: one edit per revision and change, wherever it is.
        let mut fmt: Vec<(RevId, String, Vec<((usize, u32), (usize, u32))>)> = Vec::new();
        for c in formats {
            let Some(p) = self.para(c.from.0) else { continue };
            let a = self.attrs.get(p.attr_at(c.from.1));
            let Some(m) = a.fmt else { continue };
            let what = fmt_diff(self.attrs.get(m.old), a);
            match fmt.iter_mut().find(|f| f.0 == c.rev && f.1 == what) {
                Some(f) => {
                    match f.2.last_mut() {
                        Some(l) if l.1 == c.from => l.1 = c.to,
                        _ => f.2.push((c.from, c.to)),
                    }
                }
                None => fmt.push((c.rev, what, vec![(c.from, c.to)])),
            }
        }
        for (rev, what, ranges) in fmt {
            let n = ranges.len();
            let summary = if n == 1 {
                let (_, t) = self.old_new(ranges[0].0, ranges[0].1, rev);
                format!("{what}: {}", quote(&t))
            } else {
                format!("{what} in {n} places")
            };
            out.push(Edit { kind: EditKind::Format, rev, ranges, old: String::new(), new: String::new(), minor: false, summary });
        }
        out.sort_by_key(|e| e.ranges[0].0);
        out
    }

    /// Edits by change set, sets in order.
    pub fn change_sets(&self) -> Vec<ChangeSet> {
        let mut sets: Vec<ChangeSet> = Vec::new();
        for e in self.edits() {
            let Some(r) = self.revisions.get(e.rev) else { continue };
            match sets.iter_mut().find(|s| s.set == r.set) {
                Some(s) => s.edits.push(e),
                None => sets.push(ChangeSet { set: r.set, author: r.author.to_string(), name: r.name.to_string(), edits: vec![e] }),
            }
        }
        sets.sort_by_key(|s| s.set);
        sets
    }

    /// Ops that accept or reject one edit (each of its places).
    pub fn resolve_edit_ops(&mut self, how: Resolve, e: &Edit) -> Vec<TextOp> {
        let mut ranges = e.ranges.clone();
        // Last place first, so earlier ones keep their positions.
        ranges.sort_by(|a, b| b.0.cmp(&a.0));
        let rev = e.rev;
        let mut ops = Vec::new();
        for (from, to) in ranges {
            ops.extend(self.resolve_range_ops(how, |r| r == rev, from, to));
        }
        ops
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::CharAttrs;
    use officina_core::history::{History, Op};

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n.max(1) as u64) as usize
        }
    }

    fn doc() -> TextDoc {
        let mut d = TextDoc::new();
        d.blocks.remove(0);
        let bold = d.attrs.intern(CharAttrs { bold: Some(true), ..Default::default() });
        d.push_para("The quick brown fox", 0, vec![(0, 0), (4, bold), (9, 0)]);
        d.push_para("jumps over", 0, vec![]);
        d.push_para("the lazy dög.", 1, vec![]);
        d.push_para("", 0, vec![]);
        d.push_para("Last one", 0, vec![]);
        d
    }

    /// Attributes as a reader sees them: tracking marks and comments off.
    fn plain(d: &TextDoc, a: AttrId) -> CharAttrs {
        let mut x = d.attrs.get(a).clone();
        x.rev = None;
        x.fmt = None;
        x.comments = None;
        x
    }

    /// Each paragraph's visible text as runs of (text, attributes), and its style
    /// and properties (tracking off).
    fn view(d: &TextDoc) -> Vec<(Vec<(String, CharAttrs)>, u16, ParaProps)> {
        d.blocks
            .iter()
            .filter_map(|b| b.as_para())
            .map(|p| {
                let mut v: Vec<(String, CharAttrs)> = Vec::new();
                for (s, e, a) in p.spans() {
                    if s == e || d.attrs.get(a).deleted.is_some() {
                        continue;
                    }
                    let x = plain(d, a);
                    match v.last_mut() {
                        Some(l) if l.1 == x => l.0.push_str(&p.text[s as usize..e as usize]),
                        _ => v.push((p.text[s as usize..e as usize].to_string(), x)),
                    }
                }
                let mut props = p.props.clone();
                props.rev = None;
                (v, p.style, props)
            })
            .collect()
    }

    /// Exactly: ids, texts, runs' attributes, styles, properties.
    fn exact(d: &TextDoc) -> Vec<(u64, String, Vec<(u32, CharAttrs)>, u16, ParaProps)> {
        d.blocks.iter().filter_map(|b| b.as_para()).map(|p| (p.id, p.text.clone(), p.spans().filter(|s| s.0 < s.1).map(|(s, _, a)| (s, d.attrs.get(a).clone())).collect(), p.style, p.props.clone())).collect()
    }

    /// The byte in paragraph `b` of `d` where visible byte `v` is: before any
    /// tombstones there.
    fn at(d: &TextDoc, b: usize, v: u32) -> u32 {
        let p = d.para(b).unwrap();
        let mut seen = 0;
        for (s, e, a) in p.spans() {
            if s == e || d.attrs.get(a).deleted.is_some() {
                continue;
            }
            if seen + (e - s) >= v && v >= seen {
                if v - seen < e - s || v == seen + (e - s) {
                    return s + (v - seen);
                }
            }
            seen += e - s;
        }
        p.text.len() as u32
    }

    fn vis_len(p: &(Vec<(String, CharAttrs)>, u16, ParaProps)) -> u32 {
        p.0.iter().map(|r| r.0.len() as u32).sum()
    }

    /// A char boundary of visible paragraph `p` at or before `v`.
    fn boundary_of(p: &(Vec<(String, CharAttrs)>, u16, ParaProps), v: u32) -> u32 {
        let t: String = p.0.iter().map(|r| r.0.as_str()).collect();
        let mut v = v.min(t.len() as u32);
        while !t.is_char_boundary(v as usize) {
            v -= 1;
        }
        v
    }

    /// What a reader of a format with inline deleted text rebuilds from
    /// [`TextDoc::expanded`]: the marked text and breaks made deletions again.
    fn collapse(ex: Expanded) -> TextDoc {
        let mut d = ex.doc;
        let mut ranges = Vec::new();
        for b in 0..d.blocks.len() {
            let p = d.para(b).unwrap().clone();
            if let Some(&r) = ex.deleted_breaks.get(&p.id) {
                let prev = d.para(b - 1).unwrap().text.len() as u32;
                ranges.push(((b - 1, prev), (b, 0), r));
            }
            for (s, e, a) in p.spans() {
                let x = d.attrs.get(a).clone();
                let Some(t) = x.deleted else { continue };
                if s == e {
                    continue;
                }
                let rev = d.deletions.get(t).unwrap().rev;
                ranges.push(((b, s), (b, e), rev));
                let mut y = x;
                y.deleted = None;
                let ya = d.attrs.intern(y);
                TextOp::Restyle { block: b as u32, start: s, end: e, runs: vec![(s, ya)] }.apply(&mut d);
            }
        }
        d.make_deletions(ranges);
        d
    }

    #[test]
    fn random_tracked_edits_accept_and_reject_exactly() {
        for seed in 1..=10_000u64 {
            let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let original = doc();
            let mut p = doc();
            let mut t = doc();
            let mut hp: History<TextOp> = History::new();
            let mut ht: History<TextOp> = History::new();
            let mut revs = Vec::new();
            for (i, who) in ["Ann", "Bo", "Ann"].iter().enumerate() {
                let (id, ops) = t.add_revision_ops(Revision { author: (*who).into(), date: "2026-11-15T00:00:00Z".into(), set: i as u32, name: "".into() });
                ht.run(&mut t, "rev", ops);
                revs.push(id);
            }
            ht = History::new();
            for step in 0..30 {
                let rev = revs[r.below(revs.len())];
                let vp = view(&p);
                let n = vp.len();
                match r.below(6) {
                    0 | 1 => {
                        let b = r.below(n);
                        let v = boundary_of(&vp[b], r.below(vis_len(&vp[b]) as usize + 1) as u32);
                        let bold = r.below(2) == 0;
                        let a = p.attrs.intern(CharAttrs { bold: bold.then_some(true), ..Default::default() });
                        let text = ["é", "xy", " ", "Zz"][r.below(4)];
                        hp.run(&mut p, "ins", vec![TextOp::Splice { block: b as u32, at: v, del: 0, ins: Fragment { text: text.into(), runs: vec![(0, a)] } }]);
                        let a = t.attrs.intern(CharAttrs { bold: bold.then_some(true), ..Default::default() });
                        let ta = t.with_rev(a, rev);
                        let tat = at(&t, b, v);
                        ht.run(&mut t, "ins", vec![TextOp::Splice { block: b as u32, at: tat, del: 0, ins: Fragment { text: text.into(), runs: vec![(0, ta)] } }]);
                    }
                    2 | 3 => {
                        let b0 = r.below(n);
                        let b1 = (b0 + r.below(3)).min(n - 1);
                        let v0 = boundary_of(&vp[b0], r.below(vis_len(&vp[b0]) as usize + 1) as u32);
                        let mut v1 = boundary_of(&vp[b1], r.below(vis_len(&vp[b1]) as usize + 1) as u32);
                        if b0 == b1 && v1 < v0 {
                            v1 = v0;
                        }
                        let ops = p.delete_ops(b0, v0, b1, v1);
                        hp.run(&mut p, "del", ops);
                        let (o0, o1) = (at(&t, b0, v0), at(&t, b1, v1));
                        let (ops, _) = t.tracked_delete_ops(b0, o0, b1, o1, rev, t.deletions.next_id());
                        ht.run(&mut t, "del", ops);
                    }
                    4 => {
                        let b = r.below(n);
                        let v = boundary_of(&vp[b], r.below(vis_len(&vp[b]) as usize + 1) as u32);
                        let (style, props) = { let q = p.para(b).unwrap(); (q.style, q.props.clone()) };
                        let id = p.new_id();
                        hp.run(&mut p, "split", vec![TextOp::Split { block: b as u32, at: v, id, style, props: props.clone() }]);
                        let id = t.new_id();
                        let mut tp = t.para(b).unwrap().props.clone();
                        tp.rev = Some(rev);
                        let tat = at(&t, b, v);
                        ht.run(&mut t, "split", vec![TextOp::Split { block: b as u32, at: tat, id, style, props: tp }]);
                    }
                    _ => {
                        let b = r.below(n);
                        let len = vis_len(&vp[b]);
                        let v0 = boundary_of(&vp[b], r.below(len as usize + 1) as u32);
                        let v1 = boundary_of(&vp[b], r.below(len as usize + 1) as u32).max(v0);
                        let on = r.below(2) == 0;
                        let ops = p.format_ops(b, v0, v1, |a| a.italic = on.then_some(true));
                        hp.run(&mut p, "fmt", ops);
                        let (o0, o1) = (at(&t, b, v0), at(&t, b, v1));
                        let ops = t.tracked_format_ops(b, o0, o1, rev, |a| a.italic = on.then_some(true));
                        ht.run(&mut t, "fmt", ops);
                    }
                }
                if r.below(8) == 0 {
                    hp.undo(&mut p);
                    ht.undo(&mut t);
                }
                assert_eq!(view(&t), view(&p), "seed {seed} step {step}: the proposed text is the edited text");
            }
            // Accept everything: the edited document, with no marks left.
            let ops = t.resolve_ops(Resolve::Accept, |_| true);
            ht.run(&mut t, "accept", ops);
            assert_eq!(view(&t), view(&p), "seed {seed}: accept all");
            assert!(t.changes().is_empty(), "seed {seed}: marks left after accepting: {:?}", t.changes());
            ht.undo(&mut t);
            // The file keeps it all.
            let back = crate::native::load(&crate::native::save_to_vec(&t, &[])).unwrap().doc;
            assert_eq!(exact(&back), exact(&t), "seed {seed}: native round trip");
            assert_eq!(back.tracking, t.tracking);
            // Deleted text inline (as .docx keeps it) and back: the same
            // proposed text, accepting and rejecting the same.
            let mut c = collapse(t.expanded());
            assert_eq!(view(&c), view(&p), "seed {seed}: expanded and collapsed");
            let mut cc = c.clone();
            let ops = cc.resolve_ops(Resolve::Reject, |_| true);
            for op in ops {
                op.apply(&mut cc);
            }
            assert_eq!(view(&cc), view(&original), "seed {seed}: collapsed, reject all");
            let ops = c.resolve_ops(Resolve::Accept, |_| true);
            for op in ops {
                op.apply(&mut c);
            }
            assert_eq!(view(&c), view(&p), "seed {seed}: collapsed, accept all");
            // Reject everything: the original exactly.
            let ops = t.resolve_ops(Resolve::Reject, |_| true);
            ht.run(&mut t, "reject", ops);
            assert_eq!(view(&t), view(&original), "seed {seed}: reject all");
            assert_eq!(exact(&t).iter().map(|x| (x.0, x.1.clone())).collect::<Vec<_>>(), exact(&original).iter().map(|x| (x.0, x.1.clone())).collect::<Vec<_>>(), "seed {seed}: reject all, exactly");
            assert!(t.live_deletions().is_empty());
        }
    }

    #[test]
    fn backspacing_leaves_one_tombstone_and_own_text_just_goes() {
        let mut d = doc();
        let (ann, ops) = d.add_revision_ops(Revision { author: "Ann".into(), ..Default::default() });
        let mut h: History<TextOp> = History::new();
        h.run(&mut d, "rev", ops);
        // Backspace over "fox" from its end, one character at a time.
        let mut caret = 19;
        for _ in 0..3 {
            let (ops, tomb) = d.tracked_delete_ops(0, caret - 1, 0, caret, ann, d.deletions.next_id());
            h.run(&mut d, "del", ops);
            caret = tomb.unwrap().1;
        }
        assert_eq!(d.visible_text()[0], "The quick brown ");
        assert_eq!(d.live_deletions().len(), 1, "one tombstone");
        let t = d.live_deletions()[0];
        assert_eq!(d.deletions.get(t).unwrap().text(), "fox");
        // Typing Ann's own text, then deleting part of it: no tombstone for it.
        let a = d.with_rev(0, ann);
        h.run(&mut d, "ins", vec![TextOp::Splice { block: 1, at: 0, del: 0, ins: Fragment { text: "Big ".into(), runs: vec![(0, a)] } }]);
        let (ops, tomb) = d.tracked_delete_ops(1, 0, 1, 2, ann, d.deletions.next_id());
        h.run(&mut d, "del", ops);
        assert!(tomb.is_none());
        assert_eq!(d.visible_text()[1], "g jumps over");
        // Bo deleting Ann's insertion leaves a tombstone holding it.
        let (bo, ops) = d.add_revision_ops(Revision { author: "Bo".into(), ..Default::default() });
        h.run(&mut d, "rev", ops);
        let (ops, tomb) = d.tracked_delete_ops(1, 0, 1, 2, bo, d.deletions.next_id());
        h.run(&mut d, "del", ops);
        assert!(tomb.is_some());
        // Formatting back to what it was is no change.
        let ops = d.tracked_format_ops(2, 0, 3, ann, |a| a.bold = Some(true));
        h.run(&mut d, "b", ops);
        assert!(d.changes().iter().any(|c| c.kind == ChangeKind::Format));
        let ops = d.tracked_format_ops(2, 0, 3, ann, |a| a.bold = None);
        h.run(&mut d, "b", ops);
        assert!(!d.changes().iter().any(|c| c.kind == ChangeKind::Format));
        // Rejecting only Bo's change puts Ann's "g" back as Ann's insertion.
        let ops = d.resolve_ops(Resolve::Reject, |r| r == bo);
        h.run(&mut d, "reject", ops);
        assert_eq!(d.visible_text()[1], "g jumps over");
        assert!(d.changes().iter().any(|c| c.kind == ChangeKind::Insert && c.rev == ann));
    }

    #[test]
    fn edits_read_as_people_think_of_them() {
        let mut d = doc();
        let (ann, ops) = d.add_revision_ops(Revision { author: "Ann".into(), set: 0, ..Default::default() });
        let mut h: History<TextOp> = History::new();
        h.run(&mut d, "rev", ops);
        // "quick" → "fast": a tombstone then an insertion.
        let (ops, t) = d.tracked_delete_ops(0, 4, 0, 9, ann, d.deletions.next_id());
        h.run(&mut d, "del", ops);
        let at = t.unwrap().1 + TOMB;
        let a = d.with_rev(0, ann);
        h.run(&mut d, "ins", vec![TextOp::Splice { block: 0, at, del: 0, ins: Fragment { text: "fast".into(), runs: vec![(0, a)] } }]);
        // One letter inside a word: the whole word, before and after.
        let p = d.para(1).unwrap().text.clone();
        let o = p.find("over").unwrap() as u32;
        let (ops, t) = d.tracked_delete_ops(1, o + 2, 1, o + 3, ann, d.deletions.next_id());
        h.run(&mut d, "del", ops);
        h.run(&mut d, "ins", vec![TextOp::Splice { block: 1, at: t.unwrap().1 + TOMB, del: 0, ins: Fragment { text: "a".into(), runs: vec![(0, a)] } }]);
        // A sentence moved from paragraph 4 to paragraph 2.
        let (ops, _) = d.tracked_delete_ops(4, 0, 4, 8, ann, d.deletions.next_id());
        h.run(&mut d, "del", ops);
        h.run(&mut d, "ins", vec![TextOp::Splice { block: 2, at: 0, del: 0, ins: Fragment { text: "Last one".into(), runs: vec![(0, a)] } }]);
        // Bold on two places: one edit.
        let ops = d.tracked_format_ops(2, 8, 12, ann, |x| x.bold = Some(true));
        h.run(&mut d, "b", ops);
        let ops = d.tracked_format_ops(1, 0, 3, ann, |x| x.bold = Some(true));
        h.run(&mut d, "b", ops);
        // A comma added: minor.
        let e = d.para(0).unwrap().text.find(" fox").unwrap() as u32;
        h.run(&mut d, "ins", vec![TextOp::Splice { block: 0, at: e, del: 0, ins: Fragment { text: ",".into(), runs: vec![(0, a)] } }]);
        let edits = d.edits();
        let sums: Vec<&str> = edits.iter().map(|e| e.summary.as_str()).collect();
        assert!(sums.contains(&"“quick” → “fast”"), "{sums:?}");
        assert_eq!(differing_words("two percent", "five percent"), ("two", "five"));
        assert_eq!(differing_words("in the box", "in a box"), ("the", "a"));
        assert_eq!(differing_words("a b", "a b c"), ("b", "b c"));
        assert!(sums.contains(&"“over” → “ovar”"), "{sums:?}");
        assert!(sums.contains(&"moved “Last one”"), "{sums:?}");
        assert!(sums.contains(&"bold in 2 places"), "{sums:?}");
        assert!(edits.iter().any(|e| e.minor && e.kind == EditKind::Insert), "{sums:?}");
        assert_eq!(edits.len(), 5, "{sums:?}");
        assert_eq!(d.change_sets().len(), 1);
        // Rejecting one edit leaves the others.
        let fast = edits.iter().find(|e| e.summary.contains("fast")).unwrap().clone();
        let ops = d.resolve_edit_ops(Resolve::Reject, &fast);
        h.run(&mut d, "reject", ops);
        assert!(d.visible_text()[0].starts_with("The quick"));
        assert_eq!(d.edits().len(), 4);
        // Accepting the move takes both places.
        let mv = d.edits().into_iter().find(|e| e.kind == EditKind::Move).unwrap();
        let ops = d.resolve_edit_ops(Resolve::Accept, &mv);
        h.run(&mut d, "accept", ops);
        assert_eq!(d.edits().len(), 3);
        assert_eq!(d.visible_text()[4], "");
    }
}
