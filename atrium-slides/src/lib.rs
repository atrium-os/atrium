//! atrium-slides, Atrium's presentation app (docs/spec/officina.md §9, §16.33).
//!
//! [`doc`] holds the deck, its undo history and the autosave journal a crash
//! is recovered from; [`render`] turns a slide's display list into Pergola
//! nodes; [`frame`] edits the text of one frame; [`app`] is the window; [`present`] runs a slideshow.

pub mod app;
pub mod doc;
pub mod files;
pub mod frame;
pub mod mapped;
pub mod present;
pub mod render;
