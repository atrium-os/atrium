//! The editing core: a document, a caret and selection over it, motions,
//! typing, formatting and undo, and the geometry that maps positions to pages
//! and back. It holds no pixels; [`crate::view`] draws it.
//!
//! Positions are (block, byte) on a grapheme boundary. Every change goes
//! through `officina_core::history` as `TextOp`s; the ops note which blocks
//! they touched, and the editor hands that to the paginator, which repaginates
//! from the edit's page until breaks line up again (officina.md §7.4).

use std::rc::Rc;
use std::time::Duration;

use officina_core::history::History;
use officina_text::fields;
use officina_text::find::{count, Counts, Matcher, Query, WordCounter};
use officina_text::layout::{Fonts, LayoutCache, Line, ParaLayout};
use officina_text::lists::Numbering;
use officina_text::model::{Field, Align, AttrId, Block, CellRef, CharAttrs, CharFormat, Comment, CommentId, ImageData, NoteId, NoteKind, Para, MAX_CELLS, ListKind, ListRef, ObjectRef, ParaProps, StyleId, TextDoc, MAX_LIST_LEVEL, MAX_OBJECT_TWIPS, OBJECT_CHAR};
use officina_text::ops::{Fragment, TextOp};
use officina_text::model::{RevId, Revision};
use officina_text::track::{Change, ChangeKind, Edit, Resolve};
use officina_text::paginate::Paginator;
use unicode_segmentation::UnicodeSegmentation;

/// A place in the text: a block and a byte offset in its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub struct Pos {
    pub block: usize,
    pub at: u32,
}

impl Pos {
    pub const fn new(block: usize, at: u32) -> Pos {
        Pos { block, at }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    WordLeft,
    WordRight,
    LineStart,
    LineEnd,
    Up,
    Down,
    /// About a page's height up or down.
    PageUp,
    PageDown,
    DocStart,
    DocEnd,
}

/// What a key did, for the view to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The text changed.
    Edited,
    /// Only the caret or selection moved.
    Moved,
    /// Put this on the clipboard (for cut, the text is already gone).
    Copy(String),
    /// Ask the clipboard for text, then call [`Editor::insert`].
    Paste,
    /// Ctrl+F: the app opens its find bar.
    Find,
    Ignored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Typing,
    Deleting,
    Other,
}

/// Time spent repaginating after an edit before handing back (the rest runs
/// in later frames).
pub const SETTLE: Duration = Duration::from_millis(4);

/// Where a position is drawn: the page, and its x, top and height in layout
/// units from the page's content box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spot {
    pub page: usize,
    pub x: i64,
    pub y: i64,
    pub h: i64,
}

/// One action as the autosave journal keeps it ([`Editor::replay`] runs it
/// again): the ops as they were applied, whether they joined the action before
/// them, and the attribute table entries interned for them.
#[derive(Debug, Clone, PartialEq)]
pub enum Logged {
    Run { label: String, ops: Vec<TextOp>, merge: bool, attrs: Vec<CharAttrs>, images: Vec<(u32, ImageData)> },
    Undo,
    Redo,
}

/// A spell checker the app supplies: the byte ranges of `text` it finds
/// misspelled.
pub trait Checker {
    fn misspelled(&self, text: &str) -> Vec<(u32, u32)>;
}

pub struct Editor {
    pub doc: TextDoc,
    pub history: History<TextOp>,
    pub anchor: Pos,
    pub caret: Pos,
    /// Attributes for the next typed text, set by a formatting key with no
    /// selection (Ctrl+B, then type).
    pub typing: Option<CharAttrs>,
    goal_x: Option<i64>,
    pub pages: Paginator,
    pub cache: LayoutCache,
    fonts: Rc<dyn Fonts>,
    last: Kind,
    /// Bumped on every change to the text (for saving and redraws).
    pub revision: u64,
    /// Field resolution rounds since the last change.
    field_rounds: u32,
    /// Actions since [`Editor::take_log`], when journaling.
    log: Option<Vec<Logged>>,
    attrs_logged: usize,
    images_logged: usize,
    numbering: Option<(u64, Rc<Numbering>)>,
    /// Word count, kept current through edits.
    pub words: WordCounter,
    /// The spell checker, when spelling is checked.
    pub checker: Option<Rc<dyn Checker>>,
    /// Misspellings per paragraph, by the hash of its text.
    spelling: std::collections::HashMap<u64, (u64, Rc<Vec<(u32, u32)>>)>,
    /// Who tracked edits are made as, and when this session's first was.
    pub author: std::sync::Arc<str>,
    pub now: std::sync::Arc<str>,
    /// This session's revision, once a tracked edit made it.
    track_rev: Option<RevId>,
    /// Review mode: the edits are listed beside the page, the one selected is
    /// shown in place.
    pub review: bool,
    /// The selected edit, an index into [`Editor::edits`].
    pub review_at: usize,
    edits_cache: Option<(u64, Rc<Vec<Edit>>)>,
}

impl Editor {
    pub fn new(doc: TextDoc, fonts: Rc<dyn Fonts>) -> Editor {
        let pages = Paginator::new(&doc);
        let mut e = Editor { doc, history: History::new(), anchor: Pos::default(), caret: Pos::default(), typing: None, goal_x: None, pages, cache: LayoutCache::default(), fonts, last: Kind::Other, revision: 0, field_rounds: 0, log: None, attrs_logged: 0, images_logged: 0, numbering: None, words: WordCounter::default(), checker: None, spelling: std::collections::HashMap::new(), author: "Me".into(), now: "".into(), track_rev: None, review: false, review_at: 0, edits_cache: None };
        e.words = WordCounter::new(&e.doc);
        e.paginate(SETTLE);
        e
    }

    /// Starts keeping a log of actions for a journal.
    pub fn start_log(&mut self) {
        self.log = Some(Vec::new());
        self.attrs_logged = self.doc.attrs.len();
        self.images_logged = self.doc.images.len();
    }

    /// The actions since the last call.
    pub fn take_log(&mut self) -> Vec<Logged> {
        self.log.as_mut().map(std::mem::take).unwrap_or_default()
    }

    /// Runs a logged action again (recovering from the journal). Returns false
    /// for an undo or redo with nothing to undo or redo.
    pub fn replay(&mut self, l: Logged) -> bool {
        self.doc.take_changes();
        let ok = match l {
            Logged::Run { label, ops, merge, attrs, images } => {
                for (_, img) in images {
                    self.doc.images.add(img);
                }
                for a in attrs {
                    self.doc.attrs.intern(a);
                }
                if merge {
                    self.history.extend_last(&mut self.doc, &label, ops);
                } else {
                    self.history.run(&mut self.doc, &label, ops);
                }
                true
            }
            Logged::Undo => self.history.undo(&mut self.doc).is_some(),
            Logged::Redo => self.history.redo(&mut self.doc).is_some(),
        };
        self.attrs_logged = self.doc.attrs.len();
        self.images_logged = self.doc.images.len();
        self.changed();
        let c = self.clamp(self.caret);
        self.set_caret(c, false);
        ok
    }

    pub fn fonts_rc(&self) -> Rc<dyn Fonts> {
        self.fonts.clone()
    }

    pub fn fonts(&self) -> &dyn Fonts {
        &*self.fonts
    }

    /// Paginates for up to `budget`; returns whether pagination is done.
    /// When pages are done, fields are worked out again (§7.4); any that
    /// now show something else are laid out and paginated again, at most
    /// [`fields::MAX_ROUNDS`] rounds per change.
    pub fn paginate(&mut self, budget: Duration) -> bool {
        let t = std::time::Instant::now();
        loop {
            let left = budget.saturating_sub(t.elapsed());
            if !self.pages.run_for(&self.doc, &mut self.cache, &*self.fonts, left) {
                return false;
            }
            if self.field_rounds >= fields::MAX_ROUNDS {
                return true;
            }
            self.field_rounds += 1;
            let changed = fields::resolve(&mut self.doc, self.pages.pages());
            let (Some(&first), Some(&last)) = (changed.first(), changed.last()) else {
                self.field_rounds = fields::MAX_ROUNDS;
                return true;
            };
            self.revision += 1;
            self.pages.edit(&self.doc, &mut self.cache, &*self.fonts, officina_text::model::Edit { first, last: last + 1, delta: 0 });
            if t.elapsed() >= budget {
                return false;
            }
        }
    }

    pub fn layout(&mut self, block: usize) -> Rc<ParaLayout> {
        let p = self.doc.para(block).expect("block in range");
        let w = self.doc.block_box(block, self.pages.width()).1;
        self.cache.get(&self.doc, p, w, &*self.fonts)
    }

    fn text(&self, block: usize) -> &str {
        self.doc.para(block).map_or("", |p| p.text.as_str())
    }
    fn len(&self, block: usize) -> u32 {
        self.text(block).len() as u32
    }
    fn blocks(&self) -> usize {
        self.doc.blocks.len()
    }

    /// The selection in document order.
    pub fn selection(&self) -> (Pos, Pos) {
        if self.anchor <= self.caret {
            (self.anchor, self.caret)
        } else {
            (self.caret, self.anchor)
        }
    }
    pub fn has_selection(&self) -> bool {
        self.anchor != self.caret
    }

    /// Puts the caret at `p` (clamped to the document), extending the
    /// selection or not.
    pub fn set_caret(&mut self, p: Pos, extend: bool) {
        let p = self.clamp(p);
        self.caret = p;
        if !extend {
            self.anchor = p;
        }
        self.goal_x = None;
        self.typing = None;
        self.last = Kind::Other;
    }

    fn clamp(&self, p: Pos) -> Pos {
        let b = p.block.min(self.blocks().saturating_sub(1));
        let t = self.text(b);
        let mut at = (p.at as usize).min(t.len());
        while !t.is_char_boundary(at) {
            at -= 1;
        }
        Pos::new(b, at as u32)
    }

    pub fn select_all(&mut self) {
        self.anchor = Pos::default();
        let last = self.blocks() - 1;
        self.caret = Pos::new(last, self.len(last));
        self.last = Kind::Other;
    }

    /// Selects the word at `p`.
    pub fn select_word(&mut self, p: Pos) {
        let p = self.clamp(p);
        let t = self.text(p.block);
        let mut s = 0usize;
        let mut e = t.len();
        for (i, w) in t.split_word_bound_indices() {
            if i <= p.at as usize && (p.at as usize) < i + w.len() {
                s = i;
                e = i + w.len();
                break;
            }
        }
        self.anchor = Pos::new(p.block, s as u32);
        self.caret = Pos::new(p.block, e as u32);
        self.goal_x = None;
    }

    /// The selected text, paragraphs separated by newlines.
    pub fn selected_text(&self) -> String {
        let (a, b) = self.selection();
        let mut out = String::new();
        for k in a.block..=b.block {
            let t = self.text(k);
            let s = if k == a.block { a.at as usize } else { 0 };
            let e = if k == b.block { b.at as usize } else { t.len() };
            out.push_str(&t[s..e]);
            if k != b.block {
                out.push('\n');
            }
        }
        out
    }

    // ---- changes ----

    /// Applies `ops` as one action (or as part of the last, when typing or
    /// deleting continues), then repaginates what they changed.
    fn apply(&mut self, label: &str, ops: Vec<TextOp>, kind: Kind) {
        if ops.is_empty() {
            return;
        }
        self.doc.take_changes();
        let merge = kind != Kind::Other && kind == self.last;
        if let Some(log) = &mut self.log {
            let attrs = (self.attrs_logged..self.doc.attrs.len()).map(|i| self.doc.attrs.get(i as AttrId).clone()).collect();
            self.attrs_logged = self.doc.attrs.len();
            let images = (self.images_logged..self.doc.images.len()).filter_map(|i| self.doc.images.get(i as u32).map(|d| (i as u32, (**d).clone()))).collect();
            self.images_logged = self.doc.images.len();
            log.push(Logged::Run { label: label.to_string(), ops: ops.clone(), merge, attrs, images });
        }
        if merge {
            self.history.extend_last(&mut self.doc, label, ops);
        } else {
            self.history.run(&mut self.doc, label, ops);
        }
        self.last = kind;
        self.changed();
    }

    fn changed(&mut self) {
        self.revision += 1;
        if let Some(e) = self.doc.take_changes() {
            self.words.edit(&self.doc, e);
            self.field_rounds = 0;
            if !self.pages.fits(&self.doc) {
                // New page geometry: every line may break differently.
                self.pages.reset(&self.doc);
            } else {
                self.pages.edit(&self.doc, &mut self.cache, &*self.fonts, e);
            }
            self.paginate(SETTLE);
        }
        self.cache.prune(&self.doc);
    }

    /// Ops that delete the selection, and where the caret ends up.
    /// A selection across table cells clears each cell's part but never joins
    /// cells (or a cell and the text around the table).
    fn delete_selection_ops(&mut self) -> (Vec<TextOp>, Pos) {
        let (ops, p, _) = self.delete_selection_ops_tracked();
        (ops, p)
    }

    /// This session's revision while edits are tracked, with the ops that put
    /// it in the store the first time.
    fn track(&mut self) -> Option<(RevId, Vec<TextOp>)> {
        if !self.doc.tracking {
            return None;
        }
        if let Some(r) = self.track_rev.filter(|&r| self.doc.revisions.get(r).is_some_and(|v| v.author == self.author)) {
            return Some((r, Vec::new()));
        }
        let set = self.doc.revisions.iter().map(|(_, v)| v.set + 1).max().unwrap_or(0);
        let (id, ops) = self.doc.add_revision_ops(Revision { author: self.author.clone(), date: self.now.clone(), set, name: "".into() });
        self.track_rev = Some(id);
        Some((id, ops))
    }

