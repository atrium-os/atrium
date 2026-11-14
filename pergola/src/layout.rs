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
    /// apart, as between children). On a text leaf: the text breaks into
    /// lines at the width layout gives it (`Ctx::paragraph`).
    pub wrap: bool,
    /// A wrapping text leaf: hyphenate by dictionary (`wrap::Wrap::hyphens`).
    pub hyphens: bool,
    /// A wrapping text leaf: at most this many lines, the last ending in
    /// "…" (0: no limit).
    pub max_lines: u16,
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
            hyphens: false,
            max_lines: 0,
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
    /// Breaks into lines at the width it is given, so.
    wrap: Option<crate::wrap::Wrap>,
    /// Text with inline objects instead ([`crate::flow`]); breaks too.
    flow: Option<crate::flow::Shared>,
}

thread_local! {
    static MIN_WANTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static MINS: std::cell::RefCell<Vec<(PNodeId, crate::geom::Size)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Have the next layout on this thread also find each laid-out root's
/// natural minimum ([`take_natural_min`]).
pub fn request_natural_min() {
    MIN_WANTED.with(|w| w.set(true));
    MINS.with(|m| m.borrow_mut().clear());
}

/// The natural minimum the last requested layout found for `root` (once;
/// the request ends here): the
/// narrowest it can lay out (its widest word, control or fixed part) and
/// how tall it is at its width. What a window tells the WM as its minimum
/// size (resolution-independence.md §6.3); it grows with the text size.
pub fn take_natural_min(root: Option<PNodeId>) -> Option<crate::geom::Size> {
    MIN_WANTED.with(|w| w.set(false));
    let all = MINS.with(|m| std::mem::take(&mut *m.borrow_mut()));
    all.into_iter().find(|(id, _)| Some(*id) == root).map(|(_, s)| s)
}

/// Lay out `t_root` in `available`, measuring text leaves.
fn compute(taffy: &mut TaffyTree<MeasureCtx>, t_root: taffy::NodeId, available: Size<AvailableSpace>) -> Result<(), taffy::TaffyError> {
    taffy.compute_layout_with_measure(
        t_root,
        available,
        |known, _avail, _id, ctx: Option<&mut MeasureCtx>, _style| {
            let width_of = |a: AvailableSpace| match a {
                AvailableSpace::Definite(w) => w,
                AvailableSpace::MinContent => 0.0,
                AvailableSpace::MaxContent => f32::INFINITY,
            };
            let measured = match ctx {
                Some(MeasureCtx { flow: Some(f), .. }) => {
                    let width = known.width.unwrap_or(width_of(_avail.width));
                    crate::flow::lay_out(f, width).size
                }
                Some(MeasureCtx { wrap: Some(w), content, style, .. }) => {
                    let wrap = *w;
                    let width = known.width.unwrap_or(match _avail.width {
                        AvailableSpace::Definite(w) => w,
                        AvailableSpace::MinContent => 0.0,
                        AvailableSpace::MaxContent => f32::INFINITY,
                    });
                    crate::text::measure_wrapped(content, style, width, &wrap)
                }
                Some(m) => crate::text::measure(&m.content, &m.style),
                None => return Size::ZERO,
            };
            Size {
                width: known.width.unwrap_or(measured.w),
                height: known.height.unwrap_or(measured.h),
            }
        },
    )
}

/// `t_root`'s natural minimum: its min-content width, and its height laid
/// out at `width` with no height given. Leaves its style as it was.
fn natural_min(taffy: &mut TaffyTree<MeasureCtx>, t_root: taffy::NodeId, width: f32) -> Option<crate::geom::Size> {
    let orig = taffy.style(t_root).ok()?.clone();
    let mut st = orig.clone();
    st.size = Size { width: auto(), height: auto() };
    taffy.set_style(t_root, st.clone()).ok()?;
    compute(taffy, t_root, Size { width: AvailableSpace::MinContent, height: AvailableSpace::MaxContent }).ok()?;
    let w = taffy.layout(t_root).ok()?.size.width;
    st.size.width = length(width.max(w));
    taffy.set_style(t_root, st).ok()?;
    compute(taffy, t_root, Size { width: AvailableSpace::Definite(width.max(w)), height: AvailableSpace::MinContent }).ok()?;
    // A scroll view's own height is its content's; it can be shorter.
    let h = if orig.overflow.y == taffy::Overflow::Scroll { 0.0 } else { taffy.layout(t_root).ok()?.size.height };
    taffy.set_style(t_root, orig).ok()?;
    Some(crate::geom::Size::new(w.ceil(), h.ceil()))
}

fn flex_subtree(tree: &mut NodeTree, root: PNodeId) {
    let mut taffy: TaffyTree<MeasureCtx> = TaffyTree::new();

    let t_root = match build(tree, root, &mut taffy, 0.0) {
        Some(id) => id,
        None => return,
    };

    let root_rect = tree.get(root).map(|n| n.rect()).unwrap_or(PRect::ZERO_SIZED);
    if MIN_WANTED.with(|w| w.get()) {
        if let Some(m) = natural_min(&mut taffy, t_root, root_rect.size.w) {
            MINS.with(|v| v.borrow_mut().push((root, m)));
        }
    }
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

    let result = compute(&mut taffy, t_root, available);
    if let Err(e) = result {
        log::error!("layout: taffy compute failed: {e:?}");
        return;
    }

    // Write back absolute rects. Taffy locations are parent-relative;
    // accumulate from the subtree root's declared origin. The root
    // keeps its declared origin (it was positioned by its own parent
    // context or absolutely by the app).
    write_back(tree, &taffy, t_root, root, root_rect.origin.x, root_rect.origin.y, true);
    break_lines(tree, root);

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
            if let Node::Scroll { content_h, content_w, .. } = node {
                *content_h = l.content_size.height;
                *content_w = l.content_size.width;
            }
        }
        let p_children: Vec<PNodeId> = tree.children_of(p_id).to_vec();
        let t_children = taffy.children(t_id).unwrap_or_default();
        for (tc, pc) in t_children.into_iter().zip(p_children) {
            write_back(tree, taffy, tc, pc, x, y, false);
        }
    }
}

