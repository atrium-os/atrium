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

use crate::layout::{object_size, Band, Fonts, LayoutCache, ParaLayout};
use crate::model::{BlockId, Float, HAlign, NoteId, NoteKind, ObjectRef, Rel, Section, SectionStart, TextDoc, Wrap, OBJECT_CHAR};
use std::collections::HashMap;
use crate::twips;
use std::rc::Rc;

/// Where a page starts: a block and a line in it. (Floats, footnotes and
/// counters waiting for a place join this in O3d.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Checkpoint {
    pub block: usize,
    pub line: usize,
    /// Mid-paragraph after floats on the page before: a hash of the room
    /// they left (its lines were broken around them); 0 otherwise.
    pub wrap: u64,
}

impl Checkpoint {
    pub fn at(block: usize, line: usize) -> Checkpoint {
        Checkpoint { block, line, wrap: 0 }
    }
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
    /// The room floating pictures left the paragraph (its lines were broken
    /// around them); none for most.
    pub wrap: Option<Rc<Vec<Band>>>,
}

/// A floating picture placed on a page: its anchor (block, byte), its frame
/// from the content box's top left (layout units), how text goes round it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacedFloat {
    pub block: usize,
    pub at: u32,
    pub x: i64,
    pub y: i64,
    pub w: i64,
    pub h: i64,
    pub object: ObjectRef,
}

impl PlacedFloat {
    pub fn wrap(&self) -> Wrap {
        self.object.float.map_or(Wrap::InFront, |f| f.wrap)
    }
}

/// A hash of the room floats left, for checkpoints (never 0).
fn wrap_hash(bands: &[Band]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bands.hash(&mut h);
    h.finish() | 1
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
    /// Lines between columns: (x, top, bottom).
    pub rules: Vec<(i64, i64, i64)>,
    /// Floating pictures anchored on the page.
    pub floats: Vec<PlacedFloat>,
}

impl Page {
    fn shift(&mut self, delta: isize) {
        self.start.block = (self.start.block as isize + delta) as usize;
        for s in &mut self.slices {
            s.block = (s.block as isize + delta) as usize;
        }
        for f in &mut self.floats {
            f.block = (f.block as isize + delta) as usize;
        }
    }
}

pub use crate::model::Edit;