    /// [`Editor::delete_selection_ops`], and whether a tombstone was left
    /// where the caret goes (tracked).
    fn delete_selection_ops_tracked(&mut self) -> (Vec<TextOp>, Pos, bool) {
        let (a, b) = self.selection();
        if a != b {
            if let Some((rev, mut ops)) = self.track() {
                let mut segs: Vec<(usize, usize)> = vec![(a.block, a.block)];
                for k in a.block + 1..=b.block {
                    if self.doc.same_cell(k - 1, k) {
                        segs.last_mut().expect("one").1 = k;
                    } else {
                        segs.push((k, k));
                    }
                }
                let mut id = self.doc.deletions.next_id();
                let mut at = a;
                let mut tomb = false;
                for &(s0, s1) in segs.iter().rev() {
                    let from = if s0 == a.block { a.at } else { 0 };
                    let to = if s1 == b.block { b.at } else { self.len(s1) };
                    if (s0, from) != (s1, to) {
                        let (o, t) = self.doc.tracked_delete_ops(s0, from, s1, to, rev, id);
                        if t.is_some() {
                            id += 1;
                        }
                        if s0 == a.block {
                            if let Some((tb, tat)) = t {
                                at = Pos::new(tb, tat);
                                tomb = true;
                            }
                        }
                        ops.extend(o);
                    }
                }
                return (ops, at, tomb);
            }
        }
        let (a, b) = self.selection();
        let mut segs: Vec<(usize, usize)> = vec![(a.block, a.block)];
        for k in a.block + 1..=b.block {
            if self.doc.same_cell(k - 1, k) {
                segs.last_mut().expect("one").1 = k;
            } else {
                segs.push((k, k));
            }
        }
        let mut ops = Vec::new();
        // Last first, so a join in one leaves the blocks before it in place.
        for &(s0, s1) in segs.iter().rev() {
            let from = if s0 == a.block { a.at } else { 0 };
            let to = if s1 == b.block { b.at } else { self.len(s1) };
            if (s0, from) != (s1, to) {
                ops.extend(self.doc.delete_ops(s0, from, s1, to));
            }
        }
        (ops, a, false)
    }

    /// The attributes typed text gets at `p`: those before it, with any
    /// pending formatting over them.
    /// Text typed next to a picture takes its formatting, not the picture.
    fn typing_attr(&mut self, p: Pos) -> Option<AttrId> {
        let para = self.doc.para(p.block)?;
        // Tombstones (tracked deletions) before the caret don't format typing.
        let mut k = p.at;
        while k >= 3 && para.text[..k as usize].ends_with(OBJECT_CHAR) && { let x = self.doc.attrs.get(para.attr_at(k - 3)); x.deleted.is_some() || x.conflict.is_some() } {
            k -= 3;
        }
        let before = para.attr_at(k.saturating_sub(1));
        let mut base = self.doc.attrs.get(before).clone();
        let mut pic = base.object.take().is_some() | base.note.take().is_some() | base.field.take().is_some() | base.deleted.take().is_some() | base.conflict.take().is_some() | (k != p.at);
        // Tracking marks are the typing's own, never inherited.
        pic |= base.rev.take().is_some() | base.fmt.take().is_some();
        // Typing inside commented text stays in the comment; at its end it doesn't.
        let after = para.attr_at(p.at);
        let after_comments = (p.at < para.text.len() as u32).then(|| self.doc.attrs.get(after).comments.clone()).flatten();
        let comment_change = base.comments != after_comments && base.comments.is_some();
        if comment_change {
            base.comments = base.comments.as_ref().and_then(|c| {
                let v: Vec<CommentId> = c.iter().copied().filter(|x| after_comments.as_ref().is_some_and(|a| a.contains(x))).collect();
                (!v.is_empty()).then(|| v.into())
            });
        }
        let t = match self.typing.clone() {
            Some(t) => t,
            None if pic || comment_change => CharAttrs::default(),
            None => return None,
        };
        let mut a = t.over(&base);
        a.object = None;
        a.note = None;
        a.deleted = None;
        a.conflict = None;
        a.rev = None;
        a.fmt = None;
        Some(self.doc.attrs.intern(a))
    }

    /// What text typed at `p` is formatted as, tracked: the typing attributes
    /// (or those before `p`) with this session's revision.
    fn tracked_attr(&mut self, p: Pos, rev: RevId) -> AttrId {
        let a = self.typing_attr(p).unwrap_or_else(|| self.doc.para(p.block).map_or(0, |q| q.attr_at(p.at.saturating_sub(1))));
        self.doc.with_rev(a, rev)
    }

    /// Puts a comment on the selection (or the word at the caret); its id.
    pub fn add_comment(&mut self, text: &str, author: &str, initials: &str, date: &str) -> Option<CommentId> {
        if !self.has_selection() {
            self.select_word(self.caret);
        }
        let (a, b) = self.selection();
        if a == b {
            return None;
        }
        let c = Comment { author: author.into(), initials: initials.into(), date: date.into(), text: text.to_string(), parent: None, done: false };
        let id = self.doc.comments.next_id();
        let ops = self.doc.add_comment_ops(a.block, a.at, b.block, b.at, c);
        if ops.is_empty() {
            return None;
        }
        self.apply("Comment", ops, Kind::Other);
        Some(id)
    }

    /// The comment at the caret: the innermost one over the character after it
    /// (or before it), as the first comment of its thread.
    pub fn comment_at_caret(&self) -> Option<CommentId> {
        let p = self.doc.para(self.caret.block)?;
        let at = self.caret.at;
        let pick = |b: u32| -> Option<CommentId> { self.doc.attrs.get(p.attr_at(b)).comments.as_ref().and_then(|c| c.iter().rev().copied().find(|&id| self.doc.comments.get(id).is_some())) };
        let id = if (at as usize) < p.text.len() { pick(at) } else { None }.or_else(|| if at > 0 { pick(at - 1) } else { None })?;
        Some(self.thread_root(id))
    }

    /// The first comment of the thread `id` is in.
    pub fn thread_root(&self, mut id: CommentId) -> CommentId {
        for _ in 0..64 {
            match self.doc.comments.get(id).and_then(|c| c.parent) {
                Some(p) if self.doc.comments.get(p).is_some() => id = p,
                _ => break,
            }
        }
        id
    }

    /// Comment `root` and its replies, in order.
    pub fn thread(&self, root: CommentId) -> Vec<CommentId> {
        let mut out = vec![root];
        let mut k = 0;
        while k < out.len() && out.len() < 4096 {
            for r in self.doc.comments.replies(out[k]) {
                if !out.contains(&r) {
                    out.push(r);
                }
            }
            k += 1;
        }
        out
    }

    /// Answers comment `to`; the reply's id.
    pub fn reply_comment(&mut self, to: CommentId, text: &str, author: &str, initials: &str, date: &str) -> Option<CommentId> {
        self.doc.comments.get(to)?;
        let id = self.doc.comments.next_id();
        let c = Comment { author: author.into(), initials: initials.into(), date: date.into(), text: text.to_string(), parent: Some(to), done: false };
        self.apply("Reply", vec![TextOp::SetComment { id, comment: Some(c) }], Kind::Other);
        Some(id)
    }

    /// Changes comment `id`'s text, or marks it resolved or open again.
    pub fn edit_comment(&mut self, id: CommentId, text: Option<&str>, done: Option<bool>) -> bool {
        let Some(mut c) = self.doc.comments.get(id).map(|c| (**c).clone()) else { return false };
        let before = c.clone();
        if let Some(t) = text {
            c.text = t.to_string();
        }
        if let Some(d) = done {
            c.done = d;
        }
        if c == before {
            return false;
        }
        let label = if done.is_some() { "Resolve Comment" } else { "Edit Comment" };
        self.apply(label, vec![TextOp::SetComment { id, comment: Some(c) }], Kind::Other);
        true
    }

    /// Takes out comment `id` and its replies (its text stays).
    pub fn delete_comment(&mut self, id: CommentId) -> bool {
        let ops = self.doc.delete_comment_ops(id);
        if ops.is_empty() {
            return false;
        }
        self.apply("Delete Comment", ops, Kind::Other);
        true
    }

    // ---- tracked changes ----

    /// Turns tracking on or off (an undoable change to the document).
    pub fn set_tracking(&mut self, on: bool) {
        if self.doc.tracking != on {
            self.apply(if on { "Track Changes" } else { "Stop Tracking" }, vec![TextOp::SetTracking { on }], Kind::Other);
        }
    }

    /// Accepts or rejects every change whose revision `pick` takes.
    pub fn resolve(&mut self, how: Resolve, pick: impl Fn(RevId) -> bool) -> bool {
        let ops = self.doc.resolve_ops(how, pick);
        if ops.is_empty() {
            return false;
        }
        self.apply(if how == Resolve::Accept { "Accept Changes" } else { "Reject Changes" }, ops, Kind::Other);
        self.clamp_caret();
        true
    }

    /// The change at the caret (or just before it).
    pub fn change_at_caret(&self) -> Option<Change> {
        let c = self.caret;
        self.doc.changes().into_iter().find(|ch| {
            let hit = |p: (usize, u32)| (ch.from.0, ch.from.1) <= p && p <= (ch.to.0, ch.to.1);
            hit((c.block, c.at)) || (ch.kind == ChangeKind::InsertBreak && ch.from.0 == c.block && c.at == 0)
        })
    }

    /// Accepts or rejects one change: its whole revision's text in that place
    /// (the run of it the caret is in).
    pub fn resolve_change(&mut self, how: Resolve, ch: &Change) -> bool {
        let rev = ch.rev;
        let ops = self.doc.resolve_range_ops(how, |r| r == rev, ch.from, ch.to);
        if ops.is_empty() {
            return false;
        }
        self.apply(if how == Resolve::Accept { "Accept Change" } else { "Reject Change" }, ops, Kind::Other);
        self.clamp_caret();
        true
    }

    /// Compares `original` with this document: the text stays, and what
    /// differs becomes tracked changes as `author` (one undoable step), shown
    /// in review from the first. The number of edits; none when the document
    /// has tracked changes of its own.
    pub fn compare_with(&mut self, original: &officina_text::model::TextDoc) -> Option<usize> {
        let now = if self.now.is_empty() { "".to_string() } else { self.now.to_string() };
        let author = self.author.clone();
        let ops = self.doc.compare_ops(original, &author, &now)?;
        self.apply("Compare", ops, Kind::Other);
        self.clamp_caret();
        let n = self.edits().len();
        self.review = n > 0;
        self.review_select(0);
        Some(n)
    }

    /// Merges `other`, another reviewed copy of this document's original
    /// (one undoable step): its changes come in under their own authors,
    /// places both changed differently become conflicts, and review opens on
    /// the first conflict, or the first edit. `name` stands for the other's
    /// author when its edits weren't tracked.
    pub fn merge_with(&mut self, other: &officina_text::model::TextDoc, name: &str) -> Result<officina_text::merge::Merge, officina_text::merge::MergeError> {
        let now = self.now.to_string();
        let mut mg = self.doc.merge_ops(other, name, &now)?;
        let ops = std::mem::take(&mut mg.ops);
        if !ops.is_empty() {
            self.apply("Merge", ops, Kind::Other);
            self.clamp_caret();
        }
        let e = self.edits();
        let first = e.iter().position(|e| matches!(e.kind, officina_text::track::EditKind::Conflict(_))).unwrap_or(0);
        self.review = !e.is_empty();
        self.review_select(first);
        Ok(mg)
    }

    /// Settles the selected conflict; the next edit is selected.
    pub fn review_take(&mut self, how: officina_text::merge::Take) -> bool {
        let Some(e) = self.review_edit() else { return false };
        let officina_text::track::EditKind::Conflict(id) = e.kind else { return false };
        let ops = self.doc.take_ops(id, how);
        if ops.is_empty() {
            return false;
        }
        let at = self.review_at;
        self.apply("Settle Conflict", ops, Kind::Other);
        self.clamp_caret();
        self.review_select(at);
        true
    }

    /// The document's edits (cached until the next change).
    pub fn edits(&mut self) -> Rc<Vec<Edit>> {
        if let Some((r, e)) = &self.edits_cache {
            if *r == self.revision {
                return e.clone();
            }
        }
        let e = Rc::new(self.doc.edits());
        self.edits_cache = Some((self.revision, e.clone()));
        e
    }

    /// The selected edit in review, if there are any.
    pub fn review_edit(&mut self) -> Option<Edit> {
        let e = self.edits();
        if e.is_empty() {
            return None;
        }
        self.review_at = self.review_at.min(e.len() - 1);
        Some(e[self.review_at].clone())
    }

    /// Selects edit `i` and puts the caret at its start.
    pub fn review_select(&mut self, i: usize) {
        let e = self.edits();
        if e.is_empty() {
            return;
        }
        self.review_at = i.min(e.len() - 1);
        let (b, at) = e[self.review_at].ranges[0].0;
        self.set_caret(Pos::new(b, at), false);
    }

    /// Accepts or rejects the selected edit; the next one is selected.
    pub fn review_resolve(&mut self, how: Resolve) -> bool {
        let Some(e) = self.review_edit() else { return false };
        let ops = self.doc.resolve_edit_ops(how, &e);
        if ops.is_empty() {
            return false;
        }
        let at = self.review_at;
        self.apply(if how == Resolve::Accept { "Accept Change" } else { "Reject Change" }, ops, Kind::Other);
        self.clamp_caret();
        self.review_select(at);
        true
    }

    /// Moves the caret to the next (or previous) change; whether there was one.
    pub fn next_change(&mut self, forward: bool) -> bool {
        let c = (self.caret.block, self.caret.at);
        let chs = self.doc.changes();
        let pick = if forward { chs.iter().find(|ch| ch.from > c) } else { chs.iter().rev().find(|ch| ch.from < c) };
        match pick {
            Some(ch) => {
                let (f, t) = (Pos::new(ch.from.0, ch.from.1), Pos::new(ch.to.0, ch.to.1));
                self.set_caret(f, false);
                self.set_caret(t, true);
                true
            }
            None => false,
        }
    }

    fn clamp_caret(&mut self) {
        let n = self.blocks();
        let fix = |p: Pos, e: &Self| -> Pos {
            let b = p.block.min(n.saturating_sub(1));
            let t = e.text(b);
            let mut at = (p.at as usize).min(t.len());
            while !t.is_char_boundary(at) {
                at -= 1;
            }
            Pos::new(b, at as u32)
        };
        self.caret = fix(self.caret, self);
        self.anchor = fix(self.anchor, self);
    }

