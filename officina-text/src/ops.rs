//! Changes to a text document. Each op applied returns its exact inverse, so
//! `officina_core::history::History<TextOp>` gives undo and redo, and the
//! autosave journal is the ops as they ran.
//!
//! Positions are (block index, byte offset). An op that doesn't fit the document
//! (an index past the end, an offset off a char boundary) changes nothing and
//! returns a no-op, so a damaged journal can't panic the app.

use officina_core::history::Op;

use crate::model::{AttrId, Block, ParaProps, StyleId, TextDoc};

/// Text with its attribute runs (run starts relative to the text).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Fragment {
    pub text: String,
    pub runs: Vec<(u32, AttrId)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TextOp {
    /// Replaces `del` bytes at `at` in paragraph `block` with `ins`.
    Splice { block: u32, at: u32, del: u32, ins: Fragment },
    /// Sets the attribute runs of bytes `start..end` (run starts absolute).
    Restyle { block: u32, start: u32, end: u32, runs: Vec<(u32, AttrId)> },
    /// Splits paragraph `block` at `at`; the second half is a new paragraph with
    /// id `id`, `style` and `props`.
    Split { block: u32, at: u32, id: u64, style: StyleId, props: ParaProps },
    /// Joins paragraph `block + 1` onto `block`.
    Join { block: u32 },
    /// Sets a paragraph's style and direct properties.
    SetPara { block: u32, style: StyleId, props: ParaProps },
    /// Inserts whole blocks before `at`.
    Insert { at: u32, blocks: Vec<Block> },
    /// Removes `n` blocks from `at`.
    Remove { at: u32, n: u32 },
    /// Sets the page setup, header and footer.
    SetPage { page: crate::model::PageSetup, header: Option<crate::model::HeaderFooter>, footer: Option<crate::model::HeaderFooter> },
    /// Sets the headers and footers: the default ones, the first page's and
    /// even pages' own.
    SetMargins { header: Option<crate::model::HeaderFooter>, footer: Option<crate::model::HeaderFooter>, first_page: Option<crate::model::Margins>, even_pages: Option<crate::model::Margins> },
    /// Sets note `id`'s text (None takes the note out of the store).
    SetNote { id: crate::model::NoteId, note: Option<crate::model::Note> },
    /// A drawn shape put in, changed or taken out (its anchor run is the
    /// text's; this is its outline, look and text).
    SetShape { id: u32, shape: Option<crate::model::ShapeData> },
    /// Sets comment `id` (None takes it out of the store).
    SetComment { id: crate::model::CommentId, comment: Option<crate::model::Comment> },
    /// Sets revision `id` (None takes it out).
    SetRevision { id: crate::model::RevId, rev: Option<crate::model::Revision> },
    /// Sets deletion `id`, what a tombstone holds (None takes it out).
    SetDeletion { id: crate::model::DelId, del: Option<crate::model::Deletion> },
    /// Sets merge conflict `id`, the other version it holds (None takes it out).
    SetConflict { id: crate::model::ConflictId, alt: Option<crate::model::Deletion> },
    /// Turns tracking on or off.
    SetTracking { on: bool },
    /// Changes style `id`, adds it (`id` one past the last), or takes out
    /// the last one when nothing uses it (`None`).
    SetStyle { id: StyleId, style: Option<crate::model::Style> },
    /// Takes style `id` out when nothing uses it (`None`), the styles after
    /// it moving down one; or puts `style` in at `id` (never 0), those from
    /// `id` moving up one, its references as they will read after. Each
    /// style id the document holds follows. `stale`: entries of the
    /// attribute table no text uses that named the style taken out (the
    /// inverse names them again).
    PlaceStyle { id: StyleId, style: Option<crate::model::Style>, stale: Vec<AttrId> },
    /// Nothing (what an op that didn't fit becomes).
    Nop,
}

/// The size of note text, half points (10 pt).
pub const NOTE_SIZE: u16 = 20;

fn boundary(s: &str, at: u32) -> bool {
    s.is_char_boundary(at as usize)
}

/// Whether a deletion (or a conflict's other version) is well formed here.
fn deletion_ok(d: &TextDoc, x: &crate::model::Deletion) -> bool {
    let bytes: usize = x.pieces.iter().map(|p| p.text.len()).sum();
    !(x.pieces.is_empty() || bytes > crate::model::MAX_DELETION_TEXT || x.pieces.iter().enumerate().any(|(i, p)| (i > 0) != p.para.is_some() || p.runs.iter().any(|r| r.1 as usize >= d.attrs.len() || !boundary(&p.text, r.0)) || p.para.as_ref().is_some_and(|q| q.1 as usize >= d.styles.styles.len())))
}

impl TextDoc {
    /// The runs of `start..end` in paragraph `block`, clipped and absolute.
    pub fn runs_in(&self, block: usize, start: u32, end: u32) -> Vec<(u32, AttrId)> {
        let p = self.para(block).expect("checked");
        let mut v = Vec::new();
        for (s, e, a) in p.spans() {
            if e <= start && !(s == e && s == start) {
                continue;
            }
            if s >= end && !(start == end && s == start) {
                break;
            }
            v.push((s.max(start), a));
        }
        if v.is_empty() {
            v.push((start, p.attr_at(start)));
        }
        v
    }

    fn splice(&mut self, block: usize, at: u32, del: u32, ins: Fragment) -> TextOp {
        let Some(p) = self.para(block) else { return TextOp::Nop };
        let end = at.saturating_add(del);
        if end as usize > p.text.len() || !boundary(&p.text, at) || !boundary(&p.text, end) || ins.runs.iter().any(|r| !boundary(&ins.text, r.0)) {
            return TextOp::Nop;
        }
        // What goes out, with its runs, for the inverse.
        let old_runs: Vec<(u32, AttrId)> = if del > 0 { self.runs_in(block, at, end).into_iter().map(|(s, a)| (s - at, a)).collect() } else { Vec::new() };
        let old_text = p.text[at as usize..end as usize].to_string();
        let n = ins.text.len() as u32;
        self.blocks.update(block, |b| {
            let p = b.as_para_mut().expect("checked");
            // The run after the cut keeps its attributes.
            let after = p.attr_at(end);
            p.text.replace_range(at as usize..end as usize, &ins.text);
            let mut runs: Vec<(u32, AttrId)> = p.runs.iter().copied().filter(|r| r.0 < at || (r.0 == 0 && at == 0 && ins.text.is_empty())).collect();
            if ins.text.is_empty() {
                // Nothing typed: no run here.
            } else if ins.runs.is_empty() {
                // Typed text takes the attributes before it (or the paragraph's first).
                let a = if at > 0 { p_attr(&runs, at) } else { after };
                runs.push((at, a));
            } else {
                for &(s, a) in &ins.runs {
                    runs.push((at + s, a));
                }
            }
            runs.push((at + n, after));
            for &(s, a) in p.runs.iter().filter(|r| r.0 > end) {
                runs.push((s - del + n, a));
            }
            runs.sort_by_key(|r| r.0);
            p.runs = runs;
            p.normalize();
        });
        TextOp::Splice { block: block as u32, at, del: n, ins: Fragment { text: old_text, runs: if del > 0 { old_runs } else { Vec::new() } } }
    }
}

