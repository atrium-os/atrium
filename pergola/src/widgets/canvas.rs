//! `Canvas`: a retained vector drawing surface (flowcharts, diagrams,
//! vector drawing apps). The app holds a list of keyed [`Shape`]s; the
//! canvas draws each as Fresco FILLs (curves, gradients, any affine; strokes
//! and dashes outlined once by the shared stroker) and resends only the
//! shapes whose key or content changed, since each shape keeps its node ids
//! by key. It pans and zooms as a [`ScrollView`] (vertical scroll, pinch
//! and Ctrl-wheel zoom), so moving the view resends nothing.
//!
//! A press, drag and release reach `on_pointer` in canvas coordinates
//! with the key of the topmost shape under the pointer, tested on the
//! shape's own outline (not its bounds): what an editor needs to select,
//! move and draw.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use crate::geom::{Axis, Point, Rect, Size};
use crate::interaction::DragPhase;
use crate::layout::FlexStyle;
use crate::node::{fill_to, Node, NodeId};
use crate::reactive::Mutable;
use crate::shape::Shape;
use crate::view::{Ctx, Draw, View};
use crate::widgets::scroll_view::{ScrollState, ScrollView};
use crate::Color;

/// A shape and the app's name for it.
#[derive(Clone)]
pub struct Item {
    pub key: u64,
    pub shape: Rc<Shape>,
    /// Its fills placed at the canvas's origin (cached).
    placed: Rc<RefCell<Option<(Point, Vec<Node>)>>>,
}

impl Item {
    pub fn new(key: u64, shape: Shape) -> Self {
        Self { key, shape: Rc::new(shape), placed: Rc::new(RefCell::new(None)) }
    }

    fn nodes(&self, origin: Point) -> Vec<Node> {
        let mut c = self.placed.borrow_mut();
        match &*c {
            Some((o, n)) if *o == origin => n.clone(),
            _ => {
                let back = Point::new(-origin.x, -origin.y);
                let n: Vec<Node> = self.shape.fills().iter().map(|f| Node::Fill(Arc::new(fill_to(f, back, 1.0)))).collect();
                *c = Some((origin, n.clone()));
                n
            }
        }
    }
}

/// A pointer event on the canvas.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CanvasEvent {
    pub phase: DragPhase,
    /// Canvas px (scroll and zoom undone).
    pub at: Point,
    /// The topmost shape under the pointer.
    pub hit: Option<u64>,
}

pub struct Canvas {
    pub state: Mutable<ScrollState>,
    pub rect: Rect,
    /// The drawing's extent, canvas px.
    pub size: Size,
    pub items: Rc<Vec<Item>>,
    pub background: Option<Color>,
    on_pointer: Option<Rc<dyn Fn(CanvasEvent)>>,
    key: Option<u64>,
}

