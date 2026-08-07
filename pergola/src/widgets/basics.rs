//! Small stateless widgets: `Divider`, `Dot`, `Avatar`, `ProgressBar`.
//! All token-pure; all usable absolutely (`.at`) or as flex children.

use crate::geom::Rect;
use crate::node::{Node, NodeId, TextStyle};
use crate::theme::{font, radius, stroke, Weight};
use crate::view::{Ctx, View};
use crate::Color;

/// 1px hairline. `length == 0.0` stretches on the cross axis in flex.
#[derive(Clone, Copy)]
pub struct Divider {
    vertical: bool,
    length: f32,
    at: Option<(f32, f32)>,
}

impl Divider {
    pub fn vertical(length: f32) -> Self {
        Self { vertical: true, length, at: None }
    }

    pub fn horizontal(length: f32) -> Self {
        Self { vertical: false, length, at: None }
    }

    pub fn at(mut self, x: f32, y: f32) -> Self {
        self.at = Some((x, y));
        self
    }

    pub fn emit(&self, ctx: &mut Ctx) -> NodeId {
        let (x, y) = self.at.unwrap_or((0.0, 0.0));
        let (w, h) = if self.vertical {
            (stroke::DEFAULT, self.length)
        } else {
            (self.length, stroke::DEFAULT)
        };
        let fill = ctx.theme.border_strong();
        ctx.add(Node::Rect { rect: Rect::new(x, y, w, h), fill, radius: 0.0 })
    }
}

impl View for Divider {
    fn render(&self, ctx: &mut Ctx) {
        self.emit(ctx);
    }
}

/// A small status disc — engine-state dots, badge dots.
#[derive(Clone, Copy)]
pub struct Dot {
    pub diameter: f32,
    pub color: Color,
    at: Option<(f32, f32)>,
}

impl Dot {
    pub fn new(diameter: f32, color: Color) -> Self {
        Self { diameter, color, at: None }
    }

    pub fn at(mut self, x: f32, y: f32) -> Self {
        self.at = Some((x, y));
        self
    }

    pub fn emit(&self, ctx: &mut Ctx) -> NodeId {
        let (x, y) = self.at.unwrap_or((0.0, 0.0));
        ctx.add(Node::Rect {
            rect: Rect::new(x, y, self.diameter, self.diameter),
            fill: self.color,
            radius: radius::PILL,
        })
    }
}

impl View for Dot {
    fn render(&self, ctx: &mut Ctx) {
        self.emit(ctx);
    }
}

/// Circle avatar with a single initial — accent-tinted (the
/// vestibulum / workspace-owner look).
#[derive(Clone)]
pub struct Avatar {
    pub initial: char,
    pub diameter: f32,
    at: Option<(f32, f32)>,
}

impl Avatar {
    pub fn new(initial: char, diameter: f32) -> Self {
        Self { initial, diameter, at: None }
    }

    pub fn at(mut self, x: f32, y: f32) -> Self {
        self.at = Some((x, y));
        self
    }

    pub fn emit(&self, ctx: &mut Ctx) -> NodeId {
        let t = ctx.theme;
        let (x, y) = self.at.unwrap_or((0.0, 0.0));
        let d = self.diameter;
        let disc = ctx.add(Node::Rect {
            rect: Rect::new(x, y, d, d),
            fill: t.accent_bg(),
            radius: radius::PILL,
        });
        // Initial, centered-ish: the em box sits near d/2 wide for a
        // single cap at 0.42·d — measured placement once labels are
        // measured inside widgets (M2 leaves this optical).
        let size = d * 0.42;
        ctx.tree.insert(Some(disc), Node::Text {
            rect: Rect::new(x + d * 0.30, y + (d - size) * 0.5, 0.0, 0.0),
            content: self.initial.to_string(),
            style: TextStyle {
                family: font::SANS.into(),
                size,
                weight: Weight::Semibold,
                color: t.accent_strong(),
            },
        });
        disc
    }
}

impl View for Avatar {
    fn render(&self, ctx: &mut Ctx) {
        self.emit(ctx);
    }
}

/// Determinate progress: 4px track + accent fill (the install-card
/// bar). `fraction` clamped to [0, 1].
#[derive(Clone, Copy)]
pub struct ProgressBar {
    pub fraction: f32,
    pub width: f32,
    at: Option<(f32, f32)>,
}

impl ProgressBar {
    pub fn new(fraction: f32, width: f32) -> Self {
        Self { fraction: fraction.clamp(0.0, 1.0), width, at: None }
    }

    pub fn at(mut self, x: f32, y: f32) -> Self {
        self.at = Some((x, y));
        self
    }

    pub fn emit(&self, ctx: &mut Ctx) -> NodeId {
        let t = ctx.theme;
        let (x, y) = self.at.unwrap_or((0.0, 0.0));
        const H: f32 = 4.0;
        let track = ctx.add(Node::Rect {
            rect: Rect::new(x, y, self.width, H),
            fill: t.border_default(),
            radius: H * 0.5,
        });
        ctx.tree.insert(Some(track), Node::Rect {
            rect: Rect::new(x, y, self.width * self.fraction, H),
            fill: t.accent_fg(),
            radius: H * 0.5,
        });
        track
    }
}

impl View for ProgressBar {
    fn render(&self, ctx: &mut Ctx) {
        self.emit(ctx);
    }
}