fn p_attr(runs: &[(u32, AttrId)], at: u32) -> AttrId {
    let i = runs.partition_point(|r| r.0 < at).saturating_sub(1);
    runs.get(i).map_or(0, |r| r.1)
}

impl TextOp {
    /// The change that applying the op whose inverse is `self` made.
    fn edit_of_inverse(&self) -> Option<crate::model::Edit> {
        use crate::model::Edit;
        Some(match *self {
            TextOp::Nop => return None,
            TextOp::Splice { block, .. } | TextOp::Restyle { block, .. } | TextOp::SetPara { block, .. } => Edit { first: block as usize, last: block as usize + 1, delta: 0 },
            TextOp::Join { block } => Edit { first: block as usize, last: block as usize + 2, delta: 1 },
            TextOp::Split { block, .. } => Edit { first: block as usize, last: block as usize + 1, delta: -1 },
            TextOp::Remove { at, n } => Edit { first: at as usize, last: (at + n) as usize, delta: n as isize },
            TextOp::Insert { at, ref blocks } => Edit { first: at as usize, last: at as usize, delta: -(blocks.len() as isize) },
            // Every page may change (the paginator resets on a new geometry).
            TextOp::SetPage { .. } | TextOp::SetMargins { .. } => Edit { first: 0, last: 0, delta: 0 },
            // Noted by `apply`, from the anchor.
            TextOp::SetNote { .. } | TextOp::SetShape { .. } | TextOp::SetStyle { .. } => return None,
            // Ids move; nothing looks different.
            TextOp::PlaceStyle { .. } => return None,
            // Comments change no layout; the editor redraws on every action.
            TextOp::SetComment { .. } | TextOp::SetRevision { .. } | TextOp::SetTracking { .. } => return None,
            // A tombstone takes no room; its text is drawn only in review.
            TextOp::SetDeletion { .. } | TextOp::SetConflict { .. } => return None,
        })
    }

    /// Whether the op puts in or takes out a note anchor (or may restyle one).
    fn moves_anchors(&self, d: &TextDoc) -> bool {
        let has = |t: &str| t.contains(crate::model::OBJECT_CHAR);
        match self {
            TextOp::Splice { ins, .. } => has(&ins.text),
            TextOp::Restyle { block, .. } => d.para(*block as usize).is_some_and(|p| has(&p.text)),
            TextOp::Insert { blocks, .. } => blocks.iter().any(|b| b.as_para().is_some_and(|p| has(&p.text))),
            _ => false,
        }
    }
}

impl Op<TextDoc> for TextOp {
    fn apply(self, d: &mut TextDoc) -> TextOp {
        if let TextOp::SetShape { id, .. } = &self {
            let id = *id;
            let inv = self.apply_inner(d);
            if !matches!(inv, TextOp::Nop) {
                // Its anchor's page is painted again.
                if let Some(at) = d.shape_anchor(id) {
                    d.note(crate::model::Edit { first: at, last: at + 1, delta: 0 });
                }
            }
            return inv;
        }
        if let TextOp::SetStyle { id, .. } = &self {
            let id = *id;
            // Paragraphs of the style (or styles based on it) before and
            // after are laid out again.
            let before = d.style_span(id);
            let inv = self.apply_inner(d);
            if !matches!(inv, TextOp::Nop) {
                let after = d.style_span(id);
                let span = match (before, after) {
                    (Some(a), Some(b)) => Some((a.0.min(b.0), a.1.max(b.1))),
                    (a, b) => a.or(b),
                };
                if let Some((first, last)) = span {
                    d.note(crate::model::Edit { first, last, delta: 0 });
                }
            }
            return inv;
        }
        if let TextOp::SetNote { id, .. } = &self {
            let id = *id;
            let inv = self.apply_inner(d);
            if !matches!(inv, TextOp::Nop) {
                // The note's anchor page is laid out again (and every page
                // after it for an endnote, which the paginator always redoes).
                let at = d.anchor_of(id).map_or(d.blocks.len().saturating_sub(1), |a| a.0);
                d.note(crate::model::Edit { first: at, last: at + 1, delta: 0 });
            }
            return inv;
        }
        // Anchors are counted before and after (what goes out is in the inverse).
        let before = !d.notes.is_empty() && self.moves_anchors(d);
        let inv = self.apply_inner(d);
        if let Some(e) = inv.edit_of_inverse() {
            d.note(e);
            if !d.notes.is_empty() && (before || inv.moves_anchors(d)) {
                // Later notes are numbered anew; their labels' widths can move
                // lines, so every page from here is laid out again.
                d.notes.renumber();
                let n = d.blocks.len();
                d.note(crate::model::Edit { first: e.first.min(n.saturating_sub(1)), last: n, delta: 0 });
            }
        }
        inv
    }
}

