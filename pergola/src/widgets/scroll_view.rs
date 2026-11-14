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
//! What this prototype leaves out, stated: horizontal scrolling, a
//! scrollbar, the fling blur on the wire (the renderer has no blur op
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
    pub zoom: Zoom,
    pub reduce_motion: bool,
    /// The running pinch's scale at its last step (0: no pinch).
    pub pinch_last: f64,
}

impl ScrollState {
    /// Bring the bounds up to date with the node as last laid out: the
    /// content's height at zoom 1 minus what the viewport shows of it.
    fn fit(&mut self, node: &Node) {
        if let Node::Scroll { rect, content_h, .. } = node {
            let shown = rect.h() as f64 / self.zoom.live;
            self.physics.set_max(*content_h as f64 - shown);
        }
    }

    fn viewport_h(node: &Node) -> f64 { node.rect().h() as f64 }

    /// Apply one input at `t` ms.
    pub fn input(&mut self, kind: ScrollKind, t: f64, node: &Node) {
        self.fit(node);
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
                let z0 = self.zoom.live;
                self.zoom.pinch(s / self.pinch_last, t);
                self.pinch_last = s;
                let down = matches!(phase, ScrollPhase::Begin | ScrollPhase::Update);
                self.zoom.hold(down, t);
                if !down {
                    self.pinch_last = 0.0;
                }
                self.refocus(z0, focus, node);
            }
            ScrollKind::Wheel { at, dy, modifiers, .. } if modifiers.ctrl => {
                let focus = at.map(|p| (p.y - node.rect().y()) as f64).unwrap_or(0.0);
                let z0 = self.zoom.live;
                self.zoom.pinch((-(dy as f64) * WHEEL_ZOOM_PER_PX).exp(), t);
                self.refocus(z0, focus, node);
            }
            // A touchpad that says where the fingers are: they follow it,
            // fingers down catch a fling, and lifting them throws one.
            ScrollKind::Wheel { dy, phase, .. } if phase != ScrollPhase::None => {
                if phase == ScrollPhase::Begin {
                    self.physics.stop();
                }
                if phase == ScrollPhase::End {
                    self.physics.release(t);
                } else if dy != 0.0 {
                    self.physics.hold(dy as f64 / self.zoom.live, t);
                }
            }
            ScrollKind::Wheel { dy, .. } => {
                let d = dy as f64 / self.zoom.live;
                if dy.abs() >= NOTCH_PX {
                    self.physics.jump_by(d, t);
                } else {
                    self.physics.drag(d, t);
                }
            }
            ScrollKind::Key { key, chars, modifiers } => {
                let z0 = self.zoom.live;
                match (key, chars) {
                    (Key::Up, _) => self.physics.jump_by(-LINE_PX, t),
                    (Key::Down, _) => self.physics.jump_by(LINE_PX, t),
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
                    // Keyboard zoom keeps the top of the viewport.
                    self.refocus(z0, 0.0, node);
                }
            }
        }
    }

    /// After the live zoom changed from `z0`: keep the content under the
    /// point `focus` px from the viewport's top where it is.
    fn refocus(&mut self, z0: f64, focus: f64, node: &Node) {
        let y = zoom_about(self.physics.y, focus, z0, self.zoom.live);
        self.fit(node);
        self.physics.set_position(y);
    }

    /// One frame at `t` ms. Returns whether it still needs frames.
    pub fn frame(&mut self, t: f64, node: &Node) -> bool {
        self.fit(node);
        self.physics.step(t);
        // A settle re-renders at the new zoom (the payloads change, the
        // stretch returns to 1): the "sharpen on settle" step.
        self.zoom.step(t);
        self.physics.is_moving() || self.zoom.is_pending()
    }
}

/// A vertical scroll view. `content` renders inside it, laid out in a
/// column as wide as the view.
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

/// Where the point `p` (window px) falls in the content of the scroll
/// view `node`, in content px at zoom 1.
pub fn content_point(node: &Node, p: Point) -> Option<Point> {
    let Node::Scroll { rect, offset, zoom, stretch, .. } = node else { return None };
    let z = zoom * stretch;
    Some(Point::new((p.x - rect.x()) / z, (p.y - rect.y()) / z + offset))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::event::{Event, KeyEventKind, Modifiers};
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
