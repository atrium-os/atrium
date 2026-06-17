//! `TextField` — phase 5 widget for single-line text input.
//!
//! Holds a `Mutable<String>` for its content. While focused, key
//! events update the content:
//!   - `Char` events append `chars`
//!   - `Backspace` removes the last char
//!   - other keys are ignored (full caret + selection model is a
//!     later phase; this is the MVP)
//!
//! Visual language reference: §5 radius (`XS`), §8 density
//! (`size::INPUT_HEIGHT_DEFAULT`), §4 color (border-default,
//! border-strong on focus, text-primary, text-tertiary placeholder).

use std::sync::Arc;

use crate::event::{Event, Key, KeyEventKind};
use crate::geom::Rect;
use crate::node::{Node, TextStyle};
use crate::reactive::Mutable;
use crate::theme::{font, radius, size, space, type_size, Weight};
use crate::view::{Ctx, View};

#[derive(Clone)]
pub struct TextField {
    pub content: Mutable<String>,
    pub placeholder: String,
    pub rect: Rect,
    /// Password mode: render the content as bullets, never the characters.
    pub secret: bool,
}

impl TextField {
    pub fn new(content: Mutable<String>) -> Self {
        Self {
            content,
            placeholder: String::new(),
            rect: Rect::new(0.0, 0.0, 0.0, size::INPUT_HEIGHT_DEFAULT),
            secret: false,
        }
    }

    /// Mask the content (password field) — the entered text is shown as bullets.
    pub fn secret(mut self, on: bool) -> Self {
        self.secret = on;
        self
    }

    pub fn placeholder(mut self, p: impl Into<String>) -> Self {
        self.placeholder = p.into();
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

impl View for TextField {
    fn render(&self, ctx: &mut Ctx) {
        let theme = ctx.theme;
        let value = self.content.get_cloned();

        let h = if self.rect.size.h > 0.0 { self.rect.size.h } else { size::INPUT_HEIGHT_DEFAULT };
        let rect = Rect::new(self.rect.x(), self.rect.y(), self.rect.size.w, h);

        // Background + border (currently expressed as a single fill;
        // an outline pass via scene_node_path lands in 4.5).
        let bg_id = ctx.tree.insert(None, Node::Rect {
            rect,
            fill: theme.bg_canvas(),
            radius: radius::XS,
        });

        // Text content or placeholder.
        let (display, color) = if value.is_empty() {
            (self.placeholder.clone(), theme.text_tertiary())
        } else if self.secret {
            // Mask: one bullet per character — never the characters themselves.
            ("\u{2022}".repeat(value.chars().count()), theme.text_primary())
        } else {
            (value, theme.text_primary())
        };

        let style = TextStyle {
            family: font::SANS.into(),
            size: type_size::MD,
            weight: Weight::Regular,
            color,
        };
        let text_size = Node::measure_text(&display, &style);
        let text_rect = Rect::new(
            rect.x() + space::SM,
            rect.y() + (rect.h() - text_size.h) / 2.0,
            text_size.w,
            text_size.h,
        );
        ctx.tree.insert(Some(bg_id), Node::Text {
            rect: text_rect,
            content: display,
            style,
        });

        // Make the bg focusable + register key handler.
        ctx.focusable(bg_id);
        let content = self.content.clone();
        ctx.on_key(bg_id, move |ev| {
            let Event::Key { kind, key, chars, .. } = ev else { return };
            if *kind != KeyEventKind::Down {
                return;
            }
            match key {
                Key::Char => {
                    if !chars.is_empty() {
                        let mut s = content.get_cloned();
                        s.push_str(chars);
                        content.set(s);
                    }
                }
                Key::Backspace => {
                    let mut s = content.get_cloned();
                    s.pop();
                    content.set(s);
                }
                _ => {}
            }
        });
    }
}

// Avoid unused-import warning when on_key feature lands; the Arc
// here is for prospective handler-sharing patterns in higher-level
// widgets.
#[allow(dead_code)]
fn _arc_keepalive() -> Arc<()> { Arc::new(()) }