/// Where sections start in `doc`: (block, settings), in order (marks in
/// table cells don't count).
fn sections_of(doc: &TextDoc) -> Vec<(usize, Section)> {
    doc.blocks.iter().enumerate().filter_map(|(i, b)| b.as_para().and_then(|p| p.props.section.filter(|_| p.props.cell.is_none()).map(|s| (i, s)))).collect()
}

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
    /// The content width in twips (columns are fitted to it).
    width_tw: i32,
    /// Where sections start: (block, settings), in order.
    sections: Vec<(usize, Section)>,
    /// Pages laid out since creation (for the gates).
    pub laid: u64,
    /// The room floats left paragraphs whose lines were broken around them,
    /// by paragraph (as their first page placed them).
    wraps: HashMap<BlockId, Rc<Vec<Band>>>,
    page_w: i64,
    page_h: i64,
    margin_start: i64,
    margin_top: i64,
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
        self.width_tw = (g.width - g.margin_start - g.margin_end).max(720);
        (self.page_w, self.page_h, self.margin_start, self.margin_top) = (twips(g.width as i64), twips(g.height as i64), twips(g.margin_start as i64), twips(g.margin_top as i64));
        self.wraps.clear();
        self.sections = sections_of(doc);
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

    /// The room floats left paragraph `id`, if its lines go round any.
    pub fn wrap_of(&self, id: BlockId) -> Option<Rc<Vec<Band>>> {
        self.wraps.get(&id).cloned()
    }

    /// The settings of the section block `b` is in.
    pub fn section_at(&self, b: usize) -> Section {
        let k = self.sections.partition_point(|s| s.0 <= b);
        if k == 0 {
            Section::default()
        } else {
            self.sections[k - 1].1
        }
    }

    /// Whether block `b` starts a section.
    fn starts_section(&self, b: usize) -> bool {
        self.sections.binary_search_by_key(&b, |s| s.0).is_ok_and(|_| b > 0)
    }

    /// A section's columns as laid out: count, column width and gap (layout units).
    fn columns(&self, s: &Section) -> (u8, i64, i64) {
        let (n, gap) = s.fitted(self.width_tw);
        let gap = twips(gap as i64);
        (n, ((self.width - gap * (n as i64 - 1)) / n as i64).max(crate::LU), gap)
    }

    /// The width of the column block `b` is laid out in (layout units).
    pub fn col_width(&self, b: usize) -> i64 {
        self.columns(&self.section_at(b)).1
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
        let at = Checkpoint::at(block, line);
        self.pages.partition_point(|p| p.start <= at).saturating_sub(1)
    }

    /// Records an edit: drops the pages from the one the edit can first change,
    /// keeping the rest to compare against.
    pub fn edit(&mut self, doc: &TextDoc, cache: &mut LayoutCache, fonts: &dyn Fonts, e: Edit) {
        // Sections changed: pages can't line up again before the end of the
        // last section whose start or settings changed.
        let section_change = {
            let new = sections_of(doc);
            let old_last = e.last as isize - e.delta;
            // Old starts in new numbering (one inside the edit stays where it was).
            let moved: Vec<(usize, Section)> = self.sections.iter().filter(|s| (s.0 as isize) < old_last.max(e.last as isize)).map(|&(b, x)| (if b as isize >= old_last { (b as isize + e.delta) as usize } else { b }, x)).chain(self.sections.iter().filter(|s| s.0 as isize >= old_last.max(e.last as isize)).map(|&(b, x)| ((b as isize + e.delta) as usize, x))).collect();
            let changed = moved.iter().filter(|x| !new.contains(x)).chain(new.iter().filter(|x| !moved.contains(x))).map(|x| x.0).max();
            self.sections = new;
            match changed {
                Some(at) => self.sections.iter().map(|s| s.0).find(|&b| b > at).unwrap_or(doc.blocks.len() + 1),
                None => 0,
            }
        };
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
        if k > 0 && self.pages.get(k).is_some_and(|p| p.start == Checkpoint::at(c, 0)) {
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
        self.settle = e.last.max(section_change);
        self.delta = e.delta;
        self.resume = self.cursor.map(|c| Checkpoint { block: (c.block as isize + e.delta) as usize, ..c });
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
        for sl in &page.slices {
            let Some(id) = doc.blocks.get(sl.block).map(|b| b.id()) else { continue };
            match &sl.wrap {
                Some(w) => {
                    self.wraps.insert(id, w.clone());
                }
                None if sl.lines.start == 0 => {
                    self.wraps.remove(&id);
                }
                None => {}
            }
        }
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
        let want = Checkpoint { block: (n.block as isize - self.delta) as usize, ..n };
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
        cache.get(doc, p, doc.block_box(i, self.col_width(i)).1, fonts)
    }

    /// Lays out the table row starting at block `b` at `y`; returns the slices,
    /// the cell boxes, the row's height and the block after it.
    #[allow(clippy::too_many_arguments)]
    fn row(&self, doc: &TextDoc, cache: &mut LayoutCache, fonts: &dyn Fonts, b: usize, y: i64, x0: i64, col_w: i64) -> (Vec<Slice>, Vec<CellBox>, i64, usize, Vec<PlacedFloat>) {
        let row = doc.row_range(b);
        let row = b..row.end.max(b + 1);
        let pad = twips(crate::model::CELL_PAD as i64);
        let mut slices = Vec::new();
        let mut boxes = Vec::new();
        let mut floats = Fill::default();
        let mut row_h = 0;
        for (blocks, x, w) in doc.row_cells(row.clone(), col_w) {
            let x = x + x0;
            let mut yy = y + pad;
            let last = blocks.end - 1;
            for k in blocks {
                let l = self.layout(doc, cache, fonts, k);
                // Floats in a cell are placed from it, but its text doesn't go round them.
                self.anchor_floats(doc, &mut floats, k, x + pad, (w - 2 * pad).max(crate::LU), yy);
                slices.push(Slice { block: k, lines: 0..l.lines.len(), y: yy, x: x + pad, w: (w - 2 * pad).max(crate::LU), wrap: None });
                yy += l.height + if k < last { l.space_after } else { 0 };
            }
            row_h = row_h.max(yy + pad - y);
            boxes.push(CellBox { x, y, w, h: 0 });
        }
        // A cell grows to hold the pictures its text wraps round (Word lays
        // them out in the cell); ones in front of or behind the text don't.
        for p in &floats.floats {
            if p.object.float.is_some_and(|fl| !matches!(fl.wrap, Wrap::InFront | Wrap::Behind)) && p.y >= y {
                row_h = row_h.max(p.y + p.h + pad - y);
            }
        }
        for c in &mut boxes {
            c.h = row_h;
        }
        (slices, boxes, row_h, row.end, floats.floats)
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
                return Some(Checkpoint::at(doc.blocks.len(), 0));
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
        (k < lines.len()).then_some(Checkpoint::at(doc.blocks.len(), k))
    }

    /// Lays out the page starting at `start`; returns it and where the next starts.
    ///
    /// The page is filled in bands: each band is one section's columns, filled
    /// one after another. A section that starts on the same page (continuous)
    /// ends the band above it, whose columns are balanced first.
    fn page(&self, doc: &TextDoc, cache: &mut LayoutCache, fonts: &dyn Fonts, start: Checkpoint) -> (Page, Option<Checkpoint>) {
        let full_h = self.height;
        let mut f = Fill::default();
        if start.wrap != 0 {
            // A paragraph going on from the page before, broken around its floats.
            if let Some(w) = doc.blocks.get(start.block).and_then(|b| self.wraps.get(&b.id())).filter(|w| wrap_hash(w) == start.wrap) {
                f.carry = Some((start.block, w.clone()));
            }
        }
        let mut top = 0i64;
        let mut cur = start;
        let mut rules: Vec<(i64, i64, i64)> = Vec::new();
        let finish = |f: Fill, y: i64, rules: Vec<(i64, i64, i64)>, next: Option<Checkpoint>, cache: &mut LayoutCache| {
            let mut page = Page { start, slices: f.slices, cells: f.cells, height: y, notes: Vec::new(), foot_rule: None, end_rule: None, rules, floats: f.floats };
            self.place_footnotes(doc, cache, fonts, &mut page, &f.foot, f.foot_h);
            (page, next)
        };
        loop {
            let sec = self.section_at(cur.block);
            let (n, colw, gap) = self.columns(&sec);
            let snapshot = (n > 1).then(|| f.clone());
            let (stop, bottom) = self.band(doc, cache, fonts, &mut f, cur, top, full_h, n, colw, gap);
            let mut bottom = bottom;
            if let (Stop::Section(b2), Some(snap)) = (stop, &snapshot) {
                if self.section_at(b2).start == SectionStart::Continuous && bottom > top {
                    // Balance: the least height the band fits in.
                    let fits = |h: i64, cache: &mut LayoutCache| {
                        let mut g = snap.clone();
                        let (st, bt) = self.band(doc, cache, fonts, &mut g, cur, top, top + h, n, colw, gap);
                        (matches!(st, Stop::Section(x) if x == b2), g, bt)
                    };
                    let (mut lo, mut hi) = ((bottom - top) / n as i64, bottom - top);
                    while hi - lo > crate::LU {
                        let mid = (lo + hi) / 2;
                        if fits(mid, cache).0 {
                            hi = mid;
                        } else {
                            lo = mid;
                        }
                    }
                    let (ok, g, bt) = fits(hi, cache);
                    if ok {
                        f = g;
                        bottom = bt;
                    }
                }
            }
            if sec.rule && n > 1 && bottom > top {
                for c in 1..n as i64 {
                    rules.push((c * (colw + gap) - gap / 2, top, bottom));
                }
            }
            match stop {
                Stop::Section(b2) if self.section_at(b2).start == SectionStart::Continuous && bottom < full_h - f.foot_h => {
                    // The next section goes on under this one.
                    top = bottom;
                    cur = Checkpoint::at(b2, 0);
                }
                Stop::Section(b2) | Stop::Break(b2) => return finish(f, bottom, rules, Some(Checkpoint::at(b2, 0)), cache),
                Stop::Full(next) => return finish(f, bottom, rules, Some(next), cache),
                Stop::End(line) => {
                    // The text has ended: endnotes, if any, follow it.
                    let mut page = Page { start, slices: f.slices, cells: f.cells, height: bottom, notes: Vec::new(), foot_rule: None, end_rule: None, rules, floats: f.floats };
                    let next = self.place_endnotes(doc, cache, fonts, &mut page, line, full_h - f.foot_h);
                    self.place_footnotes(doc, cache, fonts, &mut page, &f.foot, f.foot_h);
                    return (page, next);
                }
            }
        }
    }

    /// Places the floating pictures paragraph `b` anchors, its first line's
    /// top at `top` in the column at `x`, `w` wide.
    #[allow(clippy::too_many_arguments)]
    fn anchor_floats(&self, doc: &TextDoc, f: &mut Fill, b: usize, x: i64, w: i64, top: i64) {
        let Some(p) = doc.para(b) else { return };
        if !p.text.contains(OBJECT_CHAR) {
            return;
        }
        for (s, e, a) in p.spans() {
            let at = doc.attrs.get(a);
            let Some((o, fl)) = at.object.and_then(|o| o.float.map(|fl| (o, fl))).filter(|_| at.deleted.is_none()) else { continue };
            for (i, ch) in p.text[s as usize..e as usize].char_indices() {
                if ch == OBJECT_CHAR {
                    let (fx, fy, fw, fh) = self.float_frame(&o, &fl, x, w, top);
                    f.floats.push(PlacedFloat { block: b, at: s + i as u32, x: fx, y: fy, w: fw, h: fh, object: o });
                }
            }
        }
    }

    /// A floating picture's frame (layout units, from the content box's top
    /// left), its paragraph's top at `top` in the column at `x`, `w` wide.
    /// It stays on the page: inside the content box when measured from the
    /// text or the margins, on the sheet when from the page.
    fn float_frame(&self, o: &ObjectRef, fl: &Float, x: i64, w: i64, top: i64) -> (i64, i64, i64, i64) {
        let (fw, fh) = object_size(o, self.width);
        let (r0, rw) = match fl.h_rel {
            Rel::Text => (x, w),
            Rel::Margin => (0, self.width),
            Rel::Page => (-self.margin_start, self.page_w),
        };
        let fx = match fl.h_align {
            HAlign::Start => r0,
            HAlign::Center => r0 + (rw - fw) / 2,
            HAlign::End => r0 + rw - fw,
            HAlign::Offset => r0 + twips(fl.x as i64),
        };
        let fx = fx.min(self.page_w - self.margin_start - fw).max(-self.margin_start);
        let (fy, lo, hi) = match fl.v_rel {
            Rel::Text => (top + twips(fl.y as i64), 0, self.height - fh),
            Rel::Margin => (twips(fl.y as i64), 0, self.height - fh),
            Rel::Page => (twips(fl.y as i64) - self.margin_top, -self.margin_top, self.page_h - self.margin_top - fh),
        };
        (fx, fy.min(hi).max(lo), fw, fh)
    }

    /// The room the page's floats leave a paragraph whose first line's top is
    /// at `top` in the column at `x`, `w` wide.
    fn bands(&self, f: &Fill, x: i64, w: i64, top: i64) -> Vec<Band> {
        let mut v = Vec::new();
        for p in &f.floats {
            let wrap = p.wrap();
            if !matches!(wrap, Wrap::Square | Wrap::TopBottom) {
                continue;
            }
            let gap = twips(p.object.float.map_or(0, |fl| fl.gap) as i64);
            let (fl, fr) = (p.x - gap, p.x + p.w + gap);
            let (bt, bb) = (p.y - gap - top, p.y + p.h + gap - top);
            if fr <= x || fl >= x + w || bb <= 0 {
                continue;
            }
            let (left, right) = if wrap == Wrap::TopBottom {
                (w, 0)
            } else if fl - x >= x + w - fr {
                // More room before it: the text goes there.
                (0, (x + w - fl).clamp(0, w))
            } else {
                ((fr - x).clamp(0, w), 0)
            };
            v.push(Band { top: bt, bottom: bb, left, right });
        }
        v
    }

    /// One band: `n` columns `colw` wide, `gap` apart, from `top` down to
    /// `limit` (less the footnotes). Returns why it stopped and its bottom.
    #[allow(clippy::too_many_arguments)]
    fn band(&self, doc: &TextDoc, cache: &mut LayoutCache, fonts: &dyn Fonts, f: &mut Fill, from: Checkpoint, top: i64, limit: i64, n: u8, colw: i64, gap: i64) -> (Stop, i64) {
        let mut cur = from;
        let mut bottom = top;
        for c in 0..n as i64 {
            let (stop, y) = self.column(doc, cache, fonts, f, cur, c * (colw + gap), colw, top, limit, from);
            bottom = bottom.max(y);
            match stop {
                Stop::Full(next) if c + 1 < n as i64 => cur = next,
                _ => return (stop, bottom),
            }
        }
        unreachable!("the last column returns")
    }

    /// Fills one column from `from`, at `x`, `w` wide, from `top` down to
    /// `limit` (less the footnotes); `band_start` is where the band began (a
    /// section starting there doesn't stop it). Returns why it stopped and
    /// how far down it got.
    #[allow(clippy::too_many_arguments)]
    fn column(&self, doc: &TextDoc, cache: &mut LayoutCache, fonts: &dyn Fonts, f: &mut Fill, from: Checkpoint, x: i64, w: i64, top: i64, limit: i64, band_start: Checkpoint) -> (Stop, i64) {
        let full_h = self.height;
        let n = doc.blocks.len();
        let mut y = top;
        let (mut b, mut line) = (from.block, from.line);
        let has_notes = !doc.notes.is_empty();
        let first_slice = f.slices.len();
        loop {
            let h = limit.min(full_h - f.foot_h);
            if b >= n {
                return (Stop::End(line), y);
            }
            let empty = f.slices.len() == first_slice;
            if line == 0 && b != band_start.block && self.starts_section(b) {
                return (Stop::Section(b), y);
            }
            if doc.cell(b).is_some() {
                // A table row: whole, in this column if it fits (with its notes).
                let (s, c, h_row, next, fl) = self.row(doc, cache, fonts, b, y, x, w);
                let new_notes: Vec<NoteId> = if has_notes { (b..next).flat_map(|k| self.footnotes_in(doc, k, None)).filter(|id| !f.foot.contains(id)).collect() } else { Vec::new() };
                let add = self.notes_height(doc, cache, fonts, &new_notes, f.foot.is_empty());
                if !empty && y + h_row + add > h {
                    return (Stop::Full(Checkpoint::at(b, 0)), y);
                }
                f.foot.extend(new_notes);
                f.foot_h += add;
                f.slices.extend(s);
                f.cells.extend(c);
                f.floats.extend(fl);
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
            let page_empty = f.slices.is_empty();
            if line == 0 && !page_empty && l.format.page_break_before {
                return (Stop::Break(b), y);
            }
            if line == 0 && !empty {
                if let Some(need) = self.keep_need(doc, cache, fonts, b) {
                    if y + l.space_before + need > h && need <= h - top {
                        return (Stop::Full(Checkpoint::at(b, 0)), y);
                    }
                }
            }
            if line == 0 && !empty {
                y += l.space_before;
            }
            // Floating pictures: the paragraph anchoring them places them, and
            // its lines and those after go round them.
            let floats_before = f.floats.len();
            let mut wrap: Option<Rc<Vec<Band>>> = None;
            if line == 0 {
                self.anchor_floats(doc, f, b, x, w, y);
                let bands = self.bands(f, x, w, y);
                if !bands.is_empty() {
                    wrap = Some(Rc::new(bands));
                }
            } else if let Some((_, c)) = f.carry.as_ref().filter(|c| c.0 == b) {
                wrap = Some(c.clone());
            }
            let l = match &wrap {
                Some(bands) => cache.get_wrapped(doc, doc.para(b).expect("paragraph"), doc.block_box(b, self.col_width(b)).1, fonts, bands),
                None => l,
            };
            let hash = wrap.as_deref().map_or(0, |w| wrap_hash(w));
            let total = l.lines.len();
            let l0 = line;
            let y0 = y;
            let mut li = line;
            let mut yy = y;
            let notes_here = has_notes && doc.para(b).is_some_and(|p| p.text.contains(OBJECT_CHAR));
            let mut room = h;
            while li < total {
                if notes_here {
                    let new_notes: Vec<NoteId> = self.footnotes_in(doc, b, Some(&l.lines[li])).into_iter().filter(|id| !f.foot.contains(id)).collect();
                    let add = self.notes_height(doc, cache, fonts, &new_notes, f.foot.is_empty());
                    // Alone on an empty page, a line goes with its notes even if they don't fit.
                    let alone = page_empty && li == l0;
                    if yy + l.lines[li].height + add > room && !alone {
                        break;
                    }
                    f.foot.extend(new_notes);
                    f.foot_h += add;
                    room = limit.min(full_h - f.foot_h);
                } else if yy + l.lines[li].height > room {
                    break;
                }
                yy += l.lines[li].height;
                li += 1;
            }
            if li == total {
                f.slices.push(Slice { block: b, lines: l0..li, y: y0, x, w, wrap });
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
                if !empty || (!page_empty && limit < full_h) {
                    // The paragraph starts in the next column, with its notes.
                    if notes_here {
                        let moved: Vec<NoteId> = (l0..li).flat_map(|k| self.footnotes_in(doc, b, Some(&l.lines[k]))).collect();
                        f.foot.retain(|id| !moved.contains(id));
                        f.foot_h = if f.foot.is_empty() { 0 } else { self.notes_height(doc, cache, fonts, &f.foot, true) };
                    }
                    f.floats.truncate(floats_before);
                    return (Stop::Full(Checkpoint { block: b, line: l0, wrap: if l0 > 0 { hash } else { 0 } }), y - if l0 == 0 && !empty { l.space_before } else { 0 });
                }
                // Alone in an empty column it must break somewhere.
                cut = li.max(l0 + 1);
            }
            let used: i64 = l.lines[l0..cut].iter().map(|x| x.height).sum();
            if let Some(c) = &wrap {
                f.carry = Some((b, c.clone()));
            }
            f.slices.push(Slice { block: b, lines: l0..cut, y: y0, x, w, wrap });
            // Notes of lines that moved to the next column go with them.
            if notes_here && cut < li {
                let moved: Vec<NoteId> = (cut..li).flat_map(|k| self.footnotes_in(doc, b, Some(&l.lines[k]))).collect();
                if !moved.is_empty() {
                    f.foot.retain(|id| !moved.contains(id));
                    f.foot_h = if f.foot.is_empty() { 0 } else { self.notes_height(doc, cache, fonts, &f.foot, true) };
                }
            }
            return (Stop::Full(Checkpoint { block: b, line: cut, wrap: hash }), y0 + used);
        }
    }
}

/// What a page holds so far.
#[derive(Clone, Default)]
struct Fill {
    slices: Vec<Slice>,
    cells: Vec<CellBox>,
    /// Footnotes anchored on the page, and the height their area takes.
    foot: Vec<NoteId>,
    foot_h: i64,
    /// Floating pictures placed so far.
    floats: Vec<PlacedFloat>,
    /// The paragraph broken at the end of the last column and the room its
    /// floats left it.
    carry: Option<(usize, Rc<Vec<Band>>)>,
}

/// Why a column stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stop {
    /// Full: the next column (or page) starts here.
    Full(Checkpoint),
    /// Block `b` starts a section.
    Section(usize),
    /// Block `b` has a page break before it.
    Break(usize),
    /// The text has ended (endnote line to go on from).
    End(usize),
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

    fn sec(columns: u8, start: SectionStart) -> Option<Section> {
        Some(Section { start, columns, gap: 200, rule: true })
    }

    fn set_section(d: &mut TextDoc, b: usize, s: Option<Section>) {
        d.blocks.update(b, |x| x.as_para_mut().unwrap().props.section = s);
    }

    #[test]
    fn columns_fill_side_by_side_then_the_next_page() {
        let mut d = doc_with(&[3; 20], 10);
        set_section(&mut d, 0, sec(2, SectionStart::NextPage));
        let (p, mut c) = all(&d);
        let pg = &p.pages()[0];
        let xs: Vec<i64> = pg.slices.iter().map(|s| s.x).collect();
        assert!(xs[0] == 0 && xs.iter().any(|&x| x > 0), "{xs:?}");
        // In order: column one then column two, each at most the page high.
        let w = pg.slices[0].w;
        assert!(w < p.width() / 2);
        assert!(pg.slices.windows(2).all(|s| s[0].x <= s[1].x));
        let lines: usize = pg.slices.iter().map(|s| s.lines.len()).sum();
        assert!(lines > 10 && lines <= 20, "two columns of up to ten lines: {lines}");
        assert_eq!(pg.rules.len(), 1);
        // Every line of the document once, in order.
        let mut seen = Vec::new();
        for pg in p.pages() {
            for s in &pg.slices {
                for l in s.lines.clone() {
                    seen.push((s.block, l));
                }
            }
        }
        let want: Vec<(usize, usize)> = (0..20).flat_map(|b| (0..p.layout(&d, &mut c, &FixedFonts, b).lines.len()).map(move |l| (b, l))).collect();
        assert_eq!(seen, want);
    }

    #[test]
    fn a_continuous_section_balances_the_columns_above_it() {
        let mut d = doc_with(&[1; 30], 40);
        set_section(&mut d, 0, sec(3, SectionStart::NextPage));
        set_section(&mut d, 12, sec(1, SectionStart::Continuous));
        let (p, _) = all(&d);
        let pg = &p.pages()[0];
        let band: Vec<&Slice> = pg.slices.iter().filter(|s| s.block < 12).collect();
        let mut per_col: Vec<(i64, usize)> = Vec::new();
        for s in &band {
            match per_col.iter_mut().find(|c| c.0 == s.x) {
                Some(c) => c.1 += s.lines.len(),
                None => per_col.push((s.x, s.lines.len())),
            }
        }
        let n: Vec<usize> = per_col.iter().map(|c| c.1).collect();
        assert_eq!(n.len(), 3, "{n:?}");
        assert!(n.iter().max().unwrap() - n.iter().min().unwrap() <= 2, "balanced: {n:?}");
        let bottom = band.iter().map(|s| s.y + 809 * s.lines.len() as i64).max().unwrap();
        let next = pg.slices.iter().find(|s| s.block == 12).unwrap();
        assert!(next.x == 0 && next.y >= bottom && next.w == p.width(), "{next:?}");
        // A next-page section starts a page.
        set_section(&mut d, 20, sec(2, SectionStart::NextPage));
        let (p, _) = all(&d);
        assert_eq!(p.pages()[1].start, Checkpoint::at(20, 0));
    }

    #[test]
    fn random_edits_with_sections_match_full_pagination() {
        let paras: Vec<usize> = (0..90).map(|i| 1 + (i * 5) % 7).collect();
        let mut d = doc_with(&paras, 12);
        let (mut p, mut c) = all(&d);
        let mut x: u64 = 0x51ce_7105;
        let mut rnd = |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n.max(1)
        };
        for _ in 0..300 {
            let len = d.blocks.len();
            let b = rnd(len as u64) as usize;
            match rnd(5) {
                0 | 1 => {
                    let k = rnd(90) as usize;
                    let ins = crate::ops::Fragment { text: "w ".repeat(k / 2) + "v", runs: vec![(0, 0)] };
                    check_edit(&mut d, &mut p, &mut c, TextOp::Splice { block: b as u32, at: 0, del: 0, ins }, Edit { first: b, last: b + 1, delta: 0 });
                }
                2 => {
                    let id = d.new_id();
                    check_edit(&mut d, &mut p, &mut c, TextOp::Split { block: b as u32, at: 0, id, style: 0, props: Default::default() }, Edit { first: b, last: b + 2, delta: 1 });
                }
                3 if b + 1 < len => {
                    check_edit(&mut d, &mut p, &mut c, TextOp::Join { block: b as u32 }, Edit { first: b, last: b + 1, delta: -1 });
                }
                _ => {
                    let mut props = d.para(b).unwrap().props.clone();
                    let start = [SectionStart::NextPage, SectionStart::Continuous][rnd(2) as usize];
                    props.section = if rnd(3) == 0 { None } else { sec(1 + rnd(3) as u8, start) };
                    let style = d.para(b).unwrap().style;
                    check_edit(&mut d, &mut p, &mut c, TextOp::SetPara { block: b as u32, style, props }, Edit { first: b, last: b + 1, delta: 0 });
                }
            }
        }
        assert!(p.pages().iter().any(|pg| pg.slices.iter().any(|s| s.x > 0)), "columns were used");
    }

    /// A floating picture `w`×`h` twips at the start of block `b`.
    fn float_op(d: &mut TextDoc, b: usize, fl: crate::model::Float, w: u32, h: u32) -> TextOp {
        let a = d.attrs.intern(crate::model::CharAttrs { object: Some(ObjectRef { image: 0, width: w, height: h, float: Some(fl) }), ..Default::default() });
        let ins = crate::ops::Fragment { text: OBJECT_CHAR.to_string(), runs: vec![(0, a)] };
        TextOp::Splice { block: b as u32, at: 0, del: 0, ins }
    }

    fn content_tw(d: &TextDoc) -> u32 {
        (d.page.width - d.page.margin_start - d.page.margin_end) as u32
    }

    /// Every line's text box on the page: (x, top, right, bottom), layout units.
    fn line_boxes(d: &TextDoc, p: &Paginator, c: &mut LayoutCache, page: usize) -> Vec<(usize, i64, i64, i64, i64)> {
        let mut v = Vec::new();
        for s in &p.pages()[page].slices {
            let l = c.get_wrapped(d, d.para(s.block).unwrap(), s.w, &FixedFonts, s.wrap.as_deref().map_or(&[], |w| w.as_slice()));
            let mut y = s.y;
            for li in s.lines.clone() {
                let ln = &l.lines[li];
                if ln.width > 0 {
                    v.push((s.block, s.x + ln.x, y + ln.skip, s.x + ln.x + ln.width, y + ln.height));
                }
                y += ln.height;
            }
        }
        v
    }

    fn overlaps(a: (i64, i64, i64, i64), f: &PlacedFloat) -> bool {
        a.0 < f.x + f.w && a.2 > f.x && a.1 < f.y + f.h && a.3 > f.y
    }

    /// Square: the text beside the picture, on the side with room, and full
    /// width below it; top and bottom: nothing beside it; behind and in
    /// front: the text as if it weren't there.
    #[test]
    fn text_wraps_round_floating_pictures() {
        use crate::model::{Float, HAlign, Wrap};
        let base = doc_with(&[6, 6, 6], 40);
        let (p0, _) = all(&base);
        let half = content_tw(&base) / 2;
        for wrap in [Wrap::Square, Wrap::TopBottom, Wrap::Behind, Wrap::InFront] {
            let mut d = base.clone();
            let fl = Float { wrap, h_align: HAlign::End, y: 400, ..Float::default() };
            float_op(&mut d, 0, fl, half, 2000).apply(&mut d);
            let (p, mut c) = all(&d);
            let f = p.pages()[0].floats[0].clone();
            assert_eq!((f.block, f.at), (0, 0));
            assert_eq!(f.x + f.w, p.width(), "{wrap:?}: at the column's end");
            assert_eq!(f.y, twips(400), "{wrap:?}: down from the paragraph's top");
            let boxes = line_boxes(&d, &p, &mut c, 0);
            let lines = |b: usize| boxes.iter().filter(|x| x.0 == b).count();
            match wrap {
                Wrap::Square => {
                    assert!(boxes.iter().all(|x| !overlaps((x.1, x.2, x.3, x.4), &f)), "no text under it");
                    let beside: Vec<_> = boxes.iter().filter(|x| x.2 < f.y + f.h && x.4 > f.y).collect();
                    assert!(beside.len() >= 3 && beside.iter().all(|x| x.3 <= f.x - twips(180)), "lines beside it stop short of it: {beside:?}");
                    assert!(boxes.iter().any(|x| x.2 >= f.y + f.h && x.3 > f.x), "full width below it");
                    assert!(lines(0) + lines(1) > 12, "narrower lines, more of them");
                }
                Wrap::TopBottom => {
                    assert!(boxes.iter().all(|x| x.4 <= f.y || x.2 >= f.y + f.h), "nothing beside it");
                    assert!(boxes.iter().any(|x| x.2 >= f.y + f.h + twips(180) - LU && x.2 <= f.y + f.h + twips(180) + line_h()), "the text goes on just below it");
                }
                _ => assert_eq!(shape(&p), shape(&p0), "{wrap:?}: the text runs on"),
            }
        }
    }

    /// A cell grows to hold a picture its text wraps round; one in front of
    /// the text leaves the row as tall as its text.
    #[test]
    fn cells_grow_round_their_floating_pictures() {
        use crate::model::{Float, HAlign, Wrap};
        let row_h = |wrap: Option<Wrap>| {
            let mut d = with_table(&[1, 1], 40, 1, 2, 2);
            if let Some(wrap) = wrap {
                float_op(&mut d, 1, Float { wrap, h_align: HAlign::Start, ..Float::default() }, 600, 3000).apply(&mut d);
            }
            let (p, _) = all(&d);
            let pg = &p.pages()[0];
            let y = |b: usize| pg.slices.iter().find(|s| s.block == b).unwrap().y;
            y(3) - y(1)
        };
        let plain = row_h(None);
        assert!(row_h(Some(Wrap::Square)) >= twips(3000), "the picture's row holds it");
        assert_eq!(row_h(Some(Wrap::InFront)), plain);
        assert_eq!(row_h(Some(Wrap::Behind)), plain);
    }

    /// A picture measured from the page sits there whatever paragraph anchors
    /// it; one that would run off the page's foot moves up to fit.
    #[test]
    fn floats_stay_on_the_page() {
        use crate::model::{Float, HAlign, Rel};
        let mut d = doc_with(&[6, 6, 6], 30);
        let q = content_tw(&d) / 3;
        float_op(&mut d, 2, Float { h_rel: Rel::Page, h_align: HAlign::Offset, x: 50, v_rel: Rel::Page, y: 300, ..Float::default() }, q, 600).apply(&mut d);
        float_op(&mut d, 1, Float { h_align: HAlign::Start, y: 20000, ..Float::default() }, q, 1200).apply(&mut d);
        let (p, _) = all(&d);
        let fl = &p.pages()[0].floats;
        let by_block = |b: usize| fl.iter().find(|f| f.block == b).unwrap();
        assert_eq!((by_block(2).x, by_block(2).y), (twips(50 - 100), twips(300 - 100)), "from the page's corner");
        assert_eq!(by_block(1).y + by_block(1).h, p.content_height(), "moved up to the foot");
        assert_eq!(by_block(1).x, 0);
    }

    /// Random edits around floats (pictures in and out, wraps and places
    /// changed, text typed, paragraphs split and joined) repaginate the same
    /// as from scratch.
    #[test]
    fn random_edits_with_floats_match_full_pagination() {
        use crate::model::{Float, HAlign, Rel, Wrap};
        let paras: Vec<usize> = (0..60).map(|i| 1 + (i * 5) % 7).collect();
        let mut d = doc_with(&paras, 14);
        let (mut p, mut c) = all(&d);
        let mut x: u64 = 0x0f1a_7e5d;
        let mut rnd = |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n.max(1)
        };
        let cw = content_tw(&d);
        let mut placed = 0;
        for _ in 0..300 {
            let len = d.blocks.len();
            let b = rnd(len as u64) as usize;
            match rnd(6) {
                0 | 1 => {
                    let wrap = [Wrap::Square, Wrap::TopBottom, Wrap::Behind, Wrap::InFront][rnd(4) as usize];
                    let rel = [Rel::Text, Rel::Margin, Rel::Page][rnd(3) as usize];
                    let h_align = [HAlign::Start, HAlign::Center, HAlign::End, HAlign::Offset][rnd(4) as usize];
                    let fl = Float { wrap, h_rel: rel, h_align, x: rnd(3000) as i32, v_rel: [Rel::Text, Rel::Margin, Rel::Page][rnd(3) as usize], y: rnd(4000) as i32, gap: rnd(300) as i32 };
                    let op = float_op(&mut d, b, fl, 300 + rnd(cw as u64) as u32, 300 + rnd(3000) as u32);
                    check_edit(&mut d, &mut p, &mut c, op, Edit { first: b, last: b + 1, delta: 0 });
                    placed += 1;
                }
                2 => {
                    let k = rnd(90) as usize;
                    let ins = crate::ops::Fragment { text: "w ".repeat(k / 2) + "v", runs: vec![(0, 0)] };
                    let at = d.para(b).unwrap().text.len() as u32;
                    check_edit(&mut d, &mut p, &mut c, TextOp::Splice { block: b as u32, at, del: 0, ins }, Edit { first: b, last: b + 1, delta: 0 });
                }
                3 => {
                    let id = d.new_id();
                    check_edit(&mut d, &mut p, &mut c, TextOp::Split { block: b as u32, at: 0, id, style: 0, props: Default::default() }, Edit { first: b, last: b + 2, delta: 1 });
                }
                4 if b + 1 < len => {
                    check_edit(&mut d, &mut p, &mut c, TextOp::Join { block: b as u32 }, Edit { first: b, last: b + 1, delta: -1 });
                }
                _ => {
                    // Take a picture out.
                    let t = d.para(b).unwrap().text.clone();
                    if let Some(i) = t.find(OBJECT_CHAR) {
                        let ins = crate::ops::Fragment { text: String::new(), runs: vec![(0, 0)] };
                        check_edit(&mut d, &mut p, &mut c, TextOp::Splice { block: b as u32, at: i as u32, del: OBJECT_CHAR.len_utf8() as u32, ins }, Edit { first: b, last: b + 1, delta: 0 });
                    }
                }
            }
        }
        assert!(placed > 50);
        assert!(p.pages().iter().any(|pg| pg.slices.iter().any(|s| s.wrap.is_some() && s.lines.start > 0)), "a wrapped paragraph went on to the next page");
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