    /// Puts a new footnote or endnote at the caret (over the selection) with
    /// `text` (a paragraph a line); the caret goes after its anchor.
    pub fn insert_note(&mut self, kind: NoteKind, text: &str) -> Option<NoteId> {
        let (mut ops, start) = self.delete_selection_ops();
        let base = self.typing_attr(start).unwrap_or_else(|| self.doc.para(start.block).map_or(0, |p| p.attr_at(start.at.saturating_sub(1))));
        let base = self.doc.plain_attr(base);
        let id = self.doc.notes.len() as NoteId;
        // The selection's deletion runs first; the anchor goes where it began.
        let note_ops = self.doc.insert_note_ops(start.block, 0, kind, text, base);
        if note_ops.len() != 2 {
            return None;
        }
        let mut it = note_ops.into_iter();
        ops.insert(0, it.next().expect("the note"));
        if let Some(TextOp::Splice { ins, .. }) = it.next() {
            ops.push(TextOp::Splice { block: start.block as u32, at: start.at, del: 0, ins });
        }
        self.apply(if kind == NoteKind::Foot { "Insert Footnote" } else { "Insert Endnote" }, ops, Kind::Other);
        let p = Pos::new(start.block, start.at + OBJECT_CHAR.len_utf8() as u32);
        self.caret = p;
        self.anchor = p;
        self.goal_x = None;
        Some(id)
    }

    /// Inserts a table of contents (headings 1 to 3) before the caret's
    /// paragraph, or brings the document's one up to date. False when there
    /// is nothing to do (no headings, or it is current: page numbers are
    /// fields and keep themselves current).
    pub fn insert_toc(&mut self) -> bool {
        let had = fields::toc_range(&self.doc, None);
        let at = self.caret.block;
        let ops = fields::toc_ops(&mut self.doc, at, 3);
        if ops.is_empty() {
            return false;
        }
        self.apply(if had.is_some() { "Update Table of Contents" } else { "Insert Table of Contents" }, ops, Kind::Other);
        let end = fields::toc_range(&self.doc, None).map_or(0, |r| r.1);
        match had {
            // An update: text past the contents keeps its place.
            Some((_, old_end)) if self.caret.block >= old_end && self.anchor.block >= old_end => {
                let d = end as isize - old_end as isize;
                self.caret.block = (self.caret.block as isize + d) as usize;
                self.anchor.block = (self.anchor.block as isize + d) as usize;
            }
            _ => {
                let p = Pos::new(end.min(self.blocks().saturating_sub(1)), 0);
                self.caret = p;
                self.anchor = p;
            }
        }
        self.clamp_caret();
        self.goal_x = None;
        true
    }

    /// The headings a cross-reference can point to: (id, level, text).
    pub fn ref_targets(&self) -> Vec<(u64, u8, String)> {
        fields::toc_entries(&self.doc, 9).into_iter().map(|e| (e.target, e.level, e.text)).collect()
    }

    /// Inserts a field at the caret (replacing the selection): a reference
    /// to paragraph `target`'s page or text.
    pub fn insert_field(&mut self, field: Field) -> bool {
        if self.doc.index_of(field.target()).is_none() {
            return false;
        }
        let (mut ops, start) = self.delete_selection_ops();
        let base = self.typing_attr(start).unwrap_or_else(|| self.doc.para(start.block).map_or(0, |p| p.attr_at(start.at.saturating_sub(1))));
        let base = self.doc.plain_attr(base);
        let mut a = self.doc.attrs.get(base).clone();
        a.field = Some(field);
        let fa = self.doc.attrs.intern(a);
        ops.push(TextOp::Splice { block: start.block as u32, at: start.at, del: 0, ins: Fragment { text: OBJECT_CHAR.to_string(), runs: vec![(0, fa)] } });
        self.apply("Insert Cross-reference", ops, Kind::Other);
        let p = Pos::new(start.block, start.at + OBJECT_CHAR.len_utf8() as u32);
        self.caret = p;
        self.anchor = p;
        self.goal_x = None;
        true
    }

    /// The note whose anchor is just before the caret (or just after it).
    pub fn note_at_caret(&self) -> Option<NoteId> {
        let p = self.doc.para(self.caret.block)?;
        let at = self.caret.at as usize;
        let before = p.text[..at.min(p.text.len())].char_indices().next_back().filter(|&(_, c)| c == OBJECT_CHAR).map(|(i, _)| i);
        let after = p.text[at.min(p.text.len())..].starts_with(OBJECT_CHAR).then_some(at);
        [before, after].into_iter().flatten().find_map(|i| self.doc.attrs.get(p.attr_at(i as u32)).note.filter(|id| self.doc.note_numbers().contains_key(id)))
    }

    /// Note `id`'s text, a paragraph a line.
    pub fn note_text(&self, id: NoteId) -> Option<String> {
        self.doc.notes.get(id).map(|n| n.paras.iter().map(|p| p.text.as_str()).collect::<Vec<_>>().join("\n"))
    }

    /// Sets note `id`'s text (one undoable action); false if unchanged.
    pub fn set_note_text(&mut self, id: NoteId, text: &str) -> bool {
        let ops = self.doc.set_note_text_ops(id, text);
        if ops.is_empty() {
            return false;
        }
        self.apply("Edit Note", ops, Kind::Other);
        true
    }

    /// Places a picture at the caret (over the selection), in line with the
    /// text: its frame is its pixel size at 96 dpi, shrunk to fit the column
    /// and the page.
    pub fn insert_image(&mut self, img: ImageData) -> bool {
        if img.width == 0 || img.height == 0 || img.bytes.is_empty() {
            return false;
        }
        let (px_w, px_h) = (img.width as i64, img.height as i64);
        let mut w = px_w * 15;
        let mut h = px_h * 15;
        let fit_w = (self.pages.width() * 20 / officina_text::LU).min(MAX_OBJECT_TWIPS as i64).max(20);
        let fit_h = (self.pages.content_height() * 20 / officina_text::LU * 9 / 10).min(MAX_OBJECT_TWIPS as i64).max(20);
        if w > fit_w {
            h = h * fit_w / w;
            w = fit_w;
        }
        if h > fit_h {
            w = w * fit_h / h;
            h = fit_h;
        }
        let image = self.doc.images.add(img);
        let (mut ops, start) = self.delete_selection_ops();
        let mut a = self.typing_attr(start).map(|a| self.doc.attrs.get(a).clone()).unwrap_or_else(|| {
            let before = self.doc.para(start.block).map_or(0, |p| p.attr_at(start.at.saturating_sub(1)));
            self.doc.attrs.get(before).clone()
        });
        a.object = Some(ObjectRef { image, width: w.max(1) as u32, height: h.max(1) as u32 });
        let attr = self.doc.attrs.intern(a);
        let ins = Fragment { text: OBJECT_CHAR.to_string(), runs: vec![(0, attr)] };
        ops.push(TextOp::Splice { block: start.block as u32, at: start.at, del: 0, ins });
        self.apply("Insert Picture", ops, Kind::Other);
        let p = Pos::new(start.block, start.at + OBJECT_CHAR.len_utf8() as u32);
        self.caret = p;
        self.anchor = p;
        self.goal_x = None;
        true
    }

    /// Types `s` over the selection. Newlines start paragraphs; other control
    /// characters are dropped (a tab and a line break, U+2028, stay).
    pub fn insert(&mut self, s: &str) -> bool {
        let s: String = s.replace("\r\n", "\n").replace('\r', "\n").chars().filter(|&c| (!c.is_control() || c == '\n' || c == '\t') && c != OBJECT_CHAR).collect();
        if s.is_empty() {
            return false;
        }
        let (mut ops, mut start, tomb) = self.delete_selection_ops_tracked();
        let had_sel = !ops.is_empty();
        // A replacement reads old, then new: typing goes after the tombstone.
        if tomb {
            start.at += OBJECT_CHAR.len_utf8() as u32;
        }
        let track = self.track();
        let attr = match &track {
            Some((rev, rops)) => {
                ops.extend(rops.iter().cloned());
                Some(self.tracked_attr(start, *rev))
            }
            None => self.typing_attr(start),
        };
        let rev = track.map(|t| t.0);
        let frag = |t: &str| Fragment { text: t.to_string(), runs: attr.map(|a| vec![(0, a)]).unwrap_or_default() };
        let mut p = start;
        let parts: Vec<&str> = s.split('\n').collect();
        for (i, part) in parts.iter().enumerate() {
            if !part.is_empty() {
                ops.push(TextOp::Splice { block: p.block as u32, at: p.at, del: 0, ins: frag(part) });
                p.at += part.len() as u32;
            }
            if i + 1 < parts.len() {
                let (style, mut props) = self.doc.para(start.block).map(|q| (q.style, q.props.clone())).unwrap_or_default();
                props.rev = rev;
                let id = self.doc.new_id();
                ops.push(TextOp::Split { block: p.block as u32, at: p.at, id, style, props });
                p = Pos::new(p.block + 1, 0);
            }
        }
        // One character typed at the caret joins the run of typing before it.
        let kind = if had_sel || parts.len() > 1 || s.chars().count() > 1 { Kind::Other } else { Kind::Typing };
        self.apply("Typing", ops, kind);
        self.caret = p;
        self.anchor = p;
        self.goal_x = None;
        true
    }

    /// Enter: a new paragraph. At the end of a heading it takes the style's
    /// "next" (body text); elsewhere it keeps the paragraph's style.
    pub fn new_paragraph(&mut self) {
        if !self.has_selection() && self.len(self.caret.block) == 0 && self.list_of(self.caret.block).is_some() {
            self.list_level(-((MAX_LIST_LEVEL as i32) + 1));
            return;
        }
        let (mut ops, mut start, tomb) = self.delete_selection_ops_tracked();
        if tomb {
            start.at += OBJECT_CHAR.len_utf8() as u32;
        }
        let track = self.track();
        let Some(p) = self.doc.para(start.block) else { return };
        let at_end = start.at as usize == p.text.len() && !self.has_selection();
        let style = if at_end { self.doc.styles.styles.get(p.style as usize).and_then(|s| s.next).unwrap_or(p.style) } else { p.style };
        let mut props = if at_end && style != p.style { ParaProps::default() } else { p.props.clone() };
        props.rev = None;
        if let Some((rev, rops)) = track {
            props.rev = Some(rev);
            ops.extend(rops);
        }
        let id = self.doc.new_id();
        ops.push(TextOp::Split { block: start.block as u32, at: start.at, id, style, props });
        self.apply("New Paragraph", ops, Kind::Other);
        self.set_caret(Pos::new(start.block + 1, 0), false);
    }

    /// Deletes the selection, or the grapheme (or word) before the caret.
    pub fn backspace(&mut self, word: bool) -> bool {
        if self.has_selection() {
            return self.delete_selection("Delete");
        }
        let c = self.caret;
        if c.at == 0 && self.list_of(c.block).is_some() {
            // At an item's start: out of the list first, text kept.
            return self.list_level(-((MAX_LIST_LEVEL as i32) + 1));
        }
        let to = if word { self.word_left(c) } else { self.grapheme_left(c) };
        if to == c || (to.block != c.block && !self.doc.same_cell(to.block, c.block)) {
            // Cells don't join each other or the text around the table.
            return false;
        }
        self.anchor = to;
        let (ops, p, _) = self.delete_selection_ops_tracked();
        self.apply("Delete", ops, Kind::Deleting);
        self.caret = p;
        self.anchor = p;
        self.goal_x = None;
        true
    }

    /// Deletes the selection, or the grapheme (or word) after the caret.
    pub fn delete_forward(&mut self, word: bool) -> bool {
        if self.has_selection() {
            return self.delete_selection("Delete");
        }
        let c = self.caret;
        let to = if word { self.word_right(c) } else { self.grapheme_right(c) };
        if to == c || (to.block != c.block && !self.doc.same_cell(to.block, c.block)) {
            return false;
        }
        self.anchor = to;
        let (ops, mut p, tomb) = self.delete_selection_ops_tracked();
        self.apply("Delete", ops, Kind::Deleting);
        // Deleting forward leaves the caret after what it took.
        if tomb {
            p.at += OBJECT_CHAR.len_utf8() as u32;
        }
        self.caret = p;
        self.anchor = p;
        true
    }

    fn delete_selection(&mut self, label: &str) -> bool {
        let (ops, p) = self.delete_selection_ops();
        if ops.is_empty() {
            return false;
        }
        self.apply(label, ops, Kind::Other);
        self.set_caret(p, false);
        true
    }

    /// Cuts the selection; returns its text.
    pub fn cut(&mut self) -> Option<String> {
        if !self.has_selection() {
            return None;
        }
        let t = self.selected_text();
        self.delete_selection("Cut");
        Some(t)
    }

    // ---- spelling ----

    /// The misspelled words of paragraph `block` (none without a checker).
    pub fn misspelled(&mut self, block: usize) -> Rc<Vec<(u32, u32)>> {
        let (Some(c), Some(p)) = (self.checker.clone(), self.doc.para(block)) else { return Rc::new(Vec::new()) };
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        p.text.hash(&mut h);
        let hash = h.finish();
        if let Some((k, v)) = self.spelling.get(&p.id) {
            if *k == hash {
                return v.clone();
            }
        }
        let v = Rc::new(c.misspelled(&p.text));
        if self.spelling.len() > 200_000 {
            self.spelling.clear();
        }
        self.spelling.insert(p.id, (hash, v.clone()));
        v
    }

    /// Forgets what the checker said (after a word is added or ignored).
    pub fn recheck(&mut self) {
        self.spelling.clear();
        self.revision += 1;
    }

    /// Selects the next misspelled word after the selection, wrapping round.
    /// False when there is none.
    pub fn next_misspelled(&mut self) -> bool {
        if self.checker.is_none() {
            return false;
        }
        let n = self.blocks();
        let from = self.selection().1;
        for step in 0..=n {
            let b = (from.block + step) % n;
            let bad = self.misspelled(b);
            let hit = bad.iter().find(|&&(s, e)| match step {
                0 => s >= from.at && !(s == from.at && e == from.at),
                k if k == n => s < from.at,
                _ => true,
            });
            if let Some(&(s, e)) = hit {
                self.set_caret(Pos::new(b, s), false);
                self.set_caret(Pos::new(b, e), true);
                return true;
            }
        }
        false
    }

