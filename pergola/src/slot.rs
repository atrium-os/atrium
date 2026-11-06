//! Embed slots: places in a Pergola window where another process's
//! surface is shown (docs/spec/limen.md §5; for credentials,
//! docs/spec/arca-autofill.md §4).
//!
//! A widget marks a node with [`crate::view::Ctx::slot`]. After every
//! layout the `App` works out the node's rect in window coordinates and,
//! when it changed, the `Window` tells its [`SlotBinder`]. The binder is
//! Limen (not built yet; frescod's `OP_WM_CREDENTIAL_SLOT` is the stand-in
//! for its slot half), which moves the child's surface there.
//!
//! A slot is only ever placed whole. One that a scroll view clips, even
//! partly, is released until it is fully in view again, because the
//! child's surface would otherwise show over the scroll view's
//! surroundings. Each move restarts frescod's 500 ms visibility run for a
//! credential slot, so a field being scrolled cannot be filled.

use crate::geom::{Point, Rect, Size};
use crate::node::{Node, NodeId, NodeTree};

/// The app's name for a slot, stable across renders (a widget picks it).
pub type SlotKey = u64;

/// Moves embedded surfaces. `rect` is `[x, y, w, h]` in the window's
/// surface pixels (logical px times the scale); `None` releases the slot.
pub trait SlotBinder {
    fn place(&mut self, window_id: u32, key: SlotKey, rect: Option<[f32; 4]>);
}

/// `id`'s rect in window coordinates, or `None` if a scroll view clips any
/// of it or it is empty.
pub fn window_rect(tree: &NodeTree, id: NodeId) -> Option<Rect> {
    let mut r = tree.get(id)?.rect();
    let mut at = id;
    while let Some(p) = tree.parent_of(at) {
        if let Some(Node::Scroll { rect, offset, zoom, stretch, .. }) = tree.get(p) {
            // The inverse of hit testing's mapping (event.rs).
            let z = zoom * stretch;
            let o = rect.origin;
            r = Rect {
                origin: Point::new(o.x + (r.x() - o.x) * z, o.y + (r.y() - offset - o.y) * z),
                size: Size::new(r.w() * z, r.h() * z),
            };
            let inside = r.x() >= rect.x() && r.y() >= rect.y()
                && r.x() + r.w() <= rect.x() + rect.w() + 0.01
                && r.y() + r.h() <= rect.y() + rect.h() + 0.01;
            if !inside {
                return None;
            }
        }
        at = p;
    }
    (r.w() > 0.0 && r.h() > 0.0).then_some(r)
}
