//! `ScrollView` (pergola.md §10.1, §10.4): a viewport onto content taller
//! than it, scrolled by wheel, trackpad, keys and flings, and zoomed by
//! Ctrl + wheel and Ctrl + − 0. Every app scrolls the same way because
//! they all use this one.
//!
//! On the wire it is a `SCROLL` group (fresco-scene-groups.md §4): a
//! scroll frame is one transform change on its content group, and the
//! renderer shifts what is drawn and draws only the exposed strip. The
//! motion itself is [`crate::scroll`]'s, on the app's frame clock.
//!
//! Content wider than the view scrolls across too: a wheel's `dx` (or
//! Shift + wheel), a touchpad (kept to one axis unless the gesture is
//! diagonal) and Left/Right; both axes move the one content group, so
//! the renderer shifts diagonally in one step.
//!
//! What this prototype leaves out, stated: a scrollbar, the fling blur on the wire (the renderer has no blur op
//! yet; [`Physics::motion_blur_px`] says how much), and band sources
//! (the [`crate::scroll::Viewport`] side is built; `DocumentView` will be
//! its first source). Pergola-native content is laid out once at zoom 1
//! and sent at the settled zoom; while a pinch runs the content group
//! stretches it, and text keeps its size until the zoom settles.

use crate::event::{Key, ScrollPhase};
use crate::geom::{Point, Rect};
use crate::interaction::{ScrollInput, ScrollKind};
use crate::node::{Node, NodeId};
use crate::reactive::Mutable;
use crate::scroll::{zoom_about, Physics, Zoom};
use crate::view::{Ctx, View};
use crate::Color;

/// Lines an arrow key or one wheel notch scrolls (px at zoom 1).
pub const LINE_PX: f64 = 40.0;
/// Page Up/Down and Space scroll this fraction of the viewport.
pub const PAGE_FRACTION: f64 = 0.9;
/// A wheel delta at least this big (px) is a discrete notch and eases;
/// smaller ones are a trackpad's continuous stream and follow the finger.
pub const NOTCH_PX: f32 = 30.0;
/// Ctrl + wheel: zoom by e^(−dy · this) per event.
pub const WHEEL_ZOOM_PER_PX: f64 = 0.002;

/// A scroll view's state: position and motion, zoom, and the system's
/// reduce-motion setting. Kept by the app in a `Mutable` so it survives
/// renders.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ScrollState {
    pub physics: Physics,
    /// The horizontal axis: still unless the content is wider than the view.
    pub physics_x: Physics,
    pub zoom: Zoom,
    pub reduce_motion: bool,
    /// The running pinch's scale at its last step (0: no pinch).
    pub pinch_last: f64,
    /// The content's size (width, height) at zoom 1 when the widget knows
    /// it better than layout can: a grid of a billion rows is taller than
    /// an `f32` holds exactly, so it scrolls in `f64` and sends nodes
    /// near the view only (`GridView`).
    pub extent: Option<[f64; 2]>,
    /// Ignore zoom gestures (a grid keeps its scale).
    pub fixed_zoom: bool,
}

impl ScrollState {
    /// Bring the bounds up to date with the node as last laid out: the
    /// content's height at zoom 1 minus what the viewport shows of it.
    pub fn fit(&mut self, node: &Node) {
        if let Node::Scroll { rect, content_h, content_w, .. } = node {
            let [w, h] = self.extent.unwrap_or([*content_w as f64, *content_h as f64]);
            let shown = rect.h() as f64 / self.zoom.live;
            self.physics.set_max(h - shown);
            let across = rect.w() as f64 / self.zoom.live;
            self.physics_x.set_max(w - across);
        }
    }

    fn viewport_h(node: &Node) -> f64 { node.rect().h() as f64 }

