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
//! - a line taller than the page still goes on the page, alone.

use std::ops::Range;
use std::time::{Duration, Instant};

use crate::layout::{Fonts, LayoutCache, ParaLayout};
use crate::model::TextDoc;
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub start: Checkpoint,
    pub slices: Vec<Slice>,
    /// Content height used.
    pub height: i64,
}

impl Page {
    fn shift(&mut self, delta: isize) {
        self.start.block = (self.start.block as isize + delta) as usize;
        for s in &mut self.slices {
            s.block = (s.block as isize + delta) as usize;
        }
    }
}

/// Which blocks an edit touched: in the new document, blocks `first..last`
/// are new or changed and every block from `last` on is block `i - delta` of
/// the old document, unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edit {
    pub first: usize,
    pub last: usize,
    pub delta: isize,
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
        self.cursor = Some(Checkpoint::default());
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
        if !self.old.is_empty() || self.cursor.is_some_and(|cur| cur < restart) {
            // Already behind: pages past the cursor were never laid out, and a
            // second edit before convergence would compose shifts; repaginate
            // from the earlier point to the end instead.
            self.old.clear();
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
        if at.block >= doc.blocks.len() && !self.pages.is_empty() {
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
                if self.converge(n) {
                    return false;
                }
            }
        }
        self.cursor.is_some()
    }

    fn finish(&mut self) {
        self.cursor = None;
        self.old.clear();
    }

    /// If an old page started at `n` (past the edit), take it and all after it.
    fn converge(&mut self, n: Checkpoint) -> bool {
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
            self.finish();
            return true;
        }
        false
    }

    fn layout(&self, doc: &TextDoc, cache: &mut LayoutCache, fonts: &dyn Fonts, i: usize) -> Rc<ParaLayout> {
        let p = doc.para(i).expect("paragraph");
        cache.get(doc, p, self.width, fonts)
    }

    /// Height that must fit for block `b` to start here, when it keeps with the
    /// next or keeps together; None when it needn't move.
    fn keep_need(&self, doc: &TextDoc, cache: &mut LayoutCache, fonts: &dyn Fonts, b: usize) -> Option<i64> {
        let n = doc.blocks.len();
        let mut need = 0;
        let mut i = b;
        loop {
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

    /// Lays out the page starting at `start`; returns it and where the next starts.
    fn page(&self, doc: &TextDoc, cache: &mut LayoutCache, fonts: &dyn Fonts, start: Checkpoint) -> (Page, Option<Checkpoint>) {
        let h = self.height;
        let n = doc.blocks.len();
        let mut slices: Vec<Slice> = Vec::new();
        let mut y = 0i64;
        let (mut b, mut line) = (start.block, start.line);
        let done = |slices: Vec<Slice>, y: i64, next: Option<Checkpoint>| (Page { start, slices, height: y }, next);
        loop {
            if b >= n {
                return done(slices, y, None);
            }
            let l = self.layout(doc, cache, fonts, b);
            let top = slices.is_empty();
            if line == 0 && !top {
                if l.format.page_break_before {
                    return done(slices, y, Some(Checkpoint { block: b, line: 0 }));
                }
                if let Some(need) = self.keep_need(doc, cache, fonts, b) {
                    if y + l.space_before + need > h && need <= h {
                        return done(slices, y, Some(Checkpoint { block: b, line: 0 }));
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
            while li < total && yy + l.lines[li].height <= h {
                yy += l.lines[li].height;
                li += 1;
            }
            if li == total {
                slices.push(Slice { block: b, lines: l0..li, y: y0 });
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
                    // The paragraph starts on the next page.
                    return done(slices, y - if l0 == 0 { l.space_before } else { 0 }, Some(Checkpoint { block: b, line: l0 }));
                }
                // Alone on an empty page it must break somewhere.
                cut = li.max(l0 + 1);
            }
            let used: i64 = l.lines[l0..cut].iter().map(|x| x.height).sum();
            slices.push(Slice { block: b, lines: l0..cut, y: y0 });
            return done(slices, y0 + used, Some(Checkpoint { block: b, line: cut }));
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
        op.apply(d);
        let before = p.laid;
        p.edit(d, c, &FixedFonts, e);
        assert!(p.run(d, c, &FixedFonts, usize::MAX));
        let (fresh, _) = all(d);
        if let Some(i) = (0..p.pages().len().max(fresh.pages().len())).find(|&i| p.pages().get(i) != fresh.pages().get(i)) {
            panic!("{e:?}: page {i} differs\n inc {:?}\n new {:?}\n prev {:?}", p.pages().get(i), fresh.pages().get(i), fresh.pages().get(i.wrapping_sub(1)));
        }
        p.laid - before
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
            match rnd(6) {
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
}
