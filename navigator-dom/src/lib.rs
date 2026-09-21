//! The arena DOM and the HTML parser, shared.
//!
//! ★★ EXTRACTED SO THERE IS EXACTLY ONE OF EACH. The converter parses a
//! document; the backend re-parses the document the converter serialized into
//! a recording. If those were two parsers they would eventually disagree
//! about the same bytes — and a disagreement between the thing that produced
//! an artifact and the thing that renders it is not a bug you find by
//! testing either one.
//!
//! This is the same argument the converter already applies internally: its
//! `innerHTML` setter goes through the real parser rather than a second
//! hand-rolled one, for exactly this reason. Extracting it makes the rule
//! hold across crates rather than only within one.
//!
//! The crate deliberately contains no policy: no ceilings, no profile, no
//! decisions about what a document may be. Those differ between a converter
//! and a renderer, and only the TREE is common.

pub mod dom;
pub mod parse;

pub use dom::{Dom, Handle, Kind, Node};
pub use parse::{parse, parse_fragment};