    /// Apply one input at `t` ms.
    pub fn input(&mut self, kind: ScrollKind, t: f64, node: &Node) {
        self.fit(node);
        if self.fixed_zoom {
            match kind {
                ScrollKind::Magnify { .. } => return,
                ScrollKind::Wheel { modifiers, .. } if modifiers.ctrl => return,
                ScrollKind::Key { modifiers, .. } if modifiers.ctrl => return,
                _ => {}
            }
        }
        let vh = Self::viewport_h(node) / self.zoom.live;
        match kind {
            ScrollKind::Press => self.physics.stop(),
            ScrollKind::Magnify { at, scale, phase } => {
                let s = scale as f64;
                if !(s.is_finite() && s > 0.0) {
                    return;
                }
                // A new pinch: from its own start, and it catches a fling.
                if phase == ScrollPhase::Begin || self.pinch_last <= 0.0 {
                    self.pinch_last = 1.0;
                    self.physics.stop();
                }
                let focus = (at.y - node.rect().y()) as f64;
                let focus_x = (at.x - node.rect().x()) as f64;
                let z0 = self.zoom.live;
                self.zoom.pinch(s / self.pinch_last, t);
                self.pinch_last = s;
                let down = matches!(phase, ScrollPhase::Begin | ScrollPhase::Update);
                self.zoom.hold(down, t);
                if !down {
                    self.pinch_last = 0.0;
                }
                self.refocus(z0, focus, focus_x, node);
            }
            ScrollKind::Wheel { at, dy, modifiers, .. } if modifiers.ctrl => {
                let focus = at.map(|p| (p.y - node.rect().y()) as f64).unwrap_or(0.0);
                let focus_x = at.map(|p| (p.x - node.rect().x()) as f64).unwrap_or(0.0);
                let z0 = self.zoom.live;
                self.zoom.pinch((-(dy as f64) * WHEEL_ZOOM_PER_PX).exp(), t);
                self.refocus(z0, focus, focus_x, node);
            }
            // A touchpad that says where the fingers are: they follow it,
            // fingers down catch a fling, and lifting them throws one.
            ScrollKind::Wheel { dx, dy, phase, modifiers, .. } if phase != ScrollPhase::None => {
                let (dx, dy) = across(dx, dy, modifiers.shift);
                if phase == ScrollPhase::Begin {
                    self.physics.stop();
                    self.physics_x.stop();
                }
                if phase == ScrollPhase::End {
                    self.physics.release(t);
                    self.physics_x.release(t);
                } else {
                    // A gesture mostly along one axis moves only that one,
                    // so a vertical swipe doesn't wander sideways.
                    let (dx, dy) = rail(dx, dy);
                    if dy != 0.0 {
                        self.physics.hold(dy as f64 / self.zoom.live, t);
                    }
                    if dx != 0.0 {
                        self.physics_x.hold(dx as f64 / self.zoom.live, t);
                    }
                }
            }
            ScrollKind::Wheel { dx, dy, modifiers, .. } => {
                let (dx, dy) = across(dx, dy, modifiers.shift);
                for (p, d) in [(&mut self.physics, dy), (&mut self.physics_x, dx)] {
                    if d == 0.0 {
                        continue;
                    }
                    let v = d as f64 / self.zoom.live;
                    if d.abs() >= NOTCH_PX {
                        p.jump_by(v, t);
                    } else {
                        p.drag(v, t);
                    }
                }
            }
            ScrollKind::Key { key, chars, modifiers } => {
                let z0 = self.zoom.live;
                match (key, chars) {
                    (Key::Up, _) => self.physics.jump_by(-LINE_PX, t),
                    (Key::Down, _) => self.physics.jump_by(LINE_PX, t),
                    (Key::Left, _) => self.physics_x.jump_by(-LINE_PX, t),
                    (Key::Right, _) => self.physics_x.jump_by(LINE_PX, t),
                    (Key::PageUp, _) => self.physics.jump_by(-vh * PAGE_FRACTION, t),
                    (Key::PageDown, _) => self.physics.jump_by(vh * PAGE_FRACTION, t),
                    (Key::Char, ' ') => {
                        let dir = if modifiers.shift { -1.0 } else { 1.0 };
                        self.physics.jump_by(dir * vh * PAGE_FRACTION, t)
                    }
                    (Key::Home, _) => self.physics.jump_to(0.0, t),
                    (Key::End, _) => self.physics.jump_to(f64::MAX, t),
                    (Key::Char, '+' | '=') if modifiers.ctrl => self.zoom.step_level(1, t),
                    (Key::Char, '-') if modifiers.ctrl => self.zoom.step_level(-1, t),
                    (Key::Char, '0') if modifiers.ctrl => self.zoom.reset(t),
                    _ => {}
                }
                if self.zoom.live != z0 {
                    // Keyboard zoom keeps the top left of the viewport.
                    self.refocus(z0, 0.0, 0.0, node);
                }
            }
        }
    }

