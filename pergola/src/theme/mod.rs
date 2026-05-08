//! Theme tokens — the entire visual language as Rust constants.
//!
//! See `docs/design/atrium-visual-language.md`. **Widgets reference
//! these tokens; never raw values.** A widget that uses `16.0` for
//! padding or `Color::from_hex("#F2F4F6")` directly is broken.
//!
//! The token table is the single source of truth: a future theme
//! refresh edits this file and *every* widget updates.

pub mod tokens;

pub use tokens::*;
