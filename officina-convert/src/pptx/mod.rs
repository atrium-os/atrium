//! `.pptx` (PresentationML) in and out (docs/spec/officina.md §9.2, O6c).

pub mod read;
pub mod write;

pub use read::{read, Loaded, PptxError};
pub use write::write;
