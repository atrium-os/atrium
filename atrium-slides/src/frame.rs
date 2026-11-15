//! Editing the text of one frame on a slide: a caret and selection over its
//! paragraphs, typing, deleting, new paragraphs, outline levels and bold,
//! italic and underline. Every change is a `TextOp` on the frame inside a
//! [`DeckOp::Text`], so undo and the journal cover it; geometry comes from the
//! frame's layout as it is drawn ([`officina_slides::draw::frame_geometry`]),
//! so the caret sits exactly on the glyphs.

use officina_slides::draw::{self, FrameGeom, Xform};
use officina_slides::model::{find, find_mut, Deck, ShapeId, SlideId, MAX_LEVELS};
use officina_slides::ops::{DeckOp, Target};
use officina_slides::resolve::Owner;
use officina_text::layout::Fonts;
use officina_text::model::{AttrId, CharAttrs, TextDoc};
use officina_text::ops::{Fragment, TextOp};

use crate::doc::Doc;

/// A place in a frame: a paragraph and a byte offset in its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Pos {
    pub block: usize,
    pub at: u32,
}

/// The frame being edited.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameEdit {
    pub slide: SlideId,
    pub shape: ShapeId,
    pub caret: Pos,
    pub anchor: Pos,
    /// The x the caret keeps going up and down (layout units).
    pub goal_x: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Bold,
    Italic,
    Underline,
}

fn text_of(deck: &mut Deck, slide: SlideId, shape: ShapeId) -> Option<TextDoc> {
    let i = deck.slide_index(slide)?;
    find(&deck.slide(i).ok()?.shapes, shape)?.text().map(|t| t.doc.clone())
}

fn para_len(d: &TextDoc, b: usize) -> u32 {
    d.para(b).map_or(0, |p| p.text.len() as u32)
}

fn prev_char(s: &str, at: u32) -> u32 {
    s[..at as usize].char_indices().next_back().map_or(0, |(i, _)| i as u32)
}

fn next_char(s: &str, at: u32) -> u32 {
    s[at as usize..].chars().next().map_or(at, |c| at + c.len_utf8() as u32)
}

impl FrameEdit {
    pub fn new(slide: SlideId, shape: ShapeId, at: Pos) -> FrameEdit {
        FrameEdit { slide, shape, caret: at, anchor: at, goal_x: None }
    }

    pub fn has_selection(&self) -> bool {
        self.caret != self.anchor
    }

    pub fn selection(&self) -> (Pos, Pos) {
        if self.caret <= self.anchor {
            (self.caret, self.anchor)
        } else {
            (self.anchor, self.caret)
        }
    }

    /// The frame's geometry as drawn now.
    pub fn geometry(&self, deck: &mut Deck, fonts: &dyn Fonts) -> Option<FrameGeom> {
        let i = deck.slide_index(self.slide)?;
        deck.load_slide(i).ok()?;
        let s = deck.slide_if_loaded(i)?;
        let owner = Owner::Slide(s.layout);
        let base = draw::parent_xform(deck, owner, &s.shapes, self.shape)?;
        let shape = find(&s.shapes, self.shape)?;
        draw::frame_geometry(deck, owner, shape, &base, fonts)
    }

    /// Keeps the caret and anchor inside the frame's text.
    pub fn clamp(&mut self, d: &TextDoc) {
        let n = d.blocks.len().max(1);
        for p in [&mut self.caret, &mut self.anchor] {
            p.block = p.block.min(n - 1);
            let t = d.para(p.block).map_or("", |x| x.text.as_str());
            p.at = p.at.min(t.len() as u32);
            while !t.is_char_boundary(p.at as usize) {
                p.at -= 1;
            }
        }
    }

    fn op(&self, op: TextOp) -> DeckOp {
        DeckOp::Text { target: Target::Slide(self.slide), shape: self.shape, op }
    }

