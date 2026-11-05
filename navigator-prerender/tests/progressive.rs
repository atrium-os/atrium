//! Progressive conversion (legacy-web §5.4.5.7): the tier-1 recording first,
//! the readable recording once the scripts settle, the transitions after
//! exploration — and the parts lose nothing: merged, they are the recording
//! a single conversion writes, byte for byte, and the readable part is the
//! recording a control (non-exploring) conversion writes.

use navigator_prerender::{artifact, boa_impl::BoaEngine, convert_progressive, convert_with, fetch::NoNetwork,
    Conversion, Stage, TierPolicy};

const URL: &str = "https://example.test/p.html";

/// Scripted content, a click handler on a parser node, and an id'd one.
const PAGE: &str = r#"<html><head><title>t</title></head><body>
<p id=lead>served</p><ul><li><a href=#a>a</a></li><li><a href=#b id=b>b</a></li></ul>
<div id=out class=closed>panel</div>
<script>
document.getElementById('lead').textContent = 'served, then scripted';
var p = document.createElement('p'); p.textContent = 'added by a script'; document.body.appendChild(p);
document.getElementById('b').addEventListener('click', function () {
  document.getElementById('out').setAttribute('class', 'open');
});
var as = document.getElementsByTagName('a');
as[0].addEventListener('click', function () { document.getElementById('out').setAttribute('class', 'first'); });
</script></body></html>"#;

fn whole(explore: bool) -> String {
    let mut eng = BoaEngine { explore, ..Default::default() };
    let c = convert_with(PAGE, Some(URL), &mut eng, &mut NoNetwork);
    artifact::emit(Some(URL), &c, &TierPolicy::default())
}

fn parts() -> (Vec<Stage>, String, String, String, Conversion) {
    let p = TierPolicy::default();
    let mut eng = BoaEngine { explore: true, ..Default::default() };
    let (mut stages, mut first, mut readable, mut trans) = (vec![], String::new(), String::new(), String::new());
    let c = convert_progressive(PAGE, Some(URL), &mut eng, &mut NoNetwork, false, &mut |s, c| {
        stages.push(s);
        match s {
            Stage::First => first = artifact::emit(Some(URL), c, &p),
            Stage::Settled => readable = artifact::emit_readable(Some(URL), c, &p),
            Stage::Done => trans = artifact::emit_transitions(Some(URL), c, &p, &readable),
        }
    });
    (stages, first, readable, trans, c)
}

#[test]
fn the_stages_arrive_in_order_once_each() {
    let (stages, ..) = parts();
    assert_eq!(stages, vec![Stage::First, Stage::Settled, Stage::Done]);
}

#[test]
fn the_first_artifact_is_tier_one_and_ran_nothing() {
    let (_, first, ..) = parts();
    assert!(first.contains("\"tier\": 1,"), "{first}");
    assert!(first.contains("first display: scripts not run yet"));
    assert!(first.contains(">served</p>") && !first.contains(">served, then scripted<"));
}

#[test]
fn the_readable_artifact_is_the_control_recording_byte_for_byte() {
    let (_, _, readable, ..) = parts();
    assert!(readable.contains(">added by a script</p>"));
    assert!(readable.contains("\"transitions\": [],"));
    assert_eq!(readable, whole(false));
}

#[test]
fn merged_parts_are_the_whole_recording_byte_for_byte() {
    let (_, _, readable, trans, c) = parts();
    assert!(!c.transitions.is_empty() || !c.templates.is_empty(), "the page should have explored something");
    let merged = artifact::merge(&readable, &trans).unwrap();
    assert_eq!(merged, whole(true));
    assert_eq!(merged, artifact::emit(Some(URL), &c, &TierPolicy::default()));
}

#[test]
fn transitions_refuse_another_documents_readable_artifact() {
    let (_, _, readable, trans, _) = parts();
    let other = readable.replace("added by a script", "added by another script");
    assert!(artifact::merge(&other, &trans).unwrap_err().contains("another document"));
}

#[test]
fn a_progressive_conversion_is_reproducible() {
    let a = parts();
    let b = parts();
    assert_eq!((a.1, a.2, a.3), (b.1, b.2, b.3));
}
