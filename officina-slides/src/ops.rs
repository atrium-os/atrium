//! Changes to a deck, each with an exact inverse (docs/spec/officina.md §6),
//! so undo, redo and the autosave journal come from `officina_core::history`.

use officina_core::history::Op;
use officina_text::ops::TextOp;

use crate::model::*;

/// What holds a shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Target {
    Slide(SlideId),
    Layout(LayoutId),
    Master(MasterId),
}

/// The shape id that names a slide's speaker notes in [`DeckOp::Text`].
pub const NOTES: ShapeId = 0;

#[derive(Debug, Clone)]
pub enum DeckOp {
    /// Does nothing (what an op that no longer applies turns into).
    Nop,
    InsertSlide { at: usize, slide: Box<Slide> },
    RemoveSlide { id: SlideId },
    MoveSlide { id: SlideId, to: usize },
    SetSlide { id: SlideId, props: Box<SlideProps> },
    InsertShape { target: Target, group: Option<ShapeId>, at: usize, shape: Box<Shape> },
    RemoveShape { target: Target, id: ShapeId },
    /// Replaces the shape with the same id.
    SetShape { target: Target, shape: Box<Shape> },
    /// Moves a shape to index `to` among its group's (front to back order).
    MoveShape { target: Target, id: ShapeId, to: usize },
    /// An edit to a shape's text frame ([`NOTES`]: the slide's notes).
    Text { target: Target, shape: ShapeId, op: TextOp },
    SetTheme(Box<Theme>),
    SetSize(Emu, Emu),
    /// Adds (at `at`), replaces or (`None`) removes a master.
    PutMaster { id: MasterId, at: usize, master: Option<Box<Master>> },
    PutLayout { id: LayoutId, at: usize, layout: Option<Box<Layout>> },
}

fn shapes_of(deck: &mut Deck, t: Target) -> Option<&mut Vec<Shape>> {
    match t {
        Target::Slide(id) => {
            let i = deck.slide_index(id)?;
            Some(&mut deck.slide_mut(i).ok()?.shapes)
        }
        Target::Layout(id) => Some(&mut deck.layouts.iter_mut().find(|l| l.id == id)?.shapes),
        Target::Master(id) => Some(&mut deck.masters.iter_mut().find(|m| m.id == id)?.shapes),
    }
}

fn put<T>(v: &mut Vec<T>, pos: usize, new: Option<T>, at: usize) -> (usize, Option<T>) {
    match (pos < v.len(), new) {
        (true, Some(n)) => (pos, Some(std::mem::replace(&mut v[pos], n))),
        (true, None) => (pos, Some(v.remove(pos))),
        (false, Some(n)) => {
            let at = at.min(v.len());
            v.insert(at, n);
            (at, None)
        }
        (false, None) => (at, None),
    }
}

