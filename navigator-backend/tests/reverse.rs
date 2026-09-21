//! ★ GOING BACK MUST REACH THE STATE THE READER WAS ACTUALLY IN.
//!
//! The strong form is byte equality: take a step, undo it, and the document
//! must serialize to exactly what it did before. Weaker assertions — "the
//! attribute is false again" — pass on a document that has also quietly
//! gained or lost something else, which is the failure an undo path has.
//!
//! ★★ EVERY transition is reversible, including removals, because the undo is
//! derived from the document while the effect is applied rather than read out
//! of the recording. The subtree a removal destroys is in the document at the
//! moment it is destroyed; that is where it comes from. Tests that once
//! asserted a removal COULD NOT be inverted are gone, and the ones below take
//! their place — the earlier design had `History` snapshotting a whole
//! document for those steps.

use navigator_backend::apply::ApplyError;
use navigator_backend::document::Document;
use navigator_backend::reverse::{Back, History, HistoryLimits};
use navigator_backend::{Effect, Transition};

fn doc() -> Document {
    Document::parse(
        r#"<html><body><button id="t">Menu</button><nav id="m" aria-expanded="false" class="shut"><span id="a">A</span><span id="b">B</span><span id="c">C</span></nav></body></html>"#,
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

/// Apply, undo, compare bytes. The shape of every test here.
fn round_trip(effects: Vec<Effect>) {
    let d = doc();
    let before = d.dom.serialize();
    let step = t(effects);
    let (after, undo) = d.applied_with_undo(&step).expect("applies");
    assert_ne!(after.dom.serialize(), before, "the step must actually change something");
    let back = after.undone(&undo).expect("undoes");
    assert_eq!(back.dom.serialize(), before, "undo did not restore the document");
}

#[test]
fn an_attribute_change_reverses() {
    round_trip(vec![attr("aria-expanded", Some("false"), Some("true"))]);
}

#[test]
fn adding_an_attribute_reverses_to_removing_it() {
    round_trip(vec![attr("data-open", None, Some("1"))]);
}

#[test]
fn removing_an_attribute_reverses_to_adding_it() {
    round_trip(vec![attr("class", Some("shut"), None)]);
}

/// ★★ THE CASE THE OLD DESIGN COULD NOT DO. The removed subtree — its
/// markup, its parent, its position among siblings — is read from the
/// document as it is being removed.
#[test]
fn a_removal_reverses_exactly() {
    round_trip(vec![Effect::Remove { target: "#b".into() }]);
}

/// And it goes back to the same POSITION, not just back into the parent.
/// Restoring `B` after `C` would satisfy any test that only asked whether the
/// node was present.
#[test]
fn a_removal_reverses_to_the_same_position() {
    let d = doc();
    let before = d.dom.serialize();
    let (after, undo) = d.applied_with_undo(&t(vec![Effect::Remove { target: "#b".into() }]))
        .expect("applies");
    assert!(after.dom.by_id("b").is_none(), "the removal must have happened");
    let back = after.undone(&undo).expect("undoes");
    assert_eq!(back.dom.serialize(), before, "B came back in the wrong place");
}

#[test]
fn an_insert_reverses() {
    round_trip(vec![Effect::Insert {
        parent: "#m".into(), index: Some(1), html: "<span id=\"n\">N</span>".into(),
    }]);
}

/// A fragment with several roots comes back out as a whole.
#[test]
fn a_multi_root_insert_reverses() {
    round_trip(vec![Effect::Insert {
        parent: "#m".into(), index: Some(0),
        html: "<span>1</span><span>2</span><span>3</span>".into(),
    }]);
}

/// A transition mixing all three kinds reverses as one step.
#[test]
fn a_mixed_transition_reverses() {
    round_trip(vec![
        attr("aria-expanded", Some("false"), Some("true")),
        Effect::Remove { target: "#a".into() },
        Effect::Insert { parent: "#m".into(), index: Some(1), html: "<i>x</i>".into() },
    ]);
}

// ---- position ---------------------------------------------------------

/// ★ VERSION 2's REASON TO EXIST. An insert lands where the page put it, not
/// at the end. Under version 1 this document came back with `N` after `C`,
/// and nothing reported a difference.
#[test]
fn an_insert_lands_at_its_recorded_position() {
    let mut d = doc();
    d.apply(&t(vec![Effect::Insert {
        parent: "#m".into(), index: Some(1), html: "<span id=\"n\">N</span>".into(),
    }])).expect("applies");
    let ids: Vec<String> = d.dom.children_of(d.dom.by_id("m").unwrap()).iter()
        .filter_map(|&h| d.dom.attr(h, "id").map(str::to_string)).collect();
    assert_eq!(ids, vec!["a", "n", "b", "c"], "inserted in the wrong place");
}

/// ★ A version 1 recording carries no position, and appends — the older
/// behaviour, reached honestly because the field is absent rather than
/// defaulted to a plausible zero.
#[test]
fn an_insert_without_a_position_appends() {
    let mut d = doc();
    d.apply(&t(vec![Effect::Insert {
        parent: "#m".into(), index: None, html: "<span id=\"n\">N</span>".into(),
    }])).expect("applies");
    let ids: Vec<String> = d.dom.children_of(d.dom.by_id("m").unwrap()).iter()
        .filter_map(|&h| d.dom.attr(h, "id").map(str::to_string)).collect();
    assert_eq!(ids, vec!["a", "b", "c", "n"]);
}

/// An index past the end is clamped rather than refused: it degrades to the
/// version 1 behaviour instead of losing the content entirely.
#[test]
fn an_out_of_range_position_clamps_instead_of_failing() {
    let mut d = doc();
    d.apply(&t(vec![Effect::Insert {
        parent: "#m".into(), index: Some(999), html: "<span id=\"n\">N</span>".into(),
    }])).expect("must not fail");
    let ids: Vec<String> = d.dom.children_of(d.dom.by_id("m").unwrap()).iter()
        .filter_map(|&h| d.dom.attr(h, "id").map(str::to_string)).collect();
    assert_eq!(ids, vec!["a", "b", "c", "n"]);
}

/// `remove-range` is a first-class effect, so an undo is an ordinary
/// transition that could be written down and read back.
#[test]
fn remove_range_takes_out_exactly_its_range() {
    let mut d = doc();
    d.apply(&t(vec![Effect::RemoveRange {
        parent: "#m".into(), index: 1, count: 2,
    }])).expect("applies");
    let ids: Vec<String> = d.dom.children_of(d.dom.by_id("m").unwrap()).iter()
        .filter_map(|&h| d.dom.attr(h, "id").map(str::to_string)).collect();
    assert_eq!(ids, vec!["a"], "wrong range removed");
}

#[test]
fn a_remove_range_past_the_end_is_refused() {
    let mut d = doc();
    let before = d.dom.serialize();
    let e = d.apply(&t(vec![Effect::RemoveRange { parent: "#m".into(), index: 2, count: 5 }]))
        .expect_err("must be refused");
    assert!(matches!(e, ApplyError::TargetUnresolved { .. }), "{e}");
    assert_eq!(d.dom.serialize(), before);
}

// ---- History ----------------------------------------------------------

#[test]
fn history_goes_back_through_any_step() {
    let mut h = History::new(doc());
    let start = h.document().dom.serialize();
    h.go(&t(vec![attr("aria-expanded", Some("false"), Some("true"))])).expect("1");
    h.go(&t(vec![Effect::Remove { target: "#b".into() }])).expect("2");
    h.go(&t(vec![Effect::Insert { parent: "#m".into(), index: Some(0), html: "<i>x</i>".into() }])).expect("3");
    assert_eq!(h.depth(), 3);

    while h.back().expect("unwinds") == Back::Stepped {}
    assert_eq!(h.document().dom.serialize(), start, "the unwind did not reach the start");
    assert_eq!(h.depth(), 0);
}

/// ★ The undo carries only what it must: the markup a removal destroyed. It
/// is not a snapshot of the document, which is what the earlier design paid
/// for these steps.
#[test]
fn an_undo_costs_the_removed_markup_not_the_document() {
    let mut h = History::new(doc());
    let document_size = h.document().dom.serialize().len();
    h.go(&t(vec![Effect::Remove { target: "#b".into() }])).expect("goes");
    let cost = h.cost();
    assert_eq!(cost.steps, 1);
    assert!(cost.undo_bytes > 0, "the removed markup has to be kept somewhere");
    assert!(cost.undo_bytes < document_size / 2,
        "undo cost {} approaches a snapshot of {document_size}", cost.undo_bytes);
}

/// ★★ A step that removes its OWN trigger must still be undoable. The trigger
/// check asks whether a transition was recorded against this document, which
/// is already answered for an undo built from it — and a tab control replaced
/// by the panel it opens is a real shape.
#[test]
fn a_step_that_removes_its_trigger_can_still_be_undone() {
    let mut h = History::new(doc());
    let start = h.document().dom.serialize();
    h.go(&t(vec![Effect::Remove { target: "#t".into() }])).expect("goes");
    assert!(h.document().dom.by_id("t").is_none(), "the trigger must be gone");
    assert_eq!(h.back().expect("must still be undoable"), Back::Stepped);
    assert_eq!(h.document().dom.serialize(), start);
}

#[test]
fn back_at_the_beginning_says_so_and_is_not_an_error() {
    let mut h = History::new(doc());
    assert_eq!(h.back().expect("must not be an error"), Back::AtStart);
}

// ---- bounds -----------------------------------------------------------

/// ★★ THE BOUND FORGETS, AND SAYS THAT IT FORGOT. A reader who runs out of
/// history because of the limit has NOT reached the beginning of the
/// document, and a boolean return would have told them they had.
#[test]
fn a_trimmed_history_reports_forgotten_not_at_start() {
    let limits = HistoryLimits { max_steps: 2, max_undo_bytes: 1 << 20 };
    let mut h = History::with_limits(doc(), limits);
    for v in ["1", "2", "3", "4"] {
        let prev = h.document().dom.attr(h.document().dom.by_id("m").unwrap(), "data-n")
            .map(str::to_string);
        h.go(&t(vec![Effect::Attribute {
            target: "#m".into(), name: "data-n".into(),
            from: prev, to: Some(v.into()),
        }])).expect("goes");
    }
    assert_eq!(h.depth(), 2, "the bound must cap the history");
    assert_eq!(h.forgotten(), 2, "and count what it dropped");

    assert_eq!(h.back().expect("1"), Back::Stepped);
    assert_eq!(h.back().expect("2"), Back::Stepped);
    // Two states remain unreachable, and that is what the reader is told.
    assert_eq!(h.back().expect("3"), Back::Forgotten);
}

/// ★ A step whose undo is too big to keep still HAPPENS. Letting a page's own
/// content decide whether a reader may turn the page would be a far worse
/// failure than a shortened history.
#[test]
fn a_step_too_large_to_remember_is_still_taken() {
    let limits = HistoryLimits { max_steps: 64, max_undo_bytes: 4 };
    let mut h = History::with_limits(doc(), limits);
    h.go(&t(vec![Effect::Remove { target: "#b".into() }])).expect("the step must happen");
    assert!(h.document().dom.by_id("b").is_none(), "the removal did not happen");
    assert_eq!(h.depth(), 0, "its undo was too large to keep");
    assert_eq!(h.forgotten(), 1);
    assert_eq!(h.back().expect("asks"), Back::Forgotten, "and the reader is told");
}

/// The byte bound drops oldest-first, like the step bound.
#[test]
fn the_byte_bound_drops_the_oldest_steps() {
    // Each attribute undo here is a handful of bytes; allow about two.
    let limits = HistoryLimits { max_steps: 64, max_undo_bytes: 8 };
    let mut h = History::with_limits(doc(), limits);
    for v in ["aaaa", "bbbb", "cccc"] {
        let prev = h.document().dom.attr(h.document().dom.by_id("m").unwrap(), "data-n")
            .map(str::to_string);
        h.go(&t(vec![Effect::Attribute {
            target: "#m".into(), name: "data-n".into(),
            from: prev, to: Some(v.into()),
        }])).expect("goes");
    }
    assert!(h.depth() < 3, "the byte bound did not engage: {:?}", h.cost());
    assert!(h.cost().undo_bytes <= 8, "over budget: {:?}", h.cost());
    assert!(h.forgotten() > 0);
}

/// ★ And the counterweight: under the default bound, a realistic session is
/// never trimmed. A limit that engaged on ordinary use would be a bug, and
/// the measured p99 undo is 2,193 bytes against a 1 MiB budget.
#[test]
fn an_ordinary_session_is_never_trimmed() {
    let mut h = History::new(doc());
    for i in 0..60 {
        let prev = h.document().dom.attr(h.document().dom.by_id("m").unwrap(), "data-n")
            .map(str::to_string);
        h.go(&t(vec![Effect::Attribute {
            target: "#m".into(), name: "data-n".into(),
            from: prev, to: Some(i.to_string()),
        }])).expect("goes");
    }
    assert_eq!(h.forgotten(), 0, "the default bound trimmed an ordinary session");
    assert_eq!(h.depth(), 60);
    while h.back().expect("unwinds") == Back::Stepped {}
    assert_eq!(h.back().expect("at start"), Back::AtStart);
}

/// A refused step must not grow the history, or "back" would reach a state
/// the reader was never in.
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
