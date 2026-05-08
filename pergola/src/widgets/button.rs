//! `Button` — phase 5 widget.
//!
//! A clickable rectangle with an optional centered text label. The
//! `on_click` callback fires on `PointerUp` when `PointerDown`
//! occurred on the same button.
//!
//! Visual language reference: §3 spacing, §5 radius (`SM`), §6
//! motion (spring-snappy on press in a future phase), §8 density
//! (`size::BUTTON_HEIGHT_DEFAULT`).
//!
//! Three variants: `primary` (filled accent), `secondary` (border +
//! neutral fill), `ghost` (text-only). Constructors below; defaults
//! follow the design language.

use std::sync::Arc;

use crate::geom::Rect;
use crate::node::{Node, TextStyle};
use crate::theme::{font, radius, size, type_size, Weight};
use crate::view::{Ctx, View};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    Primary,
    Secondary,
    Ghost,
}

/// `Arc<dyn Fn>` so `Button` is `Clone` (re-rendered each frame
/// anyway, but callers may want to compose it into other Views).
pub type ClickCallback = Arc<dyn Fn() + Send + Sync + 'static>;

#[derive(Clone)]
pub struct Button {
    pub label: String,
    pub variant: Variant,
    pub on_click: Option<ClickCallback>,
    /// Outer rect — width is fixed (caller chooses); height defaults
    /// to `size::BUTTON_HEIGHT_DEFAULT` if `rect.size.h == 0.0`.
    pub rect: Rect,
}

impl Button {
    pub fn primary(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            variant: Variant::Primary,
            on_click: None,
            rect: Rect::new(0.0, 0.0, 0.0, size::BUTTON_HEIGHT_DEFAULT),
        }
    }

    pub fn secondary(label: impl Into<String>) -> Self {
        Self { variant: Variant::Secondary, ..Self::primary(label) }
    }

    pub fn ghost(label: impl Into<String>) -> Self {
        Self { variant: Variant::Ghost, ..Self::primary(label) }
    }

    pub fn on_click<F: Fn() + Send + Sync + 'static>(mut self, f: F) -> Self {
        self.on_click = Some(Arc::new(f));
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
}

impl View for Button {
    fn render(&self, ctx: &mut Ctx) {
        let theme = ctx.theme;

        // Resolve fill + label color per variant.
        let (fill, label_color) = match self.variant {
            Variant::Primary => (theme.accent_fg(), theme.bg_canvas()),
            Variant::Secondary => (theme.bg_surface(), theme.text_primary()),
            Variant::Ghost => (crate::Color::TRANSPARENT, theme.text_primary()),
        };

        let h = if self.rect.size.h > 0.0 { self.rect.size.h } else { size::BUTTON_HEIGHT_DEFAULT };
        let rect = Rect::new(self.rect.x(), self.rect.y(), self.rect.size.w, h);

        let bg_id = ctx.tree.insert(None, Node::Rect {
            rect,
            fill,
            radius: radius::SM,
        });

        // Label centered on the rect — phase 5 places it at a
        // hardcoded vertical center; full text metrics-aware
        // placement comes when fresco-text shaping lands (phase 4.5).
        let label_size = Node::measure_text(&self.label, &TextStyle {
            family: font::SANS.into(),
            size: type_size::MD,
            weight: Weight::Medium,
            color: label_color,
        });
        let label_rect = Rect::new(
            rect.x() + (rect.w() - label_size.w) / 2.0,
            rect.y() + (rect.h() - label_size.h) / 2.0,
            label_size.w,
            label_size.h,
        );
        ctx.tree.insert(Some(bg_id), Node::Text {
            rect: label_rect,
            content: self.label.clone(),
            style: TextStyle {
                family: font::SANS.into(),
                size: type_size::MD,
                weight: Weight::Medium,
                color: label_color,
            },
        });

        // Wire the click handler.
        if let Some(cb) = &self.on_click {
            let cb = Arc::clone(cb);
            ctx.on_click(bg_id, move || cb());
        }
    }
}
