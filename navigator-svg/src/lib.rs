//! The Navigator's SVG (threat model §2.4).
//!
//! ONE strict profile (`profile`), checked on roxmltree's tree before usvg
//! parses anything, and two ways out of it:
//! - a bitmap, drawn by resvg in the worker at the painted size
//!   (navigator-backend `svg.rs`), for everything the profile admits;
//! - filled paths (`vector`), for the part of it Fresco can draw as vector
//!   shapes: each path an NSG `path` node, one to one with Fresco's
//!   `ATRIUM_CORE_FILL`. What is outside that part keeps the bitmap path,
//!   and the reason is returned.

pub mod profile;
pub mod vector;

pub use profile::{load, Checked, LoadError};
