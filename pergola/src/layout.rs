//! Layout pass — walks the `NodeTree` and assigns concrete rects to
//! each node based on its parent's container kind.
//!
//! Phase-2 scope is intentionally minimal: a vertical/horizontal
//! `Stack` distributes children along its axis using each child's
//! intrinsic size + the stack's `spacing`. Children whose declared
//! `rect.size.w` (for vertical stacks) or `.h` (for horizontal stacks)
//! is `0.0` are auto-sized to *fill* the cross-axis; otherwise the
//! declared size wins.
//!
//! Real flex/grid (taffy integration) layers on top of this in a
//! later phase, when widget needs justify it. Most widgets we have
//! in flight (panels, button rows, login form) are pure stacks.
//!
//! The layout pass mutates the tree in place: `Node::set_rect` is
//! called on each node with its computed rect in *parent-relative*
//! coordinates. The diff in `node::diff` then sees the computed
//! geometry, so position-only changes still emit minimal deltas.

use crate::geom::{Axis, Rect};
use crate::node::{Node, NodeId, NodeTree};

/// Run layout starting from `root`. Walks the tree top-down: at each
/// Stack container, lays out its direct children along the axis;
/// recurses into each child for its own sub-layout.
pub fn layout(tree: &mut NodeTree, root: NodeId) {
    layout_subtree(tree, root);
}

fn layout_subtree(tree: &mut NodeTree, id: NodeId) {
    let parent_rect = match tree.get(id) {
        Some(node) => node.rect(),
        None => return,
    };

    let kind = tree.get(id).cloned();
    if let Some(Node::Stack { axis, spacing, .. }) = kind {
        layout_stack(tree, id, parent_rect, axis, spacing);
    }

    // Recurse — for non-Stack containers (which we don't have yet)
    // and for nested Stacks within Stacks.
    let children: Vec<NodeId> = tree.children_of(id).to_vec();
    for child in children {
        layout_subtree(tree, child);
    }
}

fn layout_stack(
    tree: &mut NodeTree,
    parent: NodeId,
    parent_rect: Rect,
    axis: Axis,
    spacing: f32,
) {
    let children: Vec<NodeId> = tree.children_of(parent).to_vec();
    if children.is_empty() {
        return;
    }

    let mut cursor = match axis {
        Axis::Vertical => parent_rect.origin.y,
        Axis::Horizontal => parent_rect.origin.x,
    };

    let cross_origin = match axis {
        Axis::Vertical => parent_rect.origin.x,
        Axis::Horizontal => parent_rect.origin.y,
    };
    let cross_extent = match axis {
        Axis::Vertical => parent_rect.size.w,
        Axis::Horizontal => parent_rect.size.h,
    };

    for (i, child_id) in children.iter().enumerate() {
        if i > 0 {
            cursor += spacing;
        }
        // Snapshot the child's intrinsic + declared sizes.
        let (intrinsic, declared) = match tree.get(*child_id) {
            Some(node) => (node.intrinsic_size(), node.rect().size),
            None => continue,
        };

        let (main_size, cross_size) = match axis {
            Axis::Vertical => {
                // child wants its intrinsic height; takes parent width unless it declared one
                let main = intrinsic.h;
                let cross = if declared.w > 0.0 { declared.w } else { cross_extent };
                (main, cross)
            }
            Axis::Horizontal => {
                let main = intrinsic.w;
                let cross = if declared.h > 0.0 { declared.h } else { cross_extent };
                (main, cross)
            }
        };

        let new_rect = match axis {
            Axis::Vertical => Rect::new(cross_origin, cursor, cross_size, main_size),
            Axis::Horizontal => Rect::new(cursor, cross_origin, main_size, cross_size),
        };

        if let Some(node) = tree.get_mut(*child_id) {
            node.set_rect(new_rect);
        }

        cursor += main_size;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::Color;
    use crate::geom::Rect;
    use crate::node::Node;
    use crate::theme::{palette, radius};

    fn red_rect(h: f32) -> Node {
        Node::Rect {
            rect: Rect::new(0.0, 0.0, 0.0, h),
            fill: Color::rgba(1.0, 0.0, 0.0, 1.0),
            radius: radius::SM,
        }
    }

    #[test]
    fn vertical_stack_distributes_children() {
        let mut tree = NodeTree::new();
        let stack = tree.insert(None, Node::Stack {
            rect: Rect::new(10.0, 20.0, 200.0, 200.0),
            axis: Axis::Vertical,
            spacing: 8.0,
        });
        let a = tree.insert(Some(stack), red_rect(24.0));
        let b = tree.insert(Some(stack), red_rect(32.0));
        let c = tree.insert(Some(stack), red_rect(16.0));

        layout(&mut tree, stack);

        assert_eq!(tree.get(a).unwrap().rect(), Rect::new(10.0, 20.0, 200.0, 24.0));
        assert_eq!(tree.get(b).unwrap().rect(), Rect::new(10.0, 20.0 + 24.0 + 8.0, 200.0, 32.0));
        assert_eq!(tree.get(c).unwrap().rect(), Rect::new(10.0, 20.0 + 24.0 + 8.0 + 32.0 + 8.0, 200.0, 16.0));
    }

    #[test]
    fn horizontal_stack_distributes_children() {
        let mut tree = NodeTree::new();
        let stack = tree.insert(None, Node::Stack {
            rect: Rect::new(0.0, 0.0, 400.0, 32.0),
            axis: Axis::Horizontal,
            spacing: 12.0,
        });
        // For a horizontal stack we set rect.size.w, leave .h to fill cross-axis.
        let a = tree.insert(Some(stack), Node::Rect {
            rect: Rect::new(0.0, 0.0, 64.0, 0.0),
            fill: palette::neutral_300(),
            radius: 0.0,
        });
        let b = tree.insert(Some(stack), Node::Rect {
            rect: Rect::new(0.0, 0.0, 80.0, 0.0),
            fill: palette::neutral_300(),
            radius: 0.0,
        });

        layout(&mut tree, stack);

        assert_eq!(tree.get(a).unwrap().rect(), Rect::new(0.0, 0.0, 64.0, 32.0));
        assert_eq!(tree.get(b).unwrap().rect(), Rect::new(64.0 + 12.0, 0.0, 80.0, 32.0));
    }
}
