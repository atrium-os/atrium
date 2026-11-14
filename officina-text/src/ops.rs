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
    /// Nothing (what an op that didn't fit becomes).
    Nop,
}

fn boundary(s: &str, at: u32) -> bool {
    s.is_char_boundary(at as usize)
}

impl TextDoc {
    /// The runs of `start..end` in paragraph `block`, clipped and absolute.
    fn runs_in(&self, block: usize, start: u32, end: u32) -> Vec<(u32, AttrId)> {
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
            TextOp::SetPage { .. } => Edit { first: 0, last: 0, delta: 0 },
        })
    }
}

impl Op<TextDoc> for TextOp {
    fn apply(self, d: &mut TextDoc) -> TextOp {
        let inv = self.apply_inner(d);
        if let Some(e) = inv.edit_of_inverse() {
            d.note(e);
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
