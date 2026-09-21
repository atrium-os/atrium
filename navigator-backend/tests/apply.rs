//! ★ APPLYING A TRANSITION IS WHERE AN UNTRUSTED RECORDING TOUCHES THE
//! DOCUMENT A READER WILL SEE.
//!
//! Everything before this point is inspection: ingest says the JSON is a
//! recording, the profile says the document is bounded, resolve says a path
//! names a node. Apply is the first operation that CHANGES anything, so it is
//! the one that has to refuse.
//!
//! Two properties are tested here more than the happy path:
//!
//!   - Atomicity. A refused transition must leave the document byte-identical.
//!     Serializing before and after is the check, because "no effects landed"
//!     is exactly the claim a partial application would falsely satisfy.
//!   - Preconditions. Every attribute effect says what it replaces; a document
//!     that does not hold that value is not the document this was recorded
//!     against, and proceeding would silently produce a state no one observed.

use navigator_backend::apply::ApplyError;
use navigator_backend::document::Document;
use navigator_backend::{Effect, Transition};

fn doc() -> Document {
    Document::parse(
        r#"<html><body><button id="t">Menu</button><nav id="m" aria-expanded="false"><span id="old">x</span></nav></body></html>"#,
    )
}

fn transition(effects: Vec<Effect>) -> Transition {
    Transition { trigger: "#t".into(), effects, anchored: true, event: "click".into() }
}

#[test]
fn an_attribute_effect_applies() {
    let mut d = doc();
    d.apply(&transition(vec![Effect::Attribute {
        target: "#m".into(), name: "aria-expanded".into(),
        from: Some("false".into()), to: Some("true".into()),
    }])).expect("must apply");
    let m = d.dom.by_id("m").unwrap();
    assert_eq!(d.dom.attr(m, "aria-expanded"), Some("true"));
}

/// `to: None` is a removal, not an empty string — the two render differently
/// and CSS distinguishes them.
#[test]
fn an_attribute_effect_with_no_new_value_removes_it() {
    let mut d = doc();
    d.apply(&transition(vec![Effect::Attribute {
        target: "#m".into(), name: "aria-expanded".into(),
        from: Some("false".into()), to: None,
    }])).expect("must apply");
    let m = d.dom.by_id("m").unwrap();
    assert_eq!(d.dom.attr(m, "aria-expanded"), None);
}

#[test]
fn an_insert_lands_under_its_parent() {
    let mut d = doc();
    d.apply(&transition(vec![Effect::Insert {
        parent: "#m".into(), html: "<a href=\"/x\">New</a>".into(),
    }])).expect("must apply");
    let out = d.dom.serialize();
    assert!(out.contains("<a href=\"/x\">New</a>"), "{out}");
}

#[test]
fn a_remove_detaches_the_node() {
    let mut d = doc();
    d.apply(&transition(vec![Effect::Remove { target: "#old".into() }])).expect("must apply");
    assert!(d.dom.by_id("old").is_none(), "{}", d.dom.serialize());
}

/// ★ THE PRECONDITION. A recording describing a different starting state is
/// refused, and the refusal carries both values so the caller can tell a
/// stale recording from a tampered one.
#[test]
fn an_attribute_effect_whose_precondition_fails_is_refused() {
    let mut d = doc();
    let e = d.apply(&transition(vec![Effect::Attribute {
        target: "#m".into(), name: "aria-expanded".into(),
        from: Some("true".into()), // the document says "false"
        to: Some("false".into()),
    }])).expect_err("must be refused");
    match e {
        ApplyError::PreconditionFailed { expected, found, .. } => {
            assert_eq!(expected, Some("true".into()));
            assert_eq!(found, Some("false".into()));
        }
        other => panic!("wrong error: {other}"),
    }
}

/// ★★ ATOMICITY. The first effect is valid and the second is not; nothing may
/// land. Compared by serialization, because an assertion that only checks the
/// second effect's absence would pass on a half-applied document.
#[test]
fn a_transition_that_fails_part_way_changes_nothing() {
    let mut d = doc();
    let before = d.dom.serialize();
    let err = d.apply(&transition(vec![
        Effect::Attribute {
            target: "#m".into(), name: "aria-expanded".into(),
            from: Some("false".into()), to: Some("true".into()),
        },
        Effect::Remove { target: "#does-not-exist".into() },
    ])).expect_err("must be refused");
    assert!(matches!(err, ApplyError::TargetUnresolved { .. }), "{err}");
    assert_eq!(d.dom.serialize(), before, "a refused transition changed the document");
}

