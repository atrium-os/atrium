//! Layout pass — taffy-backed flexbox over `Node::Stack` subtrees.
//!
//! The public contract is unchanged from the phase-2 cursor-walk:
//! `layout(tree, root)` assigns **absolute** rects in place, nodes
//! outside any `Stack` keep their declared rects (the absolute-
//! positioning escape hatch every bring-up app relies on), and a
//! `Stack` distributes its children along its axis with `spacing`
//! between them, stretching the cross axis unless a child declared a
//! cross size.
//!
//! What taffy adds on top: `FlexStyle` (per-node, via
//! `NodeTree::set_style` / `Ctx::set_flex`) — grow/shrink/basis,
//! per-child alignment, container padding and justify/align, and
//! min/max clamps. Text nodes are measured leaves: taffy calls back
//! into `crate::text::measure`, so shaped widths drive layout.
//!
//! Compatibility defaults are deliberate: `flex-shrink` is 0 (the old
//! walk never shrank children) and `align-items` is Stretch (the old
//! cross-fill rule).

use taffy::prelude::*;
use taffy::TaffyTree;

use crate::geom::{Axis, Rect as PRect};
use crate::node::{Node, NodeId as PNodeId, NodeTree, TextStyle};

/// Main-axis distribution for a `Stack` container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Justify {
    #[default]
    Start,
    Center,
    End,
    SpaceBetween,
}

/// Cross-axis alignment — container-wide (`align`) or per-child
/// (`align_self`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    #[default]
    Stretch,
    Start,
    Center,
    End,
}

/// Flex parameters attached to a node via the tree's side-table.
/// Child fields apply to any node inside a Stack; container fields
/// apply when the node is itself a Stack.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FlexStyle {
    // — as a child —
    pub grow: f32,
    pub shrink: f32,
    pub basis: Option<f32>,
    pub align_self: Option<Align>,
    pub min_width: Option<f32>,
    pub min_height: Option<f32>,
    pub max_width: Option<f32>,
    pub max_height: Option<f32>,
    /// Absolute positioning escape hatch *inside* a flex container:
    /// taffy takes the node out of flow and pins it by inset
    /// [left, top, right, bottom] (each optional, px, negatives OK)
    /// relative to the container. Used for overlays like the bell's
    /// badge dot.
    pub absolute_inset: Option<[Option<f32>; 4]>,
    // — as a container (Stack nodes) —
    pub padding: f32,
    /// Per-axis padding overrides; fall back to `padding`.
    pub padding_x: Option<f32>,
    pub padding_y: Option<f32>,
    pub justify: Justify,
    pub align: Align,
}

impl Default for FlexStyle {
    fn default() -> Self {
        Self {
            grow: 0.0,
            shrink: 0.0,
            basis: None,
            align_self: None,
            min_width: None,
            min_height: None,
            max_width: None,
            max_height: None,
            absolute_inset: None,
            padding: 0.0,
            padding_x: None,
            padding_y: None,
            justify: Justify::Start,
            align: Align::Stretch,
        }
    }
}

impl FlexStyle {
    /// A grow-only spacer/child style.
    pub fn grow(factor: f32) -> Self {
        Self { grow: factor, ..Self::default() }
    }
}

/// Run layout from `root`: find each outermost `Stack` and flex its
/// whole subtree (nested Stacks included); everything else keeps its
/// declared rect.
pub fn layout(tree: &mut NodeTree, root: PNodeId) {
    if matches!(tree.get(root), Some(Node::Stack { .. })) {
        flex_subtree(tree, root);
        return;
    }
    let children: Vec<PNodeId> = tree.children_of(root).to_vec();
    for child in children {
        layout(tree, child);
    }
}

/// Text measurement context handed to taffy leaves.
struct MeasureCtx {
    content: String,
    style: TextStyle,
}

