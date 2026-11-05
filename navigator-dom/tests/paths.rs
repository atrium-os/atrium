//! ★★ `resolve_path` IS `node_path`'S INVERSE — tested as one, on every node.
//!
//! The converter writes addresses with `node_path`; the backend replays them
//! through `resolve_path`. Until 2026-11-05 the backend had its own walk that
//! matched element tags only, and the step `node_path` writes for a text node
//! (`node:N`) never resolved: chinadaily's search tabs were unreplayable.
//! A per-node round trip over a whole document is the check that would have
//! caught it, so it is the check.

use navigator_dom::parse;

const DOC: &str = "<!doctype html><html><head><title>t</title></head><body>\
    text before<!-- a comment --><div id=\"a\">one<span>two</span>three</div>\
    <div class=\"x\"><p>p0</p>between<p id=\"\">p1</p></div>\
    <svg viewBox=\"0 0 1 1\"><linearGradient id=\"g\"></linearGradient></svg>\
    <ul><li>1</li><li>2</li><li>3</li></ul>tail</body></html>";

#[test]
fn every_connected_node_round_trips_through_its_path() {
    let d = parse(DOC);
    let mut checked = 0;
    for h in 1..d.nodes.len() as u32 {
        if !d.connected(h) { continue }
        let p = d.node_path(h);
        assert_eq!(d.resolve_path(&p), Some(h), "{p} does not lead back to node {h}");
        assert_eq!(d.resolve_path(&d.positional_path(h)), Some(h));
        checked += 1;
    }
    // Text and comment nodes are most of the count; the test is only worth
    // something if they are in it.
    assert!(checked > 20, "checked {checked} nodes");
}

#[test]
fn a_text_node_path_resolves_to_the_text_node() {
    let d = parse("<html><body><div id=\"s\">Site Search<b>x</b></div></body></html>");
    let div = d.resolve_path("#s").unwrap();
    let text = d.children_of(div)[0];
    assert_eq!(d.tag(text), None);
    assert_eq!(d.node_path(text), "html:0>body:0>div:0>node:0");
    assert_eq!(d.resolve_path("html:0>body:0>div:0>node:0"), Some(text));
    // And an element step never lands on a text node, or the reverse.
    assert_eq!(d.resolve_path("html:0>body:0>div:0>b:0"), Some(d.children_of(div)[1]));
    assert_eq!(d.resolve_path("html:0>body:0>div:0>node:1"), None);
}

/// ★ Ambiguity is a refusal, never a guess.
#[test]
fn a_shared_id_is_refused_and_exact_path_falls_back_to_position() {
    let d = parse("<html><body><i id=\"dup\">1</i><i id=\"dup\">2</i></body></html>");
    assert_eq!(d.resolve_path("#dup"), None);
    let second = d.resolve_path("html:0>body:0>i:1").unwrap();
    assert_eq!(d.node_path(second), "#dup");
    assert_eq!(d.exact_path(second), "html:0>body:0>i:1");
    assert_eq!(d.resolve_path(&d.exact_path(second)), Some(second));
}

#[test]
fn node_step_is_refused_where_a_node_element_makes_it_ambiguous() {
    let d = parse("<html><body>text<node>el</node></body></html>");
    assert_eq!(d.resolve_path("html:0>body:0>node:0"), None);
}

#[test]
fn tag_steps_are_exact_case_like_node_path() {
    let d = parse(DOC);
    let g = d.resolve_path("#g").unwrap();
    let p = d.positional_path(g);
    assert!(p.ends_with("linearGradient:0"), "{p}");
    assert_eq!(d.resolve_path(&p.replace("linearGradient", "lineargradient")), None);
}

#[test]
fn set_attr_at_restores_a_removed_attribute_in_place() {
    let mut d = parse("<html><body><div id=\"m\" hidden=\"hidden\" class=\"c\"></div></body></html>");
    let m = d.resolve_path("#m").unwrap();
    let before = d.serialize();
    let at = d.attr_index(m, "hidden").unwrap();
    d.remove_attr(m, "hidden");
    d.set_attr(m, "hidden", "hidden");
    assert_ne!(d.serialize(), before, "setAttribute appends — the bug this exists for");
    d.remove_attr(m, "hidden");
    d.set_attr_at(m, "hidden", "hidden", at);
    assert_eq!(d.serialize(), before);
}