    /// Replaces the selection with `with` as one action (a spelling fix).
    pub fn replace_selection(&mut self, with: &str) -> bool {
        let (a, b) = self.selection();
        if a.block != b.block || a == b {
            return false;
        }
        let attr = self.doc.para(a.block).map(|p| p.attr_at(a.at)).unwrap_or(0);
        let attr = self.doc.plain_attr(attr);
        let op = TextOp::Splice { block: a.block as u32, at: a.at, del: b.at - a.at, ins: Fragment { text: with.to_string(), runs: vec![(0, attr)] } };
        self.apply("Correct Spelling", vec![op], Kind::Other);
        self.set_caret(Pos::new(a.block, a.at + with.len() as u32), false);
        true
    }

    // ---- find and replace ----

    /// Selects the next match after the selection (or the one before it),
    /// wrapping round. False when there is none.
    pub fn find_next(&mut self, q: &Query, forward: bool) -> bool {
        let (a, b) = self.selection();
        let from = if forward { b } else { a };
        match self.doc.find(q, from.block, from.at, forward) {
            Some(m) => {
                self.set_caret(Pos::new(m.block, m.start), false);
                self.set_caret(Pos::new(m.block, m.end), true);
                true
            }
            None => false,
        }
    }

    /// Whether the selection is exactly a match.
    fn selection_matches(&self, q: &Query) -> bool {
        let (a, b) = self.selection();
        if a.block != b.block || a == b {
            return false;
        }
        let t = self.text(a.block);
        Matcher::new(q).all(t).iter().any(|&(s, e)| (s, e) == (a.at, b.at))
    }

    /// Replaces the selection when it is a match, then finds the next one.
    /// Returns whether it replaced.
    pub fn replace(&mut self, q: &Query, with: &str) -> bool {
        let did = self.selection_matches(q);
        if did {
            let (a, b) = self.selection();
            let attr = self.doc.para(a.block).map(|p| p.attr_at(a.at)).unwrap_or(0);
            let attr = self.doc.plain_attr(attr);
            let op = TextOp::Splice { block: a.block as u32, at: a.at, del: b.at - a.at, ins: Fragment { text: with.to_string(), runs: vec![(0, attr)] } };
            self.apply("Replace", vec![op], Kind::Other);
            let end = Pos::new(a.block, a.at + with.len() as u32);
            self.set_caret(end, false);
        }
        self.find_next(q, true);
        did
    }

    /// Replaces every match, as one undoable action. Returns how many.
    pub fn replace_all(&mut self, q: &Query, with: &str) -> usize {
        let ops = self.doc.replace_all_ops(q, with);
        let n = ops.len();
        self.apply("Replace All", ops, Kind::Other);
        let c = self.clamp(self.caret);
        self.set_caret(c, false);
        n
    }

    /// Word count over the selection (when there is one) or the document.
    pub fn counts(&self) -> Counts {
        if !self.has_selection() {
            return self.words.total();
        }
        let (a, b) = self.selection();
        if a.block == b.block {
            return count(&self.text(a.block)[a.at as usize..b.at as usize]);
        }
        let mut c = count(&self.text(a.block)[a.at as usize..]);
        c.add(self.words.over(a.block + 1..b.block));
        c.add(count(&self.text(b.block)[..b.at as usize]));
        c
    }

    pub fn undo(&mut self) -> bool {
        self.doc.take_changes();
        if self.history.undo(&mut self.doc).is_none() {
            return false;
        }
        if let Some(l) = &mut self.log {
            l.push(Logged::Undo);
        }
        self.after_history();
        true
    }

    pub fn redo(&mut self) -> bool {
        self.doc.take_changes();
        if self.history.redo(&mut self.doc).is_none() {
            return false;
        }
        if let Some(l) = &mut self.log {
            l.push(Logged::Redo);
        }
        self.after_history();
        true
    }

    fn after_history(&mut self) {
        // The caret goes to where the change was.
        let e = self.doc.take_changes();
        if let Some(e) = e {
            self.doc.note(e);
        }
        self.changed();
        let p = match e {
            Some(e) => {
                let b = e.last.saturating_sub(1).max(e.first).min(self.blocks() - 1);
                Pos::new(b, self.len(b))
            }
            None => self.caret,
        };
        self.set_caret(p, false);
    }

    // ---- formatting ----

    /// The character format at the caret (or the selection's start), with
    /// pending typing attributes.
    pub fn format_at_caret(&self) -> CharFormat {
        let (a, _) = self.selection();
        let Some(p) = self.doc.para(a.block) else { return CharFormat::default() };
        let (_, cf) = self.doc.styles.para_format(p.style, &p.props);
        let at = if self.has_selection() { a.at } else { a.at.saturating_sub(1) };
        let mut attrs = self.doc.attrs.get(p.attr_at(at)).clone();
        if let Some(t) = &self.typing {
            attrs = t.over(&attrs);
        }
        self.doc.styles.char_format(&cf, &attrs)
    }

    /// Changes the selection's characters through `f`, or, with no selection,
    /// what typing will give.
    pub fn format(&mut self, label: &str, f: impl Fn(&mut CharAttrs)) {
        if !self.has_selection() {
            let mut t = self.typing.take().unwrap_or_default();
            f(&mut t);
            self.typing = Some(t);
            self.last = Kind::Other;
            return;
        }
        let (a, b) = self.selection();
        let track = self.track();
        let mut ops = track.as_ref().map(|t| t.1.clone()).unwrap_or_default();
        for k in a.block..=b.block {
            let s = if k == a.block { a.at } else { 0 };
            let e = if k == b.block { b.at } else { self.len(k) };
            if s < e {
                match &track {
                    Some((rev, _)) => ops.extend(self.doc.tracked_format_ops(k, s, e, *rev, &f)),
                    None => ops.extend(self.doc.format_ops(k, s, e, &f)),
                }
            }
        }
        self.apply(label, ops, Kind::Other);
    }

    /// Bold, italic or underline on or off, by what the caret shows.
    pub fn toggle(&mut self, which: Toggle) {
        let cur = self.format_at_caret();
        match which {
            Toggle::Bold => {
                let v = !cur.bold;
                self.format("Bold", move |a| a.bold = Some(v))
            }
            Toggle::Italic => {
                let v = !cur.italic;
                self.format("Italic", move |a| a.italic = Some(v))
            }
            Toggle::Underline => {
                let v = !cur.underline;
                self.format("Underline", move |a| a.underline = Some(v))
            }
            Toggle::Strike => {
                let v = !cur.strike;
                self.format("Strikethrough", move |a| a.strike = Some(v))
            }
        }
    }

    /// Sets the selected paragraphs' style, or changes their direct
    /// properties through `f`.
    pub fn set_paragraphs(&mut self, label: &str, style: Option<StyleId>, f: impl Fn(&mut ParaProps)) {
        let (a, b) = self.selection();
        let mut ops = Vec::new();
        for k in a.block..=b.block {
            let Some(p) = self.doc.para(k) else { continue };
            let mut props = p.props.clone();
            let st = match style {
                Some(s) => {
                    // A new style clears direct paragraph formatting.
                    props = ParaProps::default();
                    s
                }
                None => p.style,
            };
            f(&mut props);
            if st != p.style || props != p.props {
                ops.push(TextOp::SetPara { block: k as u32, style: st, props });
            }
        }
        self.apply(label, ops, Kind::Other);
    }

    /// List numbering as the document stands (rebuilt after a change).
    pub fn numbering(&mut self) -> Rc<Numbering> {
        match &self.numbering {
            Some((r, n)) if *r == self.revision => n.clone(),
            _ => {
                let n = Rc::new(Numbering::build(&self.doc));
                self.numbering = Some((self.revision, n.clone()));
                n
            }
        }
    }

    /// The list block `b` is an item of.
    pub fn list_of(&self, b: usize) -> Option<ListRef> {
        let p = self.doc.para(b)?;
        self.doc.styles.para_format(p.style, &p.props).0.list
    }

    /// Makes the selected paragraphs items of a list of `kind`, or, when they
    /// all are already, plain paragraphs. A new list continues one of the same
    /// kind just above.
    pub fn toggle_list(&mut self, kind: ListKind) {
        let (a, b) = self.selection();
        let all = (a.block..=b.block).all(|k| self.list_of(k).is_some_and(|l| l.kind == kind));
        if all {
            self.set_paragraphs("List", None, |p| p.list = None);
            return;
        }
        let above = a.block.checked_sub(1).and_then(|k| self.list_of(k)).filter(|l| l.kind == kind);
        let id = match above {
            Some(l) => l.id,
            None => self.numbering().max_id.map_or(1, |m| m + 1),
        };
        self.set_paragraphs("List", None, move |p| {
            let level = p.list.flatten().map_or(0, |l| l.level);
            p.list = Some(Some(ListRef { id, kind, level }));
        });
    }

    /// Moves the selected list items `delta` levels deeper (negative: out);
    /// out of level 0 leaves the list. False when no paragraph is an item.
    pub fn list_level(&mut self, delta: i32) -> bool {
        let (a, b) = self.selection();
        if !(a.block..=b.block).any(|k| self.list_of(k).is_some()) {
            return false;
        }
        let items: Vec<Option<ListRef>> = (a.block..=b.block).map(|k| self.list_of(k)).collect();
        let first = a.block;
        let label = if delta > 0 { "Indent" } else { "Outdent" };
        let mut ops = Vec::new();
        for (i, l) in items.into_iter().enumerate() {
            let Some(l) = l else { continue };
            let k = first + i;
            let Some(p) = self.doc.para(k) else { continue };
            let mut props = p.props.clone();
            let lv = l.level as i32 + delta;
            props.list = if lv < 0 { if p.props.list.flatten().is_some() { None } else { Some(None) } } else { Some(Some(ListRef { level: lv.min(MAX_LIST_LEVEL as i32) as u8, ..l })) };
            if props != p.props {
                ops.push(TextOp::SetPara { block: k as u32, style: p.style, props });
            }
        }
        self.apply(label, ops, Kind::Other);
        true
    }

    /// Sets the page setup, header and footer (one undo step).
    pub fn set_page(&mut self, label: &str, page: officina_text::model::PageSetup, header: Option<officina_text::model::HeaderFooter>, footer: Option<officina_text::model::HeaderFooter>) {
        self.apply(label, vec![TextOp::SetPage { page, header, footer }], Kind::Other);
    }

    /// Page numbers in the footer ("Page 3 of 12", centred), on or off.
    pub fn toggle_page_numbers(&mut self) {
        let on = self.doc.footer.as_ref().is_some_and(|f| f.text.contains("{page}"));
        let footer = if on { None } else { Some(officina_text::model::HeaderFooter { text: "Page {page} of {pages}".into(), align: Align::Center }) };
        let (page, header) = (self.doc.page, self.doc.header.clone());
        self.set_page("Page Numbers", page, header, footer);
    }

    // ---- tables ----

    /// The blocks of the table holding block `b`.
    pub fn table_range(&self, b: usize) -> std::ops::Range<usize> {
        let Some(t) = self.doc.cell(b).map(|c| c.table) else { return b..b };
        let same = |k: usize| self.doc.cell(k).is_some_and(|c| c.table == t);
        let (mut a, mut e) = (b, b + 1);
        while a > 0 && same(a - 1) {
            a -= 1;
        }
        while same(e) {
            e += 1;
        }
        a..e
    }

    /// The rows of the table holding block `b`, in order.
    fn table_rows(&self, b: usize) -> Vec<std::ops::Range<usize>> {
        let t = self.table_range(b);
        let mut rows = Vec::new();
        let mut k = t.start;
        while k < t.end {
            let r = self.doc.row_range(k);
            let r = k..r.end.max(k + 1);
            k = r.end;
            rows.push(r);
        }
        rows
    }

    /// The cells of the row holding `b`, and which of them holds `b`.
    fn cells_of(&self, b: usize) -> (Vec<(std::ops::Range<usize>, i64, i64)>, usize) {
        let cells = self.doc.row_cells(self.doc.row_range(b), self.pages.width());
        let k = cells.iter().position(|c| c.0.contains(&b)).unwrap_or(0);
        (cells, k)
    }

    fn empty_cell(&mut self, like: usize, row: u32, col: u16, width: u32) -> Block {
        let (style, mut props) = self.doc.para(like).map(|p| (p.style, p.props.clone())).unwrap_or_default();
        let table = props.cell.map_or(0, |c| c.table);
        props.cell = Some(CellRef { table, row, col, width: width.max(1) });
        props.list = None;
        let id = self.doc.new_id();
        Block::Para(Para { id, text: String::new(), runs: vec![(0, 0)], style, props })
    }

    /// Puts a table of `rows` × `cols` empty cells after the caret's paragraph,
    /// as wide as the column; the caret goes to its first cell.
    pub fn insert_table(&mut self, rows: usize, cols: usize) -> bool {
        let b = self.caret.block;
        let (rows, cols) = (rows.clamp(1, 1000), cols.clamp(1, MAX_CELLS));
        if self.doc.cell(b).is_some() {
            return false;
        }
        let table = self.doc.blocks.iter().filter_map(|x| x.as_para().and_then(|p| p.props.cell)).map(|c| c.table).max().map_or(1, |t| t.wrapping_add(1).max(1));
        let width = ((self.pages.width() * 20 / officina_text::LU) as u32 / cols as u32).clamp(1, MAX_OBJECT_TWIPS);
        let mut blocks = Vec::with_capacity(rows * cols + 1);
        for r in 0..rows {
            for c in 0..cols {
                let id = self.doc.new_id();
                let props = ParaProps { cell: Some(CellRef { table, row: r as u32, col: c as u16, width }), space_after: Some(0), ..Default::default() };
                blocks.push(Block::Para(Para { id, text: String::new(), runs: vec![(0, 0)], style: 0, props }));
            }
        }
        if b + 1 >= self.blocks() || self.doc.cell(b + 1).is_some() {
            // Somewhere to type after the table.
            let id = self.doc.new_id();
            blocks.push(Block::Para(Para::new(id, "", 0)));
        }
        self.apply("Insert Table", vec![TextOp::Insert { at: (b + 1) as u32, blocks }], Kind::Other);
        self.set_caret(Pos::new(b + 1, 0), false);
        true
    }

