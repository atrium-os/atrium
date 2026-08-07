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
use crate::theme::{font, radius, size, stroke, type_size, Weight};
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

        let h = if self.rect.size.h > 0.0 { self.rect.size.h } else { size::BUTTON_HEIGHT_DEFAULT };
        let rect = Rect::new(self.rect.x(), self.rect.y(), self.rect.size.w, h);

        // Node structure is IDENTICAL in every state — state changes
        // colors, never node count — so ids stay stable across renders
        // and last frame's hover/press/focus (keyed by id) selects
        // this frame's look. Layers: focus halo, focus ring, body
        // (doubles as the border for Secondary), inner fill, label.
        let halo_id = ctx.tree.insert(None, Node::Rect {
            rect: inflate(rect, 3.0),
            fill: crate::Color::TRANSPARENT,
            radius: radius::SM + 3.0,
        });
        let ring_id = ctx.tree.insert(None, Node::Rect {
            rect: inflate(rect, 1.0),
            fill: crate::Color::TRANSPARENT,
            radius: radius::SM + 1.0,
        });
        let bg_id = ctx.tree.insert(None, Node::Rect {
            rect,
            fill: crate::Color::TRANSPARENT,
            radius: radius::SM,
        });
        let inner_id = ctx.tree.insert(Some(bg_id), Node::Rect {
            rect: inflate(rect, -stroke::DEFAULT),
            fill: crate::Color::TRANSPARENT,
            radius: (radius::SM - stroke::DEFAULT).max(0.0),
        });

        let hovered = ctx.is_hovered(bg_id);
        let pressed = ctx.is_pressed(bg_id);
        let focused = ctx.is_focused(bg_id);

        // Resolve per variant and state. Pressed beats hover. `body`
        // is the outer rect; `inner` is the 1px-inset fill Secondary
        // uses to fake its border until an outline pass exists.
        let (body, inner, label_color) = match self.variant {
            Variant::Primary => {
                let fill = if pressed {
                    theme.accent_pressed()
                } else if hovered {
                    theme.accent_strong()
                } else {
                    theme.accent_fg()
                };
                (fill, crate::Color::TRANSPARENT, theme.text_on_accent())
            }
            Variant::Secondary => {
                let fill = if pressed || hovered { theme.bg_surface() } else { theme.bg_elevated() };
                (theme.border_strong(), fill, theme.text_primary())
            }
            Variant::Ghost => {
                let fill = if pressed || hovered {
                    theme.bg_surface()
                } else {
                    crate::Color::TRANSPARENT
                };
                (fill, crate::Color::TRANSPARENT, theme.text_secondary())
            }
        };

        if focused {
            if let Some(Node::Rect { fill, .. }) = ctx.tree.get_mut(halo_id) {
                *fill = theme.accent_bg();
            }
            if let Some(Node::Rect { fill, .. }) = ctx.tree.get_mut(ring_id) {
                *fill = theme.focus_ring();
            }
        }
        if let Some(Node::Rect { fill, .. }) = ctx.tree.get_mut(bg_id) {
            *fill = body;
        }
        if let Some(Node::Rect { fill, .. }) = ctx.tree.get_mut(inner_id) {
            *fill = inner;
        }

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

        // Wire the click handler; clickable buttons take Tab focus.
        if let Some(cb) = &self.on_click {
            let cb = Arc::clone(cb);
            ctx.on_click(bg_id, move || cb());
            ctx.focusable(bg_id);
        }
    }
}

/// Grow (positive) or shrink (negative) a rect by `d` on every edge.
fn inflate(r: Rect, d: f32) -> Rect {
    Rect::new(r.x() - d, r.y() - d, r.w() + 2.0 * d, r.h() + 2.0 * d)
}