    /// The ops that delete the selection, and where the caret goes.
    fn delete_ops(&self, d: &TextDoc) -> (Vec<DeckOp>, Pos) {
        let (a, b) = self.selection();
        if a == b {
            return (Vec::new(), a);
        }
        let mut ops = Vec::new();
        if a.block == b.block {
            ops.push(self.op(TextOp::Splice { block: a.block as u32, at: a.at, del: b.at - a.at, ins: Fragment::default() }));
        } else {
            ops.push(self.op(TextOp::Splice { block: b.block as u32, at: 0, del: b.at, ins: Fragment::default() }));
            let middle = b.block - a.block - 1;
            if middle > 0 {
                ops.push(self.op(TextOp::Remove { at: a.block as u32 + 1, n: middle as u32 }));
            }
            let la = para_len(d, a.block);
            ops.push(self.op(TextOp::Splice { block: a.block as u32, at: a.at, del: la - a.at, ins: Fragment::default() }));
            ops.push(self.op(TextOp::Join { block: a.block as u32 }));
        }
        (ops, a)
    }

    /// Types `s` over the selection. Returns the ops run.
    pub fn insert(&mut self, doc: &mut Doc, s: &str, extend: bool) -> bool {
        let Some(d) = text_of(&mut doc.deck, self.slide, self.shape) else { return false };
        let (mut ops, at) = self.delete_ops(&d);
        // The typed text takes the formatting before the caret.
        let attr: AttrId = d.para(at.block).map_or(0, |p| p.attr_at(if at.at > 0 { prev_char(&p.text, at.at) } else { 0 }));
        let mut pos = at;
        let mut first = true;
        for line in s.split('\n') {
            if !first {
                let style = d.para(pos.block).map_or(0, |p| p.style);
                let props = d.para(pos.block).map(|p| p.props.clone()).unwrap_or_default();
                let id = fresh_id(&mut doc.deck, self.slide, self.shape);
                ops.push(self.op(TextOp::Split { block: pos.block as u32, at: pos.at, id, style, props }));
                pos = Pos { block: pos.block + 1, at: 0 };
            }
            first = false;
            let line = line.trim_end_matches('\r');
            if !line.is_empty() {
                ops.push(self.op(TextOp::Splice { block: pos.block as u32, at: pos.at, del: 0, ins: Fragment { text: line.to_string(), runs: vec![(0, attr)] } }));
                pos.at += line.len() as u32;
            }
        }
        if ops.is_empty() {
            return false;
        }
        if extend && !self.has_selection() && !s.contains('\n') {
            doc.typing("Typing", self.slide, self.shape, ops);
        } else {
            doc.run("Typing", ops);
        }
        self.caret = pos;
        self.anchor = pos;
        self.goal_x = None;
        true
    }

    /// Enter: a new paragraph at the same level.
    pub fn new_paragraph(&mut self, doc: &mut Doc) -> bool {
        self.insert(doc, "\n", false)
    }

    /// Backspace, or Delete (`forward`).
    pub fn delete(&mut self, doc: &mut Doc, forward: bool) -> bool {
        let Some(d) = text_of(&mut doc.deck, self.slide, self.shape) else { return false };
        if !self.has_selection() {
            let c = self.caret;
            let t = d.para(c.block).map_or("", |p| p.text.as_str());
            if forward {
                if c.at as usize >= t.len() {
                    if c.block + 1 >= d.blocks.len() {
                        return false;
                    }
                    self.anchor = Pos { block: c.block + 1, at: 0 };
                } else {
                    self.anchor = Pos { block: c.block, at: next_char(t, c.at) };
                }
            } else if c.at == 0 {
                if c.block == 0 {
                    return false;
                }
                // At a paragraph's start: up a level first, then join.
                let lvl = d.para(c.block).map_or(0, |p| p.style);
                if lvl > 0 {
                    return self.level(doc, -1);
                }
                self.anchor = Pos { block: c.block - 1, at: para_len(&d, c.block - 1) };
            } else {
                self.anchor = Pos { block: c.block, at: prev_char(t, c.at) };
            }
        }
        let (ops, at) = self.delete_ops(&d);
        doc.typing("Delete", self.slide, self.shape, ops);
        self.caret = at;
        self.anchor = at;
        self.goal_x = None;
        true
    }