    /// Adds a row of empty cells below the caret's row (cells as wide as its).
    pub fn insert_row_below(&mut self) -> bool {
        let b = self.caret.block;
        let Some(c) = self.doc.cell(b) else { return false };
        let row = self.doc.row_range(b);
        let fresh = self.table_rows(b).iter().filter_map(|r| self.doc.cell(r.start)).map(|x| x.row).max().unwrap_or(0).wrapping_add(1);
        let (cells, _) = self.cells_of(b);
        let mut blocks = Vec::new();
        for (r, _, _) in &cells {
            let cc = self.doc.cell(r.start).unwrap_or(c);
            blocks.push(self.empty_cell(r.start, fresh, cc.col, cc.width));
        }
        self.apply("Insert Row", vec![TextOp::Insert { at: row.end as u32, blocks }], Kind::Other);
        self.set_caret(Pos::new(row.end, 0), false);
        true
    }

    /// Adds a column right of the caret's: in every row the cell at the
    /// caret's place splits its width with a new empty cell after it.
    pub fn insert_column_right(&mut self) -> bool {
        let b = self.caret.block;
        if self.doc.cell(b).is_none() {
            return false;
        }
        let (_, k) = self.cells_of(b);
        let mut ops = Vec::new();
        let mut caret = None;
        let mut shift = 0;
        for row in self.table_rows(b).into_iter().rev() {
            let cells = self.doc.row_cells(row.clone(), self.pages.width());
            if cells.len() >= MAX_CELLS {
                continue;
            }
            let (cell, _, _) = cells[k.min(cells.len() - 1)].clone();
            let Some(cc) = self.doc.cell(cell.start) else { continue };
            let col = (row.clone()).filter_map(|x| self.doc.cell(x)).map(|x| x.col).max().unwrap_or(0).wrapping_add(1);
            let half = (cc.width / 2).max(1);
            for x in cell.clone() {
                if let Some(p) = self.doc.para(x) {
                    let mut props = p.props.clone();
                    props.cell = props.cell.map(|c| CellRef { width: half, ..c });
                    ops.push(TextOp::SetPara { block: x as u32, style: p.style, props });
                }
            }
            let blk = self.empty_cell(cell.start, cc.row, col, (cc.width - half).max(1));
            ops.push(TextOp::Insert { at: cell.end as u32, blocks: vec![blk] });
            if row.contains(&b) {
                caret = Some(cell.end);
            } else if caret.is_some() {
                // A row above: its new cell comes before the caret's.
                shift += 1;
            }
        }
        if ops.is_empty() {
            return false;
        }
        self.apply("Insert Column", ops, Kind::Other);
        if let Some(c) = caret {
            self.set_caret(Pos::new(c + shift, 0), false);
        }
        true
    }

    /// Removes the caret's row (the table goes when it was the last).
    pub fn delete_row(&mut self) -> bool {
        let b = self.caret.block;
        if self.doc.cell(b).is_none() {
            return false;
        }
        let row = self.doc.row_range(b);
        let mut ops = vec![TextOp::Remove { at: row.start as u32, n: row.len() as u32 }];
        if row.len() == self.blocks() {
            let id = self.doc.new_id();
            ops.push(TextOp::Insert { at: 0, blocks: vec![Block::Para(Para::new(id, "", 0))] });
        }
        self.apply("Delete Row", ops, Kind::Other);
        let at = row.start.min(self.blocks() - 1);
        self.set_caret(Pos::new(at, 0), false);
        true
    }

    /// Removes the cell at the caret's place from every row; its width goes to
    /// the cell before it (or after, for the first).
    pub fn delete_column(&mut self) -> bool {
        let b = self.caret.block;
        if self.doc.cell(b).is_none() {
            return false;
        }
        let (_, k) = self.cells_of(b);
        let start = self.table_range(b).start;
        let mut ops = Vec::new();
        let mut removed = 0;
        for row in self.table_rows(b).into_iter().rev() {
            let cells = self.doc.row_cells(row.clone(), self.pages.width());
            if cells.len() == 1 {
                ops.push(TextOp::Remove { at: row.start as u32, n: row.len() as u32 });
                removed += row.len();
                continue;
            }
            let i = k.min(cells.len() - 1);
            let gone = cells[i].0.clone();
            let w = self.doc.cell(gone.start).map_or(0, |c| c.width);
            let heir = cells[if i > 0 { i - 1 } else { 1 }].0.clone();
            for x in heir {
                if let Some(p) = self.doc.para(x) {
                    let mut props = p.props.clone();
                    props.cell = props.cell.map(|c| CellRef { width: (c.width + w).min(MAX_OBJECT_TWIPS), ..c });
                    ops.push(TextOp::SetPara { block: x as u32, style: p.style, props });
                }
            }
            ops.push(TextOp::Remove { at: gone.start as u32, n: gone.len() as u32 });
            removed += gone.len();
        }
        if removed == self.blocks() {
            let id = self.doc.new_id();
            ops.push(TextOp::Insert { at: 0, blocks: vec![Block::Para(Para::new(id, "", 0))] });
        }
        self.apply("Delete Column", ops, Kind::Other);
        let at = start.min(self.blocks() - 1);
        self.set_caret(Pos::new(at, 0), false);
        true
    }

    /// Tab in a table: the next cell (Shift: the one before); from the last
    /// cell, a new row.
    pub fn next_cell(&mut self, forward: bool) {
        let b = self.caret.block;
        let mut k = b;
        if forward {
            while k + 1 < self.blocks() && self.doc.same_cell(k, k + 1) {
                k += 1;
            }
            let t = self.table_range(b);
            if k + 1 < t.end {
                self.set_caret(Pos::new(k + 1, 0), false);
            } else {
                self.insert_row_below();
            }
        } else {
            while k > 0 && self.doc.same_cell(k - 1, k) {
                k -= 1;
            }
            if k > self.table_range(b).start {
                let mut s = k - 1;
                while s > 0 && self.doc.same_cell(s - 1, s) {
                    s -= 1;
                }
                self.set_caret(Pos::new(s, 0), false);
            }
        }
    }

    pub fn align(&mut self, a: Align) {
        self.set_paragraphs("Align", None, move |p| p.align = Some(a));
    }

    pub fn style_named(&mut self, name: &str) {
        if let Some(s) = self.doc.styles.by_name(name) {
            self.set_paragraphs("Style", Some(s), |_| {});
        }
    }

    // ---- motion ----

    fn grapheme_left(&self, p: Pos) -> Pos {
        if p.at == 0 {
            return if p.block == 0 { p } else { Pos::new(p.block - 1, self.len(p.block - 1)) };
        }
        let t = &self.text(p.block)[..p.at as usize];
        let g = t.grapheme_indices(true).next_back().map_or(0, |(i, _)| i);
        // Tombstones take no room: step over them to the character before.
        if self.tomb_at(Pos::new(p.block, g as u32)) {
            return self.grapheme_left(Pos::new(p.block, g as u32));
        }
        Pos::new(p.block, g as u32)
    }

    /// Whether a tombstone (a tracked deletion) or a conflict marker is at `p`.
    pub fn tomb_at(&self, p: Pos) -> bool {
        self.doc.para(p.block).is_some_and(|q| q.text[p.at.min(q.text.len() as u32) as usize..].starts_with(OBJECT_CHAR) && { let x = self.doc.attrs.get(q.attr_at(p.at)); x.deleted.is_some() || x.conflict.is_some() })
    }

    fn grapheme_right(&self, p: Pos) -> Pos {
        let t = self.text(p.block);
        if p.at as usize >= t.len() {
            return if p.block + 1 >= self.blocks() { p } else { Pos::new(p.block + 1, 0) };
        }
        let g = t[p.at as usize..].graphemes(true).next().map_or(0, str::len);
        if self.tomb_at(p) {
            return self.grapheme_right(Pos::new(p.block, p.at + g as u32));
        }
        Pos::new(p.block, p.at + g as u32)
    }

    fn word_left(&self, p: Pos) -> Pos {
        if p.at == 0 {
            return self.grapheme_left(p);
        }
        let t = &self.text(p.block)[..p.at as usize];
        let mut at = 0;
        for (i, w) in t.split_word_bound_indices().rev() {
            if w.chars().any(|c| !c.is_whitespace()) {
                at = i;
                break;
            }
        }
        Pos::new(p.block, at as u32)
    }

    fn word_right(&self, p: Pos) -> Pos {
        let t = self.text(p.block);
        if p.at as usize >= t.len() {
            return self.grapheme_right(p);
        }
        let mut at = t.len();
        let mut seen = false;
        for (i, w) in t[p.at as usize..].split_word_bound_indices() {
            let blank = w.chars().all(char::is_whitespace);
            if seen && !blank {
                at = p.at as usize + i;
                break;
            }
            if !blank {
                seen = true;
            }
        }
        Pos::new(p.block, at as u32)
    }

    /// The line holding the caret position `p` (a position at a soft break
    /// belongs to the next line).
    pub fn line_of(&mut self, p: Pos) -> (Rc<ParaLayout>, usize) {
        let l = self.layout(p.block);
        let li = l.line_of(p.at);
        (l, li)
    }

    /// x of byte `at` in line `line`, from the column's left.
    pub fn x_in_line(&self, block: usize, l: &ParaLayout, line: &Line, at: u32) -> i64 {
        let t = self.text(block).as_bytes();
        let mut x = line.x;
        for g in &l.glyphs[line.glyphs.clone()] {
            if g.cluster >= at {
                break;
            }
            x += g.adv;
            if line.space_extra != 0 && t.get(g.cluster as usize) == Some(&b' ') {
                x += line.space_extra;
            }
        }
        x
    }

    /// The position in line `li` of `block` nearest x.
    pub fn at_x(&mut self, block: usize, li: usize, x: i64) -> Pos {
        let l = self.layout(block);
        let Some(line) = l.lines.get(li) else { return Pos::new(block, 0) };
        let t = self.text(block);
        // The last place the caret can sit on this line: its end, but before a
        // break character or (on a wrapped line) before the space it hangs.
        let last_line = li + 1 == l.lines.len();
        let mut end = line.end;
        if !last_line && end > line.start {
            let prev = t[..end as usize].chars().next_back().map_or(0, char::len_utf8) as u32;
            end -= prev;
        } else if last_line && end > line.start && t[..end as usize].ends_with(['\u{2028}', '\n']) {
            end -= 3.min(t[..end as usize].chars().next_back().map_or(0, char::len_utf8) as u32);
        }
        let mut gx = line.x;
        let mut best = line.start;
        if x <= gx {
            return Pos::new(block, best);
        }
        let bytes = t.as_bytes();
        let gl = &l.glyphs[line.glyphs.clone()];
        let mut i = 0;
        while i < gl.len() {
            // A cluster's glyphs together (ligatures, marks).
            let c = gl[i].cluster;
            let mut w = 0;
            while i < gl.len() && gl[i].cluster == c {
                w += gl[i].adv;
                i += 1;
            }
            if line.space_extra != 0 && bytes.get(c as usize) == Some(&b' ') {
                w += line.space_extra;
            }
            if c >= end {
                break;
            }
            if x < gx + w / 2 {
                return Pos::new(block, c);
            }
            gx += w;
            best = if i < gl.len() { gl[i].cluster } else { end };
        }
        Pos::new(block, best.min(end).max(line.start))
    }

    /// Moves the caret, extending the selection or not. `page_h` is how far
    /// Page Up and Down go, in layout units.
    pub fn move_caret(&mut self, m: Motion, extend: bool, page_h: i64) {
        let collapse = !extend && self.has_selection();
        let (a, b) = self.selection();
        let c = self.caret;
        let keep_goal = matches!(m, Motion::Up | Motion::Down | Motion::PageUp | Motion::PageDown);
        let goal = if keep_goal { self.goal_x } else { None };
        let to = match m {
            Motion::Left if collapse => a,
            Motion::Right if collapse => b,
            Motion::Left => self.grapheme_left(c),
            Motion::Right => self.grapheme_right(c),
            Motion::WordLeft => self.word_left(c),
            Motion::WordRight => self.word_right(c),
            Motion::LineStart | Motion::LineEnd => {
                let (l, li) = self.line_of(c);
                let x = if m == Motion::LineStart { i64::MIN } else { i64::MAX };
                let _ = l;
                self.at_x(c.block, li, x)
            }
            Motion::DocStart => Pos::default(),
            Motion::DocEnd => {
                let k = self.blocks() - 1;
                Pos::new(k, self.len(k))
            }
            Motion::Up | Motion::Down | Motion::PageUp | Motion::PageDown => {
                let (l, li) = self.line_of(c);
                let x = goal.unwrap_or_else(|| self.x_in_line(c.block, &l, &l.lines[li], c.at));
                let down = matches!(m, Motion::Down | Motion::PageDown);
                let mut left = if matches!(m, Motion::PageUp | Motion::PageDown) { page_h.max(1) } else { 1 };
                let (mut blk, mut ln) = (c.block, li);
                loop {
                    let next = self.step_line(blk, ln, down);
                    let Some((nb, nl)) = next else { break };
                    blk = nb;
                    ln = nl;
                    left -= self.layout(blk).lines[ln].height.max(1);
                    if left <= 0 {
                        break;
                    }
                }
                if (blk, ln) == (c.block, li) {
                    // First or last line: to the document's edge.
                    if down {
                        Pos::new(blk, self.len(blk))
                    } else {
                        Pos::new(blk, 0)
                    }
                } else {
                    let p = self.at_x(blk, ln, x);
                    self.caret = p;
                    if !extend {
                        self.anchor = p;
                    }
                    self.goal_x = Some(x);
                    self.typing = None;
                    self.last = Kind::Other;
                    return;
                }
            }
        };
        self.set_caret(to, extend);
    }

