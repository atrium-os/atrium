//! ★ GOING BACK MUST REACH THE STATE THE READER WAS ACTUALLY IN.
//!
//! The strong form of that claim is byte equality: apply a transition, undo
//! it, and the document must serialize to exactly what it did before. Weaker
//! assertions — "the attribute is false again" — pass on a document that has
//! also quietly gained or lost something else, which is the failure mode an
//! undo path actually has.
//!
//! ★★ And reversibility is NOT uniform. Attribute effects record both sides
//! and invert exactly; Remove and Insert do not carry what would be needed,
//! and the tests below pin that they REFUSE rather than approximate. A
//! plausible-looking inverse for a Remove would have to invent a subtree, and
//! the reader would never know the difference.

use navigator_backend::apply::ApplyError;
use navigator_backend::document::Document;
use navigator_backend::reverse::{History, NotInvertible};
use navigator_backend::{Effect, Transition};

fn doc() -> Document {
    Document::parse(
        r#"<html><body><button id="t">Menu</button><nav id="m" aria-expanded="false" class="shut"><span id="old">x</span></nav></body></html>"#,
    )
}

fn t(effects: Vec<Effect>) -> Transition {
    Transition { trigger: "#t".into(), effects, anchored: true, event: "click".into() }
}

fn attr(name: &str, from: Option<&str>, to: Option<&str>) -> Effect {
    Effect::Attribute {
        target: "#m".into(), name: name.into(),
        from: from.map(str::to_string), to: to.map(str::to_string),
    }
}

/// The round trip, asserted on bytes.
#[test]
fn applying_then_undoing_restores_the_document_exactly() {
    let mut d = doc();
    let before = d.dom.serialize();
    let step = t(vec![attr("aria-expanded", Some("false"), Some("true"))]);
    d.apply(&step).expect("applies");
    assert_ne!(d.dom.serialize(), before, "the step must actually change something");
    d.apply(&step.inverse().expect("invertible")).expect("undoes");
    assert_eq!(d.dom.serialize(), before, "undo did not restore the document");
}

/// Adding an attribute inverts to removing it, and the other way round — the
/// `None` cases are where an inverse most easily becomes an empty string.
#[test]
fn adding_and_removing_an_attribute_invert_each_other() {
    let mut d = doc();
    let before = d.dom.serialize();
    let add = t(vec![attr("data-open", None, Some("1"))]);
    d.apply(&add).expect("applies");
    let m = d.dom.by_id("m").unwrap();
    assert_eq!(d.dom.attr(m, "data-open"), Some("1"));
    d.apply(&add.inverse().expect("invertible")).expect("undoes");
    let m = d.dom.by_id("m").unwrap();
    assert_eq!(d.dom.attr(m, "data-open"), None, "inverse left an empty attribute");
    assert_eq!(d.dom.serialize(), before);
}

/// ★ The inverse is checked by the same precondition machinery. Undoing a
/// step that was never taken must be refused, not silently applied.
#[test]
fn an_inverse_applied_to_the_wrong_state_is_refused() {
    let mut d = doc();
    let step = t(vec![attr("aria-expanded", Some("false"), Some("true"))]);
    let inv = step.inverse().expect("invertible");
    // Never applied `step`, so the document is still "false" and the
    // inverse's precondition ("true") cannot hold.
    let e = d.apply(&inv).expect_err("must be refused");
    assert!(matches!(e, ApplyError::PreconditionFailed { .. }), "{e}");
}

/// Multi-effect transitions come back in reverse order.
#[test]
fn a_multi_effect_transition_inverts_in_reverse_order() {
    let step = t(vec![
        attr("aria-expanded", Some("false"), Some("true")),
        attr("class", Some("shut"), Some("open")),
    ]);
    let inv = step.inverse().expect("invertible");
    match (&inv.effects[0], &inv.effects[1]) {
        (Effect::Attribute { name: a, .. }, Effect::Attribute { name: b, .. }) => {
            assert_eq!(a, "class", "inverse must undo the last effect first");
            assert_eq!(b, "aria-expanded");
        }
        other => panic!("wrong shape: {other:?}"),
    }
    let mut d = doc();
    let before = d.dom.serialize();
    d.apply(&step).expect("applies");
    d.apply(&inv).expect("undoes");
    assert_eq!(d.dom.serialize(), before);
}

