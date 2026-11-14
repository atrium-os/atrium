//! `RichTextEditor` (docs/spec/officina.md §10.1, §16.11): Pergola's rich-text
//! editor over `officina-text`.
//!
//! - [`editor`]: the editing core. Caret and selection across paragraphs,
//!   motion by grapheme, word, line and page, typing with undo runs, Enter,
//!   delete, cut, copy and paste, character and paragraph formatting keys.
//!   Every change is an undoable `TextOp`, and the pages after it are
//!   repaginated from the edit until breaks line up again.
//! - [`view`]: the paged view. Pages stacked in a scroll view, only those near
//!   the viewport sent as scene groups, text as shaped glyph runs, the
//!   selection and caret in an overlay.

pub mod editor;
pub mod pictures;
pub mod view;

pub use editor::{Checker, Editor, Logged, Motion, Outcome, Pos, Spot};

pub use view::RichTextView;