    /// After the live zoom changed from `z0`: keep the content under the
    /// point `focus` px from the viewport's top where it is.
    fn refocus(&mut self, z0: f64, focus: f64, focus_x: f64, node: &Node) {
        let y = zoom_about(self.physics.y, focus, z0, self.zoom.live);
        let x = zoom_about(self.physics_x.y, focus_x, z0, self.zoom.live);
        self.fit(node);
        self.physics.set_position(y);
        self.physics_x.set_position(x);
    }

    /// One frame at `t` ms. Returns whether it still needs frames.
    pub fn frame(&mut self, t: f64, node: &Node) -> bool {
        self.fit(node);
        self.physics.step(t);
        self.physics_x.step(t);
        // A settle re-renders at the new zoom (the payloads change, the
        // stretch returns to 1): the "sharpen on settle" step.
        self.zoom.step(t);
        self.physics.is_moving() || self.physics_x.is_moving() || self.zoom.is_pending()
    }
}

/// Shift turns a wheel's vertical turn into a horizontal one (a mouse
/// with one wheel scrolls a wide sheet across).
fn across(dx: f32, dy: f32, shift: bool) -> (f32, f32) {
    if shift && dx == 0.0 {
        (dy, 0.0)
    } else {
        (dx, dy)
    }
}

/// A touchpad delta mostly along one axis keeps to that axis (within
/// about 27° of it); a diagonal one moves both.
fn rail(dx: f32, dy: f32) -> (f32, f32) {
    if dx.abs() <= dy.abs() * 0.5 {
        (0.0, dy)
    } else if dy.abs() <= dx.abs() * 0.5 {
        (dx, 0.0)
    } else {
        (dx, dy)
    }
}

/// A scroll view. `content` renders inside it, laid out in a column as
/// wide as the view (children wider than that scroll across).
pub struct ScrollView<V: View> {
    pub state: Mutable<ScrollState>,
    pub rect: Rect,
    pub content: V,
    /// Opaque backdrop under the content: lets the renderer shift pixels
    /// without blending what is behind the view. Defaults to the theme's
    /// window background.
    pub backdrop: Option<Option<Color>>,
    /// Key for the view's node (`Ctx::key`), so it keeps its id.
    pub key: Option<u64>,
}

impl<V: View> ScrollView<V> {
    pub fn new(state: Mutable<ScrollState>, rect: Rect, content: V) -> Self {
        Self { state, rect, content, backdrop: None, key: None }
    }

    /// A transparent view (no backdrop): the renderer cannot shift what
    /// is drawn, so every scroll frame redraws the view.
    pub fn transparent(mut self) -> Self {
        self.backdrop = Some(None);
        self
    }

    pub fn backdrop(mut self, c: Color) -> Self {
        self.backdrop = Some(Some(c));
        self
    }

    pub fn key(mut self, k: u64) -> Self {
        self.key = Some(k);
        self
    }

    pub fn emit(&self, ctx: &mut Ctx) -> NodeId {
        let st = self.state.get();
        let backdrop = self.backdrop.unwrap_or(Some(ctx.theme.bg_window()));
        if let Some(k) = self.key {
            ctx.key(k);
        }
        let id = ctx.push(Node::Scroll {
            rect: self.rect,
            offset: st.physics.y as f32,
            zoom: st.zoom.settled as f32,
            stretch: st.zoom.stretch() as f32,
            backdrop,
            content_h: 0.0,
            offset_x: st.physics_x.y as f32,
            content_w: 0.0,
        });
        self.content.render(ctx);
        ctx.pop();
        ctx.focusable(id);
        let s = self.state.clone();
        ctx.on_scroll(id, move |inp: &ScrollInput| {
            let mut st = s.get();
            st.input(inp.kind, inp.t, inp.node);
            s.set(st);
        });
        let s = self.state.clone();
        ctx.on_reflow(id, move |y, node| {
            let mut st = s.get();
            st.fit(node);
            st.physics.set_position(y);
            if st != s.get() {
                s.set(st);
            }
            false
        });
        let s = self.state.clone();
        ctx.on_frame(id, move |t, node| {
            let mut st = s.get();
            let more = st.frame(t, node);
            if st != s.get() {
                s.set(st);
            }
            more
        });
        id
    }
}