    /// Tab and Shift+Tab: the selected paragraphs a level down or up.
    pub fn level(&mut self, doc: &mut Doc, delta: i32) -> bool {
        let Some(d) = text_of(&mut doc.deck, self.slide, self.shape) else { return false };
        let (a, b) = self.selection();
        let mut ops = Vec::new();
        for blk in a.block..=b.block {
            let Some(p) = d.para(blk) else { continue };
            let lvl = (p.style as i32 + delta).clamp(0, MAX_LEVELS as i32 - 1) as u16;
            if lvl != p.style {
                ops.push(self.op(TextOp::SetPara { block: blk as u32, style: lvl, props: p.props.clone() }));
            }
        }
        if ops.is_empty() {
            return false;
        }
        doc.run(if delta > 0 { "Demote" } else { "Promote" }, ops);
        true
    }

    /// Bold, italic or underline on the selection (or the word's typing
    /// format when nothing is selected: on the whole paragraph, as a frame's
    /// text is short).
    pub fn toggle(&mut self, doc: &mut Doc, m: Mark) -> bool {
        let Some(d) = text_of(&mut doc.deck, self.slide, self.shape) else { return false };
        let (a, b) = if self.has_selection() { self.selection() } else { (Pos { block: self.caret.block, at: 0 }, Pos { block: self.caret.block, at: para_len(&d, self.caret.block) }) };
        let get = |x: &CharAttrs| match m {
            Mark::Bold => x.bold,
            Mark::Italic => x.italic,
            Mark::Underline => x.underline,
        };
        let first = d.para(a.block).map(|p| d.attrs.get(p.attr_at(a.at)).clone()).unwrap_or_default();
        let on = get(&first) != Some(true);
        // The new attribute sets, interned in the frame's table first.
        let mut ops = Vec::new();
        let mut table = d.attrs.clone();
        for blk in a.block..=b.block {
            let Some(p) = d.para(blk) else { continue };
            let (s, e) = (if blk == a.block { a.at } else { 0 }, if blk == b.block { b.at } else { p.text.len() as u32 });
            if s >= e {
                continue;
            }
            let runs: Vec<(u32, AttrId)> = d
                .runs_in(blk, s, e)
                .into_iter()
                .map(|(at, id)| {
                    let mut x = d.attrs.get(id).clone();
                    match m {
                        Mark::Bold => x.bold = Some(on),
                        Mark::Italic => x.italic = Some(on),
                        Mark::Underline => x.underline = Some(on),
                    }
                    (at, table.intern(x))
                })
                .collect();
            ops.push(self.op(TextOp::Restyle { block: blk as u32, start: s, end: e, runs }));
        }
        if ops.is_empty() {
            return false;
        }
        // The table only grows; ids already in use keep their meaning.
        if let Some(i) = doc.deck.slide_index(self.slide) {
            if let Ok(s) = doc.deck.slide_mut(i) {
                if let Some(t) = find_mut(&mut s.shapes, self.shape).and_then(|s| s.text_mut()) {
                    t.doc.attrs = table;
                }
            }
        }
        doc.run(
            match m {
                Mark::Bold => "Bold",
                Mark::Italic => "Italic",
                Mark::Underline => "Underline",
            },
            ops,
        );
        true
    }

