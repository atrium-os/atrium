//! `/4` recordings: templates (legacy-web §5.4.5.6c) are EXPANDED AT INGEST
//! into the ordinary transitions they stand for, so replay, inversion and
//! sessions see one vocabulary.

use navigator_backend::session::Sessions;
use navigator_backend::{ingest, Effect, Limits, Note, Reject, FORMAT_V3, FORMAT_V4};

/// Written by navigator-prerender (exploring) for three tabs, each showing
/// the panel at its own index: one template, `$t` and one slot.
const TABS: &str = r##"{"format": "atrium-navigator-recording/4", "url": "", "tier": 2, "tier_reason": "too little text to judge; nothing to protect", "measurements": {"elements": 15, "text_before": 6, "text_after": 6, "scripts_total": 1, "scripts_failed": 0, "interactive_found": 3, "transitions_dropped": 0}, "transitions": [], "templates": [{"event": "click", "attribute_only": true, "effects": [{"kind": "attribute", "target": "$t", "name": "aria-selected", "from": null, "to": "true"}, {"kind": "attribute", "target": "$0", "name": "class", "from": "panel", "to": "panel shown"}], "instances": [{"trigger": "html:0>body:0>ul:0>li:0>a:0", "anchored": true, "slots": ["html:0>body:0>div:0>div:0"]}, {"trigger": "html:0>body:0>ul:0>li:1>a:0", "anchored": true, "slots": ["html:0>body:0>div:0>div:1"]}, {"trigger": "html:0>body:0>ul:0>li:2>a:0", "anchored": true, "slots": ["html:0>body:0>div:0>div:2"]}]}], "document": "<html><head></head><body><ul><li><a href=\"#\">a</a></li><li><a href=\"#\">b</a></li><li><a href=\"#\">c</a></li></ul><div><div class=\"panel\">1</div><div class=\"panel\">2</div><div class=\"panel\">3</div></div></body></html>"}"##;

fn with_templates(format: &str, templates: &str) -> String {
    format!(r##"{{"format":"{format}","tier":2,"transitions":[
        {{"trigger":"#x","event":"click","anchored":true,"effects":[]}}],
        "templates":{templates},"document":"<html><body><p id=x>x</p></body></html>"}}"##)
}

#[test]
fn a_template_expands_into_its_instances() {
    let r = ingest(TABS.as_bytes(), &Limits::default()).expect("ingests");
    assert!(r.notes.is_empty(), "{:?}", r.notes);
    assert_eq!(r.transitions.len(), 3);
    let t = &r.transitions[1];
    assert_eq!(t.trigger, "html:0>body:0>ul:0>li:1>a:0");
    assert_eq!(t.event, "click");
    assert_eq!(t.effects, vec![
        Effect::Attribute { target: "html:0>body:0>ul:0>li:1>a:0".into(), name: "aria-selected".into(),
            from: None, to: Some("true".into()), at: None },
        Effect::Attribute { target: "html:0>body:0>div:0>div:1".into(), name: "class".into(),
            from: Some("panel".into()), to: Some("panel shown".into()), at: None },
    ]);
}

/// ★ And it replays down the ordinary path: tab 2 shows panel 2, nothing
/// else, and going back restores the document.
#[test]
fn an_expanded_instance_replays_and_reverts() {
    let r = ingest(TABS.as_bytes(), &Limits::default()).unwrap();
    let mut s = Sessions::default();
    let id = s.open(&r, 1).expect("opens");
    let before = s.get(id).unwrap().document().dom.serialize();
    let sess = s.get_mut(id, 1).unwrap();
    sess.navigate("html:0>body:0>ul:0>li:2>a:0").expect("the instance is a trigger");
    let after = sess.document().dom.serialize();
    assert!(after.contains("<div class=\"panel shown\">3</div>"), "{after}");
    assert!(after.contains("<div class=\"panel\">2</div>"), "only its own panel");
    sess.back().expect("reverts");
    assert_eq!(sess.document().dom.serialize(), before);
}

#[test]
fn a_v3_recording_with_templates_is_refused() {
    let j = with_templates(FORMAT_V3, "[]");
    assert_eq!(ingest(j.as_bytes(), &Limits::default()),
        Err(Reject::BadField { field: "templates", why: "only a /4 recording carries templates" }));
}

#[test]
fn a_bad_address_or_slot_count_is_refused() {
    let bad_addr = with_templates(FORMAT_V4, r##"[{"event":"click","effects":[
        {"kind":"remove","target":"$x"}],"instances":[{"trigger":"#x","anchored":true,"slots":[]}]}]"##);
    assert!(matches!(ingest(bad_addr.as_bytes(), &Limits::default()),
        Err(Reject::BadField { field: "effects", .. })));
    let short = with_templates(FORMAT_V4, r##"[{"event":"click","effects":[
        {"kind":"remove","target":"$1"}],"instances":[{"trigger":"#x","anchored":true,"slots":["#a"]}]}]"##);
    assert!(matches!(ingest(short.as_bytes(), &Limits::default()),
        Err(Reject::BadField { field: "slots", .. })));
    let huge = with_templates(FORMAT_V4, r##"[{"event":"click","effects":[
        {"kind":"remove","target":"$99999999999999999999"}],"instances":[]}]"##);
    assert!(ingest(huge.as_bytes(), &Limits::default()).is_err());
}

/// The allowed case beside the refusals: a template with no instances is
/// nothing, and is not an error.
#[test]
fn an_empty_template_expands_to_nothing() {
    let j = with_templates(FORMAT_V4, r##"[{"event":"click","effects":[],"instances":[]}]"##);
    let r = ingest(j.as_bytes(), &Limits::default()).unwrap();
    assert_eq!(r.transitions.len(), 1);
}

/// ★ A few bytes of template must not stand for more than a flat recording
/// could have said: instances past `max_transitions`, or past
/// `max_total_bytes` of expanded effects, are dropped WITH A NOTE.
#[test]
fn expansion_is_bounded_and_says_so() {
    let insts: Vec<String> = (0..10).map(|i| format!(r##"{{"trigger":"#t{i}","anchored":true,"slots":[]}}"##)).collect();
    let j = with_templates(FORMAT_V4, &format!(r##"[{{"event":"click","effects":[
        {{"kind":"attribute","target":"$t","name":"class","from":null,"to":"on"}}],"instances":[{}]}}]"##, insts.join(",")));
    let lim = Limits { max_transitions: 5, ..Limits::default() };
    let r = ingest(j.as_bytes(), &lim).unwrap();
    assert_eq!(r.transitions.len(), 5, "1 plain + 4 instances");
    assert_eq!(r.notes, vec![Note::TemplateInstancesTruncated { kept: 4, discarded: 6 }]);

    let big = "x".repeat(4000);
    let j = with_templates(FORMAT_V4, &format!(r##"[{{"event":"click","effects":[
        {{"kind":"attribute","target":"$t","name":"title","from":null,"to":"{big}"}}],"instances":[{}]}}]"##, insts.join(",")));
    let lim = Limits { max_total_bytes: j.len() + 10_000, ..Limits::default() };
    let r = ingest(j.as_bytes(), &lim).unwrap();
    let kept = r.transitions.len() - 1;
    assert!(kept < 10, "the expansion stops at the byte bound");
    assert!(matches!(r.notes.as_slice(), [Note::TemplateInstancesTruncated { discarded, .. }] if *discarded == 10 - kept));
}