    /// The line above or below line `ln` of `blk`, moving through a table by
    /// cells: down from a cell's last line goes to the cell below.
    fn step_line(&mut self, blk: usize, ln: usize, down: bool) -> Option<(usize, usize)> {
        let lines = |e: &mut Editor, b: usize| e.layout(b).lines.len();
        let last_line = |e: &mut Editor, b: usize| (b, lines(e, b).saturating_sub(1));
        let n = self.blocks();
        if down && ln + 1 < lines(self, blk) {
            return Some((blk, ln + 1));
        }
        if !down && ln > 0 {
            return Some((blk, ln - 1));
        }
        let near = if down { blk + 1 } else { blk.wrapping_sub(1) };
        if near < n && self.doc.cell(blk).is_some() && self.doc.same_cell(blk, near) {
            return Some(if down { (near, 0) } else { last_line(self, near) });
        }
        let Some(c) = self.doc.cell(blk) else {
            if near >= n {
                return None;
            }
            if !down && self.doc.cell(near).is_some() {
                // Into a table from below: its last row's first cell.
                let cells = self.doc.row_cells(self.doc.row_range(near), self.pages.width());
                return Some(last_line(self, cells[0].0.end - 1));
            }
            return Some(if down { (near, 0) } else { last_line(self, near) });
        };
        let row = self.doc.row_range(blk);
        let cells = self.doc.row_cells(row.clone(), self.pages.width());
        let k = cells.iter().position(|x| x.0.contains(&blk)).unwrap_or(0);
        let beyond = if down { row.end } else { row.start.wrapping_sub(1) };
        if beyond >= n {
            return None;
        }
        if self.doc.cell(beyond).is_some_and(|d| d.table == c.table) {
            let other = self.doc.row_cells(self.doc.row_range(beyond), self.pages.width());
            let cell = other[k.min(other.len() - 1)].0.clone();
            return Some(if down { (cell.start, 0) } else { last_line(self, cell.end - 1) });
        }
        Some(if down { (beyond, 0) } else { last_line(self, beyond) })
    }

    // ---- geometry ----

    /// Paginates (synchronously) until the page holding `p` is laid out.
    pub fn ensure_paginated(&mut self, p: Pos, li: usize) {
        while !self.pages.done() {
            let last = self.pages.pages().last().map(|pg| pg.start);
            if last.is_some_and(|s| (s.block, s.line) > (p.block, li)) {
                break;
            }
            self.pages.run(&self.doc, &mut self.cache, &*self.fonts, 4);
        }
    }

    /// Where position `p` is drawn.
    pub fn spot(&mut self, p: Pos) -> Spot {
        let p = self.clamp(p);
        let (l, li) = self.line_of(p);
        self.ensure_paginated(p, li);
        let page = self.pages.page_of(p.block, li);
        let mut y = 0;
        let mut x0 = self.doc.block_box(p.block, self.pages.width()).0;
        if let Some(pg) = self.pages.pages().get(page) {
            if let Some(s) = pg.slices.iter().find(|s| s.block == p.block && s.lines.contains(&li)) {
                y = s.y + l.lines[s.lines.start..li].iter().map(|x| x.height).sum::<i64>();
                x0 = s.x;
            }
        }
        let line = &l.lines[li];
        Spot { page, x: x0 + self.x_in_line(p.block, &l, line, p.at), y, h: line.height }
    }

