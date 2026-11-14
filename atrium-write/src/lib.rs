//! atrium-write, Atrium's word processor (docs/spec/officina.md §7).
//!
//! [`doc`] holds the document: the editor (text, undo, pages) and the
//! autosave journal a crash is recovered from. [`app`] is the window: toolbar,
//! pages through Pergola's `RichTextEditor`, status line. [`files`] says what
//! a file is by its name.

pub mod app;
pub mod doc;
pub mod files;