    /// Moves the caret (`extend`: the selection's end).
    pub fn motion(&mut self, deck: &mut Deck, fonts: &dyn Fonts, m: Motion, extend: bool) {
        let Some(d) = text_of(deck, self.slide, self.shape) else { return };
        let c = self.caret;
        let t = d.para(c.block).map_or(String::new(), |p| p.text.clone());
        let mut keep_goal = false;
        let to = match m {
            Motion::Left if !extend && self.has_selection() => self.selection().0,
            Motion::Right if !extend && self.has_selection() => self.selection().1,
            Motion::Left if c.at > 0 => Pos { block: c.block, at: prev_char(&t, c.at) },
            Motion::Left if c.block > 0 => Pos { block: c.block - 1, at: para_len(&d, c.block - 1) },
            Motion::Right if (c.at as usize) < t.len() => Pos { block: c.block, at: next_char(&t, c.at) },
            Motion::Right if c.block + 1 < d.blocks.len() => Pos { block: c.block + 1, at: 0 },
            Motion::Home | Motion::End | Motion::Up | Motion::Down => {
                let Some(g) = self.geometry(deck, fonts) else { return };
                let Some((pl, _)) = g.layout.paras.get(c.block) else { return };
                let li = pl.line_of(c.at);
                match m {
                    Motion::Home => Pos { block: c.block, at: pl.lines[li].start },
                    Motion::End => {
                        let l = &pl.lines[li];
                        // Before a soft break's trailing space, after the last line's end.
                        let end = if li + 1 < pl.lines.len() { prev_char(&t, l.end.max(l.start + 1).min(t.len() as u32)).max(l.start) } else { t.len() as u32 };
                        Pos { block: c.block, at: end }
                    }
                    _ => {
                        keep_goal = true;
                        let gx = self.goal_x.unwrap_or_else(|| caret_x(&g, c));
                        self.goal_x = Some(gx);
                        let up = m == Motion::Up;
                        let (nb, nl) = if up {
                            if li > 0 {
                                (c.block, li - 1)
                            } else if c.block > 0 {
                                (c.block - 1, g.layout.paras[c.block - 1].0.lines.len().saturating_sub(1))
                            } else {
                                (0, usize::MAX)
                            }
                        } else if li + 1 < pl.lines.len() {
                            (c.block, li + 1)
                        } else if c.block + 1 < g.layout.paras.len() {
                            (c.block + 1, 0)
                        } else {
                            (c.block, usize::MAX - 1)
                        };
                        match nl {
                            usize::MAX => Pos { block: 0, at: 0 },
                            x if x == usize::MAX - 1 => Pos { block: c.block, at: t.len() as u32 },
                            _ => at_x(&g, nb, nl, gx),
                        }
                    }
                }
            }
            Motion::DocStart => Pos::default(),
            Motion::DocEnd => {
                let b = d.blocks.len().saturating_sub(1);
                Pos { block: b, at: para_len(&d, b) }
            }
            _ => c,
        };
        self.caret = to;
        if !extend {
            self.anchor = to;
        }
        if !keep_goal {
            self.goal_x = None;
        }
    }

    pub fn select_all(&mut self, deck: &mut Deck) {
        let Some(d) = text_of(deck, self.slide, self.shape) else { return };
        let b = d.blocks.len().saturating_sub(1);
        self.anchor = Pos::default();
        self.caret = Pos { block: b, at: para_len(&d, b) };
    }

    /// The selected text, paragraphs joined by `\n`.
    pub fn selected_text(&self, deck: &mut Deck) -> String {
        let Some(d) = text_of(deck, self.slide, self.shape) else { return String::new() };
        let (a, b) = self.selection();
        let mut out = Vec::new();
        for blk in a.block..=b.block {
            let t = d.para(blk).map_or("", |p| p.text.as_str());
            let s = if blk == a.block { a.at as usize } else { 0 };
            let e = if blk == b.block { b.at as usize } else { t.len() };
            out.push(t.get(s..e).unwrap_or(""));
        }
        out.join("\n")
    }
}

fn fresh_id(deck: &mut Deck, slide: SlideId, shape: ShapeId) -> u64 {
    let Some(i) = deck.slide_index(slide) else { return 1 };
    let Ok(s) = deck.slide_mut(i) else { return 1 };
    find_mut(&mut s.shapes, shape).and_then(|s| s.text_mut()).map_or(1, |t| t.doc.new_id())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    DocStart,
    DocEnd,
}

/// The caret's x in layout units from the frame's start edge.
pub fn caret_x(g: &FrameGeom, p: Pos) -> i64 {
    let Some((pl, _)) = g.layout.paras.get(p.block) else { return 0 };
    if pl.lines.is_empty() {
        return 0;
    }
    let li = pl.line_of(p.at);
    let l = &pl.lines[li];
    let text = g.layout.doc.para(p.block).map_or("", |x| x.text.as_str());
    let mut x = l.x;
    for gl in &pl.glyphs[l.glyphs.clone()] {
        if gl.cluster >= p.at {
            break;
        }
        x += gl.adv;
        if text.as_bytes().get(gl.cluster as usize) == Some(&b' ') {
            x += l.space_extra;
        }
    }
    x
}