/// And the same when the failure is a precondition rather than a missing node.
#[test]
fn a_failed_precondition_also_leaves_the_document_untouched() {
    let mut d = doc();
    let before = d.dom.serialize();
    let _ = d.apply(&transition(vec![
        Effect::Insert { parent: "#m".into(), html: "<b>first</b>".into() },
        Effect::Attribute {
            target: "#m".into(), name: "aria-expanded".into(),
            from: Some("wrong".into()), to: Some("true".into()),
        },
    ])).expect_err("must be refused");
    assert_eq!(d.dom.serialize(), before);
    assert!(!d.dom.serialize().contains("first"), "the earlier insert leaked");
}

/// A transition whose trigger is absent was not recorded against this
/// document, and that is reported as itself rather than as whatever its
/// effects happen to fail on.
#[test]
fn a_transition_whose_trigger_is_absent_is_refused_as_such() {
    let mut d = doc();
    let t = Transition {
        trigger: "#nowhere".into(),
        effects: vec![Effect::Remove { target: "#old".into() }],
        anchored: true, event: "click".into(),
    };
    let e = d.apply(&t).expect_err("must be refused");
    assert!(matches!(e, ApplyError::TriggerUnresolved { .. }), "{e}");
    assert!(d.dom.by_id("old").is_some(), "the effect ran despite the bad trigger");
}

/// ★ A recording that already knows it dropped effects cannot be replayed
/// into the state it names, so it is refused rather than applied in part.
#[test]
fn an_incomplete_transition_is_refused() {
    let mut d = doc();
    let before = d.dom.serialize();
    let e = d.apply(&transition(vec![
        Effect::Attribute {
            target: "#m".into(), name: "aria-expanded".into(),
            from: Some("false".into()), to: Some("true".into()),
        },
        Effect::Truncated { dropped: 12 },
    ])).expect_err("must be refused");
    assert!(matches!(e, ApplyError::Incomplete { dropped: 12 }), "{e}");
    assert_eq!(d.dom.serialize(), before);
}

/// ★ THE PROFILE IS RE-CHECKED ON THE RESULT. An insert small enough to pass
/// every per-effect limit can still leave a document outside the profile, so
/// the ceiling is applied to what the document BECOMES.
#[test]
fn an_insert_that_pushes_the_document_out_of_the_profile_is_refused() {
    let mut d = doc();
    let before = d.dom.serialize();
    let depth = navigator_dom::profile::MAX_DEPTH + 10;
    let html = format!("{}{}", "<div>".repeat(depth), "</div>".repeat(depth));
    let e = d.apply(&transition(vec![Effect::Insert { parent: "#m".into(), html }]))
        .expect_err("must be refused");
    assert!(matches!(e, ApplyError::OutsideProfile(_)), "{e}");
    assert_eq!(d.dom.serialize(), before);
}

/// `applied` answers "would this work" without doing it — what a reader UI
/// needs before it offers a control.
#[test]
fn applied_does_not_touch_the_original() {
    let d = doc();
    let before = d.dom.serialize();
    let next = d.applied(&transition(vec![Effect::Attribute {
        target: "#m".into(), name: "aria-expanded".into(),
        from: Some("false".into()), to: Some("true".into()),
    }])).expect("applicable");
    assert_eq!(d.dom.serialize(), before, "applied() mutated its receiver");
    assert_ne!(next.dom.serialize(), before, "and it must actually differ");
}

/// Positional triggers work too — most recorded triggers are paths, not ids,
/// and a test suite that only used `#id` would leave the common case unproven.
#[test]
fn positional_paths_resolve_as_effect_targets() {
    let mut d = Document::parse("<html><body><ul><li>a</li><li id=\"x\">b</li></ul></body></html>");
    let t = Transition {
        trigger: "html:0>body:0>ul:0>li:1".into(),
        effects: vec![Effect::Attribute {
            target: "html:0>body:0>ul:0>li:1".into(), name: "data-on".into(),
            from: None, to: Some("1".into()),
        }],
        anchored: true, event: "click".into(),
    };
    d.apply(&t).expect("must apply");
    let x = d.dom.by_id("x").unwrap();
    assert_eq!(d.dom.attr(x, "data-on"), Some("1"));
}
