//! Pagination (docs/spec/officina.md §7.4): pages of line slices, a checkpoint
//! at each page start, and repagination after an edit that starts at the page
//! the edit is on and stops when a page starts where an old page did.
//!
//! Laying out one page is a pure function of the document from its checkpoint
//! on, so once a new page starts at the old checkpoint of a page past the edit,
//! every later page is the old one with block numbers shifted by the edit.
//!
//! Rules, as Word and Writer apply them:
//! - space before a paragraph is dropped at the top of a page;
//! - a page break before a paragraph starts a new page unless the page is empty;
//! - widow and orphan control keeps at least two lines of a paragraph together
//!   on each side of a break;
//! - keep together moves a paragraph that would break to the next page, and
//!   keep with next moves a paragraph whose next one would start on the next
//!   page, both only when what they keep fits on one page;
//! - a line taller than the page still goes on the page, alone;
//! - a table row stays on one page (a row taller than the page goes on a page
//!   alone and is cut off at its foot).

use std::ops::Range;
use std::time::{Duration, Instant};

use crate::layout::{Fonts, LayoutCache, ParaLayout};
use crate::model::{NoteId, NoteKind, TextDoc, OBJECT_CHAR};
use crate::twips;
use std::rc::Rc;

/// Where a page starts: a block and a line in it. (Floats, footnotes and
/// counters waiting for a place join this in O3d.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Checkpoint {
    pub block: usize,
    pub line: usize,
}

/// Lines `lines` of block `block`, the first line's top `y` below the page's
/// content top.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slice {
    pub block: usize,
    pub lines: Range<usize>,
    pub y: i64,
    /// The lines' left edge from the content box's (a cell's text inside its
    /// padding; 0 outside tables) and the width they were laid out in.
    pub x: i64,
    pub w: i64,
}

/// A table cell's border box on a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellBox {
    pub x: i64,
    pub y: i64,
    pub w: i64,
    pub h: i64,
}

/// Lines `lines` of paragraph `para` of note `note`, the first line's top `y`
/// below the page's content top (the text starts [`NOTE_HANG`] in, after
/// the label).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteSlice {
    pub note: NoteId,
    pub para: usize,
    pub lines: Range<usize>,
    pub y: i64,
}

/// Room for a note's label before its text.
pub const NOTE_HANG: i64 = crate::pt(18);
/// The rule above footnotes and above the endnotes: its height with the space
/// around it.
pub const NOTE_RULE_SPACE: i64 = crate::pt(12);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub start: Checkpoint,
    pub slices: Vec<Slice>,
    /// Table cells' borders.
    pub cells: Vec<CellBox>,
    /// Content height used.
    pub height: i64,
    /// Footnotes at the foot and endnotes after the text.
    pub notes: Vec<NoteSlice>,
    /// Where the rules above the footnotes and above the endnotes go.
    pub foot_rule: Option<i64>,
    pub end_rule: Option<i64>,
}

impl Page {
    fn shift(&mut self, delta: isize) {
        self.start.block = (self.start.block as isize + delta) as usize;
        for s in &mut self.slices {
            s.block = (s.block as isize + delta) as usize;
        }
    }
}

pub use crate::model::Edit;

/// Longest keep-with-next chain looked at; longer chains don't keep.
const MAX_CHAIN: usize = 64;

/// Pages of one document, kept current across edits.
#[derive(Default)]
pub struct Paginator {
    pages: Vec<Page>,
    /// The next page to lay out, while pagination is behind.
    cursor: Option<Checkpoint>,
    /// Old pages from the restart point on, in old block numbers, waiting for a
    /// new page to start where one of them did.
    old: Vec<Page>,
    old_at: usize,
    settle: usize,
    delta: isize,
    /// Where pagination had got to when the edit came, in new block numbers:
    /// old pages end there, so after converging, pagination goes on from it.
    resume: Option<Checkpoint>,
    width: i64,
    height: i64,
    /// Pages laid out since creation (for the gates).
    pub laid: u64,
}

impl Paginator {
    /// Starts paginating `doc` from the beginning.
    pub fn new(doc: &TextDoc) -> Paginator {
        let mut p = Paginator::default();
        p.reset(doc);
        p
    }

    /// Throws the pages away (page setup changed).
    pub fn reset(&mut self, doc: &TextDoc) {
        let g = &doc.page;
        self.width = twips((g.width - g.margin_start - g.margin_end).max(720) as i64);
        self.height = twips((g.height - g.margin_top - g.margin_bottom).max(720) as i64);
        self.pages.clear();
        self.old.clear();
        self.resume = None;
        self.cursor = Some(Checkpoint::default());
    }

    /// Whether the pages were laid out for `doc`'s page geometry.
    pub fn fits(&self, doc: &TextDoc) -> bool {
        let g = &doc.page;
        self.width == twips((g.width - g.margin_start - g.margin_end).max(720) as i64) && self.height == twips((g.height - g.margin_top - g.margin_bottom).max(720) as i64)
    }

    /// Column width in layout units, for laying paragraphs out.
    pub fn width(&self) -> i64 {
        self.width
    }
    pub fn content_height(&self) -> i64 {
        self.height
    }
    pub fn pages(&self) -> &[Page] {
        &self.pages
    }
    /// Pages are complete and current.
    pub fn done(&self) -> bool {
        self.cursor.is_none()
    }

    /// The page holding line `line` of block `block` (or the last page laid
    /// out, if pagination hasn't reached it).
    pub fn page_of(&self, block: usize, line: usize) -> usize {
        let at = Checkpoint { block, line };
        self.pages.partition_point(|p| p.start <= at).saturating_sub(1)
    }