impl TextOp {
    fn apply_inner(self, d: &mut TextDoc) -> TextOp {
        match self {
            TextOp::Nop => TextOp::Nop,
            TextOp::Splice { block, at, del, ins } => d.splice(block as usize, at, del, ins),
            TextOp::Restyle { block, start, end, runs } => {
                let Some(p) = d.para(block as usize) else { return TextOp::Nop };
                if start > end || end as usize > p.text.len() || !boundary(&p.text, start) || !boundary(&p.text, end) || runs.iter().any(|r| r.0 < start || r.0 > end || !boundary(&p.text, r.0)) {
                    return TextOp::Nop;
                }
                let old = d.runs_in(block as usize, start, end);
                d.blocks.update(block as usize, |b| {
                    let p = b.as_para_mut().expect("checked");
                    let after = p.attr_at(end);
                    let mut v: Vec<(u32, AttrId)> = p.runs.iter().copied().filter(|r| r.0 < start).collect();
                    v.extend(runs.iter().copied());
                    if end < p.text.len() as u32 || start == end {
                        v.push((end, after));
                    }
                    v.extend(p.runs.iter().copied().filter(|r| r.0 > end));
                    v.sort_by_key(|r| r.0);
                    // Later entries at one start win: keep the last of each.
                    let mut w: Vec<(u32, AttrId)> = Vec::with_capacity(v.len());
                    for r in v {
                        match w.last_mut() {
                            Some(l) if l.0 == r.0 => *l = r,
                            _ => w.push(r),
                        }
                    }
                    p.runs = w;
                    p.normalize();
                });
                TextOp::Restyle { block, start, end, runs: old }
            }
            TextOp::Split { block, at, id, style, props } => {
                let Some(p) = d.para(block as usize) else { return TextOp::Nop };
                if at as usize > p.text.len() || !boundary(&p.text, at) {
                    return TextOp::Nop;
                }
                let n = p.text.len() as u32;
                let tail_runs: Vec<(u32, AttrId)> = d.runs_in(block as usize, at, n).into_iter().map(|(s, a)| (s - at, a)).collect();
                let mut tail = String::new();
                d.blocks.update(block as usize, |b| {
                    let p = b.as_para_mut().expect("checked");
                    tail = p.text.split_off(at as usize);
                    let typing = p.attr_at(at);
                    p.runs.retain(|r| r.0 < at || r.0 == 0);
                    if at == 0 {
                        p.runs = vec![(0, typing)];
                    }
                    p.normalize();
                });
                let mut np = crate::model::Para::new(id, &tail, style);
                np.runs = tail_runs;
                np.props = props;
                np.normalize();
                d.blocks.insert(block as usize + 1, Block::Para(np));
                d.raise_watermark(id + 1);
                TextOp::Join { block }
            }
            TextOp::Join { block } => {
                let (Some(a), Some(b)) = (d.para(block as usize), d.para(block as usize + 1)) else { return TextOp::Nop };
                let at = a.text.len() as u32;
                let (id, style, props) = (b.id, b.style, b.props.clone());
                let Some(Block::Para(second)) = d.blocks.remove(block as usize + 1) else { return TextOp::Nop };
                d.blocks.update(block as usize, |bl| {
                    let p = bl.as_para_mut().expect("checked");
                    let empty_first = p.text.is_empty();
                    p.text.push_str(&second.text);
                    if empty_first {
                        // An empty first paragraph takes the second's runs.
                        p.runs = second.runs.clone();
                    } else if !second.text.is_empty() {
                        p.runs.extend(second.runs.iter().map(|&(s, a)| (s + at, a)));
                    }
                    p.normalize();
                });
                TextOp::Split { block, at, id, style, props }
            }
            TextOp::SetPara { block, style, props } => {
                if d.para(block as usize).is_none() || style as usize >= d.styles.styles.len() {
                    return TextOp::Nop;
                }
                let mut old = None;
                d.blocks.update(block as usize, |b| {
                    let p = b.as_para_mut().expect("checked");
                    old = Some((p.style, std::mem::replace(&mut p.props, props)));
                    p.style = style;
                });
                let (style, props) = old.expect("set");
                TextOp::SetPara { block, style, props }
            }
            TextOp::Insert { at, blocks } => {
                if at as usize > d.blocks.len() {
                    return TextOp::Nop;
                }
                let n = blocks.len() as u32;
                for (k, b) in blocks.into_iter().enumerate() {
                    d.raise_watermark(b.id() + 1);
                    d.blocks.insert(at as usize + k, b);
                }
                TextOp::Remove { at, n }
            }
            TextOp::Remove { at, n } => {
                // A document keeps at least one block.
                if n == 0 || (at as usize).saturating_add(n as usize) > d.blocks.len() || n as usize == d.blocks.len() {
                    return TextOp::Nop;
                }
                let mut out = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    out.extend(d.blocks.remove(at as usize));
                }
                TextOp::Insert { at, blocks: out }
            }
            TextOp::SetNote { id, note } => {
                if let Some(n) = &note {
                    if n.paras.is_empty() || n.paras.len() > crate::model::MAX_NOTE_PARAS || n.paras.iter().any(|p| p.runs.iter().any(|r| r.1 as usize >= d.attrs.len() || !boundary(&p.text, r.0)) || p.style as usize >= d.styles.styles.len()) {
                        return TextOp::Nop;
                    }
                    for p in &n.paras {
                        d.raise_watermark(p.id + 1);
                    }
                }
                if id as usize >= crate::model::MAX_NOTES {
                    return TextOp::Nop;
                }
                let old = d.notes.set(id, note);
                d.notes.renumber();
                TextOp::SetNote { id, note: old }
            }
            TextOp::SetShape { id, shape } => {
                if id as usize >= crate::model::MAX_SHAPES {
                    return TextOp::Nop;
                }
                if let Some(sd) = &shape {
                    // Its paragraphs' runs and styles must be the document's,
                    // with no objects in them (no shape in a shape).
                    let paras = sd.all_paras();
                    let bad = |p: &crate::model::Para| p.style as usize >= d.styles.styles.len() || p.runs.iter().any(|r| r.1 as usize >= d.attrs.len() || !boundary(&p.text, r.0) || d.attrs.get(r.1).object.is_some());
                    if paras.len() > crate::model::MAX_SHAPE_PARAS * 4 || paras.iter().any(|p| bad(p)) || !sd.fits() {
                        return TextOp::Nop;
                    }
                    for p in &paras {
                        d.raise_watermark(p.id + 1);
                    }
                }
                let old = d.shapes.set(id, shape);
                TextOp::SetShape { id, shape: old }
            }
            TextOp::SetComment { id, comment } => {
                if id as usize >= crate::model::MAX_COMMENTS || comment.as_ref().is_some_and(|c| c.text.len() > crate::model::MAX_COMMENT_TEXT || c.parent == Some(id)) {
                    return TextOp::Nop;
                }
                let old = d.comments.set(id, comment);
                TextOp::SetComment { id, comment: old }
            }
            TextOp::SetRevision { id, rev } => {
                if id as usize >= crate::model::MAX_REVISIONS || rev.as_ref().is_some_and(|r| r.author.len() > 1024 || r.date.len() > 64 || r.name.len() > 1024) {
                    return TextOp::Nop;
                }
                let old = d.revisions.set(id, rev);
                TextOp::SetRevision { id, rev: old }
            }
            TextOp::SetConflict { id, alt } => {
                if id as usize >= crate::model::MAX_CONFLICTS || alt.as_ref().is_some_and(|x| !deletion_ok(d, x)) {
                    return TextOp::Nop;
                }
                for p in alt.iter().flat_map(|x| x.pieces.iter()).filter_map(|p| p.para.as_ref()) {
                    d.raise_watermark(p.0 + 1);
                }
                let old = d.conflicts.set(id, alt);
                TextOp::SetConflict { id, alt: old }
            }
            TextOp::SetDeletion { id, del } => {
                if id as usize >= crate::model::MAX_DELETIONS {
                    return TextOp::Nop;
                }
                if let Some(x) = &del {
                    let bytes: usize = x.pieces.iter().map(|p| p.text.len()).sum();
                    if x.pieces.is_empty() || bytes > crate::model::MAX_DELETION_TEXT || x.pieces.iter().enumerate().any(|(i, p)| (i > 0) != p.para.is_some() || p.runs.iter().any(|r| r.1 as usize >= d.attrs.len() || !boundary(&p.text, r.0)) || p.para.as_ref().is_some_and(|q| q.1 as usize >= d.styles.styles.len())) {
                        return TextOp::Nop;
                    }
                    for p in x.pieces.iter().filter_map(|p| p.para.as_ref()) {
                        d.raise_watermark(p.0 + 1);
                    }
                }
                let old = d.deletions.set(id, del);
                TextOp::SetDeletion { id, del: old }
            }
            TextOp::SetTracking { on } => {
                let old = std::mem::replace(&mut d.tracking, on);
                TextOp::SetTracking { on: old }
            }
            TextOp::SetMargins { header, footer, first_page, even_pages } => TextOp::SetMargins {
                header: std::mem::replace(&mut d.header, header),
                footer: std::mem::replace(&mut d.footer, footer),
                first_page: std::mem::replace(&mut d.first_page, first_page),
                even_pages: std::mem::replace(&mut d.even_pages, even_pages),
            },
            TextOp::SetStyle { id, style } => {
                let n = d.styles.styles.len();
                let i = id as usize;
                match style {
                    None => {
                        // Only the last, never Normal, and only when unused.
                        if i + 1 != n || i == 0 || d.uses_style(id) {
                            return TextOp::Nop;
                        }
                        let old = d.styles.styles.pop();
                        TextOp::SetStyle { id, style: old }
                    }
                    Some(s) => {
                        if !d.style_fits(id, &s) {
                            return TextOp::Nop;
                        }
                        if i == n {
                            d.styles.styles.push(s);
                            TextOp::SetStyle { id, style: None }
                        } else {
                            let old = std::mem::replace(&mut d.styles.styles[i], s);
                            TextOp::SetStyle { id, style: Some(old) }
                        }
                    }
                }
            }
            TextOp::PlaceStyle { id, style, stale } => {
                let n = d.styles.styles.len();
                let i = id as usize;
                match style {
                    None => {
                        if i == 0 || i >= n || d.uses_style(id) {
                            return TextOp::Nop;
                        }
                        let stale = d.attrs.with_style(id);
                        let old = d.styles.styles.remove(i);
                        let down = move |s: StyleId| if s > id { s - 1 } else { s };
                        d.remap_para_styles(&down);
                        d.attrs.remap_styles(&down, &stale, None);
                        TextOp::PlaceStyle { id, style: Some(old), stale }
                    }
                    Some(s) => {
                        let name_ok = !s.name.trim().is_empty() && s.name.len() <= 255 && d.styles.by_name(&s.name).is_none();
                        let refs_ok = [s.parent, s.next].into_iter().flatten().all(|r| (r as usize) <= n) && s.parent != Some(id);
                        let kind_ok = |r: Option<StyleId>| r.is_none_or(|r| r == id || d.styles.styles.get(if r > id { r as usize - 1 } else { r as usize }).is_some_and(|x| x.kind == s.kind));
                        if i == 0 || i > n || n >= crate::model::MAX_STYLES || !name_ok || !refs_ok || !kind_ok(s.parent) {
                            return TextOp::Nop;
                        }
                        let up = move |x: StyleId| if x >= id { x + 1 } else { x };
                        d.remap_para_styles(&up);
                        d.attrs.remap_styles(&up, &stale, Some(id));
                        d.styles.styles.insert(i, s);
                        TextOp::PlaceStyle { id, style: None, stale: Vec::new() }
                    }
                }
            }
            TextOp::SetPage { page, header, footer } => {
                let ok = |t: i32| (0..=63_360).contains(&t);
                if !(page.width >= 1440 && page.height >= 1440 && [page.width, page.height, page.margin_top, page.margin_bottom, page.margin_start, page.margin_end].into_iter().all(ok)) || page.margin_start + page.margin_end + 720 > page.width || page.margin_top + page.margin_bottom + 720 > page.height {
                    return TextOp::Nop;
                }
                let old = TextOp::SetPage { page: d.page, header: d.header.take(), footer: d.footer.take() };
                d.page = page;
                d.header = header;
                d.footer = footer;
                old
            }
        }
    }
}

