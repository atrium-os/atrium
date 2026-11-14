//! atrium-sheets, Atrium's spreadsheet (docs/spec/officina.md §8).
//!
//! [`doc`] holds the document: the workbook, undo, and the autosave journal that a crash
//! is recovered from. [`source`] shows a sheet through Pergola's `GridView`.

pub mod app;
pub mod bench;
pub mod doc;
pub mod source;