impl Canvas {
    pub fn new(state: Mutable<ScrollState>, rect: Rect, size: Size, items: Rc<Vec<Item>>) -> Self {
        Self { state, rect, size, items, background: None, on_pointer: None, key: None }
    }
    pub fn background(mut self, c: Color) -> Self { self.background = Some(c); self }
    pub fn on_pointer(mut self, f: impl Fn(CanvasEvent) + 'static) -> Self { self.on_pointer = Some(Rc::new(f)); self }
    pub fn key(mut self, k: u64) -> Self { self.key = Some(k); self }

    /// The topmost item whose ink covers `p` (canvas px).
    pub fn hit(items: &[Item], p: Point) -> Option<u64> {
        items.iter().rev().find(|i| i.shape.hit(p)).map(|i| i.key)
    }

    pub fn emit(&self, ctx: &mut Ctx) -> NodeId {
        let (rect, size, items) = (self.rect, self.size, self.items.clone());
        let bg = self.background.unwrap_or(ctx.theme.bg_canvas());
        let (state, cb) = (self.state.clone(), self.on_pointer.clone());
        let content = Draw(move |ctx: &mut Ctx| {
            let o = rect.origin;
            ctx.key(1);
            let page = ctx.push(Node::stack(Axis::Vertical, Rect::new(o.x, o.y, size.w.max(rect.w()), size.h), 0.0));
            ctx.set_flex(page, FlexStyle { free: true, ..FlexStyle::default() });
            // The paper: what an empty spot's press lands on.
            ctx.key(0);
            ctx.add(Node::Rect { rect: Rect::new(o.x, o.y, size.w.max(rect.w()), size.h), fill: bg, radius: 0.0 });
            for it in items.iter() {
                for (i, n) in it.nodes(o).into_iter().enumerate() {
                    ctx.key((it.key << 2 | i as u64 & 3).wrapping_add(4));
                    ctx.add(n);
                }
            }
            ctx.pop();
            if let Some(cb) = cb.clone() {
                let (state, items) = (state.clone(), items.clone());
                ctx.on_drag(page, move |phase, p: Point| {
                    let st = state.get();
                    let z = (st.zoom.settled * st.zoom.stretch()).max(0.01) as f32;
                    let at = Point::new((p.x - o.x) / z, (p.y - o.y) / z + st.physics.y as f32);
                    cb(CanvasEvent { phase, at, hit: Canvas::hit(&items, at) });
                });
            }
        });
        let mut v = ScrollView::new(self.state.clone(), self.rect, content);
        if let Some(k) = self.key {
            v = v.key(k);
        }
        v.emit(ctx)
    }
}

impl View for Canvas {
    fn render(&self, ctx: &mut Ctx) {
        self.emit(ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::event::Event;
    use crate::node::NodeDelta;
    use crate::shape::{Paint, PathData, Stroke};
    use std::sync::atomic::AtomicBool;

    struct Board {
        state: Mutable<ScrollState>,
        items: Mutable<Rc<Vec<Item>>>,
        log: Rc<RefCell<Vec<CanvasEvent>>>,
    }

    impl View for Board {
        fn render(&self, ctx: &mut Ctx) {
            let log = self.log.clone();
            Canvas::new(self.state.clone(), Rect::new(0.0, 0.0, 400.0, 300.0), Size::new(400.0, 2000.0), self.items.get_cloned())
                .on_pointer(move |e| log.borrow_mut().push(e))
                .emit(ctx);
        }
    }

    fn boxes(moved: f32) -> Rc<Vec<Item>> {
        let ink = Paint::Solid(Color::rgba(0.1, 0.1, 0.1, 1.0));
        Rc::new(vec![
            Item::new(1, Shape::new(PathData::rounded_rect(Rect::new(20.0, 20.0, 100.0, 60.0), 8.0)).fill(Paint::Solid(Color::rgba(0.9, 0.9, 1.0, 1.0))).stroke(Stroke::new(ink.clone(), 2.0))),
            Item::new(2, Shape::new(PathData::ellipse(Point::new(250.0, 50.0), 40.0, 25.0)).fill(Paint::Solid(Color::rgba(1.0, 0.9, 0.8, 1.0))).translate(moved, 0.0)),
            Item::new(3, Shape::new(PathData::new().move_to(120.0, 50.0).line_to(210.0, 50.0)).stroke(Stroke::new(ink, 2.0))),
        ])
    }

    fn fills(d: &[NodeDelta]) -> usize {
        d.iter().filter(|d| matches!(d, NodeDelta::Added { node: Node::Fill(_), .. } | NodeDelta::Changed { node: Node::Fill(_), .. })).count()
    }

    #[test]
    fn shapes_draw_as_fills_and_a_change_resends_only_that_shape() {
        let d = Arc::new(AtomicBool::new(true));
        let state = Mutable::with_dirty(ScrollState::default(), d.clone());
        let items = Mutable::with_dirty(boxes(0.0), d.clone());
        let log = Rc::new(RefCell::new(vec![]));
        let mut app = App::new_with_flag(Board { state: state.clone(), items: items.clone(), log: log.clone() }, d);
        let first = app.tick();
        assert_eq!(fills(&first), 4, "box fill and stroke, ellipse, connector");

        // Move the ellipse: the app replaces that one item.
        let mut v = (*boxes(0.0)).clone();
        v[1] = boxes(30.0)[1].clone();
        v[0] = items.get_cloned()[0].clone();
        v[2] = items.get_cloned()[2].clone();
        items.set(Rc::new(v));
        let next = app.tick();
        assert_eq!(fills(&next), 1, "only the ellipse is sent again");

        // Scrolled by 100 px: the view moves, no shape is resent.
        let mut st = state.get();
        st.physics.y = 100.0;
        state.set(st);
        assert_eq!(fills(&app.tick()), 0, "scrolling resends nothing");

        // A press on the moved ellipse, at canvas (280, 50): window y -50 is
        // off the top, so scroll back first.
        let mut st = state.get();
        st.physics.y = 0.0;
        state.set(st);
        app.tick();
        app.handle_event(Event::PointerDown { at: Point::new(280.0, 50.0) });
        app.handle_event(Event::PointerMove { at: Point::new(300.0, 60.0) });
        app.handle_event(Event::PointerUp { at: Point::new(300.0, 60.0) });
        app.handle_event(Event::PointerDown { at: Point::new(160.0, 50.5) });
        app.handle_event(Event::PointerUp { at: Point::new(160.0, 50.5) });
        app.handle_event(Event::PointerDown { at: Point::new(160.0, 80.0) });
        let log = log.borrow();
        assert_eq!(log[0], CanvasEvent { phase: DragPhase::Start, at: Point::new(280.0, 50.0), hit: Some(2) });
        assert_eq!(log[1].phase, DragPhase::Move);
        assert!(log.iter().any(|e| e.hit == Some(3)), "the 2 px connector is hit on its line");
        assert_eq!(log.last().unwrap().hit, None, "beside the line: nothing");
    }
}
