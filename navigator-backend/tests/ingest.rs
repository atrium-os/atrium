//! ★ A recording is UNTRUSTED, including our own. It arrives from a
//! content-addressed store that vouches for the bytes being what someone
//! published, not for their meaning. Most of these tests are therefore about
//! refusing things, and the happy path is one of them rather than the point.

use navigator_backend::{ingest, Effect, Limits, Note, Reject, Tier, FORMAT};

fn rec(body: &str) -> String {
    format!(r##"{{"format":"{FORMAT}","url":"https://example.test/p","tier":2,
      "tier_reason":"conversion retained the content",
      "measurements":{{"elements":10,"text_before":100,"text_after":120,
        "scripts_total":2,"scripts_failed":0,"interactive_found":1,
        "transitions_dropped":0}},
      {body},
      "document":"<html><body><p>hi</p></body></html>"}}"##)
}

#[test]
fn a_real_recording_is_ingested() {
    let json = rec(r##""transitions":[{"trigger":"#menu","event":"click","anchored":true,
        "attribute_only":true,
        "effects":[{"kind":"attribute","target":"#nav","name":"class",
                    "from":"nav hidden","to":"nav"}]}]"##);
    let r = ingest(json.as_bytes(), &Limits::default()).expect("should ingest");
    assert_eq!(r.tier, Tier::Two);
    assert_eq!(r.url, "https://example.test/p");
    assert_eq!(r.transitions.len(), 1);
    assert!(r.transitions[0].is_attribute_only());
    assert_eq!(r.measurements.text_after, 120);
    assert!(r.notes.is_empty(), "{:?}", r.notes);
}

/// ★★ A DERIVED FIELD IN UNTRUSTED INPUT IS A CLAIM, NOT A FACT. A consumer
/// that believed `attribute_only` could be told a transition carries no
/// content while it carries an Insert, and size its work accordingly.
#[test]
fn a_lying_derived_field_is_recomputed_and_reported() {
    let json = rec(r##""transitions":[{"trigger":"#t","event":"click","anchored":true,
        "attribute_only":true,
        "effects":[{"kind":"insert","parent":"#p","html":"<p>smuggled</p>"}]}]"##);
    let r = ingest(json.as_bytes(), &Limits::default()).expect("still usable");
    assert!(!r.transitions[0].is_attribute_only(), "the EFFECTS decide");
    assert!(matches!(r.notes.first(),
        Some(Note::DerivedFieldDisagreed { claimed: true, actual: false, .. })),
        "the disagreement must be reported, not swallowed: {:?}", r.notes);
}

#[test]
fn a_foreign_format_is_refused() {
    let json = r##"{"format":"something-else/9","tier":1,"document":"<html></html>"}"##;
    assert!(matches!(ingest(json.as_bytes(), &Limits::default()),
        Err(Reject::WrongFormat { .. })));
}