/// The place on line `li` of paragraph `block` nearest `x`.
pub fn at_x(g: &FrameGeom, block: usize, li: usize, x: i64) -> Pos {
    let Some((pl, _)) = g.layout.paras.get(block) else { return Pos { block, at: 0 } };
    let Some(l) = pl.lines.get(li) else { return Pos { block, at: 0 } };
    let text = g.layout.doc.para(block).map_or("", |x| x.text.as_str());
    let mut pen = l.x;
    let mut best = l.start;
    for gl in &pl.glyphs[l.glyphs.clone()] {
        let w = gl.adv + if text.as_bytes().get(gl.cluster as usize) == Some(&b' ') { l.space_extra } else { 0 };
        if x < pen + w / 2 {
            return Pos { block, at: gl.cluster.max(l.start) };
        }
        pen += w;
        best = next_char(text, gl.cluster).min(l.end);
    }
    // Past the end: before a soft break's space, or the paragraph's end.
    if li + 1 < pl.lines.len() {
        best = prev_char(text, l.end).max(l.start);
    }
    Pos { block, at: best }
}

/// The caret's line as (top, bottom) in layout units from the frame's top,
/// and its x.
pub fn caret_line(g: &FrameGeom, p: Pos) -> (i64, i64, i64) {
    let Some((pl, top)) = g.layout.paras.get(p.block) else { return (0, 0, 0) };
    if pl.lines.is_empty() {
        return (*top, *top + 1, 0);
    }
    let li = pl.line_of(p.at);
    let y: i64 = top + pl.lines[..li].iter().map(|l| l.height).sum::<i64>();
    let l = &pl.lines[li];
    (y + l.skip, y + l.height, caret_x(g, p))
}

/// The place under slide point (`x`, `y`) EMU, in the frame `g` lays out.
pub fn hit(g: &FrameGeom, x: f64, y: f64) -> Pos {
    let inv = invert(&g.xf);
    let (lx, ly) = draw::apply(&inv, x, y);
    let (lx, ly) = (lx as i64, ly as i64);
    let n = g.layout.paras.len();
    for (b, (pl, top)) in g.layout.paras.iter().enumerate() {
        let bottom = top + pl.height;
        if ly < bottom || b + 1 == n {
            let mut yy = *top;
            for (li, l) in pl.lines.iter().enumerate() {
                if ly < yy + l.height || li + 1 == pl.lines.len() {
                    return at_x(g, b, li, lx);
                }
                yy += l.height;
            }
            return Pos { block: b, at: 0 };
        }
    }
    Pos::default()
}

/// An affine map's inverse (the identity if it has none).
pub fn invert(m: &Xform) -> Xform {
    let det = m[0] * m[3] - m[1] * m[2];
    if det.abs() < 1e-12 {
        return draw::IDENTITY;
    }
    let (a, b, c, d) = (m[3] / det, -m[1] / det, -m[2] / det, m[0] / det);
    [a, b, c, d, -(a * m[4] + c * m[5]), -(b * m[4] + d * m[5])]
}

#[cfg(test)]
mod tests {
    use super::*;
    use officina_slides::model::PhKind;
    use officina_text::layout::FixedFonts;

    fn doc_with_title() -> (Doc, SlideId, ShapeId) {
        let doc = Doc::new("t");
        let sid = doc.deck.slide_ids().next().unwrap();
        let mut d = doc;
        let shape = d.deck.slide(0).unwrap().shapes.iter().find(|s| s.ph.unwrap().kind == PhKind::CtrTitle).unwrap().id;
        (d, sid, shape)
    }

    fn text(doc: &mut Doc, sid: SlideId, shape: ShapeId) -> String {
        let i = doc.deck.slide_index(sid).unwrap();
        find(&doc.deck.slide(i).unwrap().shapes, shape).unwrap().text().unwrap().text()
    }