/// Edits built from a selection: what an editor sends.
impl TextDoc {
    /// A note's paragraphs from plain text (a paragraph a line), in the note
    /// format: 10 pt, no space between paragraphs.
    pub fn note_paras(&mut self, text: &str) -> Vec<crate::model::Para> {
        let a = self.attrs.intern(crate::model::CharAttrs { size: Some(NOTE_SIZE), ..Default::default() });
        let mut v = Vec::new();
        let lines: Vec<&str> = text.split('\n').collect();
        let keep = lines.len().min(crate::model::MAX_NOTE_PARAS);
        for (i, l) in lines.iter().enumerate().take(keep) {
            // Lines past the limit join the last paragraph.
            let t = if i + 1 == keep && lines.len() > keep { lines[i..].join(" ") } else { l.to_string() };
            let id = self.new_id();
            let mut p = crate::model::Para::new(id, &t, 0);
            p.runs = vec![(0, a)];
            p.props.space_after = Some(0);
            p.props.space_before = Some(0);
            v.push(p);
        }
        v
    }

    /// Ops that put a new note with `text` at (`block`, `at`): the note, then its
    /// anchor (in the attributes `base` with the note set).
    pub fn insert_note_ops(&mut self, block: usize, at: u32, kind: crate::model::NoteKind, text: &str, base: AttrId) -> Vec<TextOp> {
        if self.para(block).is_none_or(|p| at as usize > p.text.len() || !p.text.is_char_boundary(at as usize)) || self.notes.len() >= crate::model::MAX_NOTES {
            return Vec::new();
        }
        let id = self.notes.len() as crate::model::NoteId;
        let paras = self.note_paras(text);
        let mut a = self.attrs.get(base).clone();
        a.object = None;
        a.note = Some(id);
        let anchor = self.attrs.intern(a);
        vec![
            TextOp::SetNote { id, note: Some(crate::model::Note { kind, paras }) },
            TextOp::Splice { block: block as u32, at, del: 0, ins: Fragment { text: crate::model::OBJECT_CHAR.to_string(), runs: vec![(0, anchor)] } },
        ]
    }

