//! `.xlsx` (SpreadsheetML): [`read`] for the converter process, [`write`] for the
//! document process.

/// Excel's maximum digit width for Calibri 11 at 96 dpi, px: column widths are in
/// these units.
pub const MDW: f64 = 7.0;

pub mod chart;
pub mod comments;
pub mod condfmt;
#[cfg(feature = "read")]
mod external;
pub mod macros;
pub mod pivot;
#[cfg(feature = "read")]
pub mod read;
pub mod styles;
pub mod validation;
mod threaded;
pub mod write;

#[cfg(feature = "read")]
pub use read::{read, Loaded, XlsxError};
pub use write::{write, write_xlsm};