    /// Records an edit: drops the pages from the one the edit can first change,
    /// keeping the rest to compare against.
    pub fn edit(&mut self, doc: &TextDoc, cache: &mut LayoutCache, fonts: &dyn Fonts, e: Edit) {
        // A paragraph that keeps with the next one breaks according to it, so
        // start from the head of the chain that leads into the edit.
        let mut c = e.first.min(doc.blocks.len());
        let mut n = 0;
        while c > 0 && n < MAX_CHAIN && self.layout(doc, cache, fonts, c - 1).format.keep_with_next {
            c -= 1;
            n += 1;
        }
        let mut k = self.page_of(c, 0);
        // A page that starts at the block may have ended the one before
        // because of it (orphan control, keeps), so that page goes too.
        if k > 0 && self.pages.get(k).is_some_and(|p| p.start == Checkpoint { block: c, line: 0 }) {
            k -= 1;
        }
        let restart = self.pages.get(k).map(|p| p.start).unwrap_or_default();
        // Old block number of the first block past the edit.
        let old_last = e.last as isize - e.delta;
        if !self.old.is_empty() || self.cursor.is_some_and(|cur| cur < restart || (cur.block as isize) < old_last) {
            // Already behind: pages past the cursor were never laid out, and a
            // second edit before convergence would compose shifts; repaginate
            // from the earlier point to the end instead.
            self.old.clear();
            self.resume = None;
            let from = self.cursor.map_or(restart, |cur| cur.min(restart));
            let k = self.pages.partition_point(|p| p.start < from);
            self.pages.truncate(k);
            self.cursor = Some(from);
            return;
        }
        self.old = self.pages.split_off(k.min(self.pages.len()));
        self.old_at = 0;
        self.settle = e.last;
        self.delta = e.delta;
        self.resume = self.cursor.map(|c| Checkpoint { block: (c.block as isize + e.delta) as usize, line: c.line });
        self.cursor = Some(restart);
    }

    /// Lays out up to `max` pages; returns whether pagination is done.
    pub fn run(&mut self, doc: &TextDoc, cache: &mut LayoutCache, fonts: &dyn Fonts, max: usize) -> bool {
        for _ in 0..max {
            if !self.step(doc, cache, fonts) {
                break;
            }
        }
        self.done()
    }

    /// Lays out pages until `budget` has passed; returns whether pagination is done.
    pub fn run_for(&mut self, doc: &TextDoc, cache: &mut LayoutCache, fonts: &dyn Fonts, budget: Duration) -> bool {
        let t = Instant::now();
        while self.step(doc, cache, fonts) {
            if t.elapsed() >= budget {
                break;
            }
        }
        self.done()
    }

    /// One page. Returns false once done.
    fn step(&mut self, doc: &TextDoc, cache: &mut LayoutCache, fonts: &dyn Fonts) -> bool {
        let Some(at) = self.cursor else { return false };
        let n_blocks = doc.blocks.len();
        if (at.block > n_blocks || (at.block == n_blocks && self.endnote_lines(doc, cache, fonts).len() <= at.line)) && !self.pages.is_empty() {
            self.finish();
            return false;
        }
        let (page, next) = self.page(doc, cache, fonts, at);
        self.pages.push(page);
        self.laid += 1;
        match next {
            None => self.finish(),
            Some(n) => {
                self.cursor = Some(n);
                // Endnote pages are always laid out again (they follow every
                // note's text).
                if n.block < n_blocks && self.converge(n, n_blocks) {
                    return false;
                }
            }
        }
        self.cursor.is_some()
    }

    fn finish(&mut self) {
        self.cursor = None;
        self.old.clear();
        self.resume = None;
    }

    /// If an old page started at `n` (past the edit), take it and all after it.
    fn converge(&mut self, n: Checkpoint, n_blocks: usize) -> bool {
        if self.old.is_empty() || n.block < self.settle {
            return false;
        }
        let want = Checkpoint { block: (n.block as isize - self.delta) as usize, line: n.line };
        while self.old_at < self.old.len() && self.old[self.old_at].start < want {
            self.old_at += 1;
        }
        if self.old_at < self.old.len() && self.old[self.old_at].start == want {
            let delta = self.delta;
            let mut rest = self.old.split_off(self.old_at);
            if delta != 0 {
                rest.iter_mut().for_each(|p| p.shift(delta));
            }
            self.pages.append(&mut rest);
            // The page the endnotes start on and those after it follow every
            // note's text: they are laid out again.
            if let Some(k) = self.pages.iter().position(|p| p.end_rule.is_some() || p.start.block >= n_blocks) {
                let at = self.pages[k].start;
                self.pages.truncate(k);
                self.old.clear();
                self.resume = None;
                self.cursor = Some(at);
                return false;
            }
            if let Some(r) = self.resume.take() {
                // The old pages stopped short of the end: carry on from there.
                self.old.clear();
                self.cursor = Some(r);
                return false;
            }
            self.finish();
            return true;
        }
        false
    }

    fn layout(&self, doc: &TextDoc, cache: &mut LayoutCache, fonts: &dyn Fonts, i: usize) -> Rc<ParaLayout> {
        let p = doc.para(i).expect("paragraph");
        cache.get(doc, p, doc.block_box(i, self.width).1, fonts)
    }

