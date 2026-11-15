//! Folium's PDF syntax layer (docs/spec/folium.md §5): everything between
//! the descriptor and a parsed object. It reads hostile files, so every
//! read is bounded by a budget of §6 ([`limits`]) and every failure is an
//! [`Error`], never a panic.
//!
//! The document is read lazily: opening reads the tail, the
//! cross-reference chain and the trailer; objects are parsed when asked
//! for and kept in a bounded cache. A file whose cross-reference is broken
//! is opened by a bounded scan for its objects ([`repair`]).

pub mod crypt;
pub mod document;
mod error;
pub mod filter;
pub mod lexer;
pub mod limits;
pub mod object;
pub mod repair;
pub mod source;
pub mod xref;

pub use document::{Decoded, Document};
pub use error::{Error, Result};
pub use object::{Dict, Name, Object, Ref, Stream, StreamData};
pub use source::{Bytes, Source};
