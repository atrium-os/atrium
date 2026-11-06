//! Grouping (visual language §14.2): a `GroupBox` puts related controls on
//! one framed surface under a small title; a `Section` heads a run of
//! controls with a title (and a line of description) and no frame.
//!
//! Both are vertical stacks: what the content closure adds is laid out
//! in them (`Ctx::place` for controls, plain text, nested stacks and
//! grids), `sm` apart. Give a width (or let the parent stretch them); the
//! height is the content's.

use crate::geom::{Axis, Rect};
use crate::layout::{Align, FlexStyle};
use crate::node::{Node, NodeId, TextStyle};
use crate::theme::{font, radius, space, stroke, type_size, Weight};
use crate::view::Ctx;

fn text(content: &str, size: f32, weight: Weight, color: crate::Color) -> Node {
    Node::Text { rect: Rect::ZERO_SIZED, content: content.into(), style: TextStyle { family: font::SANS.into(), size, weight, color } }
}

/// A framed group of controls.
pub struct GroupBox {
    pub title: Option<String>,
    pub rect: Rect,
}

impl GroupBox {
    pub fn new(title: impl Into<String>) -> Self {
        Self { title: Some(title.into()), rect: Rect::ZERO_SIZED }
    }
    /// A frame with no title.
    pub fn untitled() -> Self {
        Self { title: None, rect: Rect::ZERO_SIZED }
    }
    pub fn at(mut self, x: f32, y: f32) -> Self {
        self.rect.origin.x = x;
        self.rect.origin.y = y;
        self
    }
    pub fn width(mut self, w: f32) -> Self {
        self.rect.size.w = w;
        self
    }

    /// Draw the box with `content` inside. Returns its stack.
    pub fn show(&self, ctx: &mut Ctx, content: impl FnOnce(&mut Ctx)) -> NodeId {
        let t = ctx.theme;
        let id = ctx.push(Node::stack(Axis::Vertical, self.rect, space::SM));
        ctx.set_flex(id, FlexStyle { padding: space::MD, align: Align::Stretch, ..FlexStyle::default() });
        // The frame: an edge and a fill inset by it, both out of flow and
        // as big as the box.
        let all = |d: f32| FlexStyle { absolute_inset: Some([Some(d), Some(d), Some(d), Some(d)]), ..FlexStyle::default() };
        let edge = ctx.push(Node::Rect { rect: Rect::ZERO_SIZED, fill: t.border_default(), radius: radius::MD });
        ctx.set_flex(edge, all(0.0));
        ctx.pop();
        let fill = ctx.add(Node::Rect { rect: Rect::ZERO_SIZED, fill: t.bg_elevated(), radius: radius::MD - stroke::DEFAULT });
        ctx.set_flex(fill, all(stroke::DEFAULT));
        if let Some(title) = &self.title {
            ctx.add(text(title, type_size::SM, Weight::Semibold, t.text_secondary()));
        }
        content(ctx);
        ctx.pop();
        id
    }
}

/// A titled run of controls, unframed.
pub struct Section {
    pub title: String,
    pub description: Option<String>,
    pub rect: Rect,
}

impl Section {
    pub fn new(title: impl Into<String>) -> Self {
        Self { title: title.into(), description: None, rect: Rect::ZERO_SIZED }
    }
    pub fn description(mut self, d: impl Into<String>) -> Self {
        self.description = Some(d.into());
        self
    }
    pub fn at(mut self, x: f32, y: f32) -> Self {
        self.rect.origin.x = x;
        self.rect.origin.y = y;
        self
    }
    pub fn width(mut self, w: f32) -> Self {
        self.rect.size.w = w;
        self
    }

    /// Draw the heading with `content` under it. Returns its stack.
    pub fn show(&self, ctx: &mut Ctx, content: impl FnOnce(&mut Ctx)) -> NodeId {
        let t = ctx.theme;
        let id = ctx.push(Node::stack(Axis::Vertical, self.rect, space::SM));
        // Title and description sit together, `xxs` apart.
        let head = ctx.push(Node::stack(Axis::Vertical, Rect::ZERO_SIZED, space::XXS));
        ctx.set_flex(head, FlexStyle { align: Align::Start, ..FlexStyle::default() });
        ctx.add(text(&self.title, type_size::MD, Weight::Semibold, t.text_primary()));
        if let Some(d) = &self.description {
            ctx.add(text(d, type_size::SM, Weight::Regular, t.text_secondary()));
        }
        ctx.pop();
        content(ctx);
        ctx.pop();
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reactive::Mutable;
    use crate::view::{render, Draw};
    use crate::widgets::{Check, Checkbox};

    #[test]
    fn a_group_box_frames_its_controls_and_grows_to_fit_them() {
        crate::text::clear_measurer();
        let (a, b) = (Mutable::new(Check::On), Mutable::new(Check::Off));
        let view = Draw(move |ctx: &mut Ctx| {
            GroupBox::new("Network").at(20.0, 30.0).width(300.0).show(ctx, |ctx| {
                ctx.place(Checkbox::new(a.clone(), "Wi-Fi"));
                ctx.place(Checkbox::new(b.clone(), "Bluetooth"));
            });
        });
        let (tree, _) = render(&view, crate::theme::Semantic::LIGHT);
        let root = tree.roots().next().unwrap();
        let r = tree.get(root).unwrap().rect();
        let kids = tree.children_of(root).to_vec();
        let edge = tree.get(kids[0]).unwrap().rect();
        assert_eq!(edge, r, "the edge is the whole box");
        let fill = tree.get(kids[1]).unwrap().rect();
        assert_eq!(fill, Rect::new(r.x() + 1.0, r.y() + 1.0, r.w() - 2.0, r.h() - 2.0));
        let title = tree.get(kids[2]).unwrap().rect();
        assert_eq!((title.x(), title.y()), (20.0 + space::MD, 30.0 + space::MD));
        let last = tree.get(kids[4]).unwrap().rect();
        assert_eq!(r.y() + r.h(), last.y() + last.h() + space::MD, "as tall as its content and padding");
        assert_eq!(r.w(), 300.0);
    }
}
