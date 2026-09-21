//! Going back.
//!
//! ★★★ THE UNDO IS DERIVED FROM THE DOCUMENT, NOT FROM THE RECORDING.
//!
//! An earlier version of this module built an inverse by reading the
//! transition: an attribute effect records both sides, so swap them. It could
//! not invert a removal — the recording carries only a path, not the subtree
//! — so `History` kept a snapshot of the whole document for those steps, and
//! measured that 7 of the corpus's 264 anchored transitions paid that cost.
//!
//! The fix was not to record more. Everything an inverse needs is already in
//! the document at the moment the effect is applied: a removal is about to
//! destroy a subtree that is right there, and an insertion chooses the
//! position it lands at. `Document::applied_with_undo` reads it from the one
//! source that cannot be wrong, and every transition became exactly
//! reversible — no snapshots, no unreversible cases, nothing to measure a
//! fallback rate for.
//!
//! ★ The discarded alternative is worth keeping visible, because it is the
//! obvious one: put the removed markup in the recording. That would add a
//! second copy of a derivable fact to UNTRUSTED input, where it can disagree
//! with the document — and a consumer holding two versions of what used to be
//! at a path has to choose one, with nothing to choose on. The version 2
//! format therefore records an insert's POSITION, which is genuinely not
//! derivable, and nothing else.
//!
//! The undo is an ordinary transition in the ordinary format, so it goes back
//! through the ordinary path: preconditions, profile check, atomicity. An
//! undo that skipped them would be the one operation here that trusts its
//! input, acting on a document already modified once.

use crate::apply::ApplyError;
use crate::document::Document;
use crate::{Effect, Transition};

/// A document plus the states it came from.
pub struct History {
    current: Document,
    past: Vec<Transition>,
}

/// What a session actually costs, so the memory question is answered by
/// measurement rather than by argument. `undo_bytes` is the markup the undo
/// steps carry — the content removals destroyed, which has to live somewhere
/// for "back" to mean anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HistoryCost {
    pub steps: usize,
    pub undo_bytes: usize,
}

impl History {
    pub fn new(document: Document) -> Self {
        History { current: document, past: vec![] }
    }

    pub fn document(&self) -> &Document { &self.current }
    pub fn depth(&self) -> usize { self.past.len() }

    pub fn cost(&self) -> HistoryCost {
        let undo_bytes = self.past.iter()
            .flat_map(|t| &t.effects)
            .map(|e| match e {
                Effect::Insert { html, .. } => html.len(),
                Effect::Attribute { from, to, .. } =>
                    from.as_ref().map_or(0, String::len) + to.as_ref().map_or(0, String::len),
                _ => 0,
            })
            .sum();
        HistoryCost { steps: self.past.len(), undo_bytes }
    }

    /// Take a transition forward.
    ///
    /// ★ A refused transition must leave the history exactly as it was — the
    /// same all-or-nothing rule `apply` follows, and for the same reason: a
    /// history holding a step that did not happen would send the reader back
    /// to a state they were never in.
    pub fn go(&mut self, t: &Transition) -> Result<(), ApplyError> {
        let (next, undo) = self.current.applied_with_undo(t)?;
        self.current = next;
        self.past.push(undo);
        Ok(())
    }

    /// Go back one step. `Ok(false)` means there was nowhere to go, which is
    /// not an error — it is the reader being at the start.
    pub fn back(&mut self) -> Result<bool, ApplyError> {
        let Some(undo) = self.past.pop() else { return Ok(false) };
        match self.current.undone(&undo) {
            Ok(prev) => { self.current = prev; Ok(true) }
            Err(e) => {
                // A failed undo must not consume the step.
                self.past.push(undo);
                Err(e)
            }
        }
    }
}