fn flex_subtree(tree: &mut NodeTree, root: PNodeId) {
    let mut taffy: TaffyTree<MeasureCtx> = TaffyTree::new();

    let t_root = match build(tree, root, &mut taffy) {
        Some(id) => id,
        None => return,
    };

    let root_rect = tree.get(root).map(|n| n.rect()).unwrap_or(PRect::ZERO_SIZED);
    let available = Size {
        width: if root_rect.size.w > 0.0 {
            AvailableSpace::Definite(root_rect.size.w)
        } else {
            AvailableSpace::MaxContent
        },
        height: if root_rect.size.h > 0.0 {
            AvailableSpace::Definite(root_rect.size.h)
        } else {
            AvailableSpace::MaxContent
        },
    };

    let result = taffy.compute_layout_with_measure(
        t_root,
        available,
        |known, _avail, _id, ctx: Option<&mut MeasureCtx>, _style| {
            let measured = match ctx {
                Some(m) => crate::text::measure(&m.content, &m.style),
                None => return Size::ZERO,
            };
            Size {
                width: known.width.unwrap_or(measured.w),
                height: known.height.unwrap_or(measured.h),
            }
        },
    );
    if let Err(e) = result {
        log::error!("layout: taffy compute failed: {e:?}");
        return;
    }

    // Write back absolute rects. Taffy locations are parent-relative;
    // accumulate from the subtree root's declared origin. The root
    // keeps its declared origin (it was positioned by its own parent
    // context or absolutely by the app).
    write_back(tree, &taffy, t_root, root, root_rect.origin.x, root_rect.origin.y, true);

    fn write_back(
        tree: &mut NodeTree,
        taffy: &TaffyTree<MeasureCtx>,
        t_id: taffy::NodeId,
        p_id: PNodeId,
        origin_x: f32,
        origin_y: f32,
        is_root: bool,
    ) {
        let l = match taffy.layout(t_id) {
            Ok(l) => *l,
            Err(_) => return,
        };
        let (x, y) = if is_root {
            (origin_x, origin_y)
        } else {
            (origin_x + l.location.x, origin_y + l.location.y)
        };
        if let Some(node) = tree.get_mut(p_id) {
            node.set_rect(PRect::new(x, y, l.size.width, l.size.height));
        }
        let p_children: Vec<PNodeId> = tree.children_of(p_id).to_vec();
        let t_children = taffy.children(t_id).unwrap_or_default();
        for (tc, pc) in t_children.into_iter().zip(p_children) {
            write_back(tree, taffy, tc, pc, x, y, false);
        }
    }
}