impl Op<Deck> for DeckOp {
    fn apply(self, deck: &mut Deck) -> DeckOp {
        match self {
            DeckOp::Nop => DeckOp::Nop,
            DeckOp::InsertSlide { at, slide } => {
                if deck.slide_index(slide.id).is_some() || deck.slide_count() >= MAX_SLIDES {
                    return DeckOp::Nop;
                }
                let id = slide.id;
                deck.see_id(id);
                let mut max = 0;
                for s in &slide.shapes {
                    s.walk(&mut |x| max = max.max(x.id));
                }
                deck.see_id(max);
                deck.insert_slot(at, SlideSlot::Loaded(slide));
                DeckOp::RemoveSlide { id }
            }
            DeckOp::RemoveSlide { id } => {
                let Some(i) = deck.slide_index(id) else { return DeckOp::Nop };
                if deck.load_slide(i).is_err() {
                    return DeckOp::Nop;
                }
                deck.clean.remove(&id);
                match deck.remove_slot(i) {
                    SlideSlot::Loaded(s) => DeckOp::InsertSlide { at: i, slide: s },
                    SlideSlot::InFile { .. } => DeckOp::Nop,
                }
            }
            DeckOp::MoveSlide { id, to } => {
                let Some(i) = deck.slide_index(id) else { return DeckOp::Nop };
                let s = deck.remove_slot(i);
                deck.insert_slot(to, s);
                DeckOp::MoveSlide { id, to: i }
            }
            DeckOp::SetSlide { id, props } => {
                let Some(i) = deck.slide_index(id) else { return DeckOp::Nop };
                let Ok(s) = deck.slide_mut(i) else { return DeckOp::Nop };
                let old = s.props();
                s.set_props(*props);
                DeckOp::SetSlide { id, props: Box::new(old) }
            }
            DeckOp::InsertShape { target, group, at, shape } => {
                let mut max = 0;
                shape.walk(&mut |x| max = max.max(x.id));
                let id = shape.id;
                let Some(top) = shapes_of(deck, target) else { return DeckOp::Nop };
                let mut n = 0;
                for s in top.iter() {
                    s.walk(&mut |_| n += 1);
                }
                if find(top, id).is_some() || n >= MAX_SHAPES {
                    return DeckOp::Nop;
                }
                let Some(list) = children_mut(top, group) else { return DeckOp::Nop };
                list.insert(at.min(list.len()), *shape);
                deck.see_id(max);
                DeckOp::RemoveShape { target, id }
            }
            DeckOp::RemoveShape { target, id } => {
                let Some(top) = shapes_of(deck, target) else { return DeckOp::Nop };
                let Some((group, at)) = locate(top, id) else { return DeckOp::Nop };
                let Some(list) = children_mut(top, group) else { return DeckOp::Nop };
                let shape = list.remove(at);
                DeckOp::InsertShape { target, group, at, shape: Box::new(shape) }
            }
            DeckOp::SetShape { target, shape } => {
                let Some(top) = shapes_of(deck, target) else { return DeckOp::Nop };
                let Some(s) = find_mut(top, shape.id) else { return DeckOp::Nop };
                let old = std::mem::replace(s, *shape);
                DeckOp::SetShape { target, shape: Box::new(old) }
            }
            DeckOp::MoveShape { target, id, to } => {
                let Some(top) = shapes_of(deck, target) else { return DeckOp::Nop };
                let Some((group, at)) = locate(top, id) else { return DeckOp::Nop };
                let Some(list) = children_mut(top, group) else { return DeckOp::Nop };
                let s = list.remove(at);
                let to = to.min(list.len());
                list.insert(to, s);
                DeckOp::MoveShape { target, id, to: at }
            }
            DeckOp::Text { target, shape, op } => {
                let body = if shape == NOTES {
                    let Target::Slide(sid) = target else { return DeckOp::Nop };
                    let Some(i) = deck.slide_index(sid) else { return DeckOp::Nop };
                    let Ok(s) = deck.slide_mut(i) else { return DeckOp::Nop };
                    s.notes.as_mut()
                } else {
                    let Some(top) = shapes_of(deck, target) else { return DeckOp::Nop };
                    find_mut(top, shape).and_then(|s| s.text_mut())
                };
                let Some(body) = body else { return DeckOp::Nop };
                let inv = op.apply(&mut body.doc);
                // Frames lay out from scratch; nothing paginates them.
                body.doc.take_changes();
                DeckOp::Text { target, shape, op: inv }
            }
            DeckOp::SetTheme(t) => DeckOp::SetTheme(Box::new(std::mem::replace(&mut deck.theme, *t))),
            DeckOp::SetSize(w, h) => {
                let (ow, oh) = deck.size;
                deck.size = (w.clamp(EMU_MIN_SIDE, EMU_MAX_SIDE), h.clamp(EMU_MIN_SIDE, EMU_MAX_SIDE));
                DeckOp::SetSize(ow, oh)
            }
            DeckOp::PutMaster { id, at, master } => {
                if master.as_ref().is_some_and(|m| m.id != id) || (master.is_some() && deck.masters.len() >= MAX_MASTERS && deck.master(id).is_none()) {
                    return DeckOp::Nop;
                }
                deck.see_id(id);
                let pos = deck.masters.iter().position(|m| m.id == id).unwrap_or(usize::MAX);
                let (at, old) = put(&mut deck.masters, pos, master.map(|m| *m), at);
                DeckOp::PutMaster { id, at, master: old.map(Box::new) }
            }
            DeckOp::PutLayout { id, at, layout } => {
                if layout.as_ref().is_some_and(|l| l.id != id) || (layout.is_some() && deck.layouts.len() >= MAX_LAYOUTS && deck.layout(id).is_none()) {
                    return DeckOp::Nop;
                }
                deck.see_id(id);
                let pos = deck.layouts.iter().position(|l| l.id == id).unwrap_or(usize::MAX);
                let (at, old) = put(&mut deck.layouts, pos, layout.map(|l| *l), at);
                DeckOp::PutLayout { id, at, layout: old.map(Box::new) }
            }
        }
    }
}

/// Slide sides: 1" to 56" (PowerPoint's limits).
pub const EMU_MIN_SIDE: Emu = 914_400;
pub const EMU_MAX_SIDE: Emu = 51_206_400;

/// A new slide on `layout` with an empty placeholder for each of the
/// layout's title, body and picture placeholders (not date, footer, number).
pub fn new_slide(deck: &mut Deck, layout: LayoutId) -> Slide {
    let id = deck.new_id();
    let mut s = Slide::new(id, layout);
    let phs: Vec<Placeholder> = deck.layout(layout).map(|l| l.shapes.iter().filter_map(|x| x.ph).filter(|p| !matches!(p.kind, PhKind::Date | PhKind::Footer | PhKind::SlideNum | PhKind::Header)).collect()).unwrap_or_default();
    for p in phs {
        let sid = deck.new_id();
        let mut sh = Shape::placeholder(sid, p.kind, p.idx, "");
        sh.name = format!("{} {sid}", p.kind.name()).into();
        s.shapes.push(sh);
    }
    s
}