    /// The position nearest (x, y) on page `page` (layout units from its
    /// content box).
    pub fn hit(&mut self, page: usize, x: i64, y: i64) -> Pos {
        let Some(pg) = self.pages.pages().get(page).cloned() else {
            let k = self.blocks() - 1;
            return Pos::new(k, self.len(k));
        };
        // The slice and line at y (above the first: its first line; below
        // the last: the last line).
        let mut target = None;
        let pad = officina_text::twips(officina_text::model::CELL_PAD as i64);
        'outer: for s in &pg.slices {
            if self.doc.cell(s.block).is_some() && (x < s.x - pad || x >= s.x + s.w + pad) {
                // Another column of the table.
                continue;
            }
            let l = self.layout(s.block);
            let mut yy = s.y;
            for li in s.lines.clone() {
                let h = l.lines[li].height;
                if target.is_none() || y >= yy {
                    target = Some((s.block, li, s.x));
                }
                if y < yy + h {
                    break 'outer;
                }
                yy += h;
            }
        }
        let Some((b, li, sx)) = target else { return Pos::default() };
        self.at_x(b, li, x - sx)
    }

    // ---- keys ----

    /// Handles a key. `page_h` is how far Page Up/Down move (layout units).
    pub fn key(&mut self, key: pergola::event::Key, m: pergola::event::Modifiers, chars: &str, page_h: i64) -> Outcome {
        use pergola::event::Key;
        let cmd = m.ctrl || m.meta;
        let moved = |e: &mut Editor, mo: Motion| {
            e.move_caret(mo, m.shift, page_h);
            Outcome::Moved
        };
        let edited = |b: bool| if b { Outcome::Edited } else { Outcome::Ignored };
        match key {
            Key::Char if cmd => match chars.to_ascii_lowercase().as_str() {
                "a" => {
                    self.select_all();
                    Outcome::Moved
                }
                "c" if self.has_selection() => Outcome::Copy(self.selected_text()),
                "x" => self.cut().map_or(Outcome::Ignored, Outcome::Copy),
                "v" => Outcome::Paste,
                "f" | "h" => Outcome::Find,
                "z" if m.shift => edited(self.redo()),
                "z" => edited(self.undo()),
                "y" => edited(self.redo()),
                "b" => {
                    self.toggle(Toggle::Bold);
                    Outcome::Edited
                }
                "i" => {
                    self.toggle(Toggle::Italic);
                    Outcome::Edited
                }
                "u" => {
                    self.toggle(Toggle::Underline);
                    Outcome::Edited
                }
                "l" => {
                    self.align(Align::Start);
                    Outcome::Edited
                }
                "e" => {
                    self.align(Align::Center);
                    Outcome::Edited
                }
                "r" => {
                    self.align(Align::End);
                    Outcome::Edited
                }
                "j" => {
                    self.align(Align::Justify);
                    Outcome::Edited
                }
                "7" | "&" if m.shift => {
                    self.toggle_list(ListKind::Numbered);
                    Outcome::Edited
                }
                "8" | "*" if m.shift => {
                    self.toggle_list(ListKind::Bullet);
                    Outcome::Edited
                }
                "0" => {
                    self.style_named("Normal");
                    Outcome::Edited
                }
                d @ ("1" | "2" | "3") => {
                    self.style_named(&format!("Heading {d}"));
                    Outcome::Edited
                }
                _ => Outcome::Ignored,
            },
            Key::Char => edited(self.insert(chars)),
            // Tab in a list item (at its start, or over several paragraphs)
            // changes its level; elsewhere it types a tab.
            Key::Tab if !cmd && self.doc.cell(self.caret.block).is_some() => {
                self.next_cell(!m.shift);
                Outcome::Moved
            }
            Key::Tab if !cmd && (self.caret.at == 0 || self.selection().0.block != self.selection().1.block) && self.list_of(self.caret.block).is_some() => edited(self.list_level(if m.shift { -1 } else { 1 })),
            Key::Tab if !cmd && m.shift => Outcome::Ignored,
            Key::Tab if !cmd => edited(self.insert("\t")),
            Key::Enter if m.shift => edited(self.insert("\u{2028}")),
            Key::Enter => {
                self.new_paragraph();
                Outcome::Edited
            }
            Key::Backspace => edited(self.backspace(cmd || m.alt)),
            Key::Delete => edited(self.delete_forward(cmd || m.alt)),
            Key::Left => moved(self, if cmd || m.alt { Motion::WordLeft } else { Motion::Left }),
            Key::Right => moved(self, if cmd || m.alt { Motion::WordRight } else { Motion::Right }),
            Key::Up => moved(self, Motion::Up),
            Key::Down => moved(self, Motion::Down),
            Key::Home => moved(self, if cmd { Motion::DocStart } else { Motion::LineStart }),
            Key::End => moved(self, if cmd { Motion::DocEnd } else { Motion::LineEnd }),
            Key::PageUp => moved(self, Motion::PageUp),
            Key::PageDown => moved(self, Motion::PageDown),
            _ => Outcome::Ignored,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toggle {
    Bold,
    Italic,
    Underline,
    Strike,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use officina_text::layout::FixedFonts;
    use pergola::event::{Key, Modifiers};

    fn ed(paras: &[&str]) -> Editor {
        let mut d = TextDoc::new();
        d.blocks.remove(0);
        for p in paras {
            d.push_para(p, 0, vec![]);
        }
        Editor::new(d, Rc::new(FixedFonts))
    }
    fn texts(e: &Editor) -> Vec<String> {
        e.doc.blocks.iter().map(|b| b.as_para().unwrap().text.clone()).collect()
    }
    const NO: Modifiers = Modifiers { shift: false, ctrl: false, alt: false, meta: false };
    const SHIFT: Modifiers = Modifiers { shift: true, ctrl: false, alt: false, meta: false };
    const CTRL: Modifiers = Modifiers { shift: false, ctrl: true, alt: false, meta: false };
    const CTRL_SHIFT: Modifiers = Modifiers { shift: true, ctrl: true, alt: false, meta: false };

    fn typ(e: &mut Editor, s: &str) {
        for c in s.chars() {
            e.key(Key::Char, NO, &c.to_string(), 0);
        }
    }

    struct Teh;
    impl Checker for Teh {
        fn misspelled(&self, text: &str) -> Vec<(u32, u32)> {
            text.match_indices("teh").map(|(i, m)| (i as u32, (i + m.len()) as u32)).collect()
        }
    }

    #[test]
    fn toc_and_cross_references_keep_current() {
        let mut d = TextDoc::new();
        d.blocks.remove(0);
        let h1 = d.styles.by_name("Heading 1").unwrap();
        d.push_para("Intro text.", 0, vec![]);
        for c in 1..=3 {
            d.push_para(&format!("Chapter {c}"), h1, vec![]);
            for _ in 0..30 {
                d.push_para(&"filler words go here ".repeat(8), 0, vec![]);
            }
        }
        let mut e = Editor::new(d, Rc::new(FixedFonts));
        e.start_log();
        assert!(e.paginate(Duration::from_secs(10)));
        e.set_caret(Pos::new(0, 0), false);
        assert!(e.insert_toc());
        assert!(e.paginate(Duration::from_secs(10)));
        let shown = |e: &Editor| -> Vec<String> { (0..3).map(|i| { let p = e.doc.para(i).unwrap(); e.doc.field_text(e.doc.fields_in(p)[0].1) }).collect() };
        let page = |e: &Editor, c: usize| (e.pages.page_of(e.doc.index_of(e.ref_targets()[c].0).unwrap(), 0) + 1).to_string();
        assert_eq!(shown(&e), (0..3).map(|c| page(&e, c)).collect::<Vec<_>>());
        assert_eq!(e.caret, Pos::new(3, 0), "after the contents");
        assert!(!e.insert_toc(), "current");
        // A cross-reference to chapter 3's page.
        let target = e.ref_targets()[2].0;
        e.set_caret(Pos::new(3, 6), false);
        assert!(e.insert_field(Field::PageRef(target)));
        // Pages shift: fields follow without an update.
        let before = shown(&e);
        e.set_caret(Pos::new(3, 0), false);
        for _ in 0..80 {
            e.key(Key::Enter, NO, "", 0);
        }
        assert!(e.paginate(Duration::from_secs(10)));
        let after = shown(&e);
        assert_ne!(before, after);
        assert_eq!(after, (0..3).map(|c| page(&e, c)).collect::<Vec<_>>());
        let p = e.doc.para(83).unwrap();
        assert_eq!(e.doc.field_text(e.doc.fields_in(p)[0].1), page(&e, 2));
        // A retitled heading: Update changes that entry.
        let ch2 = e.doc.index_of(e.ref_targets()[1].0).unwrap();
        e.set_caret(Pos::new(ch2, 0), false);
        typ(&mut e, "New ");
        assert!(e.insert_toc());
        assert!(e.doc.para(1).unwrap().text.starts_with("New Chapter 2\t"));
        e.undo();
        assert!(e.doc.para(1).unwrap().text.starts_with("Chapter 2\t"));
    }

    #[test]
    fn compare_marks_what_changed_and_undoes() {
        let original = ed(&["The quick brown fox", "jumps over", "the dog"]).doc;
        let mut e = ed(&["The slow brown fox", "the dog", "jumps over"]);
        e.start_log();
        e.author = "Cy".into();
        let n = e.compare_with(&original).unwrap();
        assert!(n >= 2, "{n}");
        assert!(e.review);
        assert_eq!(texts(&e).len(), 3);
        assert_eq!(e.doc.visible_text(), vec!["The slow brown fox", "the dog", "jumps over"]);
        assert!(e.edits().iter().any(|x| x.summary == "“quick” → “slow”"), "{:?}", e.edits());
        // The journal replays it.
        let log = e.take_log();
        let mut r = ed(&["The slow brown fox", "the dog", "jumps over"]);
        for l in log {
            r.replay(l);
        }
        assert_eq!(r.edits().len(), e.edits().len());
        e.undo();
        assert!(e.edits().is_empty());
    }

    #[test]
    fn merge_brings_in_the_other_copy_and_settles_conflicts() {
        let original = ed(&["Pay within sixty days.", "Deliver in two weeks.", "Warranty of twelve months."]).doc;
        let mut e = ed(&["Pay within thirty days.", "Deliver in two weeks.", "Warranty of twelve months."]);
        e.author = "Ann".into();
        assert!(e.compare_with(&original).is_some());
        let theirs = ed(&["Pay within ninety days.", "Deliver in five weeks.", "Warranty of twelve months."]).doc;
        e.start_log();
        let start = e.doc.clone();
        let mg = e.merge_with(&theirs, "Ben").unwrap();
        assert_eq!((mg.applied, mg.conflicts), (1, 1));
        assert_eq!(e.doc.visible_text(), vec!["Pay within thirty days.", "Deliver in five weeks.", "Warranty of twelve months."]);
        // Review opens on the conflict.
        assert!(e.review);
        assert!(matches!(e.review_edit().unwrap().kind, officina_text::track::EditKind::Conflict(_)));
        assert!(e.review_take(officina_text::merge::Take::Theirs));
        assert_eq!(e.doc.visible_text()[0], "Pay within ninety days.");
        assert!(e.doc.conflicts().is_empty());
        // The journal replays it.
        let log = e.take_log();
        let mut r = Editor::new(start, Rc::new(FixedFonts));
        for l in log {
            r.replay(l);
        }
        assert_eq!(r.doc.visible_text(), e.doc.visible_text());
        assert_eq!(r.edits().len(), e.edits().len());
        // Undo takes back the pick, then the merge.
        e.undo();
        assert_eq!(e.doc.conflicts().len(), 1);
        e.undo();
        assert_eq!(e.doc.visible_text()[1], "Deliver in two weeks.");
        // Merging into the original itself: the copy is compared with it.
        let mut o = Editor::new(original, Rc::new(FixedFonts));
        let mg = o.merge_with(&theirs, "Ben").unwrap();
        assert_eq!((mg.applied, mg.conflicts), (2, 0));
    }

    #[test]
    fn tracked_editing_reads_clean_and_resolves() {
        let mut e = ed(&["The quick brown fox", "jumps over"]);
        e.start_log();
        e.author = "Ann".into();
        e.set_tracking(true);
        // Replace "quick" with "fast": select, type.
        e.set_caret(Pos::new(0, 4), false);
        e.set_caret(Pos::new(0, 9), true);
        typ(&mut e, "fast");
        assert_eq!(e.doc.visible_text()[0], "The fast brown fox");
        // Backspace "fox" one letter at a time: one tombstone.
        e.set_caret(Pos::new(0, e.len(0)), false);
        for _ in 0..3 {
            e.key(Key::Backspace, NO, "", 0);
        }
        assert_eq!(e.doc.visible_text()[0], "The fast brown ");
        assert_eq!(e.doc.live_deletions().len(), 2, "one for quick, one for fox");
        // Typing at the end goes after the tombstone and is Ann's insertion.
        typ(&mut e, "cat");
        assert_eq!(e.doc.visible_text()[0], "The fast brown cat");
        // Fixing her own typo leaves no trace of it.
        e.key(Key::Backspace, NO, "", 0);
        typ(&mut e, "r");
        assert_eq!(e.doc.visible_text()[0], "The fast brown car");
        assert_eq!(e.doc.live_deletions().len(), 2);
        // Enter is an inserted paragraph break; bold is a formatting change.
        e.set_caret(Pos::new(1, 5), false);
        e.key(Key::Enter, NO, "", 0);
        assert_eq!(e.doc.visible_text(), vec!["The fast brown car", "jumps", " over"]);
        e.set_caret(Pos::new(2, 1), false);
        e.set_caret(Pos::new(2, 5), true);
        e.toggle(Toggle::Bold);
        let kinds: Vec<ChangeKind> = e.doc.changes().iter().map(|c| c.kind).collect();
        assert!(kinds.contains(&ChangeKind::InsertBreak) && kinds.contains(&ChangeKind::Format));
        // The journal replays to the same document.
        let log = e.take_log();
        let mut f = ed(&["The quick brown fox", "jumps over"]);
        for l in log {
            f.replay(l);
        }
        assert_eq!(texts(&f), texts(&e));
        assert_eq!(f.doc.visible_text(), e.doc.visible_text());
        // Reject one change at the caret: the bold.
        e.set_caret(Pos::new(2, 2), false);
        let ch = e.change_at_caret().unwrap();
        assert_eq!(ch.kind, ChangeKind::Format);
        assert!(e.resolve_change(Resolve::Reject, &ch));
        assert!(!e.doc.changes().iter().any(|c| c.kind == ChangeKind::Format));
        // Reject everything: the original; undo, accept everything: the edit.
        assert!(e.resolve(Resolve::Reject, |_| true));
        assert_eq!(texts(&e), vec!["The quick brown fox", "jumps over"]);
        e.undo();
        assert!(e.resolve(Resolve::Accept, |_| true));
        assert_eq!(texts(&e), vec!["The fast brown car", "jumps", " over"]);
        assert!(e.doc.changes().is_empty());
        // Tracking off: typing is plain again.
        e.set_tracking(false);
        e.set_caret(Pos::new(0, 0), false);
        typ(&mut e, "X");
        assert!(e.doc.changes().is_empty());
    }

    #[test]
    fn comments_add_reply_resolve_delete_and_journal() {
        let mut e = ed(&["one two three", "four"]);
        e.start_log();
        e.set_caret(Pos::new(0, 5), false);
        let id = e.add_comment("On two.", "Ann", "A", "2026-11-14T00:00:00Z").unwrap();
        assert_eq!(e.doc.comment_ranges(), vec![(id, (0, 4), (0, 7))]);
        e.set_caret(Pos::new(0, 6), false);
        assert_eq!(e.comment_at_caret(), Some(id));
        // Typing at the end of the range doesn't stretch it.
        e.set_caret(Pos::new(0, 7), false);
        typ(&mut e, "x");
        assert_eq!(e.doc.comment_ranges(), vec![(id, (0, 4), (0, 7))]);
        let r = e.reply_comment(id, "Yes.", "Bo", "B", "2026-11-14T00:01:00Z").unwrap();
        assert_eq!(e.thread_root(r), id);
        assert_eq!(e.thread(id), vec![id, r]);
        assert!(e.edit_comment(id, None, Some(true)));
        assert!(!e.edit_comment(id, None, Some(true)), "no change, no edit");
        assert!(e.edit_comment(r, Some("Yes, done."), None));
        // A second comment overlapping the first.
        e.set_caret(Pos::new(0, 2), false);
        e.set_caret(Pos::new(0, 6), true);
        let id2 = e.add_comment("Overlap.", "Ann", "A", "2026-11-14T00:02:00Z").unwrap();
        assert_eq!(e.doc.comment_ranges().len(), 2);
        // The journal replays to the same document.
        let log = e.take_log();
        let mut f = ed(&["one two three", "four"]);
        for l in log {
            f.replay(l);
        }
        assert_eq!(texts(&f), texts(&e));
        assert_eq!(f.doc.comment_ranges(), e.doc.comment_ranges());
        assert_eq!(f.doc.live_comments(), e.doc.live_comments());
        assert_eq!(f.doc.comments.get(r).unwrap().text, "Yes, done.");
        assert!(f.doc.comments.get(id).unwrap().done);
        // Deleting a thread takes its replies, keeps the text and the other comment.
        assert!(e.delete_comment(id));
        assert_eq!(e.doc.live_comments(), vec![id2]);
        assert_eq!(texts(&e)[0], "one twox three");
        e.undo();
        assert_eq!(e.doc.live_comments().len(), 3);
        assert_eq!(e.doc.comment_ranges(), f.doc.comment_ranges());
    }

    #[test]
    fn notes_insert_edit_undo_and_journal() {
        let mut e = ed(&["one two", "three"]);
        e.start_log();
        e.set_caret(Pos::new(0, 3), false);
        let id = e.insert_note(NoteKind::Foot, "A note.").unwrap();
        assert_eq!(e.caret, Pos::new(0, 3 + OBJECT_CHAR.len_utf8() as u32));
        assert_eq!(e.note_at_caret(), Some(id));
        assert_eq!(e.doc.note_label(id), "1");
        typ(&mut e, "x");
        let p = e.doc.para(0).unwrap();
        assert!(e.doc.attrs.get(p.attr_at(p.text.find('x').unwrap() as u32)).note.is_none(), "typed text isn't an anchor");
        assert!(e.set_note_text(id, "Changed\nTwo lines."));
        assert_eq!(e.note_text(id).unwrap(), "Changed\nTwo lines.");
        while !e.paginate(Duration::from_secs(1)) {}
        assert!(e.pages.pages()[0].notes.iter().any(|n| n.note == id));
        // The journal replays to the same document.
        let log = e.take_log();
        let mut f = ed(&["one two", "three"]);
        for l in log {
            f.replay(l);
        }
        assert_eq!(texts(&f), texts(&e));
        assert_eq!(f.note_text(id), e.note_text(id));
        e.undo();
        assert_eq!(e.note_text(id).unwrap(), "A note.");
        e.undo();
        e.undo();
        assert_eq!(texts(&e)[0], "one two");
        assert!(e.doc.note_numbers().is_empty());
        e.redo();
        assert_eq!(e.doc.note_label(id), "1");
    }

    #[test]
    fn spelling_next_and_fix() {
        let mut e = ed(&["ok teh one", "fine", "teh end teh"]);
        assert!(!e.next_misspelled());
        e.checker = Some(Rc::new(Teh));
        assert_eq!(*e.misspelled(0), vec![(3, 6)]);
        assert!(e.next_misspelled());
        assert_eq!((e.anchor, e.caret), (Pos::new(0, 3), Pos::new(0, 6)));
        assert!(e.next_misspelled());
        assert_eq!(e.anchor, Pos::new(2, 0));
        assert!(e.replace_selection("the"));
        assert_eq!(texts(&e)[2], "the end teh");
        assert_eq!(*e.misspelled(2), vec![(8, 11)]);
        assert!(e.next_misspelled());
        assert_eq!(e.anchor, Pos::new(2, 8));
        assert!(e.next_misspelled());
        assert_eq!(e.anchor, Pos::new(0, 3));
        e.undo();
        assert_eq!(texts(&e)[2], "teh end teh");
    }

    #[test]
    fn find_replace_and_counts() {
        let mut e = ed(&["The cat sat.", "A cat, a Cat."]);
        let q = Query { text: "cat".into(), ..Default::default() };
        assert!(e.find_next(&q, true));
        assert_eq!((e.anchor, e.caret), (Pos::new(0, 4), Pos::new(0, 7)));
        assert!(e.find_next(&q, true));
        assert_eq!(e.anchor, Pos::new(1, 2));
        assert!(e.find_next(&q, false));
        assert_eq!(e.anchor, Pos::new(0, 4));
        // Replace replaces the selected match and moves on.
        assert!(e.replace(&q, "dog"));
        assert_eq!(texts(&e), ["The dog sat.", "A cat, a Cat."]);
        assert_eq!(e.anchor, Pos::new(1, 2));
        // Replace with no match selected only finds.
        e.set_caret(Pos::new(0, 0), false);
        assert!(!e.replace(&q, "dog"));
        assert_eq!(e.anchor, Pos::new(1, 2));
        assert_eq!(e.counts().words, 1);
        assert_eq!(e.replace_all(&q, "bird"), 2);
        assert_eq!(texts(&e), ["The dog sat.", "A bird, a bird."]);
        assert_eq!(e.words.total().words, 7);
        e.undo();
        assert_eq!(texts(&e), ["The dog sat.", "A cat, a Cat."]);
        e.undo();
        assert_eq!(texts(&e), ["The cat sat.", "A cat, a Cat."]);
        assert_eq!(e.words.total().words, 7);
        e.set_caret(Pos::new(0, 4), false);
        e.set_caret(Pos::new(1, 5), true);
        assert_eq!(e.counts().words, 4);
        let whole = Query { text: "a".into(), whole_word: true, match_case: true, ..Default::default() };
        assert!(e.find_next(&whole, true));
        assert_eq!(e.anchor, Pos::new(1, 7));
        assert!(!e.find_next(&Query { text: "zebra".into(), ..Default::default() }, true));
    }

    #[test]
    fn typing_enter_backspace_and_undo_in_steps() {
        let mut e = ed(&[""]);
        typ(&mut e, "Hello");
        e.key(Key::Enter, NO, "", 0);
        typ(&mut e, "world");
        assert_eq!(texts(&e), ["Hello", "world"]);
        assert_eq!(e.caret, Pos::new(1, 5));
        e.key(Key::Home, NO, "", 0);
        e.key(Key::Backspace, NO, "", 0);
        assert_eq!(texts(&e), ["Helloworld"]);
        assert_eq!(e.caret, Pos::new(0, 5));
        // Undo: the join, then "world", then Enter, then "Hello".
        e.key(Key::Char, CTRL, "z", 0);
        assert_eq!(texts(&e), ["Hello", "world"]);
        e.key(Key::Char, CTRL, "z", 0);
        assert_eq!(texts(&e), ["Hello", ""]);
        e.key(Key::Char, CTRL, "z", 0);
        assert_eq!(texts(&e), ["Hello"]);
        e.key(Key::Char, CTRL, "z", 0);
        assert_eq!(texts(&e), [""]);
        e.key(Key::Char, CTRL, "y", 0);
        assert_eq!(texts(&e), ["Hello"]);
    }

    #[test]
    fn graphemes_words_and_selection() {
        let mut e = ed(&["e\u{301}té naïve 👍🏽 ok"]);
        e.key(Key::Right, NO, "", 0);
        assert_eq!(e.caret.at, 3, "é with a combining mark is one step");
        e.key(Key::Right, CTRL, "", 0);
        assert_eq!(&e.text(0)[e.caret.at as usize..], "naïve 👍🏽 ok");
        e.key(Key::Right, CTRL_SHIFT, "", 0);
        assert_eq!(e.selected_text(), "naïve ");
        e.key(Key::Char, NO, "X", 0);
        assert_eq!(texts(&e), ["e\u{301}té X👍🏽 ok"]);
        e.key(Key::Right, NO, "", 0);
        e.key(Key::Backspace, NO, "", 0);
        assert_eq!(texts(&e), ["e\u{301}té X ok"], "the emoji with its modifier goes at once");
        e.key(Key::Backspace, CTRL, "", 0);
        assert_eq!(texts(&e), ["e\u{301}té  ok"]);
    }

    #[test]
    fn selection_across_paragraphs_copy_cut_paste() {
        let mut e = ed(&["one two", "three", "four five"]);
        e.set_caret(Pos::new(0, 4), false);
        e.set_caret(Pos::new(2, 4), true);
        assert_eq!(e.key(Key::Char, CTRL, "c", 0), Outcome::Copy("two\nthree\nfour".into()));
        assert_eq!(e.key(Key::Char, CTRL, "x", 0), Outcome::Copy("two\nthree\nfour".into()));
        assert_eq!(texts(&e), ["one  five"]);
        e.insert("A\nB\nC");
        assert_eq!(texts(&e), ["one A", "B", "C five"]);
        assert_eq!(e.caret, Pos::new(2, 1));
        e.undo();
        assert_eq!(texts(&e), ["one  five"]);
        e.undo();
        assert_eq!(texts(&e), ["one two", "three", "four five"]);
    }

    #[test]
    fn formatting_keys_and_typing_attributes() {
        let mut e = ed(&["plain text"]);
        e.set_caret(Pos::new(0, 0), false);
        e.set_caret(Pos::new(0, 5), true);
        e.key(Key::Char, CTRL, "b", 0);
        let p = e.doc.para(0).unwrap();
        assert!(e.doc.attrs.get(p.attr_at(0)).bold == Some(true));
        assert!(e.doc.attrs.get(p.attr_at(6)).bold.is_none());
        // Ctrl+I with no selection, then type: the new text is italic.
        e.set_caret(Pos::new(0, 10), false);
        e.key(Key::Char, CTRL, "i", 0);
        typ(&mut e, "!!");
        let p = e.doc.para(0).unwrap();
        assert_eq!(e.doc.attrs.get(p.attr_at(10)).italic, Some(true));
        assert_eq!(e.doc.attrs.get(p.attr_at(9)).italic, None);
        // Styles and alignment.
        e.key(Key::Char, CTRL, "1", 0);
        assert_eq!(e.doc.para(0).unwrap().style, e.doc.styles.by_name("Heading 1").unwrap());
        e.key(Key::Char, CTRL, "e", 0);
        assert_eq!(e.doc.para(0).unwrap().props.align, Some(Align::Center));
        // Enter at the end of a heading gives body text.
        e.key(Key::End, NO, "", 0);
        e.key(Key::Enter, NO, "", 0);
        assert_eq!(e.doc.para(1).unwrap().style, 0);
    }

    #[test]
    fn up_down_keep_the_column_and_hit_finds_positions() {
        // FixedFonts: 352 LU a character at 11 pt; a column of 40 characters.
        let mut d = TextDoc::new();
        d.page.width = (40 * 352 * 20 / 64 + 2880 + 1) as i32;
        d.blocks.remove(0);
        d.push_para(&"a".repeat(39), 0, vec![]);
        d.push_para("short", 0, vec![]);
        d.push_para(&"b".repeat(60), 0, vec![]);
        let mut e = Editor::new(d, Rc::new(FixedFonts));
        e.set_caret(Pos::new(0, 20), false);
        e.key(Key::Down, NO, "", 0);
        assert_eq!(e.caret, Pos::new(1, 5), "short line: its end");
        e.key(Key::Down, NO, "", 0);
        assert_eq!(e.caret, Pos::new(2, 20), "the goal column comes back");
        e.key(Key::Down, NO, "", 0);
        assert_eq!(e.caret.block, 2);
        assert!(e.caret.at > 40, "second line of the wrapped paragraph");
        let s = e.spot(Pos::new(2, 41));
        assert_eq!(e.hit(s.page, s.x, s.y + 1), Pos::new(2, 41));
        let s0 = e.spot(Pos::new(0, 3));
        assert_eq!(s0.x, 3 * 352);
        assert_eq!(e.hit(0, 3 * 352 + 100, 10), Pos::new(0, 3));
        assert_eq!(e.hit(0, 3 * 352 + 250, 10), Pos::new(0, 4));
        e.key(Key::End, NO, "", 0);
        e.key(Key::Home, SHIFT, "", 0);
        assert!(e.has_selection());
    }

    #[test]
    fn page_setup_and_footer_undo() {
        let paras: Vec<String> = (0..80).map(|i| format!("{i} ") + &"word ".repeat(40)).collect();
        let refs: Vec<&str> = paras.iter().map(String::as_str).collect();
        let mut e = ed(&refs);
        while !e.paginate(Duration::from_secs(1)) {}
        let a4 = e.pages.pages().len();
        e.toggle_page_numbers();
        assert!(e.doc.footer.is_some());
        let mut g = e.doc.page;
        g.width = 16838;
        g.height = 11906;
        e.set_page("Landscape", g, e.doc.header.clone(), e.doc.footer.clone());
        while !e.paginate(Duration::from_secs(1)) {}
        assert!(e.pages.pages().len() != a4, "landscape paginates again");
        e.undo();
        while !e.paginate(Duration::from_secs(1)) {}
        assert_eq!(e.pages.pages().len(), a4);
        e.undo();
        assert!(e.doc.footer.is_none());
    }

    /// A w x h PNG, one colour.
    pub(crate) fn png(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, w, h);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut wr = enc.write_header().unwrap();
            wr.write_image_data(&[200, 40, 40, 255].repeat((w * h) as usize)).unwrap();
        }
        out
    }

    #[test]
    fn a_picture_goes_in_line_and_typing_beside_it_is_text() {
        let mut e = ed(&["ab"]);
        e.start_log();
        e.set_caret(Pos::new(0, 1), false);
        let bytes = png(300, 150);
        let (_, w, h) = navigator_image::image::natural_size(&bytes).unwrap();
        assert_eq!((w, h), (300, 150));
        assert!(e.insert_image(ImageData { bytes: bytes.clone().into(), width: w, height: h }));
        assert_eq!(texts(&e), ["a\u{FFFC}b"]);
        let p = e.doc.para(0).unwrap();
        let o = e.doc.attrs.get(p.attr_at(1)).object.expect("a picture");
        assert_eq!((o.width, o.height), (300 * 15, 150 * 15), "96 dpi");
        typ(&mut e, "x");
        let p = e.doc.para(0).unwrap();
        assert!(e.doc.attrs.get(p.attr_at(4)).object.is_none(), "typed text isn't a picture");
        // Pasted object characters are dropped.
        e.insert("\u{FFFC}");
        assert_eq!(texts(&e), ["a\u{FFFC}xb"]);
        // The same picture again is stored once; a huge one fits the column.
        e.insert_image(ImageData { bytes: bytes.into(), width: w, height: h });
        e.insert_image(ImageData { bytes: png(4000, 1000).into(), width: 4000, height: 1000 });
        assert_eq!(e.doc.images.len(), 2);
        let p = e.doc.para(0).unwrap();
        let big = e.doc.attrs.get(p.attr_at(p.text.len() as u32 - 2)).object.unwrap();
        assert!(big.width as i64 <= e.pages.width() * 20 / officina_text::LU);
        // The journal replays to the same document, pictures included.
        let log = e.take_log();
        let mut r = ed(&["ab"]);
        for l in log {
            r.replay(l);
        }
        assert_eq!(texts(&r), texts(&e));
        assert_eq!(r.doc.images.len(), 2);
        while !e.paginate(Duration::from_secs(1)) {}
        e.undo();
        e.undo();
        e.undo();
        e.undo();
        assert_eq!(texts(&e), ["ab"]);
    }

    #[test]
    fn tables_by_commands_and_keys() {
        let mut e = ed(&["before", "after"]);
        e.set_caret(Pos::new(0, 6), false);
        assert!(e.insert_table(2, 3));
        assert_eq!(e.blocks(), 2 + 6);
        assert_eq!(e.caret, Pos::new(1, 0));
        // Tab walks the cells; from the last one it adds a row.
        typ(&mut e, "a");
        e.key(Key::Tab, NO, "", 0);
        assert_eq!(e.caret.block, 2);
        typ(&mut e, "b");
        for _ in 0..4 {
            e.key(Key::Tab, NO, "", 0);
        }
        assert_eq!(e.caret.block, 6);
        e.key(Key::Tab, NO, "", 0);
        assert_eq!(e.blocks(), 2 + 9, "a third row");
        assert_eq!(e.caret.block, 7);
        e.key(Key::Tab, SHIFT, "", 0);
        assert_eq!(e.caret.block, 6);
        // Backspace at a cell's start and Delete at its end don't join cells.
        e.set_caret(Pos::new(2, 0), false);
        assert_eq!(e.key(Key::Backspace, NO, "", 0), Outcome::Ignored);
        e.set_caret(Pos::new(1, 1), false);
        assert_eq!(e.key(Key::Delete, NO, "", 0), Outcome::Ignored);
        // Enter makes a second paragraph in the same cell; the row stays one row.
        e.key(Key::Enter, NO, "", 0);
        assert!(e.doc.same_cell(1, 2));
        assert_eq!(e.doc.row_cells(e.doc.row_range(1), e.pages.width()).len(), 3);
        // A selection across cells clears their text, keeping every cell
        // (paragraphs inside one cell join as usual).
        let before = e.blocks();
        e.set_caret(Pos::new(1, 0), false);
        e.set_caret(Pos::new(3, 1), true);
        e.key(Key::Backspace, NO, "", 0);
        assert_eq!(e.blocks(), before - 1);
        assert_eq!(texts(&e)[1..3].concat(), "");
        assert_eq!(e.doc.row_cells(e.doc.row_range(1), e.pages.width()).len(), 3);
        // Columns and rows.
        e.set_caret(Pos::new(1, 0), false);
        let n = e.blocks();
        assert!(e.insert_column_right());
        assert_eq!(e.blocks(), n + 3);
        assert_eq!(e.doc.row_cells(e.doc.row_range(1), e.pages.width()).len(), 4);
        let t = e.table_range(1);
        let mut k = t.start;
        while k < t.end {
            let r = e.doc.row_range(k);
            assert_eq!(e.doc.row_cells(r.clone(), e.pages.width()).len(), 4, "every row has four cells");
            k = r.end;
        }
        assert!(e.delete_column());
        assert_eq!(e.blocks(), n);
        assert!(e.delete_row());
        assert!(e.insert_row_below());
        // Undo takes it all back.
        while e.undo() {}
        assert_eq!(texts(&e), ["before", "after"]);
        // Down from a cell goes to the cell below.
        e.set_caret(Pos::new(0, 6), false);
        e.insert_table(3, 2);
        e.set_caret(Pos::new(2, 0), false);
        e.key(Key::Down, NO, "", 0);
        assert_eq!(e.caret.block, 4);
        e.key(Key::Up, NO, "", 0);
        assert_eq!(e.caret.block, 2);
        // A click in the second column lands in it.
        while !e.paginate(Duration::from_secs(1)) {}
        let s = e.spot(Pos::new(2, 0));
        assert_eq!(e.hit(s.page, s.x + 10, s.y + 10).block, 2);
        let s1 = e.spot(Pos::new(1, 0));
        assert!(s.x > s1.x && s.y == s1.y, "side by side");
    }

    #[test]
    fn lists_by_keys() {
        let mut e = ed(&["alpha", "beta", "gamma", "after"]);
        e.set_caret(Pos::new(0, 0), false);
        e.set_caret(Pos::new(2, 1), true);
        e.key(Key::Char, CTRL_SHIFT, "7", 0);
        let labels = |e: &mut Editor| (0..e.doc.blocks.len()).map(|i| e.numbering().label(i).unwrap_or_default()).collect::<Vec<_>>();
        assert_eq!(labels(&mut e), ["1.", "2.", "3.", ""]);
        // Tab at the start of an item: one level in.
        e.set_caret(Pos::new(1, 0), false);
        e.key(Key::Tab, NO, "", 0);
        assert_eq!(labels(&mut e), ["1.", "a.", "2.", ""]);
        // Enter at the end of an item: another item; Enter again on the empty one ends the list.
        e.set_caret(Pos::new(2, 5), false);
        e.key(Key::Enter, NO, "", 0);
        assert_eq!(labels(&mut e), ["1.", "a.", "2.", "3.", ""]);
        e.key(Key::Enter, NO, "", 0);
        assert_eq!(labels(&mut e), ["1.", "a.", "2.", "", ""]);
        // Backspace at an item's start takes it out of the list, keeping the text.
        e.set_caret(Pos::new(1, 0), false);
        e.key(Key::Backspace, NO, "", 0);
        assert_eq!(labels(&mut e), ["1.", "", "2.", "", ""]);
        assert_eq!(texts(&e)[1], "beta");
        // Bullets toggle off again; a tab elsewhere is a tab.
        e.set_caret(Pos::new(4, 0), false);
        e.key(Key::Char, CTRL_SHIFT, "8", 0);
        assert_eq!(labels(&mut e)[4], "•");
        e.key(Key::Char, CTRL_SHIFT, "8", 0);
        assert_eq!(labels(&mut e)[4], "");
        e.set_caret(Pos::new(4, 2), false);
        e.key(Key::Tab, NO, "", 0);
        assert!(texts(&e)[4].contains('\t'));
    }

    #[test]
    fn a_logged_session_replays_to_the_same_document() {
        let mut e = ed(&["one", "two"]);
        let base = e.doc.clone();
        e.start_log();
        typ(&mut e, "abc");
        e.key(Key::Enter, NO, "", 0);
        e.set_caret(Pos::new(0, 0), false);
        e.set_caret(Pos::new(1, 2), true);
        e.key(Key::Char, CTRL, "b", 0);
        e.key(Key::Char, CTRL, "z", 0);
        e.key(Key::Char, CTRL, "y", 0);
        e.set_caret(Pos::new(2, 1), false);
        e.key(Key::Char, CTRL, "u", 0);
        typ(&mut e, "xy");
        e.key(Key::Backspace, NO, "", 0);
        let log = e.take_log();
        let mut r = Editor::new(base, Rc::new(FixedFonts));
        for l in log {
            r.replay(l);
        }
        assert_eq!(texts(&r), texts(&e));
        let runs = |e: &Editor| e.doc.blocks.iter().map(|b| b.as_para().unwrap().runs.iter().map(|&(s, a)| (s, e.doc.attrs.get(a).clone())).collect::<Vec<_>>()).collect::<Vec<_>>();
        assert_eq!(runs(&r), runs(&e));
        // Undo history replays too.
        while e.undo() {}
        while r.undo() {}
        assert_eq!(texts(&r), texts(&e));
        assert_eq!(texts(&r), ["one", "two"]);
    }

    #[test]
    fn edits_repaginate_like_a_fresh_pagination() {
        let paras: Vec<String> = (0..300).map(|i| format!("{i} ") + &"word ".repeat(10 + i % 60)).collect();
        let refs: Vec<&str> = paras.iter().map(String::as_str).collect();
        let mut e = ed(&refs);
        while !e.paginate(Duration::from_secs(1)) {}
        let check = |e: &mut Editor| {
            while !e.paginate(Duration::from_secs(1)) {}
            let mut f = Paginator::new(&e.doc);
            let mut c = LayoutCache::default();
            f.run(&e.doc, &mut c, &FixedFonts, usize::MAX);
            assert_eq!(e.pages.pages(), f.pages());
        };
        e.set_caret(Pos::new(150, 3), false);
        typ(&mut e, &"more ".repeat(30));
        check(&mut e);
        e.key(Key::Enter, NO, "", 0);
        check(&mut e);
        e.set_caret(Pos::new(10, 0), false);
        e.set_caret(Pos::new(200, 0), true);
        e.key(Key::Backspace, NO, "", 0);
        check(&mut e);
        e.undo();
        check(&mut e);
        e.undo();
        check(&mut e);
        e.redo();
        check(&mut e);
    }
}
