//! Input events — what arrives from fresco-server (or a test driver)
//! and gets routed to handlers on widgets.
//!
//! Phase-3 surface is small: pointer down / up / move + a hit-test
//! function. Keyboard + focus dispatch arrive in phase-3.5 alongside
//! TextField. Pointer events are sufficient to wire Button, which
//! is the first widget vestibulum needs to be clickable.

use crate::geom::Point;
use crate::node::{Node, NodeId, NodeTree};

/// A single input event. Hit-testing is performed by the App; widget
/// code receives the resolved `target_id` (when applicable) via its
/// registered handler, not the raw event.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    PointerDown { at: Point },
    PointerUp { at: Point },
    PointerMove { at: Point },
    /// A keyboard key transition (down or up). `key` is the
    /// USB-HID-usage-code-style identifier from `Key`. `chars` is
    /// any text input the platform decoded for this event (printable
    /// characters and IME composition output go here, separately
    /// from the raw key — TextField appends `chars` to its content).
    Key {
        kind: KeyEventKind,
        key: Key,
        modifiers: Modifiers,
        chars: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEventKind { Down, Up }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub meta: bool,
}

/// Logical key. Phase-3.5 ships only what TextField + Form-nav need;
/// fuller coverage layers in alongside the keyboard/IME pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// Any printable character — actual text comes via `Event::Key.chars`.
    Char,
    Enter,
    Escape,
    Tab,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
}

/// Topmost-node hit test. Walks the tree depth-first; returns the
/// deepest node whose rect contains `point`. Stack containers are
/// considered for hit-testing (their rect is the visible region of
/// their children's collective bounds), but they're excluded from
/// being a hit *target* — the algorithm prefers a non-Stack
/// descendant when one is found.
///
/// Returns `None` if no node contains the point.
pub fn hit_test(tree: &NodeTree, point: Point) -> Option<NodeId> {
    // Walk roots top-down; among siblings the *last* root is on top
    // (added later → drawn later in the wire model).
    for root in tree.roots().collect::<Vec<_>>().into_iter().rev() {
        if let Some(hit) = hit_test_subtree(tree, root, point) {
            return Some(hit);
        }
    }
    None
}

fn hit_test_subtree(tree: &NodeTree, id: NodeId, point: Point) -> Option<NodeId> {
    let node = tree.get(id)?;
    if !node.rect().contains(point) {
        return None;
    }

    // Recurse into children in reverse (later-added = on top).
    for child in tree.children_of(id).iter().rev() {
        if let Some(hit) = hit_test_subtree(tree, *child, point) {
            return Some(hit);
        }
    }

    // No child claimed the hit. Painted nodes are hit targets;
    // layout-only Stacks fall through to the parent — they're layout
    // helpers, not click targets. A *filled* Stack paints (chip,
    // panel), so it takes the hit.
    match node {
        Node::Stack { fill: None, .. } => None,
        _ => Some(id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::Color;
    use crate::geom::{Axis, Rect};

    fn red(rect: Rect) -> Node {
        Node::Rect { rect, fill: Color::rgba(1.0, 0.0, 0.0, 1.0), radius: 0.0 }
    }

    #[test]
    fn outside_returns_none() {
        let mut t = NodeTree::new();
        t.insert(None, red(Rect::new(0.0, 0.0, 10.0, 10.0)));
        assert_eq!(hit_test(&t, Point::new(20.0, 20.0)), None);
    }

    #[test]
    fn picks_innermost_non_stack_child() {
        let mut t = NodeTree::new();
        let stack = t.insert(None, Node::vstack(Rect::new(0.0, 0.0, 100.0, 100.0), 0.0));
        let inner = t.insert(Some(stack), red(Rect::new(10.0, 10.0, 20.0, 20.0)));
        assert_eq!(hit_test(&t, Point::new(15.0, 15.0)), Some(inner));
    }

    #[test]
    fn stack_alone_does_not_register_a_hit() {
        let mut t = NodeTree::new();
        t.insert(None, Node::vstack(Rect::new(0.0, 0.0, 100.0, 100.0), 0.0));
        assert_eq!(hit_test(&t, Point::new(50.0, 50.0)), None);
    }

    #[test]
    fn last_sibling_is_on_top() {
        let mut t = NodeTree::new();
        let a = t.insert(None, red(Rect::new(0.0, 0.0, 100.0, 100.0)));
        let b = t.insert(None, red(Rect::new(20.0, 20.0, 40.0, 40.0)));
        // Both contain (30,30); expect b (added later → on top).
        assert_eq!(hit_test(&t, Point::new(30.0, 30.0)), Some(b));
        // Outside b but inside a: expect a.
        assert_eq!(hit_test(&t, Point::new(80.0, 80.0)), Some(a));
        let _ = a; // suppress unused warning (referenced via expression)
    }
}