    /// Ops that put comment `c` on the text from (`b0`, `o0`) to (`b1`, `o1`):
    /// the comment, then its id added to each run there.
    pub fn add_comment_ops(&mut self, b0: usize, o0: u32, b1: usize, o1: u32, c: crate::model::Comment) -> Vec<TextOp> {
        if (b1, o1) <= (b0, o0) || self.para(b1).is_none() || self.comments.next_id() as usize >= crate::model::MAX_COMMENTS {
            return Vec::new();
        }
        let id = self.comments.next_id();
        let mut ops = vec![TextOp::SetComment { id, comment: Some(c) }];
        ops.extend(self.comment_mark_ops(b0, o0, b1, o1, id, true));
        ops
    }

    /// Ops that add comment `id` to (or take it from) the runs of a range.
    pub fn comment_mark_ops(&mut self, b0: usize, o0: u32, b1: usize, o1: u32, id: crate::model::CommentId, on: bool) -> Vec<TextOp> {
        let mut ops = Vec::new();
        for b in b0..=b1 {
            let Some(p) = self.para(b) else { continue };
            let s = if b == b0 { o0 } else { 0 };
            let e = if b == b1 { o1 } else { p.text.len() as u32 };
            if s >= e {
                continue;
            }
            ops.extend(self.format_ops(b, s, e, |a| {
                let mut v: Vec<crate::model::CommentId> = a.comments.as_deref().unwrap_or(&[]).to_vec();
                v.retain(|&x| x != id);
                if on {
                    v.push(id);
                    v.sort_unstable();
                }
                a.comments = if v.is_empty() { None } else { Some(v.into()) };
            }));
        }
        ops
    }

    /// Ops that take out comment `id`, its marks on the text and its replies.
    pub fn delete_comment_ops(&mut self, id: crate::model::CommentId) -> Vec<TextOp> {
        let mut ids = vec![id];
        let mut k = 0;
        while k < ids.len() && ids.len() < crate::model::MAX_COMMENTS {
            for r in self.comments.replies(ids[k]) {
                if !ids.contains(&r) {
                    ids.push(r);
                }
            }
            k += 1;
        }
        // One pass over the text they cover, so no op undoes another's work.
        let ranges: Vec<_> = self.comment_ranges().into_iter().filter(|r| ids.contains(&r.0)).collect();
        let mut ops = Vec::new();
        if let (Some(a), Some(b)) = (ranges.iter().map(|r| r.1).min(), ranges.iter().map(|r| r.2).max()) {
            for blk in a.0..=b.0 {
                let Some(p) = self.para(blk) else { continue };
                let (s, e) = (if blk == a.0 { a.1 } else { 0 }, if blk == b.0 { b.1 } else { p.text.len() as u32 });
                if s >= e {
                    continue;
                }
                let gone = ids.clone();
                ops.extend(self.format_ops(blk, s, e, move |at| {
                    let v: Vec<crate::model::CommentId> = at.comments.as_deref().unwrap_or(&[]).iter().copied().filter(|x| !gone.contains(x)).collect();
                    at.comments = if v.is_empty() { None } else { Some(v.into()) };
                }));
            }
        }
        for &c in ids.iter().rev() {
            if self.comments.get(c).is_some() {
                ops.push(TextOp::SetComment { id: c, comment: None });
            }
        }
        ops
    }

    /// Ops that set note `id`'s text to `text` (a paragraph a line), keeping
    /// its kind; nothing if it is the same.
    pub fn set_note_text_ops(&mut self, id: crate::model::NoteId, text: &str) -> Vec<TextOp> {
        let Some(n) = self.notes.get(id) else { return Vec::new() };
        let kind = n.kind;
        let old: Vec<&str> = n.paras.iter().map(|p| p.text.as_str()).collect();
        if old.join("\n") == text {
            return Vec::new();
        }
        let paras = self.note_paras(text);
        vec![TextOp::SetNote { id, note: Some(crate::model::Note { kind, paras }) }]
    }

    /// A header or footer whose text is `text` (a paragraph a line), keeping
    /// what it can of `old`'s format: a changed line keeps the format round
    /// the change, a new line takes the last one's. None when `text` is
    /// empty. One plain line on a header that was one line stays one line.
    pub fn edit_margin(&mut self, old: Option<crate::model::HeaderFooter>, text: &str, footer: bool) -> Option<crate::model::HeaderFooter> {
        use crate::model::{Align, HeaderFooter, Para, MAX_MARGIN_PARAS};
        let text = text.trim_end_matches('\n');
        if text.trim().is_empty() {
            return None;
        }
        let align = old.as_ref().map_or(if footer { Align::Center } else { Align::Start }, |h| h.align);
        let old_paras = old.map(|h| h.paras).unwrap_or_default();
        if old_paras.is_empty() && !text.contains('\n') {
            return Some(HeaderFooter::line(text, align));
        }
        let plain = self.attrs.intern(crate::model::CharAttrs::default());
        let lines: Vec<&str> = text.split('\n').collect();
        let keep = lines.len().min(MAX_MARGIN_PARAS);
        let mut paras = Vec::with_capacity(keep);
        for i in 0..keep {
            let t = if i + 1 == keep && lines.len() > keep { lines[i..].join(" ") } else { lines[i].to_string() };
            let p = match (old_paras.get(i), old_paras.last()) {
                (Some(o), _) => o.retext(&t, &self.attrs, plain),
                (None, Some(last)) => {
                    let a = last.attr_at(last.text.len() as u32);
                    let x = self.attrs.get(a);
                    let a = if x.object.is_some() || x.note.is_some() || x.field.is_some() { plain } else { a };
                    let mut p = Para::new(self.new_id(), &t, last.style);
                    p.props = last.props.clone();
                    p.runs = vec![(0, a)];
                    p
                }
                (None, None) => {
                    let mut p = Para::new(self.new_id(), &t, 0);
                    p.runs = vec![(0, plain)];
                    p.props.align = Some(align);
                    p.props.space_before = Some(0);
                    p.props.space_after = Some(0);
                    p
                }
            };
            paras.push(p);
        }
        let words: Vec<String> = paras.iter().map(|p| p.text.replace(crate::model::OBJECT_CHAR, "")).collect();
        let text: String = words.iter().map(|w| w.trim()).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ");
        Some(HeaderFooter { text: text.chars().take(1000).collect(), align, paras })
    }

