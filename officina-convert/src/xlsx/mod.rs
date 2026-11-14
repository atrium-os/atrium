//! `.xlsx` (SpreadsheetML): [`read`] for the converter process, [`write`] for the
//! document process.

pub mod chart;
pub mod macros;
pub mod read;
pub mod styles;
pub mod write;

pub use read::{read, Loaded, XlsxError};
pub use write::{write, write_xlsm};
