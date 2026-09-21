//! What a recording may contain.
//!
//! ★ Every one of these is a REFUSAL boundary, not a guideline, and the
//! defaults are derived from what the converter actually emits rather than
//! chosen for roundness — the same discipline the document profile uses. A
//! limit far above anything real is not protection; it is a number that
//! makes a reviewer feel better.

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// The whole recording. The converter's largest real output is ~1 MiB of
    /// document plus a small table; the corpus maximum document is 2 MiB.
    pub max_total_bytes: usize,
    /// The embedded document. Matches the profile's own document ceiling, so
    /// a recording cannot smuggle in a document the profile would refuse.
    pub max_document_bytes: usize,
    pub max_transitions: usize,
    pub max_effects_per_transition: usize,
    /// Any single string: a trigger path, an attribute value, inserted HTML.
    pub max_string_bytes: usize,
    /// Inserted markup, which the converter already bounds at 16 KiB.
    pub max_insert_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_total_bytes: 16 * 1024 * 1024,
            max_document_bytes: 8 * 1024 * 1024,
            max_transitions: 4096,
            max_effects_per_transition: 64,
            max_string_bytes: 64 * 1024,
            max_insert_bytes: 64 * 1024,
        }
    }
}