impl<V: View> View for ScrollView<V> {
    fn render(&self, ctx: &mut Ctx) {
        self.emit(ctx);
    }
}

/// Where a scroll view's reader is: the text (a paragraph, a label) nearest
/// the top of what it shows, and how far into it the top is (resolution-independence.md §6.3).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Anchor {
    scroll: NodeId,
    offset: f32,
    leaf: NodeId,
    /// The leaf's words ([`words`]), so another node that took its id is
    /// not mistaken for it.
    text: Option<String>,
    /// How far the top is into the leaf: a fraction of its height when the
    /// top is inside it, else points above it (negative).
    within: f32,
    inside: bool,
}

/// A text leaf's words, or a wrapped paragraph's (a stack of its lines,
/// `layout::break_lines`): letters and digits only, so the same text
/// broken at other places is the same; `None` for anything else.
fn words(tree: &crate::node::NodeTree, id: NodeId) -> Option<String> {
    let keep = |t: &str| t.chars().filter(|c| c.is_alphanumeric()).collect::<String>();
    match tree.get(id)? {
        Node::Text { content, .. } => Some(keep(content)),
        Node::Stack { .. } => {
            let kids = tree.children_of(id);
            let mut out = String::new();
            for &k in kids {
                match tree.get(k) {
                    Some(Node::Text { content, .. }) if tree.children_of(k).is_empty() => out += &keep(content),
                    _ => return None,
                }
            }
            (!kids.is_empty()).then_some(out)
        }
        _ => None,
    }
}

/// The anchors of the scroll views among `ids` in `tree`.
pub(crate) fn anchors(tree: &crate::node::NodeTree, ids: impl Iterator<Item = NodeId>) -> Vec<Anchor> {
    let mut out = Vec::new();
    for id in ids {
        let Some(Node::Scroll { rect, offset, .. }) = tree.get(id) else { continue };
        let (top, origin) = (*offset, rect.y());
        // Leaves below the top's line, text first: a background or hit
        // rect as tall as the content does not move when text reflows.
        let mut best: Option<(bool, f32, NodeId)> = None;
        let mut stack: Vec<NodeId> = tree.children_of(id).iter().rev().copied().collect();
        while let Some(n) = stack.pop() {
            let kids = tree.children_of(n);
            let para = !kids.is_empty() && words(tree, n).is_some();
            if !kids.is_empty() && !para {
                stack.extend(kids.iter().rev().copied());
                continue;
            }
            let Some(node) = tree.get(n) else { continue };
            let r = node.rect();
            let (y, h) = (r.y() - origin, r.h());
            if h <= 0.0 || y + h <= top {
                continue;
            }
            let text = para || matches!(node, Node::Text { .. } | Node::Glyphs { .. } | Node::Image { .. });
            let better = match best {
                None => true,
                Some((bt, by, _)) => (text && !bt) || (text == bt && y < by),
            };
            if better {
                best = Some((text, y, n));
            }
        }
        let Some((_, y, leaf)) = best else { continue };
        let node = tree.get(leaf).unwrap();
        let h = node.rect().h();
        let inside = top >= y;
        out.push(Anchor {
            scroll: id,
            offset: top,
            leaf,
            text: words(tree, leaf),
            within: if inside { (top - y) / h } else { top - y },
            inside,
        });
    }
    out
}

