//! `.odp` (OpenDocument Presentation) in and out (docs/spec/officina.md §9.2, O6c).

mod geom;
pub mod read;
pub mod write;

pub use read::{read, OdpError};
pub use write::write;
