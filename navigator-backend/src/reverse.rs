//! Going back.
//!
//! ★★ REVERSIBILITY IS NOT UNIFORM, AND PRETENDING IT IS WOULD BE THE BUG.
//!
//! An `Attribute` effect records both sides of the change, so its inverse is
//! exact: swap `from` and `to` and the precondition machinery in `apply`
//! checks the reversal as rigorously as it checked the original.
//!
//! `Remove` and `Insert` do not have that property, and no amount of care
//! here can invent it:
//!
//!   - `Remove` records only a path. What was at that path — its subtree, its
//!     attributes, its position among its siblings — is not in the recording.
//!     An "inverse" would have to guess a document.
//!   - `Insert` records a parent and markup, but not WHERE under that parent
//!     the markup went, and replay appends. Removing "what was inserted"
//!     means identifying nodes by a position the recording never stated.
//!
//! So `inverse()` refuses rather than approximating, and `History` pays for
//! the difference where it is actually owed: an exact inverse costs nothing,
//! and only a transition that has none costs a snapshot. On the corpus that
//! is the rare case — the overwhelming majority of recorded transitions are
//! attribute-only, which is the same measurement that decides whether tier 2
//! earns its cost at all.
//!
//! The alternative, snapshotting every state, would be simpler and would also
//! make the 96% case pay for the 4%.

use crate::apply::ApplyError;
use crate::document::Document;
use crate::{Effect, Transition};

/// Why a transition cannot be turned around.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotInvertible {
    /// The removed subtree is not in the recording, so nothing can restore it.
    RemovedContentNotRecorded { target: String },
    /// The insert's position under its parent is not in the recording, so
    /// nothing can identify what to take back out.
    InsertExtentNotRecorded { parent: String },
    /// An incomplete transition does not describe a step in either direction.
    Incomplete { dropped: u64 },
}

impl std::fmt::Display for NotInvertible {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotInvertible::RemovedContentNotRecorded { target } => write!(f,
                "remove of {target:?} cannot be undone: the recording does not carry what was there"),
            NotInvertible::InsertExtentNotRecorded { parent } => write!(f,
                "insert under {parent:?} cannot be undone: the recording does not say where it went"),
            NotInvertible::Incomplete { dropped } => write!(f,
                "transition is incomplete ({dropped} effects dropped) and has no inverse"),
        }
    }
}

impl Transition {
    /// The transition that undoes this one, when the recording contains
    /// enough to build it.
    ///
    /// ★ The effects come back in REVERSE order. For attribute writes on
    /// distinct nodes the order is immaterial, and every inverse this can
    /// currently build is of that shape — so reversing costs nothing today
    /// and is correct for the ordered effects a later format version may add.
    /// Getting this right while it is free is cheaper than noticing it later.
    pub fn inverse(&self) -> Result<Transition, NotInvertible> {
        let mut effects = Vec::with_capacity(self.effects.len());
        for e in self.effects.iter().rev() {
            effects.push(match e {
                Effect::Attribute { target, name, from, to } => Effect::Attribute {
                    target: target.clone(),
                    name: name.clone(),
                    from: to.clone(),
                    to: from.clone(),
                },
                Effect::Remove { target } =>
                    return Err(NotInvertible::RemovedContentNotRecorded { target: target.clone() }),
                Effect::Insert { parent, .. } =>
                    return Err(NotInvertible::InsertExtentNotRecorded { parent: parent.clone() }),
                Effect::Truncated { dropped } =>
                    return Err(NotInvertible::Incomplete { dropped: *dropped }),
            });
        }
        Ok(Transition {
            trigger: self.trigger.clone(),
            event: self.event.clone(),
            anchored: self.anchored,
            effects,
        })
    }

    /// Whether going back from this transition is free, which a caller wants
    /// to know BEFORE it decides how much memory a session will cost.
    pub fn is_invertible(&self) -> bool { self.inverse().is_ok() }
}

/// A document plus the states it came from.
///
/// ★ The history is what makes "back" mean the state the reader was actually
/// in, rather than a state reconstructed from a recording that may not
/// describe one. Steps that can be inverted exactly are stored as their
/// inverse; steps that cannot are stored as the document that preceded them.
pub struct History {
    current: Document,
    past: Vec<Step>,
}

enum Step {
    /// The transition that undoes the one taken — a handful of strings.
    Inverse(Box<Transition>),
    /// The whole prior document, for a step with no derivable inverse.
    Snapshot(Box<Document>),
}

/// What a session actually cost, so the memory question is answered by
/// measurement rather than by argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HistoryCost {
    pub steps: usize,
    pub inverses: usize,
    pub snapshots: usize,
}

impl History {
    pub fn new(document: Document) -> Self {
        History { current: document, past: vec![] }
    }

    pub fn document(&self) -> &Document { &self.current }
    pub fn depth(&self) -> usize { self.past.len() }

    pub fn cost(&self) -> HistoryCost {
        let snapshots = self.past.iter().filter(|s| matches!(s, Step::Snapshot(_))).count();
        HistoryCost { steps: self.past.len(), inverses: self.past.len() - snapshots, snapshots }
    }

    /// Take a transition forward.
    ///
    /// ★ The inverse is computed BEFORE the step is taken and against the
    /// document it applies to. A refused transition must leave the history
    /// exactly as it was — the same all-or-nothing rule `apply` follows, and
    /// for the same reason: a history that grew by a step that did not happen
    /// would send the reader "back" to a state they were never in.
    pub fn go(&mut self, t: &Transition) -> Result<(), ApplyError> {
        let next = self.current.applied(t)?;
        let prev = std::mem::replace(&mut self.current, next);
        // `prev` is simply dropped when an exact inverse exists — which is
        // the whole point: the common case costs a few strings, not a tree.
        self.past.push(match t.inverse() {
            Ok(inv) => Step::Inverse(Box::new(inv)),
            Err(_) => Step::Snapshot(Box::new(prev)),
        });
        Ok(())
    }

    /// Go back one step. `Ok(false)` means there was nowhere to go, which is
    /// not an error — it is the reader being at the start.
    pub fn back(&mut self) -> Result<bool, ApplyError> {
        let Some(step) = self.past.pop() else { return Ok(false) };
        match step {
            Step::Inverse(inv) => {
                // ★ Applied through the ordinary path, preconditions and
                // profile check included. An undo that skipped them would be
                // the one operation in this backend that trusts a recording,
                // and it is the operation acting on a document that has
                // already been modified once.
                match self.current.applied(&inv) {
                    Ok(prev) => { self.current = prev; Ok(true) }
                    Err(e) => {
                        // Put it back: a failed undo must not consume the step.
                        self.past.push(Step::Inverse(inv));
                        Err(e)
                    }
                }
            }
            Step::Snapshot(prev) => { self.current = *prev; Ok(true) }
        }
    }
}
