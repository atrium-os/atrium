//! `Button` — phase 5 widget.
//!
//! A clickable rectangle with an optional centered text label. The
//! `on_click` callback fires on `PointerUp` when `PointerDown`
//! occurred on the same button.
//!
//! Visual language reference: §3 spacing, §5 radius (`SM`), §6
//! motion (spring-snappy on press in a future phase), §8 density
//! (`size::button_height()`).
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
    /// A primary action that destroys something (delete, discard).
    Destructive,
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
    /// to `size::button_height()` if `rect.size.h == 0.0`.
    pub rect: Rect,
    /// Shown after a hover (an icon-only button names itself here).
    pub tooltip: Option<String>,
    retain_focus_on_press: bool,
    /// Drawn instead of the label, from `font::ICONS`, 1 em of the label's
    /// size so it grows with the text (an icon-only button). The label
    /// still names it: its tooltip, and its entry in an overflow menu.
    pub icon: Option<char>,
    /// Drawn before the label (icon and label, centred together), from
    /// `font::ICONS` at the label's size.
    pub lead: Option<char>,
}

impl Button {
    pub fn primary(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            variant: Variant::Primary,
            on_click: None,
            rect: Rect::new(0.0, 0.0, 0.0, size::button_height()),
            tooltip: None,
            retain_focus_on_press: false,
            icon: None,
            lead: None,
        }
    }

    pub fn secondary(label: impl Into<String>) -> Self {
        Self { variant: Variant::Secondary, ..Self::primary(label) }
    }

    pub fn destructive(label: impl Into<String>) -> Self {
        Self { variant: Variant::Destructive, ..Self::primary(label) }
    }

    pub fn ghost(label: impl Into<String>) -> Self {
        Self { variant: Variant::Ghost, ..Self::primary(label) }
    }

    /// Draw `ch` (an icon in `font::ICONS`) instead of the label; the
    /// label becomes its tooltip unless it has one.
    pub fn icon(mut self, ch: char) -> Self {
        self.icon = Some(ch);
        if self.tooltip.is_none() {
            self.tooltip = Some(self.label.clone());
        }
        self
    }

    /// Draw `ch` (an icon in `font::ICONS`) before the label.
    pub fn lead(mut self, ch: char) -> Self {
        self.lead = Some(ch);
        self
    }

    /// Keep the current editor focused when pressed with the pointer.
    /// The button still participates in keyboard focus traversal.
    pub fn retain_focus_on_press(mut self) -> Self {
        self.retain_focus_on_press = true;
        self
    }

    pub fn tooltip(mut self, t: impl Into<String>) -> Self {
        self.tooltip = Some(t.into());
        self
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
    #[cfg_attr(debug_assertions, track_caller)]
    fn render(&self, ctx: &mut Ctx) {
        ctx.widget_site();
        let h = if self.rect.size.h > 0.0 { self.rect.size.h } else { size::button_height() };
        let rect = Rect::new(self.rect.x(), self.rect.y(), self.natural_width(), h);

        self.render_at(ctx, rect, None);
    }
}

impl Button {
    /// Draw a text button at the given width, wrapping its label and returning the row height.
    pub fn render_wrapped(&self, ctx: &mut Ctx, width: f32) -> f32 {
        let style = TextStyle { family: font::SANS.into(), size: type_size::MD, weight: Weight::Medium, color: ctx.theme.text_primary() };
        let lines = crate::text::lines(&self.label, &style, (width - 16.0).max(1.0));
        let height = self.rect.h().max(lines.len() as f32 * crate::text::line_height(&style) + 12.0);
        self.render_at(ctx, Rect::new(self.rect.x(), self.rect.y(), width, height), Some(&lines));
        height
    }