    #[test]
    fn typing_deleting_and_paragraphs() {
        let (mut doc, sid, shape) = doc_with_title();
        let mut e = FrameEdit::new(sid, shape, Pos::default());
        for c in ["H", "e", "l", "l", "o"] {
            e.insert(&mut doc, c, true);
        }
        e.insert(&mut doc, " world", true);
        assert_eq!(text(&mut doc, sid, shape), "Hello world");
        e.new_paragraph(&mut doc);
        e.insert(&mut doc, "Second", true);
        assert_eq!(text(&mut doc, sid, shape), "Hello world\nSecond");
        e.motion(&mut doc.deck, &FixedFonts, Motion::DocStart, false);
        e.motion(&mut doc.deck, &FixedFonts, Motion::Right, true);
        e.insert(&mut doc, "J", true);
        assert_eq!(text(&mut doc, sid, shape), "Jello world\nSecond");
        // Backspace at the second paragraph's start joins them.
        e.caret = Pos { block: 1, at: 0 };
        e.anchor = e.caret;
        e.delete(&mut doc, false);
        assert_eq!(text(&mut doc, sid, shape), "Jello worldSecond");
        // A selection across paragraphs goes in one delete.
        e.insert(&mut doc, "\nA\nB\nC", false);
        e.anchor = Pos { block: 0, at: 2 };
        e.caret = Pos { block: 3, at: 1 };
        e.delete(&mut doc, false);
        assert_eq!(text(&mut doc, sid, shape), "JeSecond");
        // Undo walks it all back.
        while doc.undo() {}
        assert_eq!(text(&mut doc, sid, shape), "");
    }

    #[test]
    fn levels_and_marks() {
        let (mut doc, _, _) = doc_with_title();
        let obj = doc.deck.layout_of_kind("obj").unwrap().id;
        let s = officina_slides::ops::new_slide(&mut doc.deck, obj);
        let sid = s.id;
        let body = s.shapes.iter().find(|x| x.ph.unwrap().kind == PhKind::Body).unwrap().id;
        doc.run("New", vec![DeckOp::InsertSlide { at: 1, slide: Box::new(s) }]);
        let mut e = FrameEdit::new(sid, body, Pos::default());
        e.insert(&mut doc, "Point\nDetail", false);
        e.level(&mut doc, 1);
        let i = doc.deck.slide_index(sid).unwrap();
        let lv: Vec<u16> = find(&doc.deck.slide(i).unwrap().shapes, body).unwrap().text().unwrap().doc.blocks.iter().filter_map(|b| b.as_para()).map(|p| p.style).collect();
        assert_eq!(lv, vec![0, 1]);
        e.select_all(&mut doc.deck);
        assert!(e.toggle(&mut doc, Mark::Bold));
        let d = find(&doc.deck.slide(i).unwrap().shapes, body).unwrap().text().unwrap().doc.clone();
        assert_eq!(d.attrs.get(d.para(0).unwrap().attr_at(0)).bold, Some(true));
        assert_eq!(d.attrs.get(d.para(1).unwrap().attr_at(2)).bold, Some(true));
        // Backspace at an indented paragraph's start promotes it first.
        e.caret = Pos { block: 1, at: 0 };
        e.anchor = e.caret;
        e.delete(&mut doc, false);
        let d = find(&doc.deck.slide(i).unwrap().shapes, body).unwrap().text().unwrap().doc.clone();
        assert_eq!(d.para(1).unwrap().style, 0);
    }

    #[test]
    fn caret_geometry_round_trips() {
        let (mut doc, sid, shape) = doc_with_title();
        let mut e = FrameEdit::new(sid, shape, Pos::default());
        e.insert(&mut doc, "A title long enough to wrap onto a second line in the frame, surely, yes", false);
        let g = e.geometry(&mut doc.deck, &FixedFonts).unwrap();
        assert!(g.layout.paras[0].0.lines.len() >= 2);
        // Every caret place maps to a point that maps back to it.
        let text = g.layout.doc.para(0).unwrap().text.clone();
        for at in (0..=text.len() as u32).step_by(5) {
            let p = Pos { block: 0, at };
            let (top, bottom, x) = caret_line(&g, p);
            let (sx, sy) = draw::apply(&g.xf, x as f64 + 1.0, ((top + bottom) / 2) as f64);
            let back = hit(&g, sx, sy);
            // A soft break's place is the end of one line or the start of the next.
            assert!(back == p || text[back.at.min(p.at) as usize..back.at.max(p.at) as usize].trim().is_empty(), "{p:?} -> {back:?}");
        }
        // Down from the first line lands on the second.
        e.caret = Pos { block: 0, at: 3 };
        e.anchor = e.caret;
        e.motion(&mut doc.deck, &FixedFonts, Motion::Down, false);
        assert!(e.caret.at > g.layout.paras[0].0.lines[0].start + 3);
    }
}
