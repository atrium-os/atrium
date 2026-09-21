//! Parsing the document a recording carries.
//!
//! ★ Deliberately a SECOND stage, after ingest. Ingest answers "is this a
//! recording"; this answers "what document does it carry". A caller that
//! cannot tell those apart cannot tell a malformed recording from a
//! well-formed one containing an unreasonable document.

use navigator_dom::{parse, Dom};

/// The document, parsed, plus what the transitions can actually address.
pub struct Document {
    pub dom: Dom,
}

impl Document {
    /// ★ The same parser the converter used — that is the whole reason
    /// `navigator-dom` exists. Re-parsing with a different one would let the
    /// producer and the renderer disagree about the same bytes.
    pub fn parse(html: &str) -> Self {
        Document { dom: parse(html) }
    }

    /// Resolve a transition's trigger against this document.
    ///
    /// ★★ A TRIGGER IS A CLAIM ABOUT THIS DOCUMENT, and this is where it
    /// stops being one. The recording says `#menu-toggle`; whether that
    /// element exists here is a question only the parsed document can answer,
    /// and a recording that names elements which are not present is either
    /// stale, tampered with, or was recorded against a different tier — all
    /// of which the caller needs to know BEFORE it starts applying effects.
    pub fn resolve(&self, trigger: &str) -> Option<navigator_dom::Handle> {
        if let Some(id) = trigger.strip_prefix('#') {
            return self.dom.by_id(id);
        }
        // Positional paths (`tag:n>tag:n`) address the tree by shape, which
        // survives re-parsing only if the document is byte-identical — true
        // for tier 2, since that is the document the paths were recorded
        // against.
        let mut cur = self.dom.root();
        for step in trigger.split('>') {
            let (tag, nth) = step.rsplit_once(':')?;
            let nth: usize = nth.parse().ok()?;
            let mut seen = 0;
            let mut found = None;
            for c in self.dom.children_of(cur) {
                if self.dom.tag(c).map(|t| t.eq_ignore_ascii_case(tag)).unwrap_or(false) {
                    if seen == nth { found = Some(c); break }
                    seen += 1;
                }
            }
            cur = found?;
        }
        Some(cur)
    }
}
