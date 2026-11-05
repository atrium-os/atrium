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

        // ★★★ EVERY ADDRESS NAMES A NODE IN THE DOCUMENT AS IT WAS BEFORE THE
        // STEP — this one — and is resolved here, before anything changes.
        //
        // That is how the converter writes them: it diffs the pre-probe tree
        // against the post-probe tree and names every node by its pre-probe
        // path (`diff_effects`). Replay used to resolve each effect against
        // the document as the EARLIER effects of the same step had left it,
        // and the two readings part as soon as one effect moves an address:
        // ynet renames two ids and its undo named them by the old ones;
        // yahoo.jp inserts a `<div>` before the `div:1` a later effect
        // addresses, which then named a different node, or none. Handles are
        // stable across the copy below, so resolving once and acting on
        // handles reads every address the way it was written.
        let mut ops: Vec<Op> = Vec::with_capacity(t.effects.len());
        for e in &t.effects {
            ops.push(match e {
                Effect::Attribute { target, name, from, to, at } => Op::Attr {
                    h: self.target(target)?, target, name, from, to, at: *at,
                },
                Effect::Remove { target } => {
                    let h = self.target(target)?;
                    if self.dom.get(h).and_then(|n| n.parent).is_none() {
                        return Err(ApplyError::TargetUnresolved { path: target.clone() });
                    }
                    Op::Remove(vec![h])
                }
                Effect::RemoveRange { parent, index, count } => {
                    let p = self.target(parent)?;
                    let kids = self.dom.children_of(p);
                    if index + count > kids.len() {
                        return Err(ApplyError::TargetUnresolved {
                            path: format!("{parent}[{index}..{}]", index + count),
                        });
                    }
                    Op::Remove(kids[*index..index + count].to_vec())
                }
                Effect::Insert { parent, index, html } =>
                    Op::Insert { p: self.target(parent)?, parent, index: *index, html },
                Effect::Truncated { dropped } =>
                    return Err(ApplyError::Incomplete { dropped: *dropped }),
            });
        }

        let mut next = Document { dom: self.dom.clone() };
        let mut removed: Vec<Handle> = vec![];
        let mut inserted: Vec<Handle> = vec![];

        // Attributes, in the order recorded. The precondition is read from
        // the copy, so two effects on one attribute compose.
        let mut touched: Vec<(Handle, &str, &Option<String>, Option<usize>)> = vec![];
        for op in &ops {
            let Op::Attr { h, target, name, from, to, at } = op else { continue };
            let found = next.dom.attr(*h, name).map(str::to_string);
            if &found != *from {
                return Err(ApplyError::PreconditionFailed {
                    path: (*target).clone(), name: (*name).clone(),
                    expected: (*from).clone(), found,
                });
            }
            touched.push((*h, name.as_str(), *from, next.dom.attr_index(*h, name)));
            match (to, at) {
                (Some(v), Some(i)) => next.dom.set_attr_at(*h, name, v, *i),
                (Some(v), None) => next.dom.set_attr(*h, name, v),
                (None, _) => next.dom.remove_attr(*h, name),
            }
        }

        // Removals. A node already gone — named twice, or inside a subtree
        // this step removes — is not a node of the document being edited.
        for op in &ops {
            let Op::Remove(hs) = op else { continue };
            for &h in hs {
                if !next.dom.connected(h) {
                    return Err(ApplyError::TargetUnresolved { path: self.dom.exact_path(h) });
                }
                // `detach`, not `detach_for_move`: this IS a removal, and the
                // distinction is what the protected-subtree experiment turned
                // on. Reusing the move path here would quietly exempt replay
                // from a rule the converter enforces.
                next.dom.detach(h);
                removed.push(h);
            }
        }

        // ★ Insertions, in ascending index. An index is the markup's position
        // among the parent's children AFTER the step, so inserting lowest
        // first is what puts each one where it was observed, whatever order
        // the recording lists them in.
        let mut ins: Vec<(Handle, &String, Option<usize>, &String)> = ops.iter()
            .filter_map(|op| match op {
                Op::Insert { p, parent, index, html } => Some((*p, *parent, *index, *html)),
                _ => None,
            }).collect();
        ins.sort_by_key(|&(_, _, index, _)| index.unwrap_or(usize::MAX));
        for (p, parent, index, html) in ins {
            if !next.dom.connected(p) {
                return Err(ApplyError::TargetUnresolved { path: parent.clone() });
            }
            // ★ Through the shared parser, like every other piece of
            // markup in this system. Inserted HTML is the most hostile
            // input the backend handles and is exactly where a second,
            // more forgiving parser would get written.
            let frag = parse_fragment(html);
            let roots = frag.children_of(frag.root());
            // ★ An out-of-range index is CLAMPED, not refused. The index
            // is a position in a tree the recording describes and this
            // document need not match it exactly; appending is what a
            // version 1 recording does anyway, so a nonsensical index
            // degrades to the older behaviour rather than losing content.
            let at = index.unwrap_or(usize::MAX).min(next.dom.children_of(p).len());
            for (i, c) in roots.into_iter().enumerate() {
                let g = next.dom.graft(&frag, c);
                let before = next.dom.children_of(p).get(at + i).copied();
                next.dom.insert_before(p, g, before);
                inserted.push(g);
            }
        }

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

        // ★★ THE UNDO IS BUILT FROM THE TWO DOCUMENTS, NOT THE RECORDING, and
        // is itself addressed the way every transition is: against the
        // document it will be applied to — `next` — by paths that resolve
        // there (`exact_path` checks that, falling back to position when an
        // id is shared or was just renamed away). Removed content is taken
        // from `self`, before this step's attribute writes touched it.
        let mut undo = vec![];
        for (h, name, from, index) in touched.into_iter().rev() {
            // A node this step removed comes back whole, from `self`.
            if !next.dom.connected(h) { continue }
            let now = next.dom.attr(h, name).map(str::to_string);
            undo.push(Effect::Attribute {
                target: next.dom.exact_path(h), name: name.to_string(),
                from: now, to: from.clone(),
                at: if from.is_some() { index } else { None },
            });
        }
        for &g in &inserted {
            let p = next.dom.get(g).and_then(|n| n.parent).expect("just inserted");
            let index = next.dom.children_of(p).iter().position(|&c| c == g).expect("a child");
            undo.push(Effect::RemoveRange { parent: next.dom.exact_path(p), index, count: 1 });
        }
        for &h in &removed {
            let p = self.dom.get(h).and_then(|n| n.parent).expect("checked above");
            let index = self.dom.children_of(p).iter().position(|&c| c == h).unwrap_or(0);
            undo.push(Effect::Insert {
                parent: next.dom.exact_path(p), index: Some(index), html: self.dom.outer_html(h),
            });
        }
        let inverse = Transition {
            trigger: t.trigger.clone(), event: t.event.clone(),
            anchored: t.anchored, effects: undo,
        };
        Ok((next, inverse))
    }

    fn target(&self, path: &str) -> Result<Handle, ApplyError> {
        self.resolve(path).ok_or_else(|| ApplyError::TargetUnresolved { path: path.to_string() })
    }
}

/// One effect, its addresses resolved against the pre-step document.
enum Op<'a> {
    Attr { h: Handle, target: &'a String, name: &'a String, from: &'a Option<String>,
           to: &'a Option<String>, at: Option<usize> },
    Remove(Vec<Handle>),
    Insert { p: Handle, parent: &'a String, index: Option<usize>, html: &'a String },
}
