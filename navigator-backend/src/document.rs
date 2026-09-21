//! Parsing the document a recording carries.
//!
//! ★ Deliberately a SECOND stage, after ingest. Ingest answers "is this a
//! recording"; this answers "what document does it carry". A caller that
//! cannot tell those apart cannot tell a malformed recording from a
//! well-formed one containing an unreasonable document.

use navigator_dom::{parse, profile, Dom};

pub use navigator_dom::profile::Violation;

/// The document, parsed, plus what the transitions can actually address.
#[derive(Debug)]
pub struct Document {
    pub dom: Dom,
}

impl Document {
    /// ★ The same parser the converter used — that is the whole reason
    /// `navigator-dom` exists. Re-parsing with a different one would let the
    /// producer and the renderer disagree about the same bytes.
    ///
    /// This does NOT check the profile. Callers handling a recording from the
    /// store want `accept`; this stays for tests and for tools that need to
    /// look at a document precisely because it is out of bounds.
    pub fn parse(html: &str) -> Self {
        Document { dom: parse(html) }
    }

    /// ★★ THE PROFILE IS A RENDERER'S REFUSAL, NOT A CONVERTER'S WARNING.
    ///
    /// The converter reports violations and keeps going: it still holds the
    /// document, and saying *which* ceiling a real page broke is how the
    /// ceilings got corrected in the first place. The backend cannot afford
    /// that posture. Guarantee G3 is boundedness, and a document outside the
    /// profile is precisely the one for which no bound was ever established —
    /// so here a violation is a refusal.
    ///
    /// Both sides read the ceilings from `navigator_dom::profile`, for the
    /// same reason they read the tree from one parser. A renderer refusing
    /// documents its own converter happily emits is not a safety property, it
    /// is a broken pipeline.
    ///
    /// Note the ORDER: bytes are bounded at ingest, before this ever runs, so
    /// the parse below is over input of already-known size. Checking element
    /// and depth ceilings requires a tree, and building a tree from unbounded
    /// bytes to find out whether the bytes were bounded would be the check
    /// defeating itself.
    pub fn accept(html: &str) -> Result<Self, Vec<Violation>> {
        let dom = parse(html);
        let v = profile::check(&dom, html.len());
        if v.is_empty() { Ok(Document { dom }) } else { Err(v) }
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
