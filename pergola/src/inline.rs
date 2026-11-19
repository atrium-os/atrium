//! Inline text emphasis shared by the text field and read-only text runs.

use crate::geom::Rect;
use crate::node::{Node, TextStyle};
use crate::theme::Weight;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Emphasis {
    Bold = 1,
    Italic = 2,
    Underline = 4,
    Strike = 8,
}

pub fn style(base: &TextStyle, marks: u8) -> TextStyle {
    let mut style = base.clone();
    if marks & Emphasis::Bold as u8 != 0 {
        style.weight = Weight::Bold;
    }
    if marks & Emphasis::Italic as u8 != 0 {
        style.family = "IBMPlexSans-Italic".into();
    }
    style
}

/// Decorations sit relative to the same baseline as the text node.
pub fn rules(rect: Rect, style: &TextStyle, marks: u8) -> impl Iterator<Item = Node> {
    let em = style.em();
    let color = style.color;
    let baseline = rect.y() + crate::text::baseline(rect.h(), style);
    [(Emphasis::Underline, 0.08), (Emphasis::Strike, -0.28)]
        .into_iter()
        .filter(move |(mark, _)| marks & *mark as u8 != 0 && rect.w() > 0.0)
        .map(move |(_, offset)| Node::Rect {
            rect: Rect::new(rect.x(), baseline + offset * em, rect.w(), (em / 16.0).max(1.0)),
            fill: color,
            radius: 0.0,
        })
}
