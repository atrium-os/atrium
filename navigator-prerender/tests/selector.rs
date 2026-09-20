use navigator_prerender::{parse::parse, selector};

fn sel(d: &navigator_prerender::dom::Dom, s: &str) -> Vec<String> {
    let q = selector::parse(s).expect("parse");
    selector::select(d, d.root(), &q).into_iter()
        .map(|h| d.attr(h, "id").unwrap_or("?").to_string()).collect()
}

const DOC: &str = r#"<html><body>
  <div id="a" class="box wide" data-role="panel">
    <p id="p1" class="lede">one</p>
    <p id="p2">two</p>
    <span id="s1"><em id="e1">deep</em></span>
  </div>
  <div id="b" class="box">
    <p id="p3" class="lede">three</p>
  </div>
  <a id="l1" href="https://example.test/x.html">link</a>
</body></html>"#;

#[test] fn type_and_id_and_class() {
    let d = parse(DOC);
    assert_eq!(sel(&d, "p"), ["p1", "p2", "p3"]);
    assert_eq!(sel(&d, "#p2"), ["p2"]);
    assert_eq!(sel(&d, ".lede"), ["p1", "p3"]);
    assert_eq!(sel(&d, "p.lede"), ["p1", "p3"]);
    assert_eq!(sel(&d, ".box.wide"), ["a"]);
}

#[test] fn descendant_and_child_combinators() {
    let d = parse(DOC);
    assert_eq!(sel(&d, "#a p"), ["p1", "p2"]);
    assert_eq!(sel(&d, "#a > p"), ["p1", "p2"]);
    assert_eq!(sel(&d, "#a em"), ["e1"], "descendant must cross a level");
    assert_eq!(sel(&d, "#a > em"), Vec::<String>::new(), "child must not");
    assert_eq!(sel(&d, "body div p"), ["p1", "p2", "p3"]);
}

#[test] fn sibling_combinators() {
    let d = parse(DOC);
    assert_eq!(sel(&d, "#p1 + p"), ["p2"]);
    assert_eq!(sel(&d, "#p1 ~ span"), ["s1"]);
    assert_eq!(sel(&d, "#p2 + p"), Vec::<String>::new());
}

#[test] fn attribute_predicates() {
    let d = parse(DOC);
    assert_eq!(sel(&d, "[data-role]"), ["a"]);
    assert_eq!(sel(&d, "[data-role=panel]"), ["a"]);
    assert_eq!(sel(&d, r#"[href^="https://"]"#), ["l1"]);
    assert_eq!(sel(&d, r#"[href$=".html"]"#), ["l1"]);
    assert_eq!(sel(&d, r#"[href*="example"]"#), ["l1"]);
    assert_eq!(sel(&d, r#"[class~="wide"]"#), ["a"]);
}

#[test] fn selector_lists_and_pseudos() {
    let d = parse(DOC);
    assert_eq!(sel(&d, "#p1, #p3"), ["p1", "p3"]);
    assert_eq!(sel(&d, "#a p:first-child"), ["p1"]);
    assert_eq!(sel(&d, "#a p:last-child"), Vec::<String>::new(), "span is last");
    assert_eq!(sel(&d, "#b p:only-child"), ["p3"]);
}

/// An unknown pseudo must lose that rule, not the query or the document.
#[test] fn unknown_pseudo_matches_nothing_and_does_not_error() {
    let d = parse(DOC);
    let q = selector::parse("p:hover").expect("must still parse");
    assert!(selector::select(&d, d.root(), &q).is_empty());
}

#[test] fn malformed_selectors_are_rejected_not_panicked() {
    for bad in ["", "   ", "#", ".", "div >", "[", "p[", ">"] {
        let _ = selector::parse(bad); // must not panic
    }
    assert!(selector::parse("#").is_none());
    assert!(selector::parse("div >").is_none());
}
