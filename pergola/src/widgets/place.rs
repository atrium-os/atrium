//! How each control takes part in layout ([`Widget`]): the size it wants
//! and where its label's baseline is, so `Ctx::place` can put it in a
//! stack or grid cell and line labels up across controls.

use crate::geom::{Rect, Size};
use crate::node::{Node, TextStyle};
use crate::theme::{font, size, space, type_size, Weight};
use crate::view::Widget;

use super::choice::{BOX, LABEL_GAP, RADIO_ROW_GAP, TAB_GAP, TOGGLE_H, TOGGLE_W};
use super::{Button, Checkbox, Dropdown, RadioGroup, Segmented, Slider, Tabs, TextField, Toggle};

/// The least width of a field or slider that fills.
pub const MIN_FIELD: f32 = 120.0;

fn style(sz: f32, weight: Weight) -> TextStyle {
    TextStyle { family: font::SANS.into(), size: sz, weight, color: crate::Color::TRANSPARENT }
}

fn text_w(s: &str, sz: f32, weight: Weight) -> f32 {
    Node::measure_text(s, &style(sz, weight)).w
}

fn line_h(sz: f32) -> f32 {
    Node::measure_text("Mg", &style(sz, Weight::Medium)).h
}

/// The baseline of a label of size `sz` centred in a box `h` tall.
fn centred(h: f32, sz: f32) -> f32 {
    crate::text::baseline(h, &style(sz, Weight::Medium))
}

fn frame(r: &mut Rect, to: Rect) {
    *r = to;
}

impl Button {
    /// Its width: as given, or the label with `sm` padding each side.
    pub fn natural_width(&self) -> f32 {
        if self.rect.w() > 0.0 { self.rect.w() } else { text_w(&self.label, type_size::MD, Weight::Medium) + 2.0 * space::SM }
    }
}

impl Widget for Button {
    fn natural_size(&self) -> Size {
        let h = if self.rect.h() > 0.0 { self.rect.h() } else { size::BUTTON_HEIGHT_DEFAULT };
        Size::new(self.natural_width(), h)
    }
    fn framed(mut self, r: Rect) -> Self {
        frame(&mut self.rect, r);
        self
    }
    fn baseline(&self, s: Size) -> Option<f32> {
        Some(centred(s.h, type_size::MD))
    }
}

impl Widget for TextField {
    fn natural_size(&self) -> Size {
        let h = if self.rect.h() > 0.0 { self.rect.h() } else { size::INPUT_HEIGHT_DEFAULT };
        Size::new(if self.rect.w() > 0.0 { self.rect.w() } else { MIN_FIELD }, h)
    }
    /// A field with no width fills.
    fn fills(&self) -> bool {
        self.rect.w() <= 0.0
    }
    fn framed(mut self, r: Rect) -> Self {
        frame(&mut self.rect, r);
        self
    }
    fn baseline(&self, s: Size) -> Option<f32> {
        Some(centred(s.h, type_size::MD))
    }
}

impl Widget for Checkbox {
    fn natural_size(&self) -> Size {
        Size::new(BOX + LABEL_GAP + text_w(&self.label, type_size::MD, Weight::Medium), BOX.max(line_h(type_size::MD)))
    }
    fn framed(mut self, r: Rect) -> Self {
        // As tall as it wants, centred in what it is given.
        let h = self.natural_size().h;
        (self.x, self.y) = (r.x(), r.y() + ((r.h() - h) / 2.0).max(0.0));
        self
    }
    fn baseline(&self, s: Size) -> Option<f32> {
        Some(centred(s.h, type_size::MD))
    }
}

impl Widget for Toggle {
    fn natural_size(&self) -> Size {
        Size::new(TOGGLE_W + LABEL_GAP + text_w(&self.label, type_size::MD, Weight::Medium), TOGGLE_H.max(line_h(type_size::MD)))
    }
    fn framed(mut self, r: Rect) -> Self {
        let h = self.natural_size().h;
        (self.x, self.y) = (r.x(), r.y() + ((r.h() - h) / 2.0).max(0.0));
        self
    }
    fn baseline(&self, s: Size) -> Option<f32> {
        Some(centred(s.h, type_size::MD))
    }
}

impl Widget for RadioGroup {
    fn natural_size(&self) -> Size {
        let row = BOX.max(line_h(type_size::MD));
        let w = self.labels.iter().map(|l| text_w(l, type_size::MD, Weight::Medium)).fold(0.0, f32::max);
        let n = self.labels.len() as f32;
        Size::new(BOX + LABEL_GAP + w, (n * (row + RADIO_ROW_GAP) - RADIO_ROW_GAP).max(0.0))
    }
    fn framed(mut self, r: Rect) -> Self {
        (self.x, self.y) = (r.x(), r.y());
        self
    }
    /// Its first option's.
    fn baseline(&self, _: Size) -> Option<f32> {
        Some(centred(BOX.max(line_h(type_size::MD)), type_size::MD))
    }
}

impl Segmented {
    /// Its width: as given, or every segment as wide as the widest label
    /// with `md` padding each side.
    pub fn natural_width(&self) -> f32 {
        if self.rect.w() > 0.0 {
            return self.rect.w();
        }
        let n = self.labels.len().max(1) as f32;
        let widest = self.labels.iter().map(|l| text_w(l, type_size::SM, Weight::Medium)).fold(0.0f32, f32::max);
        n * (widest + 2.0 * space::MD) + 2.0 * (n + 1.0)
    }
}

impl Widget for Segmented {
    fn natural_size(&self) -> Size {
        Size::new(self.natural_width(), self.rect.h())
    }
    fn framed(mut self, r: Rect) -> Self {
        frame(&mut self.rect, r);
        self
    }
    fn baseline(&self, s: Size) -> Option<f32> {
        Some(centred(s.h, type_size::SM))
    }
}

