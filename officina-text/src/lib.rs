//! Officina's word-processing engine (docs/spec/officina.md §7, §7.4).
//!
//! - [`model`]: a document is an indexed tree of blocks ([`tree::BlockTree`]); a
//!   paragraph is text with attribute runs, a named paragraph style and direct
//!   paragraph properties; styles inherit as ODF and OOXML styles do.
//! - [`ops`]: every change is a [`ops::TextOp`] with an exact inverse, so undo,
//!   redo and the autosave journal come from `officina_core::history`.
//! - [`layout`]: a paragraph shaped and broken into lines at a width, cached by
//!   (content, style, width), so typing relayouts one paragraph.
//! - [`paginate`]: pages with a checkpoint at each page start; an edit repaginates
//!   from its page and stops when a page starts where it did before.
//!
//! Lengths are in layout units ([`LU`] per point, integers), so layout is exact
//! and the same on every machine.

pub mod layout;
pub mod model;
pub mod ops;
pub mod paginate;
pub mod tree;

/// Layout units per point.
pub const LU: i64 = 64;
/// Twips (1/20 pt, the file formats' unit) to layout units.
pub const fn twips(t: i64) -> i64 {
    t * LU / 20
}
/// Points to layout units.
pub const fn pt(p: i64) -> i64 {
    p * LU
}
