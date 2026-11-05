//! ★★★ THE CONVERTER'S ADDRESSING CONTRACT, ONE CASE PER WAY REPLAY BROKE IT.
//!
//! Each test is the smallest recording with the SHAPE of a real page whose
//! recorded step failed `tests/roundtrip.rs` on the 2026-11-05 r3 corpus
//! (six failures in four documents, plus one the broker test was hiding). They
//! are written as recordings — JSON through `ingest`, exactly what the
//! converter emits — so they test the contract, not a constructor.
//!
//! The contract, as `navigator-prerender`'s `diff_effects` writes it:
//!   - every address is a `node_path` in the document BEFORE the step;
//!   - a text or comment node's step is `node:N`;
//!   - an insert's index is its position among the parent's children AFTER.

use navigator_backend::document::Document;
use navigator_backend::navigatord::{Event, InProcessHost, Navigatord, Request};
use navigator_backend::reverse::{Back, Forward, History};
use navigator_backend::{ingest, Limits, Recording};

fn recording(document: &str, effects: &str) -> Recording {
    let json = format!(
        r##"{{"format":"atrium-navigator-recording/3","url":"https://a.test/","tier":2,
            "tier_reason":"","measurements":{{}},"stylesheets":[],"images":[],
            "transitions":[{{"trigger":"#t","event":"click","anchored":true,"effects":[{effects}]}}],
            "document":{}}}"##,
        serde_json::to_string(document).unwrap());
    ingest(json.as_bytes(), &Limits::default()).expect("ingests")
}

/// Forward lands on `expected`; back lands on the original bytes; forward
/// again lands on `expected` again — through `History`, a reader's path.
fn replays(r: &Recording, expected: &str) {
    let base = Document::accept(&r.document).expect("inside the profile");
    let before = base.dom.serialize();
    let mut h = History::new(base);
    h.go(&r.transitions[0]).expect("applies to the document it was recorded on");
    let after = h.document().dom.serialize();
    assert_eq!(after, Document::parse(expected).dom.serialize(), "forward");
    assert_eq!(h.back().expect("undoes"), Back::Stepped);
    assert_eq!(h.document().dom.serialize(), before, "back");
    assert_eq!(h.forward().expect("redoes"), Forward::Stepped);
    assert_eq!(h.document().dom.serialize(), after, "forward again");
}