/// Recursively mirror the Pergola subtree into taffy nodes.
fn build(
    tree: &NodeTree,
    id: PNodeId,
    taffy: &mut TaffyTree<MeasureCtx>,
) -> Option<taffy::NodeId> {
    let node = tree.get(id)?;
    let fs = tree.style(id).copied().unwrap_or_default();

    let dim = |v: f32| -> Dimension {
        if v > 0.0 { length(v) } else { auto() }
    };
    let opt_dim = |v: Option<f32>| -> Dimension {
        match v {
            Some(v) => length(v),
            None => auto(),
        }
    };

    let mut style = Style {
        flex_grow: fs.grow,
        flex_shrink: fs.shrink,
        flex_basis: opt_dim(fs.basis),
        align_self: fs.align_self.map(|a| match a {
            Align::Stretch => AlignSelf::STRETCH,
            Align::Start => AlignSelf::FLEX_START,
            Align::Center => AlignSelf::CENTER,
            Align::End => AlignSelf::FLEX_END,
        }),
        min_size: Size { width: opt_dim(fs.min_width), height: opt_dim(fs.min_height) },
        max_size: Size { width: opt_dim(fs.max_width), height: opt_dim(fs.max_height) },
        ..Style::default()
    };
    if let Some([l, t, r, b]) = fs.absolute_inset {
        style.position = Position::Absolute;
        let side = |v: Option<f32>| -> LengthPercentageAuto {
            match v {
                Some(v) => length(v),
                None => auto(),
            }
        };
        style.inset = taffy::Rect { left: side(l), top: side(t), right: side(r), bottom: side(b) };
    }

    match node {
        Node::Stack { rect, axis, spacing, .. } => {
            style.display = Display::Flex;
            style.flex_direction = match axis {
                Axis::Horizontal => FlexDirection::Row,
                Axis::Vertical => FlexDirection::Column,
            };
            style.gap = Size { width: length(*spacing), height: length(*spacing) };
            let px = fs.padding_x.unwrap_or(fs.padding);
            let py = fs.padding_y.unwrap_or(fs.padding);
            style.padding = taffy::Rect {
                left: length(px),
                right: length(px),
                top: length(py),
                bottom: length(py),
            };
            style.justify_content = Some(match fs.justify {
                Justify::Start => JustifyContent::FLEX_START,
                Justify::Center => JustifyContent::CENTER,
                Justify::End => JustifyContent::FLEX_END,
                Justify::SpaceBetween => JustifyContent::SPACE_BETWEEN,
            });
            style.align_items = Some(match fs.align {
                Align::Stretch => AlignItems::STRETCH,
                Align::Start => AlignItems::FLEX_START,
                Align::Center => AlignItems::CENTER,
                Align::End => AlignItems::FLEX_END,
            });
            style.size = Size { width: dim(rect.size.w), height: dim(rect.size.h) };

            let child_ids: Vec<PNodeId> = tree.children_of(id).to_vec();
            let mut t_children = Vec::with_capacity(child_ids.len());
            for c in child_ids {
                if let Some(tc) = build(tree, c, taffy) {
                    t_children.push(tc);
                }
            }
            taffy.new_with_children(style, &t_children).ok()
        }
        Node::Text { content, style: text_style, .. } => {
            taffy
                .new_leaf_with_context(
                    style,
                    MeasureCtx { content: content.clone(), style: text_style.clone() },
                )
                .ok()
        }
        Node::Rect { .. } | Node::Path { .. } => {
            let r = node.rect();
            style.size = Size { width: dim(r.size.w), height: dim(r.size.h) };
            taffy.new_leaf(style).ok()
        }
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
        let stack = tree.insert(None, Node::vstack(Rect::new(10.0, 20.0, 200.0, 200.0), 8.0));
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
        let stack = tree.insert(None, Node::hstack(Rect::new(0.0, 0.0, 400.0, 32.0), 12.0));
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

    #[test]
    fn grow_spacer_pushes_following_children_to_the_end() {
        let mut tree = NodeTree::new();
        let bar = tree.insert(None, Node::hstack(Rect::new(0.0, 0.0, 500.0, 38.0), 0.0));
        let left = tree.insert(Some(bar), Node::Rect {
            rect: Rect::new(0.0, 0.0, 100.0, 0.0),
            fill: palette::neutral_300(),
            radius: 0.0,
        });
        let spacer = tree.insert(Some(bar), Node::hstack(Rect::ZERO_SIZED, 0.0));
        tree.set_style(spacer, FlexStyle::grow(1.0));
        let right = tree.insert(Some(bar), Node::Rect {
            rect: Rect::new(0.0, 0.0, 60.0, 0.0),
            fill: palette::neutral_300(),
            radius: 0.0,
        });

        layout(&mut tree, bar);

        assert_eq!(tree.get(left).unwrap().rect(), Rect::new(0.0, 0.0, 100.0, 38.0));
        // Spacer swallowed 500-160; the right block hugs the end.
        assert_eq!(tree.get(right).unwrap().rect(), Rect::new(440.0, 0.0, 60.0, 38.0));
    }

    #[test]
    fn container_padding_and_center_justify() {
        let mut tree = NodeTree::new();
        let col = tree.insert(None, Node::vstack(Rect::new(0.0, 0.0, 100.0, 100.0), 0.0));
        tree.set_style(col, FlexStyle {
            padding: 10.0,
            justify: Justify::Center,
            align: Align::Center,
            ..FlexStyle::default()
        });
        let child = tree.insert(Some(col), Node::Rect {
            rect: Rect::new(0.0, 0.0, 40.0, 20.0),
            fill: palette::neutral_300(),
            radius: 0.0,
        });

        layout(&mut tree, col);

        // Centered in the padded 80×80 content box.
        assert_eq!(tree.get(child).unwrap().rect(), Rect::new(30.0, 40.0, 40.0, 20.0));
    }

    #[test]
    fn text_leaf_is_measured_for_main_axis() {
        use crate::node::TextStyle;
        use crate::theme::tokens::Weight;
        // Headless: the estimate (0.55·size per char) drives layout.
        crate::text::clear_measurer();
        let mut tree = NodeTree::new();
        let row = tree.insert(None, Node::hstack(Rect::new(0.0, 0.0, 400.0, 24.0), 4.0));
        let label = tree.insert(Some(row), Node::Text {
            rect: Rect::ZERO_SIZED,
            content: "abcd".into(),
            style: TextStyle {
                family: "system-sans".into(),
                size: 10.0,
                weight: Weight::Regular,
                color: Color::TRANSPARENT,
            },
        });
        let after = tree.insert(Some(row), Node::Rect {
            rect: Rect::new(0.0, 0.0, 30.0, 0.0),
            fill: palette::neutral_300(),
            radius: 0.0,
        });

        layout(&mut tree, row);

        let lw = 4.0 * 10.0 * 0.55;
        assert_eq!(tree.get(label).unwrap().rect().size.w, lw);
        assert_eq!(tree.get(after).unwrap().rect().origin.x, lw + 4.0);
    }
}