    /// A shape's paragraphs for `text` (a line a paragraph, at most
    /// [`crate::model::MAX_SHAPE_PARAS`]), keeping `old`'s format round each
    /// change; new lines take the last line's format, or `fresh` (an
    /// attribute and alignment) when there was none. No objects: the shape
    /// text is words only.
    pub fn shape_paras(&mut self, old: &[crate::model::Para], text: &str, fresh: (crate::model::CharAttrs, Option<crate::model::Align>)) -> Vec<crate::model::Para> {
        use crate::model::{Para, MAX_SHAPE_PARAS, OBJECT_CHAR};
        let text: String = text.trim_end_matches('\n').chars().filter(|&c| c != OBJECT_CHAR && (!c.is_control() || c == '\n' || c == '\t')).collect();
        if text.is_empty() {
            return Vec::new();
        }
        let plain = self.attrs.intern(fresh.0);
        let lines: Vec<&str> = text.split('\n').collect();
        let keep = lines.len().min(MAX_SHAPE_PARAS);
        let mut paras = Vec::with_capacity(keep);
        for i in 0..keep {
            let t = if i + 1 == keep && lines.len() > keep { lines[i..].join(" ") } else { lines[i].to_string() };
            let attrs = &self.attrs;
            let ok = |a: crate::model::AttrId| {
                let x = attrs.get(a);
                x.object.is_none() && x.note.is_none() && x.field.is_none() && x.deleted.is_none()
            };
            let p = match (old.get(i), old.last()) {
                (Some(o), _) => {
                    let mut p = o.retext(&t, &self.attrs, plain);
                    for r in p.runs.iter_mut() {
                        if !ok(r.1) {
                            r.1 = plain;
                        }
                    }
                    p
                }
                (None, Some(last)) => {
                    let a = last.attr_at(last.text.len() as u32);
                    let a = if ok(a) { a } else { plain };
                    let mut p = Para::new(self.new_id(), &t, last.style);
                    p.props = last.props.clone();
                    p.runs = vec![(0, a)];
                    p
                }
                (None, None) => {
                    let mut p = Para::new(self.new_id(), &t, 0);
                    p.runs = vec![(0, plain)];
                    p.props.align = fresh.1;
                    p.props.space_before = Some(0);
                    p.props.space_after = Some(0);
                    p
                }
            };
            paras.push(p);
        }
        paras
    }

    /// Ops that delete from (`b0`, `o0`) to (`b1`, `o1`), across paragraphs.
    pub fn delete_ops(&self, b0: usize, o0: u32, b1: usize, o1: u32) -> Vec<TextOp> {
        if (b1, o1) <= (b0, o0) {
            return Vec::new();
        }
        if b0 == b1 {
            return vec![TextOp::Splice { block: b0 as u32, at: o0, del: o1 - o0, ins: Fragment::default() }];
        }
        let len0 = self.para(b0).map_or(0, |p| p.text.len() as u32);
        let mut ops = vec![TextOp::Splice { block: b0 as u32, at: o0, del: len0 - o0, ins: Fragment::default() }];
        if b1 > b0 + 1 {
            ops.push(TextOp::Remove { at: b0 as u32 + 1, n: (b1 - b0 - 1) as u32 });
        }
        // The last paragraph's head goes, then it joins the first.
        ops.push(TextOp::Splice { block: b0 as u32 + 1, at: 0, del: o1, ins: Fragment::default() });
        ops.push(TextOp::Join { block: b0 as u32 });
        ops
    }

