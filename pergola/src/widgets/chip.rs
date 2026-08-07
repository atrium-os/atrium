//! `Chip` — the shell's small interactive container (surface chips,
//! the workspace chip), and `ListRow` — its full-width sibling
//! (launcher rows, popover rows).
//!
//! Both are a bordered, hoverable, clickable horizontal flex box over
//! filled Stacks. The border layer always exists (transparent when
//! borderless) so node ids stay stable across state changes.

use std::sync::Arc;

use crate::geom::{Axis, Rect};
use crate::layout::{Align, FlexStyle};
use crate::node::Node;
use crate::theme::radius;
use crate::view::{Ctx, View};
use crate::widgets::button::ClickCallback;
use crate::Color;

#[derive(Clone)]
pub struct Chip<C: View> {
    pub content: C,
    pub height: f32,
    pub gap: f32,
    /// Total horizontal padding per side (includes the 1px border layer).
    pub pad: f32,
    pub radius: f32,
    /// Active chips get `bg_canvas` + `border_strong` (the focused
    /// surface chip / current workspace look).
    pub active: bool,
    /// Extra opacity-style muting for stashed chips is the caller's
    /// business (M3 alpha); structural look lives here.
    pub on_click: Option<ClickCallback>,
    at: Option<(f32, f32)>,
}

impl<C: View> Chip<C> {
    pub fn new(height: f32, content: C) -> Self {
        Self {
            content,
            height,
            gap: 6.0,
            pad: 9.0,
            radius: radius::SM,
            active: false,
            on_click: None,
            at: None,
        }
    }

    pub fn gap(mut self, gap: f32) -> Self {
        self.gap = gap;
        self
    }

    pub fn pad(mut self, pad: f32) -> Self {
        self.pad = pad;
        self
    }

    pub fn active(mut self, active: bool) -> Self {
        self.active = active;
        self
    }

    pub fn on_click<F: Fn() + Send + Sync + 'static>(mut self, f: F) -> Self {
        self.on_click = Some(Arc::new(f));
        self
    }

    pub fn at(mut self, x: f32, y: f32) -> Self {
        self.at = Some((x, y));
        self
    }
}

impl<C: View> View for Chip<C> {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;
        let (x, y) = self.at.unwrap_or((0.0, 0.0));

        // Border layer (1px padding) wrapping the body layer.
        let outer = ctx.push(Node::stack_filled(
            Axis::Horizontal,
            Rect::new(x, y, 0.0, self.height),
            0.0,
            Color::TRANSPARENT,
            self.radius,
        ));
        ctx.set_flex(outer, FlexStyle { padding: 1.0, ..FlexStyle::default() });

        let inner = ctx.push(Node::stack_filled(
            Axis::Horizontal,
            Rect::ZERO_SIZED,
            self.gap,
            Color::TRANSPARENT,
            (self.radius - 1.0).max(0.0),
        ));
        ctx.set_flex(inner, FlexStyle {
            padding: (self.pad - 1.0).max(0.0),
            align: Align::Center,
            grow: 1.0,
            ..FlexStyle::default()
        });

        self.content.render(ctx);
        ctx.pop();
        ctx.pop();

        let hovered = ctx.is_hovered(outer);
        let (border, body) = if self.active {
            (t.border_strong(), t.bg_canvas())
        } else if hovered && self.on_click.is_some() {
            (Color::TRANSPARENT, t.bg_canvas())
        } else {
            (Color::TRANSPARENT, Color::TRANSPARENT)
        };
        if let Some(Node::Stack { fill, .. }) = ctx.tree.get_mut(outer) {
            *fill = Some(border);
        }
        if let Some(Node::Stack { fill, .. }) = ctx.tree.get_mut(inner) {
            *fill = Some(body);
        }

        if let Some(cb) = &self.on_click {
            let cb = Arc::clone(cb);
            ctx.on_click(outer, move || cb());
        }
    }
}

/// Full-width hoverable row (launcher, popover lists): hover fills
/// `bg_surface`; the current/selected row carries an accent border.
#[derive(Clone)]
pub struct ListRow<C: View> {
    pub content: C,
    pub height: f32,
    pub gap: f32,
    pub pad: f32,
    pub selected: bool,
    pub on_click: Option<ClickCallback>,
}

impl<C: View> ListRow<C> {
    pub fn new(height: f32, content: C) -> Self {
        Self { content, height, gap: 10.0, pad: 8.0, selected: false, on_click: None }
    }

    pub fn gap(mut self, gap: f32) -> Self {
        self.gap = gap;
        self
    }

    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    pub fn on_click<F: Fn() + Send + Sync + 'static>(mut self, f: F) -> Self {
        self.on_click = Some(Arc::new(f));
        self
    }
}

impl<C: View> View for ListRow<C> {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;

        let outer = ctx.push(Node::stack_filled(
            Axis::Horizontal,
            Rect::new(0.0, 0.0, 0.0, self.height),
            0.0,
            Color::TRANSPARENT,
            radius::SM,
        ));
        ctx.set_flex(outer, FlexStyle { padding: 1.0, ..FlexStyle::default() });

        let inner = ctx.push(Node::stack_filled(
            Axis::Horizontal,
            Rect::ZERO_SIZED,
            self.gap,
            Color::TRANSPARENT,
            (radius::SM - 1.0).max(0.0),
        ));
        ctx.set_flex(inner, FlexStyle {
            padding: (self.pad - 1.0).max(0.0),
            align: Align::Center,
            grow: 1.0,
            ..FlexStyle::default()
        });

        self.content.render(ctx);
        ctx.pop();
        ctx.pop();

        let hovered = ctx.is_hovered(outer);
        let border = if self.selected { t.accent_fg() } else { Color::TRANSPARENT };
        let body = if hovered && self.on_click.is_some() {
            t.bg_surface()
        } else {
            Color::TRANSPARENT
        };
        if let Some(Node::Stack { fill, .. }) = ctx.tree.get_mut(outer) {
            *fill = Some(border);
        }
        if let Some(Node::Stack { fill, .. }) = ctx.tree.get_mut(inner) {
            *fill = Some(body);
        }

        if let Some(cb) = &self.on_click {
            let cb = Arc::clone(cb);
            ctx.on_click(outer, move || cb());
        }
    }
}
