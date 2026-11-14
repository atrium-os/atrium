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
use officina_text::find::{count, Counts, Matcher, Query, WordCounter};
use officina_text::layout::{Fonts, LayoutCache, Line, ParaLayout};
use officina_text::lists::Numbering;
use officina_text::model::{Align, AttrId, Block, CellRef, CharAttrs, CharFormat, ImageData, Para, MAX_CELLS, ListKind, ListRef, ObjectRef, ParaProps, StyleId, TextDoc, MAX_LIST_LEVEL, MAX_OBJECT_TWIPS, OBJECT_CHAR};
use officina_text::ops::{Fragment, TextOp};
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
    /// Actions since [`Editor::take_log`], when journaling.
    log: Option<Vec<Logged>>,
    attrs_logged: usize,
    images_logged: usize,
    numbering: Option<(u64, Rc<Numbering>)>,
    /// Word count, kept current through edits.
    pub words: WordCounter,
}

impl Editor {
    pub fn new(doc: TextDoc, fonts: Rc<dyn Fonts>) -> Editor {
        let pages = Paginator::new(&doc);
        let mut e = Editor { doc, history: History::new(), anchor: Pos::default(), caret: Pos::default(), typing: None, goal_x: None, pages, cache: LayoutCache::default(), fonts, last: Kind::Other, revision: 0, log: None, attrs_logged: 0, images_logged: 0, numbering: None, words: WordCounter::default() };
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
    pub fn paginate(&mut self, budget: Duration) -> bool {
        self.pages.run_for(&self.doc, &mut self.cache, &*self.fonts, budget)
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
    fn delete_selection_ops(&self) -> (Vec<TextOp>, Pos) {
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
        (ops, a)
    }

    /// The attributes typed text gets at `p`: those before it, with any
    /// pending formatting over them.
    /// Text typed next to a picture takes its formatting, not the picture.
    fn typing_attr(&mut self, p: Pos) -> Option<AttrId> {
        let para = self.doc.para(p.block)?;
        let before = para.attr_at(p.at.saturating_sub(1));
        let mut base = self.doc.attrs.get(before).clone();
        let pic = base.object.take().is_some();
        let t = match self.typing.clone() {
            Some(t) => t,
            None if pic => CharAttrs::default(),
            None => return None,
        };
        let mut a = t.over(&base);
        a.object = None;
        Some(self.doc.attrs.intern(a))
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
        let (mut ops, start) = self.delete_selection_ops();
        let had_sel = !ops.is_empty();
        let attr = self.typing_attr(start);
        let frag = |t: &str| Fragment { text: t.to_string(), runs: attr.map(|a| vec![(0, a)]).unwrap_or_default() };
        let mut p = start;
        let parts: Vec<&str> = s.split('\n').collect();
        for (i, part) in parts.iter().enumerate() {
            if !part.is_empty() {
                ops.push(TextOp::Splice { block: p.block as u32, at: p.at, del: 0, ins: frag(part) });
                p.at += part.len() as u32;
            }
            if i + 1 < parts.len() {
                let (style, props) = self.doc.para(start.block).map(|q| (q.style, q.props.clone())).unwrap_or_default();
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
        let (mut ops, start) = self.delete_selection_ops();
        let Some(p) = self.doc.para(start.block) else { return };
        let at_end = start.at as usize == p.text.len() && !self.has_selection();
        let style = if at_end { self.doc.styles.styles.get(p.style as usize).and_then(|s| s.next).unwrap_or(p.style) } else { p.style };
        let props = if at_end && style != p.style { ParaProps::default() } else { p.props.clone() };
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
        let (ops, p) = self.delete_selection_ops();
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
        let (ops, p) = self.delete_selection_ops();
        self.apply("Delete", ops, Kind::Deleting);
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
        let mut ops = Vec::new();
        for k in a.block..=b.block {
            let s = if k == a.block { a.at } else { 0 };
            let e = if k == b.block { b.at } else { self.len(k) };
            if s < e {
                ops.extend(self.doc.format_ops(k, s, e, &f));
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
        Pos::new(p.block, g as u32)
    }

    fn grapheme_right(&self, p: Pos) -> Pos {
        let t = self.text(p.block);
        if p.at as usize >= t.len() {
            return if p.block + 1 >= self.blocks() { p } else { Pos::new(p.block + 1, 0) };
        }
        let g = t[p.at as usize..].graphemes(true).next().map_or(0, str::len);
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
