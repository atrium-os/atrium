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

/// What a session may remember.
///
/// ★ DERIVED, not chosen for roundness. Measured across the corpus, an undo
/// costs a median of 14 bytes, p99 2,193, max 2,284 — because the
/// overwhelming majority of transitions are attribute toggles, and an
/// attribute undo is two short strings. The whole undo history of every
/// transition in all 98 documents is 40 KB.
///
/// So the step count is what actually binds on real content: 64 steps of
/// measured traffic is roughly 140 KB. The byte bound only engages when a
/// page removes large subtrees, which the corpus does not do but a page is
/// free to — one removal can carry up to a whole document. It is set at an
/// eighth of the document ceiling so that a full history can never approach
/// the cost of the document it belongs to.
#[derive(Debug, Clone, Copy)]
pub struct HistoryLimits {
    pub max_steps: usize,
    pub max_undo_bytes: usize,
}

impl Default for HistoryLimits {
    fn default() -> Self {
        Self { max_steps: 64, max_undo_bytes: navigator_dom::profile::MAX_DOCUMENT_BYTES / 8 }
    }
}

/// The result of asking to go back.
///
/// ★★ `AtStart` AND `Forgotten` ARE DIFFERENT, AND A BOOLEAN WOULD MERGE
/// THEM. A reader who has taken 80 steps under a 64-step bound and presses
/// back 64 times has NOT arrived at the beginning — the earlier states were
/// discarded. Reporting that as "nothing before this" would be a lie told by
/// omission, and the reader would believe they had seen the whole document's
/// history. The bound is allowed to forget; it is not allowed to pretend it
/// did not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Back {
    /// Went back one state.
    Stepped,
    /// There is nothing before this: the document as the recording published it.
    AtStart,
    /// There were earlier states, and the bound discarded them.
    Forgotten,
}

/// A document plus the states it came from.
pub struct History {
    current: Document,
    past: std::collections::VecDeque<Transition>,
    limits: HistoryLimits,
    /// Steps dropped to stay inside the bound. Never reset: it is the reason
    /// `back` answers `Forgotten` rather than `AtStart`.
    forgotten: usize,
    undo_bytes: usize,
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

fn weight(t: &Transition) -> usize {
    t.effects.iter().map(|e| match e {
        Effect::Insert { html, .. } => html.len(),
        Effect::Attribute { from, to, .. } =>
            from.as_ref().map_or(0, String::len) + to.as_ref().map_or(0, String::len),
        Effect::RemoveRange { parent, .. } => parent.len(),
        _ => 0,
    }).sum()
}

impl History {
    pub fn new(document: Document) -> Self {
        History::with_limits(document, HistoryLimits::default())
    }

    pub fn with_limits(document: Document, limits: HistoryLimits) -> Self {
        History {
            current: document, past: Default::default(),
            limits, forgotten: 0, undo_bytes: 0,
        }
    }

    pub fn document(&self) -> &Document { &self.current }
    pub fn depth(&self) -> usize { self.past.len() }
    /// How many reachable states the bound has discarded, in total.
    pub fn forgotten(&self) -> usize { self.forgotten }

    pub fn cost(&self) -> HistoryCost {
        HistoryCost { steps: self.past.len(), undo_bytes: self.undo_bytes }
    }

    /// ★ FORWARD MOTION IS NEVER BLOCKED BY THE UNDO BUDGET. A step whose
    /// undo is too large to keep still happens; what it costs is the ability
    /// to come back from it. Refusing the step instead would let a page's own
    /// content decide whether a reader may turn the page, which is a far
    /// worse failure than a shortened history — and the reader is told,
    /// because `back` then answers `Forgotten`.
    fn trim(&mut self) {
        while self.past.len() > self.limits.max_steps
            || self.undo_bytes > self.limits.max_undo_bytes
        {
            let Some(dropped) = self.past.pop_front() else { break };
            self.undo_bytes -= weight(&dropped).min(self.undo_bytes);
            self.forgotten += 1;
        }
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
        self.undo_bytes += weight(&undo);
        self.past.push_back(undo);
        self.trim();
        Ok(())
    }

    /// Go back one step, saying which of the three things happened.
    pub fn back(&mut self) -> Result<Back, ApplyError> {
        let Some(undo) = self.past.pop_back() else {
            return Ok(if self.forgotten > 0 { Back::Forgotten } else { Back::AtStart });
        };
        match self.current.undone(&undo) {
            Ok(prev) => {
                self.current = prev;
                self.undo_bytes -= weight(&undo).min(self.undo_bytes);
                Ok(Back::Stepped)
            }
            Err(e) => {
                // A failed undo must not consume the step.
                self.past.push_back(undo);
                Err(e)
            }
        }
    }
}
