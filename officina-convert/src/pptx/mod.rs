//! `.pptx` (PresentationML) in and out (docs/spec/officina.md §9.2, O6c).

#[cfg(feature = "read")]
pub mod read;
pub mod write;

#[cfg(feature = "read")]
pub use read::{read, Loaded, PptxError};
pub use write::write;