    /// Lays out the table row starting at block `b` at `y`; returns the slices,
    /// the cell boxes, the row's height and the block after it.
    fn row(&self, doc: &TextDoc, cache: &mut LayoutCache, fonts: &dyn Fonts, b: usize, y: i64) -> (Vec<Slice>, Vec<CellBox>, i64, usize) {
        let row = doc.row_range(b);
        let row = b..row.end.max(b + 1);
        let pad = twips(crate::model::CELL_PAD as i64);
        let mut slices = Vec::new();
        let mut boxes = Vec::new();
        let mut row_h = 0;
        for (blocks, x, w) in doc.row_cells(row.clone(), self.width) {
            let mut yy = y + pad;
            let last = blocks.end - 1;
            for k in blocks {
                let l = self.layout(doc, cache, fonts, k);
                slices.push(Slice { block: k, lines: 0..l.lines.len(), y: yy, x: x + pad, w: (w - 2 * pad).max(crate::LU) });
                yy += l.height + if k < last { l.space_after } else { 0 };
            }
            row_h = row_h.max(yy + pad - y);
            boxes.push(CellBox { x, y, w, h: 0 });
        }
        for c in &mut boxes {
            c.h = row_h;
        }
        (slices, boxes, row_h, row.end)
    }

    /// Height that must fit for block `b` to start here, when it keeps with the
    /// next or keeps together; None when it needn't move.
    fn keep_need(&self, doc: &TextDoc, cache: &mut LayoutCache, fonts: &dyn Fonts, b: usize) -> Option<i64> {
        let n = doc.blocks.len();
        let mut need = 0;
        let mut i = b;
        loop {
            if i > b && doc.cell(i).is_some() {
                // Keeps don't reach into a table.
                return if i == b + 1 { None } else { Some(need) };
            }
            let l = self.layout(doc, cache, fonts, i);
            let f = &l.format;
            let kwn = f.keep_with_next && i + 1 < n;
            if i == b && !kwn && !f.keep_together {
                return None;
            }
            if i > b {
                need += l.space_before;
            }
            if kwn && i - b < MAX_CHAIN {
                need += l.height + l.space_after;
                i += 1;
                continue;
            }
            if i == b || f.keep_together {
                need += l.height;
            } else {
                let first = if f.widows { 2 } else { 1 };
                need += l.lines.iter().take(first).map(|x| x.height).sum::<i64>();
            }
            return Some(need);
        }
    }

    /// Footnotes anchored in block `b` (in `line` of it, or anywhere).
    fn footnotes_in(&self, doc: &TextDoc, b: usize, line: Option<&crate::layout::Line>) -> Vec<NoteId> {
        let Some(p) = doc.para(b) else { return Vec::new() };
        let (s, e) = line.map_or((0, p.text.len()), |l| (l.start as usize, (l.end as usize).min(p.text.len())));
        let Some(seg) = p.text.get(s..e) else { return Vec::new() };
        if !seg.contains(OBJECT_CHAR) {
            return Vec::new();
        }
        let numbers = doc.note_numbers();
        let mut v = Vec::new();
        for (i, ch) in seg.char_indices() {
            if ch != OBJECT_CHAR {
                continue;
            }
            if let Some(id) = doc.attrs.get(p.attr_at((s + i) as u32)).note {
                if numbers.get(&id).is_some_and(|x| x.0 == NoteKind::Foot) && !v.contains(&id) {
                    v.push(id);
                }
            }
        }
        v
    }

    /// The layout of paragraph `k` of note `id`.
    pub fn note_layout(&self, doc: &TextDoc, cache: &mut LayoutCache, fonts: &dyn Fonts, id: NoteId, k: usize) -> Option<Rc<ParaLayout>> {
        let p = doc.notes.get(id)?.paras.get(k)?;
        Some(cache.get(doc, p, (self.width - NOTE_HANG).max(crate::LU), fonts))
    }

    /// Height of note `id`'s text.
    fn note_height(&self, doc: &TextDoc, cache: &mut LayoutCache, fonts: &dyn Fonts, id: NoteId) -> i64 {
        let k = doc.notes.get(id).map_or(0, |n| n.paras.len());
        (0..k).filter_map(|i| self.note_layout(doc, cache, fonts, id, i)).map(|l| l.height).sum()
    }

    /// Height notes `ids` add to a page's foot (with the rule if they are the first).
    fn notes_height(&self, doc: &TextDoc, cache: &mut LayoutCache, fonts: &dyn Fonts, ids: &[NoteId], first: bool) -> i64 {
        if ids.is_empty() {
            return 0;
        }
        ids.iter().map(|&id| self.note_height(doc, cache, fonts, id)).sum::<i64>() + if first { NOTE_RULE_SPACE } else { 0 }
    }

    /// Puts footnotes `foot` (taking `foot_h`) at the foot of `page`; lines
    /// past the foot of a page they overfill are cut.
    fn place_footnotes(&self, doc: &TextDoc, cache: &mut LayoutCache, fonts: &dyn Fonts, page: &mut Page, foot: &[NoteId], foot_h: i64) {
        if foot.is_empty() {
            return;
        }
        let h = self.height;
        let top = (h - foot_h).max(page.height);
        page.foot_rule = Some(top);
        let mut y = top + NOTE_RULE_SPACE;
        for &id in foot {
            let k = doc.notes.get(id).map_or(0, |n| n.paras.len());
            for para in 0..k {
                let Some(l) = self.note_layout(doc, cache, fonts, id, para) else { continue };
                let mut n = 0;
                let mut yy = y;
                while n < l.lines.len() && yy + l.lines[n].height <= h {
                    yy += l.lines[n].height;
                    n += 1;
                }
                if n > 0 {
                    page.notes.push(NoteSlice { note: id, para, lines: 0..n, y });
                }
                y += l.height;
            }
        }
    }

