//! Code editing for Artifex (dev-tools.md §3.4; plan M11, P4): the buffer
//! model ([`CodeBuffer`]: a rope, several selections, undo, the edit log a
//! syntax tree follows) and how highlighting reaches the grid
//! ([`Highlight`]). The view is [`crate::widgets::CodeGrid`].
//!
//! Separate from [`crate::text_edit::TextEdit`] (one-line and short
//! fields) and from Officina's paragraph engine: code is monospace lines,
//! so the grid lays out by column, not by shaping paragraphs.

pub mod buffer;
pub mod highlight;

pub use buffer::{CodeBuffer, Edit, Motion, Point, Selection};
pub use highlight::{Highlight, Span, Tag};