/// Turn each wrapping text leaf under `id` into a column of its lines at
/// its laid-out width: a free stack (so layout leaves it as it is) of
/// one-line runs, which is what the wire draws.
fn break_lines(tree: &mut NodeTree, id: PNodeId) {
    for c in tree.children_of(id).to_vec() {
        break_lines(tree, c);
    }
    let fs0 = tree.style(id).copied().unwrap_or_default();
    if let (Some(f), false) = (tree.flow(id).cloned(), fs0.free) {
        // Text with inline objects: its runs and objects, where they sit.
        let rect = tree.get(id).map(|n| n.rect()).unwrap_or(PRect::ZERO_SIZED);
        let laid = crate::flow::lay_out(&f, rect.w());
        let baseline = laid.lines.first().map(|l| l.1);
        tree.set_style(id, FlexStyle { wrap: false, free: true, baseline, ..fs0 });
        for p in &laid.items {
            tree.insert(Some(id), crate::flow::node_of(p, rect.origin));
        }
        return;
    }
    let wraps = fs0.wrap;
    let Some(Node::Text { rect, content, style }) = tree.get(id).cloned() else { return };
    if !wraps {
        return;
    }
    let lh = crate::text::line_height(&style);
    let lines = crate::text::lines_with(&content, &style, rect.w(), &crate::wrap::Wrap { hyphens: fs0.hyphens, max_lines: fs0.max_lines });
    if let Some(n) = tree.get_mut(id) {
        *n = Node::stack(Axis::Vertical, rect, 0.0);
    }
    let fs = tree.style(id).copied().unwrap_or_default();
    tree.set_style(id, FlexStyle { wrap: false, free: true, baseline: Some(crate::text::baseline(lh, &style)), ..fs });
    for (i, l) in lines.into_iter().enumerate() {
        let w = crate::text::measure(&l, &style).w;
        tree.insert(Some(id), Node::Text { rect: PRect::new(rect.x(), rect.y() + i as f32 * lh, w, lh), content: l, style: style.clone() });
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
            let wraps = tree.style(id).is_some_and(|s| s.wrap);
            let h = if wraps { crate::text::line_height(style) } else if rect.size.h > 0.0 { rect.size.h } else { crate::text::measure(content, style).h };
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

    if let (Some(f), false) = (tree.flow(id), fs.free) {
        let r = node.rect();
        style.size = Size { width: dim(r.size.w), height: dim(r.size.h) };
        let ctx = MeasureCtx { content: String::new(), style: TextStyle { family: String::new(), size: 0.0, weight: crate::theme::Weight::Regular, color: crate::color::Color::TRANSPARENT }, wrap: None, flow: Some(f.clone()) };
        return taffy.new_leaf_with_context(style, ctx).ok();
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
                    MeasureCtx {
                        content: content.clone(),
                        style: text_style.clone(),
                        wrap: fs.wrap.then_some(crate::wrap::Wrap { hyphens: fs.hyphens, max_lines: fs.max_lines }),
                        flow: None,
                    },
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

    /// The natural minimum: as narrow as the widest fixed part or word,
    /// as tall as the content at the window's width; the layout itself is
    /// what it would have been (resolution-independence.md §6.3).
    #[test]
    fn the_natural_minimum_is_the_widest_part_and_the_height_at_the_width() {
        let style = crate::node::TextStyle { family: crate::theme::font::SANS.into(), size: 14.0, weight: crate::theme::Weight::Regular, color: palette::neutral_300() };
        let build = || {
            let mut tree = NodeTree::new();
            let col = tree.insert(None, Node::vstack(Rect::new(0.0, 0.0, 600.0, 900.0), 10.0));
            tree.set_style(col, FlexStyle { padding: 8.0, align: Align::Stretch, ..FlexStyle::default() });
            tree.insert(Some(col), Node::Rect { rect: Rect::new(0.0, 0.0, 220.0, 30.0), fill: palette::neutral_300(), radius: 0.0 });
            let p = tree.insert(Some(col), Node::Text { rect: Rect::ZERO_SIZED, content: "short words then Incomprehensibilities".repeat(3), style: style.clone() });
            tree.set_style(p, FlexStyle { wrap: true, shrink: 1.0, ..FlexStyle::default() });
            (tree, col)
        };
        let (mut plain, col) = build();
        layout(&mut plain, col);
        let (mut tree, col) = build();
        request_natural_min();
        layout(&mut tree, col);
        let m = take_natural_min(Some(col)).expect("measured");
        let word = crate::text::measure("Incomprehensibilities", &style).w;
        assert_eq!(m.w, (220.0f32.max(word) + 16.0).ceil(), "the widest part and the padding");
        let text_h: f32 = tree.children_of(tree.children_of(col)[1]).iter().map(|&l| tree.get(l).unwrap().rect().h()).sum();
        assert_eq!(m.h, (8.0 + 30.0 + 10.0 + text_h + 8.0).ceil(), "the content's height at 600 wide");
        let rects = |t: &NodeTree| t.iter().map(|(_, n)| n.rect()).collect::<Vec<_>>();
        assert_eq!(rects(&tree), rects(&plain), "measuring did not change the layout");
        assert_eq!(take_natural_min(Some(col)), None, "taken once");
        // Not asked: not measured.
        let (mut again, col) = build();
        layout(&mut again, col);
        assert_eq!(take_natural_min(Some(col)), None);
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
    fn a_paragraph_breaks_into_lines_at_its_width_and_pushes_what_follows_down() {
        use crate::node::TextStyle;
        use crate::theme::tokens::Weight;
        crate::text::clear_measurer();
        // Headless: 0.55·size per char, so 10 px text is 5.5 px a char.
        let style = TextStyle { family: "system-sans".into(), size: 10.0, weight: Weight::Regular, color: Color::TRANSPARENT };
        let mut tree = NodeTree::new();
        let col = tree.insert(None, Node::vstack(Rect::new(0.0, 0.0, 100.0, 0.0), 4.0));
        let p = tree.insert(Some(col), Node::Text { rect: Rect::ZERO_SIZED, content: "aaaa bbbb cccc dddd eeee\nff".into(), style: style.clone() });
        tree.set_style(p, FlexStyle { wrap: true, shrink: 1.0, ..FlexStyle::default() });
        let after = tree.insert(Some(col), red_rect(10.0));
        layout(&mut tree, col);
        // 100 px holds 18 chars: "aaaa bbbb cccc" (14), then "dddd eeee", then "ff".
        let lines: Vec<String> = tree.children_of(p).iter().map(|c| match tree.get(*c) {
            Some(Node::Text { content, .. }) => content.clone(),
            _ => panic!("a line is a text run"),
        }).collect();
        assert_eq!(lines, ["aaaa bbbb cccc", "dddd eeee", "ff"]);
        let lh = crate::text::line_height(&style);
        assert_eq!(tree.get(p).unwrap().rect().h(), (3.0 * lh).round());
        assert_eq!(tree.get(after).unwrap().rect().y(), (3.0 * lh).round() + 4.0);
        let second = tree.get(tree.children_of(p)[1]).unwrap().rect();
        assert_eq!(second.y(), lh);
    }

    #[test]
    fn a_paragraph_with_a_line_limit_ends_in_an_ellipsis() {
        use crate::node::TextStyle;
        use crate::theme::tokens::Weight;
        crate::text::clear_measurer();
        let style = TextStyle { family: "system-sans".into(), size: 10.0, weight: Weight::Regular, color: Color::TRANSPARENT };
        let mut tree = NodeTree::new();
        let col = tree.insert(None, Node::vstack(Rect::new(0.0, 0.0, 100.0, 0.0), 0.0));
        let p = tree.insert(Some(col), Node::Text { rect: Rect::ZERO_SIZED, content: "aaaa bbbb cccc dddd eeee ffff gggg".into(), style: style.clone() });
        tree.set_style(p, FlexStyle { wrap: true, shrink: 1.0, max_lines: 2, ..FlexStyle::default() });
        layout(&mut tree, col);
        let lines: Vec<String> = tree.children_of(p).iter().map(|c| match tree.get(*c) {
            Some(Node::Text { content, .. }) => content.clone(),
            _ => unreachable!(),
        }).collect();
        assert_eq!(lines, ["aaaa bbbb cccc", "dddd eeee ffff…"]);
        assert_eq!(tree.get(p).unwrap().rect().h(), (2.0 * crate::text::line_height(&style)).round(), "two lines tall");
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