    /// Every endnote line in order: (note, paragraph, line, height).
    fn endnote_lines(&self, doc: &TextDoc, cache: &mut LayoutCache, fonts: &dyn Fonts) -> Vec<(NoteId, usize, usize, i64)> {
        let mut v = Vec::new();
        if doc.notes.is_empty() {
            return v;
        }
        for id in doc.endnotes() {
            let k = doc.notes.get(id).map_or(0, |n| n.paras.len());
            for para in 0..k {
                if let Some(l) = self.note_layout(doc, cache, fonts, id, para) {
                    for (li, x) in l.lines.iter().enumerate() {
                        v.push((id, para, li, x.height));
                    }
                }
            }
        }
        v
    }

    /// Puts endnote lines from `from` on `page` below its text, in height `h`;
    /// where the next page starts, if lines are left.
    fn place_endnotes(&self, doc: &TextDoc, cache: &mut LayoutCache, fonts: &dyn Fonts, page: &mut Page, from: usize, h: i64) -> Option<Checkpoint> {
        let lines = self.endnote_lines(doc, cache, fonts);
        if from >= lines.len() {
            return None;
        }
        let empty = page.slices.is_empty();
        let mut y = page.height;
        if from == 0 {
            if !empty && y + NOTE_RULE_SPACE + lines[0].3 > h {
                return Some(Checkpoint { block: doc.blocks.len(), line: 0 });
            }
            page.end_rule = Some(y);
            y += NOTE_RULE_SPACE;
        }
        let mut k = from;
        while k < lines.len() && (y + lines[k].3 <= h || (k == from && empty)) {
            let (id, para, li, lh) = lines[k];
            match page.notes.last_mut() {
                Some(s) if s.note == id && s.para == para && s.lines.end == li => s.lines.end = li + 1,
                _ => page.notes.push(NoteSlice { note: id, para, lines: li..li + 1, y }),
            }
            y += lh;
            k += 1;
        }
        page.height = y;
        (k < lines.len()).then_some(Checkpoint { block: doc.blocks.len(), line: k })
    }

