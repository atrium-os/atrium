//! Applying a transition's effects to the document it was recorded against.
//!
//! ★★ ALL OR NOTHING. A transition is one observed step of a state machine:
//! the converter clicked something, diffed the tree, and recorded the whole
//! difference. Applying half of it produces a document that no one ever
//! observed and nothing ever validated — a menu marked open whose contents
//! were never inserted, a node removed whose replacement failed to resolve.
//! That is worse than refusing, because it looks like a rendered page.
//!
//! So every effect is checked before any effect lands, the work happens on a
//! copy, and the copy replaces the document only if all of it succeeded.
//!
//! ★ AND THE RECORDING IS UNTRUSTED. It arrives from a content-addressed
//! store; it may be stale, may have been recorded against a different
//! document, may be hostile. Three things follow, and each is a refusal
//! rather than a best effort:
//!
//!   - Every path must resolve, in THIS document.
//!   - Every attribute effect states what it is replacing, and the document
//!     must actually hold that. A recording that says `aria-expanded` goes
//!     `false -> true` against a document where it is already `true` is not
//!     describing this document.
//!   - The result must still be inside the Document Profile. `Insert` carries
//!     markup, and markup is how a bounded document becomes an unbounded one.

use crate::document::Document;
use crate::{Effect, Transition};
use navigator_dom::{parse_fragment, profile, Handle};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyError {
    /// The transition's own trigger does not exist here.
    TriggerUnresolved { trigger: String },
    /// An effect names a node this document does not have.
    TargetUnresolved { path: String },
    /// ★ The document is not in the state the recording expects. Stale,
    /// tampered with, or recorded against a different tier — the caller needs
    /// to tell those apart, so the refusal carries both values.
    PreconditionFailed { path: String, name: String, expected: Option<String>, found: Option<String> },
    /// The recording says effects were dropped when it was made, so it does
    /// not describe a complete step and cannot be replayed into one.
    Incomplete { dropped: u64 },
    /// Applying it would leave a document outside the profile.
    OutsideProfile(Vec<profile::Violation>),
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApplyError::TriggerUnresolved { trigger } =>
                write!(f, "trigger {trigger:?} does not resolve in this document"),
            ApplyError::TargetUnresolved { path } =>
                write!(f, "effect target {path:?} does not resolve in this document"),
            ApplyError::PreconditionFailed { path, name, expected, found } =>
                write!(f, "{path:?}: expected {name}={expected:?} but found {found:?}"),
            ApplyError::Incomplete { dropped } =>
                write!(f, "transition is incomplete: {dropped} effects were dropped when recorded"),
            ApplyError::OutsideProfile(v) => {
                write!(f, "applying it leaves the document outside the profile: ")?;
                for (i, x) in v.iter().enumerate() {
                    if i > 0 { write!(f, ", ")? }
                    write!(f, "{x}")?;
                }
                Ok(())
            }
        }
    }
}

impl Document {
    /// Apply one transition, or change nothing and say why.
    ///
    /// ★ The copy is the atomicity mechanism, and it is not free: it clones
    /// the arena, which for the corpus's largest document is about 2 MiB. The
    /// alternative — applying in place with an undo log — means the rollback
    /// path is code that runs only when something has already gone wrong,
    /// which is the code least likely to be right. A copy is correct by
    /// construction, and if this ever becomes the bottleneck the measurement
    /// will say so before an optimisation does.
    pub fn apply(&mut self, t: &Transition) -> Result<(), ApplyError> {
        let next = self.applied(t)?;
        *self = next;
        Ok(())
    }

    /// The document as it would be after the transition, leaving this one
    /// untouched. Useful on its own: a reader UI wants to know a transition
    /// is applicable before it offers the control that triggers it.
    pub fn applied(&self, t: &Transition) -> Result<Document, ApplyError> {
        // ★ FIRST, because it is the cheapest refusal and the most
        // informative: a transition whose own trigger is absent was not
        // recorded against this document at all, and every other error it
        // would produce is a consequence of that one.
        if self.resolve(&t.trigger).is_none() {
            return Err(ApplyError::TriggerUnresolved { trigger: t.trigger.clone() });
        }
        // A recording that already knows it is missing effects cannot be
        // replayed into the state it names. Checked before anything is
        // applied, not discovered part-way through.
        for e in &t.effects {
            if let Effect::Truncated { dropped } = e {
                return Err(ApplyError::Incomplete { dropped: *dropped });
            }
        }

        let mut next = Document { dom: self.dom.clone() };
        for e in &t.effects {
            next.apply_one(e)?;
        }

        // ★ Re-checked on the RESULT, not on the inserted markup alone. An
        // insert that is individually small can still be the one that pushes
        // a document past the element ceiling, and the ceiling is a property
        // of the document, not of any single edit. The per-effect limits do
        // not subsume it: 64 effects of 64 KiB each is 4 MiB of growth, and
        // the document ceiling is 8 MiB.
        //
        // This serializes the whole tree to measure its bytes, which is the
        // expensive part of applying a transition — replaying all 264 of the
        // corpus's anchored transitions takes tens of seconds, dominated by
        // this and by the clone above. Both are stated rather than optimised
        // because neither is on a reader's path yet; when one is, the number
        // to beat is measured rather than guessed.
        let v = profile::check(&next.dom, next.dom.serialize().len());
        if !v.is_empty() { return Err(ApplyError::OutsideProfile(v)) }
        Ok(next)
    }

    fn target(&self, path: &str) -> Result<Handle, ApplyError> {
        self.resolve(path).ok_or_else(|| ApplyError::TargetUnresolved { path: path.to_string() })
    }

    fn apply_one(&mut self, e: &Effect) -> Result<(), ApplyError> {
        match e {
            Effect::Attribute { target, name, from, to } => {
                let h = self.target(target)?;
                let found = self.dom.attr(h, name).map(str::to_string);
                if &found != from {
                    return Err(ApplyError::PreconditionFailed {
                        path: target.clone(), name: name.clone(),
                        expected: from.clone(), found,
                    });
                }
                match to {
                    Some(v) => self.dom.set_attr(h, name, v),
                    None => self.dom.remove_attr(h, name),
                }
            }
            Effect::Remove { target } => {
                let h = self.target(target)?;
                // `detach`, not `detach_for_move`: this IS a removal, and the
                // distinction is what the protected-subtree experiment turned
                // on. Reusing the move path here would quietly exempt replay
                // from a rule the converter enforces.
                self.dom.detach(h);
            }
            Effect::Insert { parent, html } => {
                let p = self.target(parent)?;
                // ★ Through the shared parser, like every other piece of
                // markup in this system. Inserted HTML is the most hostile
                // input the backend handles and is exactly where a second,
                // more forgiving parser would get written.
                let frag = parse_fragment(html);
                for c in frag.children_of(frag.root()) {
                    let g = self.dom.graft(&frag, c);
                    self.dom.append(p, g);
                }
            }
            // Handled before application begins; unreachable here, and
            // written as a refusal rather than a `_ => {}` so a new effect
            // kind cannot be silently ignored by this match.
            Effect::Truncated { dropped } =>
                return Err(ApplyError::Incomplete { dropped: *dropped }),
        }
        Ok(())
    }
}