impl Widget for Tabs {
    /// No width: it stretches (its hairline runs the row).
    fn fills(&self) -> bool {
        self.rect.w() <= 0.0
    }
    fn natural_size(&self) -> Size {
        let w: f32 = self.labels.iter().map(|l| text_w(l, type_size::MD, Weight::Medium) + TAB_GAP).sum::<f32>() - TAB_GAP;
        Size::new(if self.rect.w() > 0.0 { self.rect.w() } else { w.max(0.0) }, self.rect.h())
    }
    fn framed(mut self, r: Rect) -> Self {
        frame(&mut self.rect, r);
        self
    }
    fn baseline(&self, s: Size) -> Option<f32> {
        Some(centred(s.h, type_size::MD))
    }
}

impl Widget for Dropdown {
    fn natural_size(&self) -> Size {
        self.rect.size
    }
    fn framed(mut self, r: Rect) -> Self {
        frame(&mut self.rect, r);
        self
    }
    fn baseline(&self, s: Size) -> Option<f32> {
        Some(centred(s.h, type_size::MD))
    }
}

impl Widget for crate::widgets::MenuButton {
    fn natural_size(&self) -> Size {
        self.rect.size
    }
    fn framed(mut self, r: Rect) -> Self {
        frame(&mut self.rect, r);
        self
    }
    fn baseline(&self, s: Size) -> Option<f32> {
        Some(centred(s.h, type_size::MD))
    }
}

impl Widget for Slider {
    fn natural_size(&self) -> Size {
        Size::new(if self.rect.w() > 0.0 { self.rect.w() } else { MIN_FIELD }, self.rect.h())
    }
    /// A slider with no width fills.
    fn fills(&self) -> bool {
        self.rect.w() <= 0.0
    }
    fn framed(mut self, r: Rect) -> Self {
        let h = self.rect.h();
        self.rect = Rect::new(r.x(), r.y() + ((r.h() - h) / 2.0).max(0.0), r.w(), h);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::Axis;
    use crate::layout::{Align, FlexStyle, Grid, Track};
    use crate::reactive::Mutable;
    use crate::view::{render, Ctx, Draw};
    use crate::widgets::Check;

    #[test]
    fn placed_widgets_draw_where_the_row_puts_them_with_labels_on_one_baseline() {
        crate::text::clear_measurer();
        let check = Mutable::new(Check::Off);
        let view = Draw(move |ctx: &mut Ctx| {
            let row = ctx.push(Node::stack(Axis::Horizontal, Rect::new(10.0, 20.0, 600.0, 0.0), 12.0));
            ctx.set_flex(row, FlexStyle { align: Align::Baseline, ..FlexStyle::default() });
            ctx.place(Button::primary("Save"));
            ctx.place(Checkbox::new(check.clone(), "Remember me"));
            ctx.pop();
        });
        let (tree, _) = render(&view, crate::theme::Semantic::LIGHT);
        let texts: Vec<(String, Rect, f32)> = tree.iter().filter_map(|(_, n)| match n {
            Node::Text { content, rect, style } => Some((content.clone(), *rect, crate::text::baseline(rect.h(), style) + rect.y())),
            _ => None,
        }).collect();
        let save = texts.iter().find(|t| t.0 == "Save").unwrap();
        let rem = texts.iter().find(|t| t.0 == "Remember me").unwrap();
        assert!(save.1.x() > 10.0, "inside the button, which starts the row");
        let bw = Button::primary("Save").natural_width();
        assert!(rem.1.x() > 10.0 + bw + 12.0, "the checkbox comes after the button and the gap");
        assert!((save.2 - rem.2).abs() < 0.01, "one baseline: {} vs {}", save.2, rem.2);
    }

    #[test]
    fn a_form_grid_puts_labels_in_one_column_and_stretches_fields() {
        crate::text::clear_measurer();
        let (a, b) = (Mutable::new(crate::text_edit::TextEdit::default()), Mutable::new(crate::text_edit::TextEdit::default()));
        let view = Draw(move |ctx: &mut Ctx| {
            let g = ctx.push(Node::stack(Axis::Vertical, Rect::new(0.0, 0.0, 400.0, 0.0), 0.0));
            ctx.grid(g, Grid::new([Track::Auto, Track::Fr(1.0)]).gaps(12.0, 8.0).align(Align::Baseline));
            ctx.add(Node::Text { rect: Rect::ZERO_SIZED, content: "Name".into(), style: style(type_size::MD, Weight::Regular) });
            ctx.place(TextField::new(a.clone()));
            ctx.add(Node::Text { rect: Rect::ZERO_SIZED, content: "Email address".into(), style: style(type_size::MD, Weight::Regular) });
            ctx.place(TextField::new(b.clone()));
            ctx.pop();
        });
        let (tree, _) = render(&view, crate::theme::Semantic::LIGHT);
        let slots: Vec<Rect> = tree.iter().filter(|(id, n)| matches!(n, Node::Stack { .. }) && tree.style(*id).is_some_and(|s| s.free)).map(|(_, n)| n.rect()).collect();
        assert_eq!(slots.len(), 2);
        // Layout rounds to whole px.
        let label_w = text_w("Email address", type_size::MD, Weight::Regular).round();
        assert_eq!(slots[0].x(), label_w + 12.0, "fields start after the widest label");
        assert_eq!(slots[0].x(), slots[1].x());
        assert!((slots[0].w() - (400.0 - label_w - 12.0)).abs() <= 1.0, "and take the rest: {}", slots[0].w());
        assert_eq!(slots[1].y(), slots[0].h() + 8.0);
    }
}
