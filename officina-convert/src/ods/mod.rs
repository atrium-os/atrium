//! `.ods` (OpenDocument spreadsheet): [`read`] for the converter process, [`write`]
//! for the document process.

pub(crate) mod chart;
pub(crate) mod condfmt;
#[cfg(feature = "read")]
mod filter;
pub mod formula;
pub(crate) mod macros;
pub(crate) mod pivot;
pub(crate) mod settings;
pub(crate) mod validation;
#[cfg(feature = "read")]
pub mod read;
pub mod write;

#[cfg(feature = "read")]
pub use read::{read, OdsError};
pub use write::write;