    fn render_at(&self, ctx: &mut Ctx, rect: Rect, wrapped_lines: Option<&[String]>) {
        let theme = ctx.theme;
        // Node structure is IDENTICAL in every state — state changes
        // colors, never node count — so ids stay stable across renders
        // and last frame's hover/press/focus (keyed by id) selects
        // this frame's look. Layers: focus halo, focus ring, body
        // (doubles as the border for Secondary), inner fill, label.
        let halo_id = ctx.add(Node::Rect {
            rect: inflate(rect, 3.0),
            fill: crate::Color::TRANSPARENT,
            radius: radius::SM + 3.0,
        });
        let ring_id = ctx.add(Node::Rect {
            rect: inflate(rect, 1.0),
            fill: crate::Color::TRANSPARENT,
            radius: radius::SM + 1.0,
        });
        let bg_id = ctx.add(Node::Rect {
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

        // Resolve per variant and state. Pressed beats hover. `body`
        // is the outer rect; `inner` is the 1px-inset fill Secondary
        // uses to fake its border until an outline pass exists.
        // Visual language rev. 2 §14.1: filled buttons step through
        // accent-fill, -hover, -pressed; the others take the one hover and
        // press overlay on their own fill. Pressed beats hover.
        let overlay = if pressed {
            Some(theme.state_pressed())
        } else if hovered {
            Some(theme.state_hover())
        } else {
            None
        };
        let (body, inner, label_color) = match self.variant {
            Variant::Primary => {
                let fill = if pressed {
                    theme.accent_fill_pressed()
                } else if hovered {
                    theme.accent_fill_hover()
                } else {
                    theme.accent_fill()
                };
                (fill, crate::Color::TRANSPARENT, theme.text_on_accent())
            }
            Variant::Destructive => {
                let base = crate::theme::palette::danger_500();
                (overlay.map_or(base, |o| o.over(base)), crate::Color::TRANSPARENT, theme.text_on_accent())
            }
            Variant::Secondary => {
                let base = theme.bg_elevated();
                (theme.border_strong(), overlay.map_or(base, |o| o.over(base)), theme.text_primary())
            }
            Variant::Ghost => {
                // Over whatever is behind it: the overlay alone.
                (overlay.unwrap_or(crate::Color::TRANSPARENT), crate::Color::TRANSPARENT, theme.text_secondary())
            }
        };

        if ctx.is_focus_visible(bg_id) {
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
        let (content, family, weight) = match self.icon {
            Some(ch) => (ch.to_string(), font::ICONS, Weight::Regular),
            None => (self.label.clone(), font::SANS, Weight::Medium),
        };
        let style = TextStyle { family: family.into(), size: type_size::MD, weight, color: label_color };
        if let Some(lines) = wrapped_lines {
            let line_height = crate::text::line_height(&style);
            let top = rect.y() + (rect.h() - lines.len() as f32 * line_height) / 2.0;
            for (i, line) in lines.iter().enumerate() {
                ctx.tree.insert(Some(bg_id), Node::Text {
                    rect: Rect::new(rect.x() + 8.0, top + i as f32 * line_height, (rect.w() - 16.0).max(1.0), line_height),
                    content: line.clone(),
                    style: style.clone(),
                });
            }
        } else {
            let label_size = Node::measure_text(&content, &style);
            let lead = self.lead.filter(|_| self.icon.is_none()).map(|ch| {
                let icon_style = TextStyle { family: font::ICONS.into(), size: type_size::MD, weight: Weight::Regular, color: label_color };
                let size = Node::measure_text(&ch.to_string(), &icon_style);
                (ch, icon_style, size)
            });
            let gap = if lead.is_some() { crate::theme::size::em(6.0) } else { 0.0 };
            let lead_w = lead.as_ref().map_or(0.0, |l| l.2.w);
            let mut x = rect.x() + (rect.w() - label_size.w - lead_w - gap) / 2.0;
            if let Some((ch, icon_style, size)) = lead {
                let r = Rect::new(x, rect.y() + (rect.h() - size.h) / 2.0, size.w, size.h);
                ctx.tree.insert(Some(bg_id), Node::Text { rect: r, content: ch.to_string(), style: icon_style });
                x += size.w + gap;
            }
            let label_rect = Rect::new(x, rect.y() + (rect.h() - label_size.h) / 2.0, label_size.w, label_size.h);
            ctx.tree.insert(Some(bg_id), Node::Text { rect: label_rect, content, style });
        }

        // Wire the click handler; clickable buttons take Tab focus.
        if let Some(cb) = &self.on_click {
            let cb = Arc::clone(cb);
            ctx.on_click(bg_id, move || cb());
            ctx.focusable(bg_id);
            ctx.interactions.entry(bg_id).retain_focus_on_press = self.retain_focus_on_press;
        }
        if let Some(tip) = &self.tooltip {
            ctx.tooltip(bg_id, tip.clone());
        }
    }
}

/// Grow (positive) or shrink (negative) a rect by `d` on every edge.
fn inflate(r: Rect, d: f32) -> Rect {
    Rect::new(r.x() - d, r.y() - d, r.w() + 2.0 * d, r.h() + 2.0 * d)
}