/// Move each anchored scroll view in `tree` whose position did not change
/// but whose anchor did, so the anchor is where it was on screen; the
/// views moved and their new positions.
pub(crate) fn reanchor(tree: &mut crate::node::NodeTree, anchors: &[Anchor]) -> Vec<(NodeId, f64)> {
    let mut out = Vec::new();
    for a in anchors {
        let Some(Node::Scroll { rect, offset, zoom, content_h, .. }) = tree.get(a.scroll) else { continue };
        if *offset != a.offset {
            continue;
        }
        if a.text.is_some() && words(tree, a.leaf) != a.text {
            continue;
        }
        let Some(leaf) = tree.get(a.leaf) else { continue };
        let r = leaf.rect();
        let y = r.y() - rect.y();
        let top = if a.inside { y + a.within * r.h() } else { y + a.within };
        let max = (content_h - rect.h() / zoom.max(1e-3)).max(0.0);
        let top = top.clamp(0.0, max);
        if (top - a.offset).abs() < 0.5 {
            continue;
        }
        if let Some(Node::Scroll { offset, .. }) = tree.get_mut(a.scroll) {
            *offset = top;
        }
        out.push((a.scroll, top as f64));
    }
    out
}

/// Where the point `p` (window px) falls in the content of the scroll
/// view `node`, in content px at zoom 1.
pub fn content_point(node: &Node, p: Point) -> Option<Point> {
    let Node::Scroll { rect, offset, offset_x, zoom, stretch, .. } = node else { return None };
    let z = zoom * stretch;
    Some(Point::new((p.x - rect.x()) / z + offset_x, (p.y - rect.y()) / z + offset))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::event::{Event, KeyEventKind, Modifiers, ScrollPhase};
    use crate::node::NodeDelta;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    /// A 200 × 300 view over 50 rows of 40 px (2,000 px of content).
    struct Doc {
        state: Mutable<ScrollState>,
    }

    impl View for Doc {
        fn render(&self, ctx: &mut Ctx) {
            let rows = crate::view::Draw(|ctx: &mut Ctx| {
                for i in 0..50 {
                    ctx.add(Node::Rect {
                        rect: Rect::new(0.0, 0.0, 200.0, 40.0),
                        fill: Color::rgba(i as f32 / 50.0, 0.0, 0.0, 1.0),
                        radius: 0.0,
                    });
                }
            });
            ScrollView::new(self.state.clone(), Rect::new(0.0, 0.0, 200.0, 300.0), rows).emit(ctx);
        }
    }

    fn app() -> (App<Doc>, Mutable<ScrollState>) {
        let dirty = Arc::new(AtomicBool::new(true));
        let state = Mutable::with_dirty(ScrollState::default(), dirty.clone());
        let mut app = App::new_with_flag(Doc { state: state.clone() }, dirty);
        app.tick();
        (app, state)
    }

    /// Paragraphs in a view whose width the test changes.
    struct Prose {
        state: Mutable<ScrollState>,
        width: Mutable<f32>,
    }

    impl View for Prose {
        fn render(&self, ctx: &mut Ctx) {
            let w = self.width.get();
            let body = crate::view::Draw(move |ctx: &mut Ctx| {
                let col = ctx.push(Node::vstack(Rect::new(0.0, 0.0, w, 0.0), 8.0));
                ctx.set_flex(col, crate::layout::FlexStyle { align: crate::Align::Stretch, ..Default::default() });
                for i in 0..40 {
                    let text = format!("Paragraph {i}: ") + &"words that wrap again when the view gets narrower or the text bigger ".repeat(4);
                    ctx.paragraph(text, crate::node::TextStyle { family: crate::theme::font::SANS.into(), size: 14.0, weight: crate::theme::Weight::Regular, color: Color::rgba(0.0, 0.0, 0.0, 1.0) });
                }
                ctx.pop();
            });
            ScrollView::new(self.state.clone(), Rect::new(0.0, 0.0, w, 300.0), body).emit(ctx);
        }
    }

    fn scroll_node(app: &App<Prose>) -> (NodeId, f32) {
        app.tree().iter().find_map(|(id, n)| match n { Node::Scroll { offset, .. } => Some((id, *offset)), _ => None }).unwrap()
    }

    /// A reflow keeps the reader's place: the text at the top of the view
    /// before a width change is at the same place on screen after it,
    /// though everything above it got taller (resolution-independence.md §6.3).
    #[test]
    fn a_reflow_keeps_the_readers_place() {
        let dirty = Arc::new(AtomicBool::new(true));
        let state = Mutable::with_dirty(ScrollState::default(), dirty.clone());
        let width = Mutable::with_dirty(420.0f32, dirty.clone());
        let mut app = App::new_with_flag(Prose { state: state.clone(), width: width.clone() }, dirty);
        app.tick();
        let mut st = state.get();
        let (id, _) = scroll_node(&app);
        st.fit(app.tree().get(id).unwrap());
        st.physics.set_position(1500.0);
        state.set(st);
        app.tick();
        let a = anchors(app.tree(), std::iter::once(id)).pop().expect("an anchor");
        let leaf = app.tree().get(a.leaf).unwrap().rect();
        let before = leaf.y() - 1500.0;
        let frac = (1500.0 - leaf.y()) / leaf.h();
        width.set(260.0);
        app.tick();
        app.tick();
        let (_, off) = scroll_node(&app);
        let leaf2 = app.tree().get(a.leaf).unwrap().rect();
        assert!(leaf2.y() > leaf.y() + 200.0, "what is above it rewrapped taller");
        assert!(off > 1700.0, "the position followed the text down: {off}");
        let after = leaf2.y() + frac * leaf2.h() - off;
        assert!((after - (before + frac * leaf.h())).abs() < 1.0, "the same place in the text is at the top: {after}");
        assert_eq!(state.get().physics.y as f32, off, "the state agrees with what was drawn");
        // A scroll by the reader is not undone.
        let mut st = state.get();
        st.physics.set_position(100.0);
        state.set(st);
        app.tick();
        assert_eq!(scroll_node(&app).1, 100.0);
    }

    fn run_frames(app: &mut App<Doc>, from: f64, ms: f64) -> Vec<Vec<NodeDelta>> {
        let mut t = from;
        let mut out = Vec::new();
        while t < from + ms {
            t += 1000.0 / 120.0;
            app.set_time(t);
            app.frame();
            out.push(app.tick());
        }
        out
    }

    fn wheel(dy: f32) -> Event {
        Event::Scroll { at: Some(Point::new(100.0, 100.0)), dx: 0.0, dy, modifiers: Modifiers::default(), phase: crate::event::ScrollPhase::None }
    }

    #[test]
    fn layout_measures_the_content() {
        let (app, _) = app();
        let d = diff_all(&app);
        let h = d.iter().find_map(|d| match d {
            NodeDelta::Added { node: Node::Scroll { content_h, .. }, .. } => Some(*content_h),
            _ => None,
        });
        assert_eq!(h, Some(2000.0));
    }

    fn diff_all(_app: &App<Doc>) -> Vec<NodeDelta> {
        // The first tick's deltas, from a fresh app.
        let dirty = Arc::new(AtomicBool::new(true));
        let state = Mutable::with_dirty(ScrollState::default(), dirty.clone());
        let mut a = App::new_with_flag(Doc { state }, dirty);
        a.tick()
    }

    #[test]
    fn a_swipe_scrolls_and_each_frame_changes_only_the_view() {
        let (mut app, state) = app();
        for i in 0..8 {
            app.set_time(i as f64 * 8.0);
            app.handle_event(wheel(16.0));
        }
        assert!(app.wants_frame());
        let frames = run_frames(&mut app, 56.0, 3000.0);
        let y = state.get().physics.y;
        assert!(y > 300.0, "flung to {y}");
        assert!(y <= 2000.0 - 300.0, "within the content: {y}");
        // Every frame with a change sends only the scroll view (the rows
        // keep their content-group coordinates).
        for d in frames.iter().filter(|d| !d.is_empty()) {
            assert_eq!(d.len(), 1, "{d:?}");
            assert!(matches!(d[0], NodeDelta::Changed { node: Node::Scroll { .. }, .. }));
        }
        assert!(!app.wants_frame(), "the fling came to rest");
    }

    #[test]
    fn end_goes_to_the_bottom_and_home_back() {
        let (mut app, state) = app();
        // Focus the view, then press End.
        app.handle_event(Event::PointerDown { at: Point::new(10.0, 10.0) });
        app.handle_event(Event::PointerUp { at: Point::new(10.0, 10.0) });
        let key = |k| Event::Key { kind: KeyEventKind::Down, key: k, modifiers: Modifiers::default(), chars: String::new() };
        app.handle_event(key(Key::End));
        run_frames(&mut app, 0.0, 400.0);
        assert_eq!(state.get().physics.y, 1700.0);
        app.handle_event(key(Key::Home));
        run_frames(&mut app, 400.0, 400.0);
        assert_eq!(state.get().physics.y, 0.0);
    }

    #[test]
    fn keys_reach_the_only_scroll_view_unfocused_and_not_under_the_pointer() {
        let (mut app, state) = app();
        // The pointer outside the view (a page's margin), nothing focused.
        app.handle_event(Event::PointerMove { at: Point::new(-50.0, -50.0) });
        let ctrl = Modifiers { ctrl: true, ..Default::default() };
        app.handle_event(Event::Key { kind: KeyEventKind::Down, key: Key::Char, modifiers: ctrl, chars: "=".into() });
        assert!(state.get().zoom.live > 1.0, "Ctrl-= zoomed");
        app.handle_event(Event::Key { kind: KeyEventKind::Down, key: Key::End, modifiers: Modifiers::default(), chars: String::new() });
        run_frames(&mut app, 0.0, 400.0);
        assert!(state.get().physics.y > 0.0, "End scrolled");
    }

    #[test]
    fn a_click_lands_on_the_row_drawn_there() {
        let (mut app, state) = app();
        let mut st = state.get();
        st.physics.set_max(1700.0);
        st.physics.set_position(400.0);
        state.set(st);
        app.tick();
        // 400 px down, the pixel at y = 20 shows row 10 (420 / 40).
        let tree_hit = crate::event::hit_test(app_tree(&app), Point::new(10.0, 20.0)).unwrap();
        let row = app_tree(&app).get(tree_hit).unwrap().rect();
        assert_eq!(row.y(), 400.0);
    }

    fn app_tree(app: &App<Doc>) -> &crate::node::NodeTree { app.tree() }

    #[test]
    fn ctrl_wheel_zooms_and_settles() {
        let (mut app, state) = app();
        let ctrl = Modifiers { ctrl: true, ..Default::default() };
        app.set_time(0.0);
        app.handle_event(Event::Scroll { at: Some(Point::new(100.0, 0.0)), dx: 0.0, dy: -100.0, modifiers: ctrl, phase: crate::event::ScrollPhase::None });
        let st = state.get();
        assert!(st.zoom.live > 1.0 && st.zoom.settled == 1.0);
        let frames = run_frames(&mut app, 0.0, 200.0);
        let st = state.get();
        assert_eq!(st.zoom.settled, st.zoom.live);
        // Settled: the rows are re-sent at the new zoom.
        let z = st.zoom.settled as f32;
        let resized = frames.iter().flatten().any(|d| matches!(d,
            NodeDelta::Changed { node: Node::Rect { rect, .. }, .. } if (rect.w() - 200.0 * z).abs() < 1e-3));
        assert!(resized, "rows sent at zoom {z}");
        assert!(!app.wants_frame());
    }

    /// A 200 × 300 view over a 900 × 2,000 sheet of rows.
    struct Wide {
        state: Mutable<ScrollState>,
    }

    impl View for Wide {
        fn render(&self, ctx: &mut Ctx) {
            let rows = crate::view::Draw(|ctx: &mut Ctx| {
                for i in 0..50 {
                    ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, 900.0, 40.0), fill: Color::rgba(i as f32 / 50.0, 0.0, 0.0, 1.0), radius: 0.0 });
                }
            });
            ScrollView::new(self.state.clone(), Rect::new(0.0, 0.0, 200.0, 300.0), rows).emit(ctx);
        }
    }

    #[test]
    fn wide_content_scrolls_across_and_diagonally() {
        let dirty = Arc::new(AtomicBool::new(true));
        let state = Mutable::with_dirty(ScrollState::default(), dirty.clone());
        let mut app = App::new_with_flag(Wide { state: state.clone() }, dirty);
        let d = app.tick();
        let w = d.iter().find_map(|d| match d {
            NodeDelta::Added { node: Node::Scroll { content_w, .. }, .. } => Some(*content_w),
            _ => None,
        });
        assert_eq!(w, Some(900.0), "layout measures the width");
        // Shift + a wheel notch goes across.
        let shift = Modifiers { shift: true, ..Default::default() };
        app.set_time(0.0);
        app.handle_event(Event::Scroll { at: Some(Point::new(100.0, 100.0)), dx: 0.0, dy: 40.0, modifiers: shift, phase: ScrollPhase::None });
        let mut t = 0.0;
        while t < 400.0 {
            t += 1000.0 / 120.0;
            app.set_time(t);
            app.frame();
            app.tick();
        }
        let st = state.get();
        assert_eq!((st.physics_x.y, st.physics.y), (40.0, 0.0));
        // A touchpad swipe mostly down stays on the vertical rail.
        let pad = |dx, dy, phase| Event::Scroll { at: Some(Point::new(100.0, 100.0)), dx, dy, modifiers: Modifiers::default(), phase };
        app.handle_event(pad(0.0, 0.0, ScrollPhase::Begin));
        for i in 0..5 {
            app.set_time(t + i as f64 * 8.0);
            app.handle_event(pad(3.0, 20.0, ScrollPhase::Update));
        }
        let st = state.get();
        assert_eq!(st.physics_x.y, 40.0, "no sideways drift");
        assert_eq!(st.physics.y, 100.0);
        // A diagonal one moves both, in one content transform.
        for i in 0..5 {
            app.set_time(t + 40.0 + i as f64 * 8.0);
            app.handle_event(pad(10.0, 10.0, ScrollPhase::Update));
        }
        let d = app.tick();
        let st = state.get();
        assert_eq!((st.physics_x.y, st.physics.y), (90.0, 150.0));
        assert_eq!(d.len(), 1, "one node changes: {d:?}");
        assert!(matches!(d[0], NodeDelta::Changed { node: Node::Scroll { offset_x, offset, .. }, .. } if offset_x == 90.0 && offset == 150.0));
        // Hits land on what is drawn there.
        let hit = crate::event::hit_test(app.tree(), Point::new(10.0, 20.0)).unwrap();
        assert_eq!(app.tree().get(hit).unwrap().rect().y(), 160.0);
        // The edge clamps: 900 − 200.
        for i in 0..40 {
            app.set_time(t + 100.0 + i as f64 * 8.0);
            app.handle_event(pad(100.0, 0.0, ScrollPhase::Update));
        }
        assert_eq!(state.get().physics_x.y, 700.0);
    }

    #[test]
    fn narrow_content_never_scrolls_across() {
        let (mut app, state) = app();
        app.handle_event(Event::Scroll { at: Some(Point::new(100.0, 100.0)), dx: 50.0, dy: 0.0, modifiers: Modifiers::default(), phase: ScrollPhase::None });
        run_frames(&mut app, 0.0, 400.0);
        assert_eq!(state.get().physics_x.y, 0.0);
    }

    #[test]
    fn a_pinch_zooms_about_its_point_and_settles_when_lifted() {
        use crate::event::ScrollPhase::*;
        let (mut app, state) = app();
        let mut st = state.get();
        st.physics.set_max(1700.0);
        st.physics.set_position(400.0);
        state.set(st);
        app.tick();
        app.set_time(0.0);
        let at = Point::new(100.0, 100.0);
        let pinch = |scale, phase| Event::Magnify { at, scale, phase };
        // The content under the pinch point: 400 + 100 at zoom 1.
        app.handle_event(pinch(1.0, Begin));
        app.handle_event(pinch(1.5, Update));
        app.handle_event(pinch(2.0, Update));
        let st = state.get();
        assert!((st.zoom.live - 2.0).abs() < 1e-9, "the spread, not its steps: {}", st.zoom.live);
        assert!((st.physics.y + 100.0 / 2.0 - 500.0).abs() < 1e-6, "kept under the fingers: {}", st.physics.y);
        // Fingers resting: no settling.
        run_frames(&mut app, 0.0, 300.0);
        assert_eq!(state.get().zoom.settled, 1.0);
        app.handle_event(pinch(2.0, End));
        run_frames(&mut app, 300.0, 300.0);
        let st = state.get();
        assert_eq!(st.zoom.settled, 2.0);
        assert_eq!(st.pinch_last, 0.0);
        // The next pinch starts from its own 1.0.
        app.handle_event(pinch(1.0, Begin));
        app.handle_event(pinch(0.5, Update));
        assert!((state.get().zoom.live - 1.0).abs() < 1e-9);
    }
}