    /// Lays out the page starting at `start`; returns it and where the next starts.
    fn page(&self, doc: &TextDoc, cache: &mut LayoutCache, fonts: &dyn Fonts, start: Checkpoint) -> (Page, Option<Checkpoint>) {
        let full_h = self.height;
        let n = doc.blocks.len();
        let mut slices: Vec<Slice> = Vec::new();
        let mut y = 0i64;
        let (mut b, mut line) = (start.block, start.line);
        let mut cells: Vec<CellBox> = Vec::new();
        // Footnotes anchored on this page, and the height their area takes.
        let mut foot: Vec<NoteId> = Vec::new();
        let mut foot_h = 0i64;
        let has_notes = !doc.notes.is_empty();
        let done = |slices: Vec<Slice>, cells: Vec<CellBox>, y: i64, next: Option<Checkpoint>, foot: Vec<NoteId>, foot_h: i64, cache: &mut LayoutCache| {
            let mut page = Page { start, slices, cells, height: y, notes: Vec::new(), foot_rule: None, end_rule: None };
            self.place_footnotes(doc, cache, fonts, &mut page, &foot, foot_h);
            (page, next)
        };
        loop {
            let h = full_h - foot_h;
            if b >= n {
                // The text has ended: endnotes, if any, follow it.
                let mut page = Page { start, slices, cells, height: y, notes: Vec::new(), foot_rule: None, end_rule: None };
                let next = self.place_endnotes(doc, cache, fonts, &mut page, line, h);
                self.place_footnotes(doc, cache, fonts, &mut page, &foot, foot_h);
                return (page, next);
            }
            if doc.cell(b).is_some() {
                // A table row: whole, on this page if it fits (with its notes).
                let (s, c, h_row, next) = self.row(doc, cache, fonts, b, y);
                let new_notes: Vec<NoteId> = if has_notes { (b..next).flat_map(|k| self.footnotes_in(doc, k, None)).filter(|id| !foot.contains(id)).collect() } else { Vec::new() };
                let add = self.notes_height(doc, cache, fonts, &new_notes, foot.is_empty());
                if !slices.is_empty() && y + h_row + add > h {
                    return done(slices, cells, y, Some(Checkpoint { block: b, line: 0 }), foot, foot_h, cache);
                }
                foot.extend(new_notes);
                foot_h += add;
                slices.extend(s);
                cells.extend(c);
                y += h_row;
                b = next;
                line = 0;
                if doc.cell(b).is_none() && b < n {
                    // Space between the table and what follows.
                    y += twips(120);
                }
                continue;
            }
            let l = self.layout(doc, cache, fonts, b);
            let top = slices.is_empty();
            if line == 0 && !top {
                if l.format.page_break_before {
                    return done(slices, cells, y, Some(Checkpoint { block: b, line: 0 }), foot, foot_h, cache);
                }
                if let Some(need) = self.keep_need(doc, cache, fonts, b) {
                    if y + l.space_before + need > h && need <= h {
                        return done(slices, cells, y, Some(Checkpoint { block: b, line: 0 }), foot, foot_h, cache);
                    }
                }
            }
            if line == 0 && !top {
                y += l.space_before;
            }
            let total = l.lines.len();
            let l0 = line;
            let y0 = y;
            let mut li = line;
            let mut yy = y;
            let notes_here = has_notes && doc.para(b).is_some_and(|p| p.text.contains(OBJECT_CHAR));
            let mut room = h;
            while li < total {
                if notes_here {
                    let new_notes: Vec<NoteId> = self.footnotes_in(doc, b, Some(&l.lines[li])).into_iter().filter(|id| !foot.contains(id)).collect();
                    let add = self.notes_height(doc, cache, fonts, &new_notes, foot.is_empty());
                    // Alone on an empty page, a line goes with its notes even if they don't fit.
                    let alone = slices.is_empty() && li == l0;
                    if yy + l.lines[li].height + add > room && !alone {
                        break;
                    }
                    foot.extend(new_notes);
                    foot_h += add;
                    room = full_h - foot_h;
                } else if yy + l.lines[li].height > room {
                    break;
                }
                yy += l.lines[li].height;
                li += 1;
            }
            if li == total {
                slices.push(Slice { block: b, lines: l0..li, y: y0, x: 0, w: self.width });
                y = yy + l.space_after;
                b += 1;
                line = 0;
                continue;
            }
            // The paragraph breaks at `li`.
            let mut cut = li;
            if l.format.widows && total >= 2 {
                if total - cut < 2 {
                    cut = total - 2;
                }
                if l0 == 0 && cut == 1 {
                    cut = 0;
                }
                cut = cut.max(l0);
            }
            if cut == l0 {
                if !top {
                    // The paragraph starts on the next page, with its notes.
                    if notes_here {
                        let moved: Vec<NoteId> = (l0..li).flat_map(|k| self.footnotes_in(doc, b, Some(&l.lines[k]))).collect();
                        foot.retain(|id| !moved.contains(id));
                        foot_h = if foot.is_empty() { 0 } else { self.notes_height(doc, cache, fonts, &foot, true) };
                    }
                    return done(slices, cells, y - if l0 == 0 { l.space_before } else { 0 }, Some(Checkpoint { block: b, line: l0 }), foot, foot_h, cache);
                }
                // Alone on an empty page it must break somewhere.
                cut = li.max(l0 + 1);
            }
            let used: i64 = l.lines[l0..cut].iter().map(|x| x.height).sum();
            slices.push(Slice { block: b, lines: l0..cut, y: y0, x: 0, w: self.width });
            // Notes of lines that moved to the next page go with them.
            if notes_here && cut < li {
                let moved: Vec<NoteId> = (cut..li).flat_map(|k| self.footnotes_in(doc, b, Some(&l.lines[k]))).collect();
                if !moved.is_empty() {
                    foot.retain(|id| !moved.contains(id));
                    foot_h = if foot.is_empty() { 0 } else { self.notes_height(doc, cache, fonts, &foot, true) };
                }
            }
            return done(slices, cells, y0 + used, Some(Checkpoint { block: b, line: cut }), foot, foot_h, cache);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::FixedFonts;
    use crate::model::{Block, Para, ParaProps};
    use crate::ops::TextOp;
    use crate::LU;
    use officina_core::history::Op;

    /// FixedFonts: 11 pt, line 12.65 pt = 809.6 → 809 LU.
    fn line_h() -> i64 {
        11 * LU * 115 / 100
    }

    /// A page `lines` lines tall, wide enough for 40 characters, no spacing.
    fn doc_with(paras: &[usize], lines: i64) -> TextDoc {
        let mut d = TextDoc::new();
        d.blocks.remove(0);
        for (i, &n) in paras.iter().enumerate() {
            let id = d.new_id();
            // n lines of 40 characters each.
            let text = vec!["x".repeat(39); n].join(" ");
            let mut p = Para::new(id, &text, 0);
            p.props.space_after = Some(0);
            let _ = i;
            d.blocks.insert(d.blocks.len(), Block::Para(p));
        }
        let lh_twips = line_h() * 20 / LU + 1;
        d.page = crate::model::PageSetup { width: (40 * 352 * 20 / LU + 1) as i32 + 200, height: (lh_twips * lines) as i32 + 200, margin_top: 100, margin_bottom: 100, margin_start: 100, margin_end: 100 };
        d
    }

    fn all(d: &TextDoc) -> (Paginator, LayoutCache) {
        let mut c = LayoutCache::default();
        let mut p = Paginator::new(d);
        assert!(p.run(d, &mut c, &FixedFonts, usize::MAX));
        (p, c)
    }

    fn shape(p: &Paginator) -> Vec<Vec<(usize, Range<usize>)>> {
        p.pages().iter().map(|pg| pg.slices.iter().map(|s| (s.block, s.lines.clone())).collect()).collect()
    }

    #[test]
    fn fills_pages_and_keeps_two_lines_together() {
        let d = doc_with(&[3, 3], 5);
        let (p, _) = all(&d);
        assert_eq!(p.width(), {
            let w = d.page.width - 200;
            twips(w as i64)
        });
        // 3 + 2 would leave one line (a widow); widow control moves two.
        assert_eq!(shape(&p), vec![vec![(0, 0..3)], vec![(1, 0..3)]]);
        let d = doc_with(&[4, 4], 5);
        let (p, _) = all(&d);
        // One line of the second would be an orphan, so it moves whole.
        assert_eq!(shape(&p), vec![vec![(0, 0..4)], vec![(1, 0..4)]]);
        let d = doc_with(&[12], 5);
        let (p, _) = all(&d);
        assert_eq!(shape(&p), vec![vec![(0, 0..5)], vec![(0, 5..10)], vec![(0, 10..12)]]);
        let d = doc_with(&[6], 5);
        let (p, _) = all(&d);
        assert_eq!(shape(&p), vec![vec![(0, 0..4)], vec![(0, 4..6)]], "the widow takes a line along");
    }

    #[test]
    fn breaks_keeps_and_spacing() {
        let mut d = doc_with(&[2, 1, 2], 6);
        d.blocks.update(1, |b| b.as_para_mut().unwrap().props.page_break_before = Some(true));
        let (p, _) = all(&d);
        assert_eq!(shape(&p), vec![vec![(0, 0..2)], vec![(1, 0..1), (2, 0..2)]]);
        // Keep with next: block 1 (a "heading") would end page 1 with block 2 on page 2.
        let mut d = doc_with(&[4, 1, 2], 6);
        d.blocks.update(1, |b| b.as_para_mut().unwrap().props.keep_with_next = Some(true));
        let (p, _) = all(&d);
        assert_eq!(shape(&p), vec![vec![(0, 0..4)], vec![(1, 0..1), (2, 0..2)]]);
        // Keep together.
        let mut d = doc_with(&[3, 4], 6);
        d.blocks.update(1, |b| b.as_para_mut().unwrap().props.keep_together = Some(true));
        let (p, _) = all(&d);
        assert_eq!(shape(&p), vec![vec![(0, 0..3)], vec![(1, 0..4)]]);
        // Space before is dropped at a page top.
        let mut d = doc_with(&[6, 2], 6);
        d.blocks.update(1, |b| b.as_para_mut().unwrap().props.space_before = Some(240));
        let (p, _) = all(&d);
        assert_eq!(p.pages()[1].slices[0].y, 0);
        let mut d = doc_with(&[2, 2], 6);
        d.blocks.update(1, |b| b.as_para_mut().unwrap().props = ParaProps { space_before: Some(240), space_after: Some(0), ..Default::default() });
        let (p, _) = all(&d);
        assert_eq!(p.pages()[0].slices[1].y, 2 * line_h() + twips(240));
    }

    #[test]
    fn empty_document_has_one_page() {
        let d = TextDoc::new();
        let (p, _) = all(&d);
        assert_eq!(p.pages().len(), 1);
        assert_eq!(p.pages()[0].slices.len(), 1);
    }

    /// Applies `op`, tells the paginator, repaginates, and checks the pages
    /// against a fresh pagination. Returns pages laid out.
    fn check_edit(d: &mut TextDoc, p: &mut Paginator, c: &mut LayoutCache, op: TextOp, e: Edit) -> u64 {
        d.take_changes();
        op.apply(d);
        if let Some(noted) = d.take_changes() {
            assert_eq!(noted, e, "the op notes the edit it made");
        }
        let before = p.laid;
        p.edit(d, c, &FixedFonts, e);
        assert!(p.run(d, c, &FixedFonts, usize::MAX));
        let (fresh, _) = all(d);
        if let Some(i) = (0..p.pages().len().max(fresh.pages().len())).find(|&i| p.pages().get(i) != fresh.pages().get(i)) {
            panic!("{e:?}: page {i} differs\n inc {:?}\n new {:?}\n prev {:?}", p.pages().get(i), fresh.pages().get(i), fresh.pages().get(i.wrapping_sub(1)));
        }
        p.laid - before
    }

    /// `doc_with`, then rows of `cols` one-line cells in place of blocks `at..`.
    fn with_table(paras: &[usize], lines: i64, at: usize, rows: usize, cols: usize) -> TextDoc {
        let mut d = doc_with(paras, lines);
        let w = (d.page.width - 200) as u32 / cols as u32;
        for r in 0..rows {
            for c in 0..cols {
                let id = d.new_id();
                let mut p = Para::new(id, &format!("r{r}c{c}"), 0);
                p.props.space_after = Some(0);
                p.props.cell = Some(crate::model::CellRef { table: 1, row: r as u32, col: c as u16, width: w });
                d.blocks.insert(at + r * cols + c, Block::Para(p));
            }
        }
        d
    }

    #[test]
    fn table_rows_sit_side_by_side_and_stay_on_one_page() {
        let d = with_table(&[1, 1], 5, 1, 4, 3);
        let (p, _) = all(&d);
        let pg = &p.pages()[0];
        // Block 0, then row 0's three cells at one y, left to right.
        let r0: Vec<&Slice> = pg.slices.iter().filter(|s| (1..4).contains(&s.block)).collect();
        assert_eq!(r0.len(), 3);
        assert!(r0.iter().all(|s| s.y == r0[0].y));
        assert!(r0[0].x < r0[1].x && r0[1].x < r0[2].x);
        assert_eq!(pg.cells.len() % 3, 0);
        // Each row is whole on its page.
        for pg in p.pages() {
            for s in &pg.slices {
                if d.cell(s.block).is_some() {
                    let row = d.row_range(s.block);
                    assert!(row.clone().all(|k| pg.slices.iter().any(|t| t.block == k)), "row {row:?} split");
                }
            }
        }
        assert!(p.pages().len() >= 2, "4 rows and 2 lines don't fit 5 lines");
    }

    #[test]
    fn an_edit_in_a_cell_repaginates_like_a_fresh_pagination() {
        let paras: Vec<usize> = (0..120).map(|i| 1 + (i * 7) % 5).collect();
        let mut d = with_table(&paras, 20, 30, 40, 3);
        let (mut p, mut c) = all(&d);
        // A cell grows a line: its row and the rows after move.
        check_edit(&mut d, &mut p, &mut c, TextOp::Splice { block: 40, at: 0, del: 0, ins: crate::ops::Fragment { text: "y".repeat(30), runs: vec![(0, 0)] } }, Edit { first: 40, last: 41, delta: 0 });
        let id = d.new_id();
        let props = d.para(41).unwrap().props.clone();
        check_edit(&mut d, &mut p, &mut c, TextOp::Split { block: 41, at: 1, id, style: 0, props }, Edit { first: 41, last: 43, delta: 1 });
    }

    #[test]
    fn an_edit_before_pagination_finishes_still_reaches_the_end() {
        let paras: Vec<usize> = (0..400).map(|i| 1 + (i * 7) % 5).collect();
        let mut d = doc_with(&paras, 20);
        let mut p = Paginator::new(&d);
        let mut c = LayoutCache::default();
        p.run(&d, &mut c, &FixedFonts, 20);
        assert!(!p.done());
        let edge = p.pages().last().unwrap().start.block;
        // Behind the cursor: converges onto the old pages, then goes on.
        check_edit(&mut d, &mut p, &mut c, TextOp::Splice { block: 10, at: 0, del: 1, ins: Default::default() }, Edit { first: 10, last: 11, delta: 0 });
        // Across the cursor.
        let mut p = Paginator::new(&d);
        p.run(&d, &mut c, &FixedFonts, 20);
        let id = d.new_id();
        check_edit(&mut d, &mut p, &mut c, TextOp::Split { block: edge as u32, at: 1, id, style: 0, props: Default::default() }, Edit { first: edge, last: edge + 2, delta: 1 });
    }

    #[test]
    fn edits_repaginate_until_pages_line_up() {
        let paras: Vec<usize> = (0..400).map(|i| 1 + (i * 7) % 5).collect();
        let mut d = doc_with(&paras, 20);
        let (mut p, mut c) = all(&d);
        let pages = p.pages().len();
        assert!(pages > 50);
        // Deleting a character in block 200 changes no breaks.
        let id = d.para(200).unwrap().id;
        let _ = id;
        let n = check_edit(&mut d, &mut p, &mut c, TextOp::Splice { block: 200, at: 0, del: 1, ins: Default::default() }, Edit { first: 200, last: 201, delta: 0 });
        assert!(n <= 2, "{n} pages");
        // Adding a line to block 100 pushes breaks until a paragraph soaks it up.
        let n = check_edit(&mut d, &mut p, &mut c, TextOp::Splice { block: (100) as u32, at: 0, del: 0, ins: crate::ops::Fragment { text: "y".repeat(40), runs: vec![(0, 0)] } }, Edit { first: 100, last: 101, delta: 0 });
        assert!(n < pages as u64, "{n} of {pages}");
        // Splitting and joining shift block numbers.
        let id = d.new_id();
        let n = check_edit(&mut d, &mut p, &mut c, TextOp::Split { block: (50) as u32, at: 40, id, style: 0, props: Default::default() }, Edit { first: 50, last: 52, delta: 1 });
        assert!(n < pages as u64);
        check_edit(&mut d, &mut p, &mut c, TextOp::Join { block: (50 ) as u32}, Edit { first: 50, last: 51, delta: -1 });
        // Removing blocks at the end.
        let len = d.blocks.len();
        check_edit(&mut d, &mut p, &mut c, TextOp::Remove { at: (len - 3) as u32, n: 3u32 }, Edit { first: len - 3, last: len - 3, delta: -3 });
        // A keep-with-next block before the edit.
        d.blocks.update(299, |b| b.as_para_mut().unwrap().props.keep_with_next = Some(true));
        p.reset(&d);
        p.run(&d, &mut c, &FixedFonts, usize::MAX);
        check_edit(&mut d, &mut p, &mut c, TextOp::Splice { block: (300) as u32, at: 0, del: 0, ins: crate::ops::Fragment { text: "z".repeat(80), runs: vec![(0, 0)] } }, Edit { first: 300, last: 301, delta: 0 });
    }

    #[test]
    fn random_edits_match_full_pagination() {
        let paras: Vec<usize> = (0..120).map(|i| 1 + (i * 5) % 7).collect();
        let mut d = doc_with(&paras, 9);
        let (mut p, mut c) = all(&d);
        let mut x: u64 = 0x9e37_79b9;
        let mut rnd = |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n.max(1)
        };
        for round in 0..300 {
            let len = d.blocks.len();
            let b = rnd(len as u64) as usize;
            let tl = d.para(b).unwrap().text.len();
            match rnd(7) {
                0 | 1 => {
                    let k = rnd(90) as usize;
                    let ins = crate::ops::Fragment { text: "w ".repeat(k / 2) + "v", runs: vec![(0, 0)] };
                    check_edit(&mut d, &mut p, &mut c, TextOp::Splice { block: (b) as u32, at: 0, del: 0, ins }, Edit { first: b, last: b + 1, delta: 0 });
                }
                2 => {
                    let del = (rnd(60) as u32).min(tl as u32);
                    check_edit(&mut d, &mut p, &mut c, TextOp::Splice { block: (b) as u32, at: 0, del, ins: Default::default() }, Edit { first: b, last: b + 1, delta: 0 });
                }
                3 => {
                    let at = (tl as u32 / 2) & !0;
                    let at = if d.para(b).unwrap().text.is_char_boundary(at as usize) { at } else { 0 };
                    let id = d.new_id();
                    check_edit(&mut d, &mut p, &mut c, TextOp::Split { block: (b) as u32, at, id, style: 0, props: Default::default() }, Edit { first: b, last: b + 2, delta: 1 });
                }
                6 if b + 1 < len => {
                    // A selection across paragraphs deleted as one action.
                    let b1 = (b + 1 + rnd(4) as usize).min(len - 1);
                    let o0 = 0;
                    let o1 = (d.para(b1).unwrap().text.len() as u32).min(5);
                    d.take_changes();
                    for op in d.delete_ops(b, o0, b1, o1) {
                        op.apply(&mut d);
                    }
                    let e = d.take_changes().unwrap();
                    p.edit(&d, &mut c, &FixedFonts, e);
                    assert!(p.run(&d, &mut c, &FixedFonts, usize::MAX));
                    let (fresh, _) = all(&d);
                    assert_eq!(p.pages(), fresh.pages(), "{e:?}");
                }
                4 if b + 1 < len => {
                    check_edit(&mut d, &mut p, &mut c, TextOp::Join { block: (b ) as u32}, Edit { first: b, last: b + 1, delta: -1 });
                }
                _ => {
                    let flag = rnd(3);
                    let v = rnd(2) == 0;
                    let mut props = d.para(b).unwrap().props.clone();
                    match flag {
                        0 => props.keep_with_next = Some(v),
                        1 => props.keep_together = Some(v),
                        _ => props.page_break_before = Some(v),
                    }
                    let style = d.para(b).unwrap().style; check_edit(&mut d, &mut p, &mut c, TextOp::SetPara { block: b as u32, style, props }, Edit { first: b, last: b + 1, delta: 0 });
                }
            }
            let _ = round;
        }
    }

    /// Footnotes on the anchor's page, endnotes after the text, labels in
    /// order, and repagination after random edits (anchors in, out, note text
    /// changed) the same as from scratch.
    #[test]
    fn notes_go_with_their_anchors() {
        use crate::model::{NoteKind, OBJECT_CHAR};
        let paras: Vec<usize> = (0..40).map(|i| 1 + (i * 3) % 5).collect();
        let mut d = doc_with(&paras, 12);
        let notes_of = |d: &TextDoc, p: &Paginator| -> Vec<Vec<NoteId>> { p.pages().iter().map(|pg| pg.notes.iter().map(|n| n.note).collect()).collect() };
        // A footnote in block 5 and block 30, an endnote in block 10.
        for (b, kind, text) in [(5usize, NoteKind::Foot, "first note"), (30, NoteKind::Foot, "second note\nwith two lines"), (10, NoteKind::End, "an endnote")] {
            for op in d.insert_note_ops(b, 0, kind, text, 0) {
                op.apply(&mut d);
            }
        }
        d.take_changes();
        let (mut p, mut c) = all(&d);
        assert_eq!(d.note_label(0), "1");
        assert_eq!(d.note_label(1), "2");
        assert_eq!(d.note_label(2), "i");
        let pg5 = p.page_of(5, 0);
        assert!(p.pages()[pg5].notes.iter().any(|n| n.note == 0), "{:?}", notes_of(&d, &p));
        assert!(p.pages()[pg5].foot_rule.is_some());
        let pg30 = p.page_of(30, 0);
        assert_eq!(p.pages()[pg30].notes.iter().filter(|n| n.note == 1).count(), 2, "both paragraphs");
        let last = p.pages().last().unwrap();
        assert!(last.notes.iter().any(|n| n.note == 2) && last.end_rule.is_some(), "the endnote ends the document");
        // The text and its notes fit the page.
        for pg in p.pages() {
            let body = pg.slices.iter().map(|s| s.y).max().unwrap_or(0);
            if let Some(r) = pg.foot_rule {
                assert!(body < r);
            }
        }
        // A footnote added before the first renumbers it.
        for op in d.insert_note_ops(1, 0, NoteKind::Foot, "earlier", 0) {
            op.apply(&mut d);
        }
        let e = d.take_changes().unwrap();
        p.edit(&d, &mut c, &FixedFonts, e);
        assert!(p.run(&d, &mut c, &FixedFonts, usize::MAX));
        assert_eq!((d.note_label(3), d.note_label(0)), ("1".into(), "2".into()));
        assert_eq!(p.pages(), all(&d).0.pages());

        let mut x: u64 = 0x1234_5678;
        let mut rnd = |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n.max(1)
        };
        for _ in 0..200 {
            let len = d.blocks.len();
            let b = rnd(len as u64) as usize;
            let ops = match rnd(5) {
                0 => {
                    let kind = if rnd(4) == 0 { NoteKind::End } else { NoteKind::Foot };
                    let lines = "note text ".repeat(rnd(12) as usize + 1);
                    d.insert_note_ops(b, 0, kind, &lines, 0)
                }
                1 => {
                    // Take out a note's anchor.
                    match d.para(b).unwrap().text.find(OBJECT_CHAR) {
                        Some(at) => vec![TextOp::Splice { block: b as u32, at: at as u32, del: OBJECT_CHAR.len_utf8() as u32, ins: Default::default() }],
                        None => Vec::new(),
                    }
                }
                2 => {
                    let ids: Vec<NoteId> = d.note_numbers().keys().copied().collect();
                    if ids.is_empty() {
                        Vec::new()
                    } else {
                        let id = ids[rnd(ids.len() as u64) as usize];
                        let t = "changed ".repeat(rnd(30) as usize + 1);
                        d.set_note_text_ops(id, &t)
                    }
                }
                3 => vec![TextOp::Splice { block: b as u32, at: 0, del: 0, ins: crate::ops::Fragment { text: "w ".repeat(rnd(40) as usize) + "v", runs: vec![(0, 0)] } }],
                _ => {
                    let tl = d.para(b).unwrap().text.len() as u32;
                    let del = (rnd(30) as u32).min(tl);
                    if d.para(b).unwrap().text.is_char_boundary(del as usize) {
                        vec![TextOp::Splice { block: b as u32, at: 0, del, ins: Default::default() }]
                    } else {
                        Vec::new()
                    }
                }
            };
            d.take_changes();
            for op in ops {
                op.apply(&mut d);
            }
            if let Some(e) = d.take_changes() {
                p.edit(&d, &mut c, &FixedFonts, e);
            }
            assert!(p.run(&d, &mut c, &FixedFonts, usize::MAX));
            let fresh = all(&d).0;
            assert_eq!(p.pages(), fresh.pages());
            // Every anchored footnote is on its anchor's page.
            for (&id, &(k, _)) in d.note_numbers().iter() {
                if k != NoteKind::Foot {
                    continue;
                }
                let (ab, at) = d.anchor_of(id).unwrap();
                let l = c.get(&d, d.para(ab).unwrap(), p.width(), &FixedFonts);
                let pg = p.page_of(ab, l.line_of(at));
                assert!(p.pages()[pg].notes.iter().any(|n| n.note == id), "note {id} not on page {pg}");
            }
        }
    }
}
