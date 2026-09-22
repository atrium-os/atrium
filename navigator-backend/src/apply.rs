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
        self.applied_with_undo(t).map(|(d, _)| d)
    }

    /// The document after the transition, AND the transition that undoes it.
    ///
    /// ★★ THE UNDO IS BUILT BY DOING THE WORK, NOT BY READING THE RECORDING.
    /// Everything an inverse needs — the subtree a removal destroys, the
    /// position an insert lands at — is present in the document at the moment
    /// the effect is applied. Deriving it here means the undo describes what
    /// actually happened rather than what a file claims happened, and it
    /// keeps a second copy of a derivable fact out of untrusted input, where
    /// it could disagree with the document and leave a consumer to pick.
    ///
    /// The undo is an ordinary transition in the ordinary format, so applying
    /// it goes through the ordinary path: preconditions, profile check, and
    /// the same all-or-nothing rule.
    pub fn applied_with_undo(&self, t: &Transition) -> Result<(Document, Transition), ApplyError> {
        self.apply_inner(t, true)
    }

    /// Apply an undo produced by `applied_with_undo`.
    ///
    /// ★ Identical to applying any other transition EXCEPT that the trigger
    /// is not required to resolve. The trigger check asks "was this recorded
    /// against this document"; for an undo built from this very document that
    /// question is already answered, and a forward step that removed its own
    /// trigger element — a tab control replaced by the panel it opens — would
    /// otherwise be impossible to undo. Preconditions, the profile check and
    /// atomicity all still apply: those ask whether the edit is VALID, which
    /// is a different question and one the undo still has to answer.
    pub fn undone(&self, undo: &Transition) -> Result<Document, ApplyError> {
        self.apply_inner(undo, false).map(|(d, _)| d)
    }

    fn apply_inner(&self, t: &Transition, check_trigger: bool)
        -> Result<(Document, Transition), ApplyError>
    {
        // ★ FIRST, because it is the cheapest refusal and the most
        // informative: a transition whose own trigger is absent was not
        // recorded against this document at all, and every other error it
        // would produce is a consequence of that one.
        if check_trigger && self.resolve(&t.trigger).is_none() {
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
        let mut undo: Vec<Effect> = vec![];
        for e in &t.effects {
            next.apply_one(e, &mut undo)?;
        }
        // ★ Undone last-first. The forward effects ran in an order the
        // converter chose — attributes, then removals, then insertions — and
        // reversing it is what makes the positional paths in the earlier
        // effects mean again what they meant when they were recorded.
        undo.reverse();

        // ★ Re-checked on the RESULT, not on the inserted markup alone. An
        // insert that is individually small can still be the one that pushes
        // a document past the element ceiling, and the ceiling is a property
        // of the document, not of any single edit. The per-effect limits do
        // not subsume it: 64 effects of 64 KiB each is 4 MiB of growth, and
        // the document ceiling is 8 MiB.
        //
        // The byte count comes from `serialized_len`, which runs the real
        // serializer into a counter. It used to build the whole document to
        // take its length: that was 64% of an apply, and an apply IS a
        // navigation in the worker — 16 ms p50 per step on the VM, on the
        // reader's path. The clone above is the next cost (19%).
        let v = profile::check(&next.dom, next.dom.serialized_len());
        if !v.is_empty() { return Err(ApplyError::OutsideProfile(v)) }
        let inverse = Transition {
            trigger: t.trigger.clone(), event: t.event.clone(),
            anchored: t.anchored, effects: undo,
        };
        Ok((next, inverse))
    }

    fn target(&self, path: &str) -> Result<Handle, ApplyError> {
        self.resolve(path).ok_or_else(|| ApplyError::TargetUnresolved { path: path.to_string() })
    }

    fn apply_one(&mut self, e: &Effect, undo: &mut Vec<Effect>) -> Result<(), ApplyError> {
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
                undo.push(Effect::Attribute {
                    target: target.clone(), name: name.clone(),
                    from: to.clone(), to: from.clone(),
                });
                match to {
                    Some(v) => self.dom.set_attr(h, name, v),
                    None => self.dom.remove_attr(h, name),
                }
            }
            Effect::Remove { target } => {
                let h = self.target(target)?;
                // ★★ THE INVERSE COMES FROM THE DOCUMENT, NOT THE RECORDING.
                //
                // Restoring a removal needs the subtree, its parent and its
                // position — and all three are RIGHT HERE, in the document
                // about to lose them. Recording them in the file as well
                // would put a second copy of a derivable fact into untrusted
                // input, where it can disagree with the document and leave a
                // consumer to decide which to believe. Read it from the one
                // that cannot be wrong.
                let Some(p) = self.dom.get(h).and_then(|n| n.parent) else {
                    return Err(ApplyError::TargetUnresolved { path: target.clone() });
                };
                let index = self.dom.children_of(p).iter().position(|&c| c == h).unwrap_or(0);
                undo.push(Effect::Insert {
                    parent: self.dom.node_path(p),
                    index: Some(index),
                    html: self.dom.outer_html(h),
                });
                // `detach`, not `detach_for_move`: this IS a removal, and the
                // distinction is what the protected-subtree experiment turned
                // on. Reusing the move path here would quietly exempt replay
                // from a rule the converter enforces.
                self.dom.detach(h);
            }
            Effect::Insert { parent, index, html } => {
                let p = self.target(parent)?;
                // ★ Through the shared parser, like every other piece of
                // markup in this system. Inserted HTML is the most hostile
                // input the backend handles and is exactly where a second,
                // more forgiving parser would get written.
                let frag = parse_fragment(html);
                let roots = frag.children_of(frag.root());
                let count = roots.len();
                // ★ An out-of-range index is CLAMPED, not refused. The index
                // is a position in a tree the recording describes and this
                // document need not match it exactly; appending is what a
                // version 1 recording does anyway, so a nonsensical index
                // degrades to the older behaviour rather than losing content.
                let at = index.unwrap_or(usize::MAX).min(self.dom.children_of(p).len());
                for (i, c) in roots.into_iter().enumerate() {
                    let g = self.dom.graft(&frag, c);
                    let before = self.dom.children_of(p).get(at + i).copied();
                    self.dom.insert_before(p, g, before);
                }
                undo.push(Effect::RemoveRange {
                    parent: self.dom.node_path(p), index: at, count,
                });
            }
            Effect::RemoveRange { parent, index, count } => {
                let p = self.target(parent)?;
                let kids = self.dom.children_of(p);
                if index + count > kids.len() {
                    return Err(ApplyError::TargetUnresolved {
                        path: format!("{parent}[{index}..{}]", index + count),
                    });
                }
                for &h in kids[*index..index + count].iter().rev() {
                    undo.push(Effect::Insert {
                        parent: parent.clone(),
                        index: Some(*index),
                        html: self.dom.outer_html(h),
                    });
                    self.dom.detach(h);
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
