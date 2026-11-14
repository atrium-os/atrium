//! The formula language (docs/spec/officina.md §8.2): Excel's syntax and names, parsed
//! into one AST with relative references stored as offsets, so a formula filled down a
//! column is one AST shared by every row.

pub mod ast;
pub mod parse;
pub mod print;

pub use ast::*;
pub use parse::{parse, parse_stored, ParseError, MAX_DEPTH, MAX_FORMULA_LEN, MAX_STORED_FORMULA_LEN};
pub use print::to_a1;
