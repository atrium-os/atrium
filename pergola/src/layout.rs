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
    /// Children's label baselines on one line (a row; a grid's rows).
    /// A child's baseline is the one its widget reports
    /// (`Widget::baseline`), a text leaf's own, or its bottom.
    Baseline,
}

/// One grid track's size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Track {
    /// Fixed px.
    Px(f32),
    /// As wide (tall) as its widest (tallest) cell.
    Auto,
    /// A share of what is left.
    Fr(f32),
}

/// A stack laid out as a grid (`NodeTree::set_grid`, `Ctx::grid`): its
/// children fill the cells row by row, left to right; a child spans
/// columns with `FlexStyle::span`. Rows are as tall as their cells.
#[derive(Debug, Clone, PartialEq)]
pub struct Grid {
    pub columns: Vec<Track>,
    pub column_gap: f32,
    pub row_gap: f32,
    /// Cells' vertical alignment in their row (`Baseline` lines up each
    /// row's labels).
    pub align: Align,
    /// Cells' horizontal alignment in their column.
    pub justify: Align,
}

impl Grid {
    pub fn new(columns: impl Into<Vec<Track>>) -> Self {
        Self { columns: columns.into(), column_gap: 0.0, row_gap: 0.0, align: Align::Stretch, justify: Align::Stretch }
    }
    pub fn gaps(mut self, column: f32, row: f32) -> Self {
        self.column_gap = column;
        self.row_gap = row;
        self
    }
    pub fn align(mut self, a: Align) -> Self {
        self.align = a;
        self
    }
    pub fn justify(mut self, a: Align) -> Self {
        self.justify = a;
        self
    }
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
    /// A `Stack` whose children keep the rects they were declared with
    /// (a document, a canvas): layout sizes and places the stack from its
    /// own declared rect, never looks inside, and moves the whole subtree
    /// by however far the stack moved.
    pub free: bool,
    /// The child's baseline, px below its top (set by `Ctx::place`).
    pub baseline: Option<f32>,
    /// Grid columns the child spans (0 and 1: one).
    pub span: u16,
    /// Horizontal alignment in a grid cell (overrides the grid's).
    pub justify_self: Option<Align>,
    // — as a container (Stack nodes) —
    pub padding: f32,
    /// Per-axis padding overrides; fall back to `padding`.
    pub padding_x: Option<f32>,
    pub padding_y: Option<f32>,
    pub justify: Justify,
    pub align: Align,
    /// A row that runs out of width carries on on the next line (`spacing`
    /// apart, as between children).
    pub wrap: bool,
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
            free: false,
            baseline: None,
            span: 0,
            justify_self: None,
            padding: 0.0,
            padding_x: None,
            padding_y: None,
            justify: Justify::Start,
            align: Align::Stretch,
            wrap: false,
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
    if matches!(tree.get(root), Some(Node::Stack { .. } | Node::Scroll { .. })) {
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

    let t_root = match build(tree, root, &mut taffy, 0.0) {
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
        if tree.style(p_id).is_some_and(|s| s.free) {
            let was = tree.get(p_id).map(|n| n.rect().origin).unwrap_or(crate::geom::Point::new(x, y));
            if let Some(node) = tree.get_mut(p_id) {
                node.set_rect(PRect::new(x, y, l.size.width, l.size.height));
            }
            shift(tree, p_id, x - was.x, y - was.y);
            return;
        }
        if let Some(node) = tree.get_mut(p_id) {
            node.set_rect(PRect::new(x, y, l.size.width, l.size.height));
            if let Node::Scroll { content_h, .. } = node {
                *content_h = l.content_size.height;
            }
        }
        let p_children: Vec<PNodeId> = tree.children_of(p_id).to_vec();
        let t_children = taffy.children(t_id).unwrap_or_default();
        for (tc, pc) in t_children.into_iter().zip(p_children) {
            write_back(tree, taffy, tc, pc, x, y, false);
        }
    }
}

/// Move everything under `id` by `(dx, dy)`.
fn shift(tree: &mut NodeTree, id: PNodeId, dx: f32, dy: f32) {
    if dx == 0.0 && dy == 0.0 {
        return;
    }
    for c in tree.children_of(id).to_vec() {
        if let Some(n) = tree.get_mut(c) {
            let moved = n.clone().to_group(crate::geom::Point::new(-dx, -dy), 1.0);
            *n = moved;
        }
        shift(tree, c, dx, dy);
    }
}

fn align_self(a: Align) -> AlignSelf {
    match a {
        Align::Stretch => AlignSelf::STRETCH,
        Align::Start | Align::Baseline => AlignSelf::FLEX_START,
        Align::Center => AlignSelf::CENTER,
        Align::End => AlignSelf::FLEX_END,
    }
}

fn align_items(a: Align) -> AlignItems {
    align_self(a)
}

/// A child's baseline, px below its top: what it reports, a text
/// leaf's own, else its bottom.
fn child_baseline(tree: &NodeTree, id: PNodeId) -> f32 {
    if let Some(b) = tree.style(id).and_then(|s| s.baseline) {
        return b;
    }
    match tree.get(id) {
        Some(Node::Text { content, style, rect }) => {
            let h = if rect.size.h > 0.0 { rect.size.h } else { crate::text::measure(content, style).h };
            crate::text::baseline(h, style)
        }
        Some(n) => {
            // A stack: its first child's (its first line).
            if let Some(&c) = tree.children_of(id).first() {
                if matches!(n, Node::Stack { .. }) && !tree.style(id).is_some_and(|s| s.free) {
                    return child_baseline(tree, c);
                }
            }
            n.rect().size.h
        }
        None => 0.0,
    }
}

/// Recursively mirror the Pergola subtree into taffy nodes.
fn build(
    tree: &NodeTree,
    id: PNodeId,
    taffy: &mut TaffyTree<MeasureCtx>,
    margin_top: f32,
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
        align_self: fs.align_self.map(align_self),
        justify_self: fs.justify_self.map(align_self),
        margin: taffy::Rect { left: zero(), right: zero(), top: length(margin_top), bottom: zero() },
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
        Node::Stack { rect, .. } if fs.free => {
            style.size = Size { width: dim(rect.size.w), height: dim(rect.size.h) };
            taffy.new_leaf(style).ok()
        }
        Node::Stack { rect, axis, spacing, .. } => {
            style.display = Display::Flex;
            style.flex_direction = match axis {
                Axis::Horizontal => FlexDirection::Row,
                Axis::Vertical => FlexDirection::Column,
            };
            style.gap = Size { width: length(*spacing), height: length(*spacing) };
            if fs.wrap {
                style.flex_wrap = FlexWrap::Wrap;
                style.align_content = Some(AlignContent::FLEX_START);
            }
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
            style.align_items = Some(align_items(fs.align));
            style.size = Size { width: dim(rect.size.w), height: dim(rect.size.h) };

            let child_ids: Vec<PNodeId> = tree.children_of(id).to_vec();
            // Which line each child is on, for baseline alignment.
            let mut lines = vec![0usize; child_ids.len()];
            let mut baseline = fs.align == Align::Baseline && *axis == Axis::Horizontal;
            if let Some(g) = tree.grid(id) {
                style.display = Display::Grid;
                style.grid_template_columns = g.columns.iter().map(|t| match *t {
                    Track::Px(v) => length(v),
                    Track::Auto => auto(),
                    Track::Fr(f) => fr(f),
                }).collect();
                style.gap = Size { width: length(g.column_gap), height: length(g.row_gap) };
                style.align_items = Some(align_items(g.align));
                style.justify_items = Some(align_items(g.justify));
                baseline = g.align == Align::Baseline;
                let n = g.columns.len().max(1);
                let mut at = 0usize;
                for (i, &c) in child_ids.iter().enumerate() {
                    let span = tree.style(c).map_or(1, |s| s.span.max(1) as usize).min(n);
                    if at % n + span > n {
                        at += n - at % n;
                    }
                    lines[i] = at / n;
                    at += span;
                }
            }
            let mut tops = vec![0.0f32; child_ids.len()];
            if baseline {
                style.align_items = Some(AlignItems::FLEX_START);
                let bs: Vec<f32> = child_ids.iter().map(|&c| child_baseline(tree, c)).collect();
                let mut max = std::collections::HashMap::<usize, f32>::new();
                for (i, b) in bs.iter().enumerate() {
                    let m = max.entry(lines[i]).or_insert(0.0);
                    *m = m.max(*b);
                }
                for (i, b) in bs.iter().enumerate() {
                    tops[i] = max[&lines[i]] - b;
                }
            }
            let mut t_children = Vec::with_capacity(child_ids.len());
            for (i, c) in child_ids.into_iter().enumerate() {
                if let Some(tc) = build(tree, c, taffy, tops[i]) {
                    if let Some(g) = tree.grid(id) {
                        let span = tree.style(c).map_or(1, |s| s.span.max(1)).min(g.columns.len().max(1) as u16);
                        if span > 1 {
                            let mut st = taffy.style(tc).cloned().unwrap_or_default();
                            st.grid_column = taffy::Line { start: taffy::style_helpers::span(span), end: auto() };
                            let _ = taffy.set_style(tc, st);
                        }
                    }
                    t_children.push(tc);
                }
            }
            taffy.new_with_children(style, &t_children).ok()
        }
        Node::Scroll { rect, .. } => {
            // A column the viewport's width, as tall as its content:
            // scrolling moves it, layout never sees the offset.
            style.display = Display::Flex;
            style.flex_direction = FlexDirection::Column;
            style.align_items = Some(AlignItems::STRETCH);
            style.overflow = taffy::Point { x: taffy::Overflow::Hidden, y: taffy::Overflow::Scroll };
            style.scrollbar_width = 0.0;
            style.size = Size { width: dim(rect.size.w), height: dim(rect.size.h) };
            let px = fs.padding_x.unwrap_or(fs.padding);
            let py = fs.padding_y.unwrap_or(fs.padding);
            style.padding = taffy::Rect { left: length(px), right: length(px), top: length(py), bottom: length(py) };
            let mut t_children = Vec::new();
            for &c in tree.children_of(id) {
                if let Some(tc) = build(tree, c, taffy, 0.0) {
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
        Node::Rect { .. } | Node::Path { .. } | Node::Glyphs { .. } | Node::Fill(_) | Node::Image { .. } => {
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
    fn a_free_stack_keeps_its_childrens_rects_and_moves_them_with_it() {
        let mut tree = NodeTree::new();
        let col = tree.insert(None, Node::vstack(Rect::new(0.0, 0.0, 300.0, 0.0), 0.0));
        tree.insert(Some(col), red_rect(50.0));
        let doc = tree.insert(Some(col), Node::vstack(Rect::new(0.0, 0.0, 300.0, 400.0), 0.0));
        tree.set_style(doc, FlexStyle { free: true, ..FlexStyle::default() });
        let a = tree.insert(Some(doc), Node::Rect { rect: Rect::new(10.0, 10.0, 20.0, 5.0), fill: palette::neutral_300(), radius: 0.0 });
        let b = tree.insert(Some(doc), Node::Rect { rect: Rect::new(40.0, 300.0, 20.0, 5.0), fill: palette::neutral_300(), radius: 0.0 });
        layout(&mut tree, col);
        assert_eq!(tree.get(doc).unwrap().rect(), Rect::new(0.0, 50.0, 300.0, 400.0));
        assert_eq!(tree.get(a).unwrap().rect(), Rect::new(10.0, 60.0, 20.0, 5.0), "not laid out, only moved");
        assert_eq!(tree.get(b).unwrap().rect(), Rect::new(40.0, 350.0, 20.0, 5.0));
    }

    #[test]
    fn a_wrapping_row_carries_on_on_the_next_line() {
        let mut tree = NodeTree::new();
        let row = tree.insert(None, Node::hstack(Rect::new(0.0, 0.0, 100.0, 0.0), 10.0));
        tree.set_style(row, FlexStyle { wrap: true, align: Align::Start, ..FlexStyle::default() });
        let ids: Vec<_> = (0..3).map(|_| tree.insert(Some(row), Node::Rect { rect: Rect::new(0.0, 0.0, 40.0, 20.0), fill: palette::neutral_300(), radius: 0.0 })).collect();
        layout(&mut tree, row);
        assert_eq!(tree.get(ids[1]).unwrap().rect(), Rect::new(50.0, 0.0, 40.0, 20.0));
        assert_eq!(tree.get(ids[2]).unwrap().rect(), Rect::new(0.0, 30.0, 40.0, 20.0), "no room: next line");
        assert_eq!(tree.get(row).unwrap().rect().h(), 50.0);
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
