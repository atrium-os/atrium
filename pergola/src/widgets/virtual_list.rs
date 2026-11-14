//! `VirtualList`: a list of any length that only emits the rows near the
//! viewport (visual language rev. 2 §14.2, list row). It is a
//! [`ScrollView`] whose content is a spacer, the rows in view plus an
//! overscan, and a spacer, so the scroll range is the whole list while the
//! scene holds a few screens of rows. Rows are keyed by index, so a row
//! keeps its node ids as the window moves over it.
//!
//! Rows have one fixed height (`size::LIST_ROW_DEFAULT` or `_DENSE`).
//! Variable heights wait for the band sources `DocumentView` brings.

use std::rc::Rc;

use crate::geom::Rect;
use crate::node::Node;
use crate::reactive::Mutable;
use crate::view::{Ctx, Draw};
use crate::widgets::scroll_view::{ScrollState, ScrollView};
use crate::Color;

/// Rows emitted beyond each edge of the viewport, in viewports.
pub const OVERSCAN_VIEWPORTS: f64 = 1.0;

/// Draws row `i` into `ctx`, `row_h` high and as wide as the list.
pub type RowFn = Rc<dyn Fn(usize, &mut Ctx)>;

pub struct VirtualList {
    pub state: Mutable<ScrollState>,
    pub rect: Rect,
    pub count: usize,
    pub row_h: f32,
    row: RowFn,
    key: Option<u64>,
    backdrop: Option<Color>,
}

impl VirtualList {
    pub fn new(state: Mutable<ScrollState>, rect: Rect, count: usize, row_h: f32, row: impl Fn(usize, &mut Ctx) + 'static) -> Self {
        Self { state, rect, count, row_h, row: Rc::new(row), key: None, backdrop: None }
    }

    pub fn key(mut self, k: u64) -> Self {
        self.key = Some(k);
        self
    }

    /// The opaque colour under the rows (the scroll view's backdrop;
    /// default the theme's `bg_window`).
    pub fn backdrop(mut self, c: Color) -> Self {
        self.backdrop = Some(c);
        self
    }

    /// The rows to emit for scroll position `y` (content px at zoom 1)
    /// over a viewport `shown` px tall.
    pub fn window(count: usize, row_h: f32, y: f64, shown: f64) -> std::ops::Range<usize> {
        if count == 0 || row_h <= 0.0 {
            return 0..0;
        }
        let h = row_h as f64;
        let pad = shown * OVERSCAN_VIEWPORTS;
        let first = ((y - pad) / h).floor().max(0.0) as usize;
        let last = (((y + shown + pad) / h).ceil() as usize).min(count);
        first.min(last)..last
    }

    pub fn emit(&self, ctx: &mut Ctx) -> crate::node::NodeId {
        let st = self.state.get();
        let shown = self.rect.h() as f64 / st.zoom.settled.max(0.01);
        let rows = Self::window(self.count, self.row_h, st.physics.y, shown);
        let (w, h, count) = (self.rect.w(), self.row_h, self.count);
        let row = self.row.clone();
        let content = Draw(move |ctx: &mut Ctx| {
            let spacer = |ctx: &mut Ctx, key: u64, n: usize| {
                ctx.key(key);
                ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, w, n as f32 * h), fill: Color::TRANSPARENT, radius: 0.0 });
            };
            spacer(ctx, 0, rows.start);
            for i in rows.clone() {
                ctx.key(i as u64 + 1);
                row(i, ctx);
            }
            spacer(ctx, u64::MAX, count - rows.end);
        });
        let mut v = ScrollView::new(self.state.clone(), self.rect, content);
        if let Some(k) = self.key {
            v = v.key(k);
        }
        if let Some(c) = self.backdrop {
            v = v.backdrop(c);
        }
        v.emit(ctx)
    }
}

impl crate::view::View for VirtualList {
    fn render(&self, ctx: &mut Ctx) {
        self.emit(ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::node::NodeDelta;
    use crate::view::View;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    struct Big {
        state: Mutable<ScrollState>,
    }

    impl View for Big {
        fn render(&self, ctx: &mut Ctx) {
            VirtualList::new(self.state.clone(), Rect::new(0.0, 0.0, 300.0, 320.0), 100_000, 32.0, |i, ctx| {
                ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, 300.0, 32.0), fill: Color::rgba((i % 7) as f32 / 7.0, 0.0, 0.0, 1.0), radius: 0.0 });
            })
            .emit(ctx);
        }
    }

    fn rects(d: &[NodeDelta]) -> usize {
        d.iter().filter(|d| matches!(d, NodeDelta::Added { node: Node::Rect { .. }, .. })).count()
    }

    #[test]
    fn the_window_covers_the_viewport_and_one_more_each_side() {
        assert_eq!(VirtualList::window(100_000, 32.0, 0.0, 320.0), 0..20);
        assert_eq!(VirtualList::window(100_000, 32.0, 3200.0, 320.0), 90..120);
        assert_eq!(VirtualList::window(25, 32.0, 3200.0, 320.0), 25..25, "past the end: nothing");
        assert_eq!(VirtualList::window(0, 32.0, 0.0, 320.0), 0..0);
    }

    #[test]
    fn a_hundred_thousand_rows_send_a_few_screens() {
        let dirty = Arc::new(AtomicBool::new(true));
        let state = Mutable::with_dirty(ScrollState::default(), dirty.clone());
        let mut app = App::new_with_flag(Big { state: state.clone() }, dirty);
        let d = app.tick();
        // 20 rows, two spacers, the scroll view's backdrop.
        assert!(rects(&d) <= 24, "{} rects", rects(&d));
        let h = d.iter().find_map(|d| match d {
            NodeDelta::Added { node: Node::Scroll { content_h, .. }, .. } => Some(*content_h),
            _ => None,
        });
        assert_eq!(h, Some(3_200_000.0), "the scroll range is the whole list");

        // Far down the list: the window moves, the scene stays small.
        let mut st = state.get();
        st.physics.y = 1_600_000.0;
        state.set(st);
        let d = app.tick();
        // Three viewports of rows (the view and one each side) replace the
        // first twenty; nothing else in the list is ever sent.
        let added = rects(&d);
        assert_eq!(added, 30, "the new window's rows");
        let removed = d.iter().filter(|d| matches!(d, NodeDelta::Removed { .. })).count();
        assert_eq!(removed, 20, "the old window's rows go");
    }
}