    /// Ops that make `start..end` of paragraph `block` take `f` of each run's
    /// attributes (bold on, a size, …), interning what they need.
    pub fn format_ops(&mut self, block: usize, start: u32, end: u32, f: impl Fn(&mut crate::model::CharAttrs)) -> Vec<TextOp> {
        if self.para(block).is_none() {
            return Vec::new();
        }
        let runs: Vec<(u32, AttrId)> = self
            .runs_in(block, start, end)
            .into_iter()
            .map(|(s, a)| {
                let mut x = self.attrs.get(a).clone();
                f(&mut x);
                (s, self.attrs.intern(x))
            })
            .collect();
        vec![TextOp::Restyle { block: block as u32, start, end, runs }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::CharAttrs;
    use officina_core::history::History;

    fn snapshot(d: &TextDoc) -> Vec<(u64, String, Vec<(u32, AttrId)>, StyleId)> {
        d.blocks.iter().map(|b| b.as_para().unwrap()).map(|p| (p.id, p.text.clone(), p.runs.clone(), p.style)).collect()
    }

    #[test]
    fn shapes_set_undo_and_bad_ones_are_refused() {
        use crate::model::{Geom, ObjectRef, Para, ShapeData, OBJECT_CHAR};
        let mut d = TextDoc::new();
        let mut sd = ShapeData::new(Geom::preset("rect"));
        sd.paras.push(Para::new(d.new_id(), "inside", 0));
        let inv = TextOp::SetShape { id: 0, shape: Some(sd.clone()) }.apply(&mut d);
        assert_eq!(inv, TextOp::SetShape { id: 0, shape: None });
        assert_eq!(d.shapes.get(0).map(|s| (**s).clone()), Some(sd.clone()));
        inv.apply(&mut d);
        assert!(d.shapes.get(0).is_none());
        // A shape whose text holds an object (a shape in a shape).
        let a = d.attrs.intern(CharAttrs { object: Some(ObjectRef { image: 0, width: 10, height: 10, float: None, shape: true }), ..Default::default() });
        let mut bad = sd.clone();
        bad.paras = vec![Para { id: d.new_id(), text: OBJECT_CHAR.to_string(), runs: vec![(0, a)], style: 0, props: Default::default() }];
        assert_eq!(TextOp::SetShape { id: 0, shape: Some(bad) }.apply(&mut d), TextOp::Nop);
        // A run with an attribute the document lacks.
        let mut bad = sd;
        bad.paras[0].runs = vec![(0, 9_999)];
        assert_eq!(TextOp::SetShape { id: 0, shape: Some(bad) }.apply(&mut d), TextOp::Nop);
    }

    #[test]
    fn a_style_in_the_middle_goes_and_comes_back() {
        use crate::model::{Style, StyleKind};
        let mut d = doc();
        let n = d.styles.styles.len() as u16;
        let st = |name: &str, kind, parent, next| Style { name: name.into(), kind, parent, next, para: ParaProps::default(), chars: CharAttrs::default() };
        // Unused U, then a character style C and a paragraph style Q after it.
        for (i, s) in [st("U", StyleKind::Para, Some(0), None), st("C", StyleKind::Char, None, None), st("Q", StyleKind::Para, Some(0), Some(n + 2))].into_iter().enumerate() {
            assert_ne!(TextOp::SetStyle { id: n + i as u16, style: Some(s) }.apply(&mut d), TextOp::Nop);
        }
        let props = d.para(0).unwrap().props.clone();
        TextOp::SetPara { block: 0, style: n + 2, props }.apply(&mut d);
        let c = d.attrs.intern(CharAttrs { style: Some(n + 1), ..Default::default() });
        let mut runs = d.para(0).unwrap().runs.clone();
        runs[0].1 = c;
        d.blocks.update(0, |b| b.as_para_mut().unwrap().runs = runs);
        // An attribute entry nothing uses that names U.
        let stale = d.attrs.intern(CharAttrs { style: Some(n), bold: Some(true), ..Default::default() });
        let snap = |d: &TextDoc| (d.styles.styles.clone(), d.para(0).unwrap().style, d.attrs.get(c).style, d.attrs.get(stale).style);
        let before = snap(&d);
        // C is used: it stays. U goes; Q and C move down one.
        assert_eq!(TextOp::PlaceStyle { id: n + 1, style: None, stale: vec![] }.apply(&mut d), TextOp::Nop);
        let inv = TextOp::PlaceStyle { id: n, style: None, stale: vec![] }.apply(&mut d);
        assert_eq!(inv, TextOp::PlaceStyle { id: n, style: Some(before.0[n as usize].clone()), stale: vec![stale] });
        assert_eq!(d.styles.styles.len(), n as usize + 2);
        assert_eq!((d.para(0).unwrap().style, d.attrs.get(c).style, d.attrs.get(stale).style), (n + 1, Some(n), None));
        assert_eq!((&*d.styles.styles[n as usize + 1].name, d.styles.styles[n as usize + 1].next), ("Q", Some(n + 1)));
        assert_eq!(d.attrs.intern(CharAttrs { style: Some(n), ..Default::default() }), c, "the lookup follows");
        // Back: every id as it was.
        let again = inv.apply(&mut d);
        assert_eq!(again, TextOp::PlaceStyle { id: n, style: None, stale: vec![] });
        assert_eq!(snap(&d), before);
        // Refused: Normal, a gap, a taken name, a base of another kind.
        let bad = |d: &mut TextDoc, id: u16, s: Option<Style>| assert_eq!(TextOp::PlaceStyle { id, style: s, stale: vec![] }.apply(d), TextOp::Nop, "{id}");
        bad(&mut d, 0, None);
        bad(&mut d, 0, Some(st("Z", StyleKind::Para, None, None)));
        bad(&mut d, n + 4, Some(st("Z", StyleKind::Para, None, None)));
        bad(&mut d, 1, Some(st("Q", StyleKind::Para, None, None)));
        bad(&mut d, 1, Some(st("Z", StyleKind::Para, Some(n + 2), None)));
        // The op through the journal's bytes.
        let op = TextOp::PlaceStyle { id: n, style: Some(before.0[n as usize].clone()), stale: vec![stale] };
        assert_eq!(crate::native::decode_op(&crate::native::encode_op(&op), d.attrs.len(), d.styles.styles.len()).unwrap(), op);
    }

    #[test]
    fn styles_change_add_and_refuse_bad_ones() {
        use crate::model::{Style, StyleKind};
        let mut d = doc();
        let n = d.styles.styles.len();
        let normal = d.styles.styles[0].clone();
        // Normal made bigger: its paragraphs are laid out again.
        let big = Style { chars: CharAttrs { size: Some(30), ..Default::default() }, ..normal.clone() };
        let inv = TextOp::SetStyle { id: 0, style: Some(big.clone()) }.apply(&mut d);
        assert_eq!(inv, TextOp::SetStyle { id: 0, style: Some(normal.clone()) });
        assert_eq!(d.styles.styles[0], big);
        inv.apply(&mut d);
        assert_eq!(d.styles.styles[0], normal);
        // A new one after the last; taking it out again.
        let quote = Style { name: "Quote".into(), kind: StyleKind::Para, parent: Some(0), next: Some(n as u16), para: ParaProps::default(), chars: CharAttrs { italic: Some(true), ..Default::default() } };
        let inv = TextOp::SetStyle { id: n as u16, style: Some(quote.clone()) }.apply(&mut d);
        assert_eq!(inv, TextOp::SetStyle { id: n as u16, style: None });
        // Used by a paragraph, it can't go.
        let p = d.para(0).unwrap().props.clone();
        TextOp::SetPara { block: 0, style: n as u16, props: p.clone() }.apply(&mut d);
        assert_eq!(inv.clone().apply(&mut d), TextOp::Nop);
        TextOp::SetPara { block: 0, style: 0, props: p }.apply(&mut d);
        inv.apply(&mut d);
        assert_eq!(d.styles.styles.len(), n);
        // Refused: a name taken, an empty name, a gap, its own parent, a
        // loop, a kind change, Normal based on something, Normal removed.
        let bad = |d: &mut TextDoc, id: usize, s: Option<Style>| assert_eq!(TextOp::SetStyle { id: id as u16, style: s }.apply(d), TextOp::Nop, "{id}");
        bad(&mut d, n, Some(Style { name: "Heading 1".into(), ..quote.clone() }));
        bad(&mut d, n, Some(Style { name: " ".into(), ..quote.clone() }));
        bad(&mut d, n + 1, Some(quote.clone()));
        bad(&mut d, n, Some(Style { parent: Some(n as u16), ..quote.clone() }));
        bad(&mut d, 0, Some(Style { parent: Some(1), ..normal.clone() }));
        let h1 = d.styles.styles[1].clone();
        bad(&mut d, 1, Some(Style { kind: StyleKind::Char, ..h1.clone() }));
        // Heading 1 based on Heading 2, Heading 2 already on Normal: fine;
        // then Heading 2 on Heading 1 would loop.
        assert_ne!(TextOp::SetStyle { id: 1, style: Some(Style { parent: Some(2), ..h1 }) }.apply(&mut d), TextOp::Nop);
        let h2 = d.styles.styles[2].clone();
        bad(&mut d, 2, Some(Style { parent: Some(1), ..h2 }));
        bad(&mut d, 0, None);
        bad(&mut d, 1, None);
    }

    #[test]
    fn comments_add_overlap_delete_and_undo() {
        use crate::model::Comment;
        let mut d = doc();
        d.push_para("Third one here", 0, vec![]);
        let before = snapshot(&d);
        let mut h: History<TextOp> = History::new();
        let c = |t: &str| Comment { author: "A".into(), text: t.into(), ..Default::default() };
        let ops = d.add_comment_ops(0, 2, 1, 6, c("first"));
        h.run(&mut d, "Comment", ops);
        let ops = d.add_comment_ops(1, 0, 2, 5, c("second"));
        h.run(&mut d, "Comment", ops);
        h.run(&mut d, "Reply", vec![TextOp::SetComment { id: 2, comment: Some(Comment { parent: Some(0), ..c("reply") }) }]);
        assert_eq!(d.comment_ranges(), vec![(0, (0, 2), (1, 6)), (1, (1, 0), (2, 5))]);
        assert_eq!(d.live_comments(), vec![0, 2, 1]);
        // Delete the first with its reply; the second's marks stay whole.
        let ops = d.delete_comment_ops(0);
        h.run(&mut d, "Delete Comment", ops);
        assert_eq!(d.comment_ranges(), vec![(1, (1, 0), (2, 5))]);
        assert!(d.comments.get(2).is_none());
        h.undo(&mut d);
        assert_eq!(d.live_comments(), vec![0, 2, 1]);
        h.undo(&mut d);
        h.undo(&mut d);
        h.undo(&mut d);
        assert_eq!(snapshot(&d), before);
        assert!(d.comment_ranges().is_empty());
    }

    fn doc() -> TextDoc {
        let mut d = TextDoc::new();
        d.blocks.update(0, |b| b.as_para_mut().unwrap().text.push_str("Hello, wörld"));
        d.push_para("Second paragraph", 0, vec![]);
        d.push_para("Third", 1, vec![]);
        d
    }

    #[test]
    fn typing_bold_split_join_and_undo_all() {
        let mut d = doc();
        let before = snapshot(&d);
        let mut h: History<TextOp> = History::new();
        // Bold "wörld".
        let ops = d.format_ops(0, 7, 13, |a| a.bold = Some(true));
        h.run(&mut d, "Bold", ops);
        let bold = d.attrs.intern(CharAttrs { bold: Some(true), ..Default::default() });
        assert_eq!(d.para(0).unwrap().runs, vec![(0, 0), (7, bold)]);
        // Typing at the end of the bold word continues bold.
        h.run(&mut d, "Typing", vec![TextOp::Splice { block: 0, at: 13, del: 0, ins: Fragment { text: "!".into(), runs: vec![] } }]);
        assert_eq!(d.para(0).unwrap().text, "Hello, wörld!");
        assert_eq!(d.para(0).unwrap().attr_at(13), bold);
        // Typing at the start takes the paragraph's first run.
        h.run(&mut d, "Typing", vec![TextOp::Splice { block: 0, at: 0, del: 0, ins: Fragment { text: ">".into(), runs: vec![] } }]);
        assert_eq!(d.para(0).unwrap().runs, vec![(0, 0), (8, bold)]);
        // Enter in the middle of the bold word.
        let id = d.new_id();
        h.run(&mut d, "Enter", vec![TextOp::Split { block: 0, at: 9, id, style: 0, props: ParaProps::default() }]);
        assert_eq!(d.para(0).unwrap().text, ">Hello, w");
        assert_eq!(d.para(1).unwrap().text, "örld!");
        assert_eq!(d.para(1).unwrap().runs, vec![(0, bold)]);
        assert_eq!(d.blocks.len(), 4);
        // Delete across paragraphs: from "Hel|lo" to "Sec|ond".
        let ops = d.delete_ops(0, 4, 2, 3);
        h.run(&mut d, "Delete", ops);
        assert_eq!(d.para(0).unwrap().text, ">Helond paragraph");
        assert_eq!(d.blocks.len(), 2);
        assert_eq!(d.blocks.counts().chars, ">Helond paragraph".len() as u64 + 1 + 5 + 1);
        // Heading style on the last.
        h.run(&mut d, "Style", vec![TextOp::SetPara { block: 1, style: 2, props: ParaProps::default() }]);
        while h.undo(&mut d).is_some() {}
        assert_eq!(snapshot(&d), before);
        while h.redo(&mut d).is_some() {}
        assert_eq!(d.para(0).unwrap().text, ">Helond paragraph");
        assert_eq!(d.para(1).unwrap().style, 2);
    }

    #[test]
    fn ops_that_dont_fit_do_nothing() {
        let mut d = doc();
        let before = snapshot(&d);
        for op in [
            TextOp::Splice { block: 9, at: 0, del: 0, ins: Fragment::default() },
            TextOp::Splice { block: 0, at: 9, del: 1, ins: Fragment::default() }, // inside "ö"
            TextOp::Splice { block: 0, at: 0, del: 999, ins: Fragment::default() },
            TextOp::Restyle { block: 0, start: 5, end: 2, runs: vec![] },
            TextOp::Split { block: 0, at: 99, id: 50, style: 0, props: ParaProps::default() },
            TextOp::Join { block: 2 },
            TextOp::SetPara { block: 0, style: 999, props: ParaProps::default() },
            TextOp::Remove { at: 0, n: 3 },
            TextOp::Insert { at: 9, blocks: vec![] },
        ] {
            assert_eq!(op.apply(&mut d), TextOp::Nop);
        }
        assert_eq!(snapshot(&d), before);
    }

    #[test]
    fn random_edits_undo_exactly() {
        let mut d = doc();
        let before = snapshot(&d);
        let mut h: History<TextOp> = History::new();
        h.limit = 100_000;
        let mut x: u64 = 77;
        let mut rnd = |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n.max(1)
        };
        let words = ["a", "é", "日本", " ", "xyz"];
        for _ in 0..3000 {
            let b = rnd(d.blocks.len() as u64) as usize;
            let t = d.para(b).unwrap().text.clone();
            let cuts: Vec<u32> = t.char_indices().map(|(i, _)| i as u32).chain([t.len() as u32]).collect();
            let at = cuts[rnd(cuts.len() as u64) as usize];
            let to = cuts[rnd(cuts.len() as u64) as usize].max(at);
            let ops = match rnd(6) {
                0 | 1 => vec![TextOp::Splice { block: b as u32, at, del: to - at, ins: Fragment { text: words[rnd(5) as usize].into(), runs: vec![] } }],
                2 => d.format_ops(b, at, to, |a| a.italic = Some(!a.italic.unwrap_or(false))),
                3 => {
                    let id = d.new_id();
                    vec![TextOp::Split { block: b as u32, at, id, style: rnd(3) as u16, props: ParaProps::default() }]
                }
                4 if b + 1 < d.blocks.len() => vec![TextOp::Join { block: b as u32 }],
                _ => {
                    let b1 = (b + rnd(3) as usize).min(d.blocks.len() - 1);
                    let o1 = if b1 == b { to } else { d.para(b1).unwrap().text.char_indices().map(|(i, _)| i as u32).next().unwrap_or(0) };
                    d.delete_ops(b, at, b1, o1)
                }
            };
            h.run(&mut d, "Edit", ops);
            for b in d.blocks.iter() {
                let p = b.as_para().unwrap();
                assert!(p.runs[0].0 == 0 && p.runs.windows(2).all(|w| w[0].0 < w[1].0 && w[0].1 != w[1].1) && p.runs.iter().all(|r| p.text.is_char_boundary(r.0 as usize)), "{p:?}");
            }
        }
        while h.undo(&mut d).is_some() {}
        assert_eq!(snapshot(&d), before);
    }
}
