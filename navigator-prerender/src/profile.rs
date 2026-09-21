//! Document Profile v1 ceilings, as code.
//!
//! ★ The profile calls these ceilings "normative and checked by the parser
//! and the scene-graph validator". Neither existed, so the numbers were
//! prose: nothing could exceed them because nothing measured them. These are
//! the subset computable from a parsed document alone, which is the subset
//! this converter can honestly check.
//!
//! ★★ Keeping them here rather than in the validator is deliberate. A
//! ceiling that is only enforced at the end of the pipeline is discovered
//! late and by the wrong component; a converter that knows the profile can
//! say "this document is outside it" while it still has the document.
//!
//! Values and their derivations live in `docs/spec/atrium-document-profile-v1.md`.
//! Three of them were corrected after measurement — tree depth was four times
//! too loose, stylesheet and font-face counts were below the corpus median —
//! so the constants below are the measured ones, not the original guesses.

use crate::dom::{Dom, Handle, Kind};

pub const MAX_DOCUMENT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_ELEMENTS: usize = 131_072;
pub const MAX_DEPTH: usize = 256;
pub const MAX_STYLESHEETS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub ceiling: &'static str,
    pub measured: usize,
    pub allowed: usize,
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {} exceeds {}", self.ceiling, self.measured, self.allowed)
    }
}

/// Stylesheets a document declares: `<style>` blocks plus `<link>` elements
/// whose `rel` names a stylesheet. Counted from the DOM, which is exact,
/// rather than from the markup.
pub fn stylesheet_count(dom: &Dom) -> usize {
    let styles = dom.by_tag("style").len();
    let links = dom.by_tag("link").into_iter().filter(|&h| {
        dom.attr(h, "rel").map(|r| r.to_ascii_lowercase().contains("stylesheet"))
            .unwrap_or(false)
    }).count();
    styles + links
}

/// Declared `@font-face` rules are NOT checked here, and that is a finding
/// rather than an omission: the count needs the document's CSS, which this
/// converter does not fetch. See the profile's note that total font bytes is
/// not a document property at all.
pub fn check(dom: &Dom, source_bytes: usize) -> Vec<Violation> {
    let mut out = vec![];
    let mut add = |ceiling, measured: usize, allowed: usize| {
        if measured > allowed { out.push(Violation { ceiling, measured, allowed }) }
    };
    add("total document bytes", source_bytes, MAX_DOCUMENT_BYTES);
    add("elements per document", dom.element_count(), MAX_ELEMENTS);
    add("tree depth", dom.max_depth(), MAX_DEPTH);
    add("stylesheets per document", stylesheet_count(dom), MAX_STYLESHEETS);
    out
}

/// The deepest chain of ELEMENTS, which is what the depth ceiling bounds —
/// text nodes are leaves and a document of one paragraph is not two deep
/// because of its text.
pub fn element_depth(dom: &Dom) -> usize {
    fn go(d: &Dom, h: Handle, cur: usize, best: &mut usize) {
        let is_el = matches!(d.get(h).map(|n| &n.kind), Some(Kind::Element(_)));
        let here = if is_el { cur + 1 } else { cur };
        *best = (*best).max(here);
        for c in d.children_of(h) { go(d, c, here, best) }
    }
    let mut best = 0;
    go(dom, dom.root(), 0, &mut best);
    best
}