#[test]
fn a_recording_without_a_document_is_not_a_recording() {
    let json = format!(r##"{{"format":"{FORMAT}","tier":1}}"##);
    assert_eq!(ingest(json.as_bytes(), &Limits::default()),
               Err(Reject::MissingField("document")));
}

/// ★ THE SIZE CHECK COMES BEFORE THE PARSE. Handing megabytes of adversarial
/// JSON to a parser and *then* deciding it was too big has already done the
/// work the limit exists to prevent.
#[test]
fn an_oversized_recording_is_refused_without_parsing() {
    let limits = Limits { max_total_bytes: 512, ..Default::default() };
    // Deliberately malformed: if this is refused for SIZE rather than for
    // being unparseable, the size check ran first.
    let junk = format!("{{\"format\":\"{FORMAT}\",{}", "x".repeat(4096));
    match ingest(junk.as_bytes(), &limits) {
        Err(Reject::TooLarge { what, .. }) => assert_eq!(what, "recording"),
        other => panic!("expected a size refusal before a parse error, got {other:?}"),
    }
}

#[test]
fn an_oversized_document_is_refused() {
    // Below the fixture's 35-byte document — a limit ABOVE it would have
    // tested nothing and passed.
    let limits = Limits { max_document_bytes: 16, ..Default::default() };
    let json = rec(r##""transitions":[]"##);
    match ingest(json.as_bytes(), &limits) {
        Err(Reject::TooLarge { what, .. }) => assert_eq!(what, "document"),
        other => panic!("expected a document size refusal, got {other:?}"),
    }
}

/// Too many transitions TRUNCATE rather than refuse — the document is still
/// usable — but the caller is never silently handed a subset.
#[test]
fn too_many_transitions_truncate_and_say_so() {
    let one = r##"{"trigger":"#a","event":"click","anchored":true,"effects":[]}"##;
    let many: Vec<&str> = std::iter::repeat(one).take(50).collect();
    let json = rec(&format!(r##""transitions":[{}]"##, many.join(",")));
    let limits = Limits { max_transitions: 10, ..Default::default() };
    let r = ingest(json.as_bytes(), &limits).expect("usable");
    assert_eq!(r.transitions.len(), 10);
    assert!(matches!(r.notes.first(),
        Some(Note::TransitionsTruncated { kept: 10, discarded: 40 })), "{:?}", r.notes);
}

/// ★ An unknown effect kind is SKIPPED, not fatal: a recording from a later
/// version may carry effects this one does not implement, and refusing a
/// whole document over one unknown entry makes the format unable to grow.
#[test]
fn an_unknown_effect_kind_is_skipped_not_fatal() {
    let json = rec(r##""transitions":[{"trigger":"#t","event":"click","anchored":true,
        "effects":[{"kind":"teleport","to":"mars"},
                   {"kind":"remove","target":"#gone"}]}]"##);
    let r = ingest(json.as_bytes(), &Limits::default()).expect("must still ingest");
    assert_eq!(r.transitions[0].effects, vec![Effect::Remove { target: "#gone".into() }]);
    assert!(matches!(r.notes.first(), Some(Note::UnknownEffectKind { .. })), "{:?}", r.notes);
}

/// ★ Malformed input must REFUSE, never panic — the whole point of a
/// validator is that it is the thing that survives bad input.
#[test]
fn hostile_and_malformed_inputs_refuse_without_panicking() {
    let limits = Limits::default();
    let cases: Vec<Vec<u8>> = vec![
        b"".to_vec(),
        b"null".to_vec(),
        b"[]".to_vec(),
        b"{".to_vec(),
        b"\xff\xfe\x00\x01".to_vec(),                       // not UTF-8
        format!(r##"{{"format":"{FORMAT}","tier":"two","document":"x"}}"##).into_bytes(),
        format!(r##"{{"format":"{FORMAT}","tier":9,"document":"x"}}"##).into_bytes(),
        format!(r##"{{"format":"{FORMAT}","tier":1,"document":123}}"##).into_bytes(),
        format!(r##"{{"format":"{FORMAT}","tier":1,"document":"x","transitions":"no"}}"##).into_bytes(),
        format!(r##"{{"format":"{FORMAT}","tier":1,"document":"x","transitions":[1,2]}}"##).into_bytes(),
        format!(r##"{{"format":"{FORMAT}","tier":1,"document":"x","url":{{"a":1}}}}"##).into_bytes(),
        // deep nesting, the classic parser bomb
        format!(r##"{{"format":"{FORMAT}","tier":1,"document":"x","transitions":{}}}"##,
                "[".repeat(2000)).into_bytes(),
    ];
    for c in cases {
        let got = ingest(&c, &limits);
        assert!(got.is_err(), "should have refused: {:?}", String::from_utf8_lossy(&c));
    }
}

/// A transition entry missing its trigger is refused: without one there is
/// nothing to replay against, so it is not a transition.
#[test]
fn a_transition_without_a_trigger_is_refused() {
    let json = rec(r##""transitions":[{"event":"click","effects":[]}]"##);
    assert_eq!(ingest(json.as_bytes(), &Limits::default()),
               Err(Reject::MissingField("trigger")));
}