/// chinadaily: a search tab rewrites a label's TEXT. The converter names the
/// text node `…>div:0>node:0`; the backend's own walk matched element tags
/// only, so the removal never resolved.
#[test]
fn a_text_node_named_node_n_resolves() {
    let r = recording(
        r##"<html><body><button id="t">x</button><div class="l">Site Search</div></body></html>"##,
        r##"{"kind":"remove","target":"html:0>body:0>div:0>node:0"},
           {"kind":"insert","parent":"html:0>body:0>div:0","index":0,"html":"Sogou Search"}"##);
    replays(&r, r##"<html><body><button id="t">x</button><div class="l">Sogou Search</div></body></html>"##);
}

/// ynet: a step RENAMES ids. Forward named the elements by their old ids,
/// which is right (pre-step addressing); the undo named them by the old ids
/// too, in a document where only the new ones exist.
#[test]
fn a_step_that_renames_an_id_goes_back() {
    let r = recording(
        r##"<html><body><button id="t">x</button><div id="a">1</div><div id="b">2</div></body></html>"##,
        r##"{"kind":"attribute","target":"#a","name":"id","from":"a","to":"a2"},
           {"kind":"attribute","target":"#b","name":"id","from":"b","to":"b2"}"##);
    replays(&r, r##"<html><body><button id="t">x</button><div id="a2">1</div><div id="b2">2</div></body></html>"##);
}

/// …and the forward half of the same rule: an effect addressing an element
/// by the id an EARLIER effect of the same step renamed. Pre-step addressing
/// means `#a` is still the element that was `#a`.
#[test]
fn an_effect_after_a_rename_still_names_the_pre_step_node() {
    let r = recording(
        r##"<html><body><button id="t">x</button><div id="a" class="c">1</div></body></html>"##,
        r##"{"kind":"attribute","target":"#a","name":"class","from":"c","to":"d"},
           {"kind":"attribute","target":"#a","name":"id","from":"a","to":"z"},
           {"kind":"insert","parent":"#a","index":1,"html":"<b>n</b>"}"##);
    replays(&r, r##"<html><body><button id="t">x</button><div id="z" class="d">1<b>n</b></div></body></html>"##);
}

/// yahoo.jp: a step inserts a `<div>` ahead of the `div:1` that a LATER
/// effect of the same step addresses. Resolved after the insert, `div:1`
/// was the new div and `div:1>button:1` named nothing.
#[test]
fn an_insert_does_not_move_the_addresses_after_it() {
    let r = recording(
        r##"<html><body><button id="t">x</button><section><div>a</div><div><button>p</button><button>q</button></div></section></body></html>"##,
        r##"{"kind":"insert","parent":"html:0>body:0>section:0","index":1,"html":"<div>new</div>"},
           {"kind":"insert","parent":"html:0>body:0>section:0>div:1>button:1","index":1,"html":"<span>s</span>"}"##);
    replays(&r, r##"<html><body><button id="t">x</button><section><div>a</div><div>new</div><div><button>p</button><button>q<span>s</span></button></div></section></body></html>"##);
}

/// The index is a position AFTER the step, so inserts land lowest-first
/// whatever order the recording lists them in (the converter lists them in
/// the order their nodes were created).
#[test]
fn inserts_listed_out_of_order_land_where_they_were_observed() {
    let r = recording(
        r##"<html><body><button id="t">x</button><ul><li>a</li><li>b</li></ul></body></html>"##,
        r##"{"kind":"insert","parent":"html:0>body:0>ul:0","index":2,"html":"<li>n2</li>"},
           {"kind":"insert","parent":"html:0>body:0>ul:0","index":1,"html":"<li>n1</li>"}"##);
    replays(&r, r##"<html><body><button id="t">x</button><ul><li>a</li><li>n1</li><li>n2</li><li>b</li></ul></body></html>"##);
}

/// gov.uk: the menu toggle removes `hidden`, which sits BEFORE `class`.
/// Undoing it with `setAttribute` semantics appended it, and the document
/// "undid to a DIFFERENT document" — same meaning, different bytes.
#[test]
fn undoing_an_attribute_removal_restores_its_position() {
    let r = recording(
        r##"<html><body><button id="t">x</button><div id="m" hidden="hidden" class="menu">m</div></body></html>"##,
        r##"{"kind":"attribute","target":"#m","name":"hidden","from":"hidden","to":null}"##);
    replays(&r, r##"<html><body><button id="t">x</button><div id="m" class="menu">m</div></body></html>"##);
}

/// A step that changes an attribute of a node it then removes comes back
/// whole — the removed subtree is taken from the document before the step,
/// not after its attribute writes.
#[test]
fn a_node_changed_then_removed_comes_back_as_it_was() {
    let r = recording(
        r##"<html><body><button id="t">x</button><p id="p" class="a">t</p><i>k</i></body></html>"##,
        r##"{"kind":"attribute","target":"#p","name":"class","from":"a","to":"b"},
           {"kind":"remove","target":"#p"}"##);
    replays(&r, r##"<html><body><button id="t">x</button><i>k</i></body></html>"##);
}

/// doc13: the broker offered a trigger whose probe dropped effects, then
/// refused it as incomplete when the reader took it — a control that could
/// only ever say no. It must not be offered at all.
#[test]
fn the_broker_does_not_offer_an_incomplete_step() {
    let json = br##"{"format":"atrium-navigator-recording/3","url":"https://a.test/","tier":2,
        "tier_reason":"","measurements":{},"stylesheets":[],"images":[],
        "transitions":[
          {"trigger":"#t","event":"click","anchored":true,
           "effects":[{"kind":"attribute","target":"#m","name":"class","from":null,"to":"open"},
                      {"kind":"truncated","dropped":82}]},
          {"trigger":"#u","event":"click","anchored":true,
           "effects":[{"kind":"attribute","target":"#m","name":"data-u","from":null,"to":"1"}]}],
        "document":"<html><body><button id=\"t\">t</button><button id=\"u\">u</button><nav id=\"m\"></nav></body></html>"}"##;
    let mut n = Navigatord::unconfined_for_testing(InProcessHost::default());
    n.tick(1_000);
    let (session, triggers) = match n.handle(Request::OpenSession { recording: json.to_vec() }).pop() {
        Some(Event::SessionOpened { session, triggers }) => (session, triggers),
        other => panic!("{other:?}"),
    };
    assert_eq!(triggers, vec!["#u".to_string()]);
    match n.handle(Request::Navigate { session, trigger: "#t".into() }).pop() {
        Some(Event::Blocked { .. }) => {}
        other => panic!("an unoffered trigger must be refused: {other:?}"),
    }
}
