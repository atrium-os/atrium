//! `.ods` (OpenDocument spreadsheet): [`read`] for the converter process, [`write`]
//! for the document process.

pub mod formula;
pub mod read;
pub mod write;

pub use read::{read, OdsError};
pub use write::write;
