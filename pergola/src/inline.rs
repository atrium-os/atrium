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
    /// With `Underline`: two lines (a spreadsheet's double underline).
    Double = 16,
    /// Smaller text, raised or lowered (a whole spreadsheet cell's).
    Super = 32,
    Sub = 64,
    /// With `Underline`: the line runs across the whole cell (a
    /// spreadsheet's accounting underline).
    Accounting = 128,
}

/// The share of the size super- and subscript text is drawn at, and how far
/// (in ems of the full size) it is moved from the line's middle.
pub const SCRIPT_SCALE: f32 = 0.62;
pub const SCRIPT_SHIFT: f32 = 0.3;

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
    let double = marks & (Emphasis::Underline as u8 | Emphasis::Double as u8) == Emphasis::Underline as u8 | Emphasis::Double as u8;
    [(Emphasis::Underline, 0.08), (Emphasis::Double, 0.2), (Emphasis::Strike, -0.28)]
        .into_iter()
        .filter(move |(mark, _)| marks & *mark as u8 != 0 && (*mark != Emphasis::Double || double) && rect.w() > 0.0)
        .map(move |(_, offset)| Node::Rect {
            rect: Rect::new(rect.x(), baseline + offset * em, rect.w(), (em / 16.0).max(1.0)),
            fill: color,
            radius: 0.0,
        })
}
