//! `.odp` (OpenDocument Presentation) in and out (docs/spec/officina.md §9.2, O6c).

pub(crate) mod geom;
#[cfg(feature = "read")]
pub mod read;
pub mod write;

#[cfg(feature = "read")]
pub use read::{read, OdpError};
pub use write::write;
