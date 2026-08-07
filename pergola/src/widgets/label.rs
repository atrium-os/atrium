//! `Label` — styled single-line text, and `Glyph` — a Phosphor icon
//! rendered through the text pipeline (measured, atlased, tinted like
//! any other glyph run).
//!
//! In a flex context leave the rect zero and let layout place it; for
//! absolute placement use `.at(x, y)`.

use crate::geom::Rect;
use crate::node::{Node, NodeId, TextStyle};
use crate::theme::{font, type_size, Weight};
use crate::view::{Ctx, View};
use crate::Color;

#[derive(Clone)]
pub struct Label {
    pub content: String,
    pub family: String,
    pub size: f32,
    pub weight: Weight,
    /// `None` → `text_primary` at render time.
    pub color: Option<Color>,
    pub rect: Rect,
}

impl Label {
    pub fn new(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            family: font::SANS.into(),
            size: type_size::SM,
            weight: Weight::Regular,
            color: None,
            rect: Rect::ZERO_SIZED,
        }
    }

    /// Machine-text: Plex Mono.
    pub fn mono(mut self) -> Self {
        self.family = font::MONO.into();
        self
    }

    pub fn size(mut self, size: f32) -> Self {
        self.size = size;
        self
    }

    pub fn weight(mut self, weight: Weight) -> Self {
        self.weight = weight;
        self
    }

    pub fn color(mut self, color: Color) -> Self {
        self.color = Some(color);
        self
    }

    pub fn at(mut self, x: f32, y: f32) -> Self {
        self.rect.origin.x = x;
        self.rect.origin.y = y;
        self
    }

    /// Emit into `ctx` and return the node id (for styling/flex).
    pub fn emit(&self, ctx: &mut Ctx) -> NodeId {
        let color = self.color.unwrap_or_else(|| ctx.theme.text_primary());
        ctx.add(Node::Text {
            rect: self.rect,
            content: self.content.clone(),
            style: TextStyle {
                family: self.family.clone(),
                size: self.size,
                weight: self.weight,
                color,
            },
        })
    }
}

impl View for Label {
    fn render(&self, ctx: &mut Ctx) {
        self.emit(ctx);
    }
}

/// A single Phosphor icon glyph. Size is the *font* size — Phosphor
/// glyphs fill their em square, so this matches the icon-size tokens.
#[derive(Clone)]
pub struct Glyph {
    pub glyph: char,
    pub size: f32,
    pub color: Option<Color>,
    pub rect: Rect,
}

impl Glyph {
    pub fn new(glyph: char) -> Self {
        Self { glyph, size: crate::theme::icon::SM, color: None, rect: Rect::ZERO_SIZED }
    }

    pub fn size(mut self, size: f32) -> Self {
        self.size = size;
        self
    }

    pub fn color(mut self, color: Color) -> Self {
        self.color = Some(color);
        self
    }

    pub fn at(mut self, x: f32, y: f32) -> Self {
        self.rect.origin.x = x;
        self.rect.origin.y = y;
        self
    }

    pub fn emit(&self, ctx: &mut Ctx) -> NodeId {
        let color = self.color.unwrap_or_else(|| ctx.theme.text_secondary());
        ctx.add(Node::Text {
            rect: self.rect,
            content: self.glyph.to_string(),
            style: TextStyle {
                family: font::ICONS.into(),
                size: self.size,
                weight: Weight::Regular,
                color,
            },
        })
    }
}

impl View for Glyph {
    fn render(&self, ctx: &mut Ctx) {
        self.emit(ctx);
    }
}
