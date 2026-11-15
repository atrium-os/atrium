//! `.ods` (OpenDocument spreadsheet): [`read`] for the converter process, [`write`]
//! for the document process.

pub(crate) mod chart;
pub(crate) mod condfmt;
pub mod formula;
pub(crate) mod macros;
pub mod read;
pub mod write;

pub use read::{read, OdsError};
pub use write::write;