/// ★★ WHAT CANNOT BE INVERTED SAYS SO. The recording carries a path for a
/// Remove and nothing else, so restoring it would mean inventing a subtree.
#[test]
fn a_remove_has_no_inverse_and_does_not_pretend_to() {
    let step = t(vec![Effect::Remove { target: "#old".into() }]);
    let e = step.inverse().expect_err("must refuse");
    assert!(matches!(e, NotInvertible::RemovedContentNotRecorded { .. }), "{e}");
    assert!(!step.is_invertible());
}

/// And an Insert, because the recording does not say where under the parent
/// the markup went — replay appends, so there is nothing to identify.
#[test]
fn an_insert_has_no_inverse_either() {
    let step = t(vec![Effect::Insert { parent: "#m".into(), html: "<b>x</b>".into() }]);
    let e = step.inverse().expect_err("must refuse");
    assert!(matches!(e, NotInvertible::InsertExtentNotRecorded { .. }), "{e}");
}

/// A transition that mixes an invertible effect with one that is not has no
/// inverse at all — a partial inverse would undo half a step.
#[test]
fn a_mixed_transition_has_no_partial_inverse() {
    let step = t(vec![
        attr("aria-expanded", Some("false"), Some("true")),
        Effect::Remove { target: "#old".into() },
    ]);
    assert!(step.inverse().is_err(), "a partial inverse is worse than none");
}

// ---- History ----------------------------------------------------------

/// History `back()` restores exactly, through an invertible step.
#[test]
fn history_goes_back_through_an_invertible_step() {
    let mut h = History::new(doc());
    let before = h.document().dom.serialize();
    h.go(&t(vec![attr("aria-expanded", Some("false"), Some("true"))])).expect("goes");
    assert_eq!(h.depth(), 1);
    assert!(h.back().expect("comes back"));
    assert_eq!(h.document().dom.serialize(), before);
    assert_eq!(h.depth(), 0);
    assert_eq!(h.cost().snapshots, 0, "an invertible step must not cost a snapshot");
}

/// ★ And through one that is NOT invertible, by snapshot — the reader gets
/// the same guarantee, the session just pays for it.
#[test]
fn history_goes_back_through_a_removal_by_snapshot() {
    let mut h = History::new(doc());
    let before = h.document().dom.serialize();
    h.go(&t(vec![Effect::Remove { target: "#old".into() }])).expect("goes");
    assert!(h.document().dom.by_id("old").is_none(), "the step must have happened");
    assert_eq!(h.cost().snapshots, 1, "a non-invertible step must cost one");
    assert!(h.back().expect("comes back"));
    assert_eq!(h.document().dom.serialize(), before, "snapshot did not restore exactly");
}

/// Several steps, forward and all the way back.
#[test]
fn history_unwinds_a_mixed_sequence_to_the_start() {
    let mut h = History::new(doc());
    let start = h.document().dom.serialize();
    h.go(&t(vec![attr("aria-expanded", Some("false"), Some("true"))])).expect("1");
    h.go(&t(vec![Effect::Insert { parent: "#m".into(), html: "<b>new</b>".into() }])).expect("2");
    h.go(&t(vec![attr("class", Some("shut"), Some("open"))])).expect("3");
    let c = h.cost();
    assert_eq!((c.steps, c.inverses, c.snapshots), (3, 2, 1),
        "only the insert should have cost a snapshot");

    while h.back().expect("unwinds") {}
    assert_eq!(h.document().dom.serialize(), start);
    assert_eq!(h.depth(), 0);
}

/// Back at the start is not an error — it is the reader being at the start.
#[test]
fn back_at_the_beginning_is_false_not_an_error() {
    let mut h = History::new(doc());
    assert!(!h.back().expect("must not be an error"));
}

/// ★ A REFUSED STEP MUST NOT GROW THE HISTORY. A history that recorded a step
/// that did not happen would send the reader back to a state they were never
/// in — the same all-or-nothing rule apply follows.
#[test]
fn a_refused_step_leaves_the_history_untouched() {
    let mut h = History::new(doc());
    let before = h.document().dom.serialize();
    let e = h.go(&t(vec![attr("aria-expanded", Some("wrong"), Some("true"))]))
        .expect_err("must be refused");
    assert!(matches!(e, ApplyError::PreconditionFailed { .. }), "{e}");
    assert_eq!(h.depth(), 0, "a refused step was recorded");
    assert_eq!(h.document().dom.serialize(), before);
}
