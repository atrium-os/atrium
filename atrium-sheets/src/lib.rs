//! atrium-sheets, Atrium's spreadsheet (docs/spec/officina.md §8).
//!
//! [`doc`] holds the document: the workbook, undo, and the autosave journal that a crash
//! is recovered from. [`source`] shows a sheet through Pergola's `GridView`.

pub mod app;
pub mod bench;
mod clipboard;
mod comment;
pub mod condfmt;
pub mod data;
pub mod doc;
pub mod files;
pub mod fnhelp;
pub mod format;
pub mod lists;
pub mod pivotpane;
pub mod print;
pub mod review;
pub mod source;
mod sheet_protection;
pub mod tabs;
pub mod validation;
pub mod whatif;
pub mod scenarios;
pub mod series;
pub mod audit;
pub mod sparkbar;
pub mod formulas;
pub mod checks;
pub mod arrows;
pub mod dataform;
pub mod special;
pub mod symbols;
pub mod lock;
pub mod sheetbars;
pub mod spelling;
pub mod autocorrect;
pub mod customviews;
pub mod accessibility;
pub mod checkbox;
pub mod analysis;
