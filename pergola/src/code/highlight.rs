//! What the grid colours: spans of a line tagged with a syntax class. A
//! syntax module (tree-sitter in Artifex, plan P6) implements [`Highlight`];
//! the grid maps tags to theme colours, so every language looks the same.

use std::ops::Range;

/// A syntax class, the small set every grammar's captures map onto.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tag {
    Plain,
    Keyword,
    Type,
    Function,
    String,
    Number,
    Comment,
    Constant,
    Operator,
    Punctuation,
    Attribute,
    Macro,
    Variable,
    Property,
    Error,
}

impl Tag {
    /// The tag a tree-sitter capture name maps to (`keyword.control` is a
    /// keyword: only the first part counts).
    pub fn from_capture(name: &str) -> Tag {
        match name.split('.').next().unwrap_or("") {
            "keyword" | "conditional" | "repeat" | "include" | "exception" | "storageclass" => Tag::Keyword,
            "type" => Tag::Type,
            "function" | "method" | "constructor" => Tag::Function,
            "string" | "character" | "escape" => Tag::String,
            "number" | "float" | "boolean" => Tag::Number,
            "comment" => Tag::Comment,
            "constant" => Tag::Constant,
            "operator" => Tag::Operator,
            "punctuation" | "delimiter" => Tag::Punctuation,
            "attribute" | "label" | "annotation" => Tag::Attribute,
            "macro" => Tag::Macro,
            "variable" | "parameter" => Tag::Variable,
            "property" | "field" => Tag::Property,
            "error" => Tag::Error,
            _ => Tag::Plain,
        }
    }
}

/// `tag` over chars `cols` of a line (char columns, not display columns).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub cols: Range<usize>,
    pub tag: Tag,
}

/// Highlighting for the lines the grid shows.
pub trait Highlight {
    /// The spans of `line` (sorted, not overlapping); uncovered chars are
    /// [`Tag::Plain`].
    fn line(&self, line: usize) -> Vec<Span>;
}

/// No highlighting.
pub struct Plain;

impl Highlight for Plain {
    fn line(&self, _line: usize) -> Vec<Span> {
        Vec::new()
    }
}
