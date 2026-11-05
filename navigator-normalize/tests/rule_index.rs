//! ★ The rule index (legacy-web §5.4.5.7) must not change WHICH elements a
//! selector matches, nor their order: for every selector, the matches found
//! by trying only the index's candidates equal the matches of the full scan.

use navigator_dom::dom::Kind;
use navigator_normalize::sel::{parse_list, Matcher, RuleIndex};

const DOC: &str = r#"<html><body id=top class="a b">
<div id=x class="a a c"><p class=b>one</p><P CLASS="c d">two</P><span id=x>dup id</span></div>
<ul class=list><li class="item on">1</li><li class=item>2</li><li>3</li></ul>
<section><DIV class=A>case</DIV><a href=/q class=b>q</a></section></body></html>"#;

const SELECTORS: &[&str] = &[
    "*", "p", "P", "div", "DIV", ".a", ".A", ".b", ".c.d", "p.c", "#x", "#top", "#x.a", "div#x > p",
    "ul .item", ".item.on", "li:not(.item)", ":is(.a, .b)", "section a.b", ".list > li + li", "body *",
    "#missing", ".missing", "span", "li:first-child", "div p ~ span", ".a .b", "html body .c",
];

#[test]
fn candidates_give_exactly_the_full_scans_matches_in_order() {
    let dom = navigator_dom::parse(DOC);
    let els: Vec<_> = (0..dom.nodes.len() as u32)
        .filter(|h| matches!(dom.get(*h).map(|n| &n.kind), Some(Kind::Element(_)))).collect();
    let ix = RuleIndex::new(&dom, &els);
    let m = Matcher::new(&dom);
    let mut narrowed = 0;
    for s in SELECTORS {
        let toks = navigator_style::token::tokenize(s);
        for c in parse_list(&toks).unwrap_or_else(|e| panic!("{s}: {e}")) {
            let full: Vec<_> = els.iter().copied().filter(|&h| m.matches(h, &c)).collect();
            let cand = ix.candidates(&c);
            let fast: Vec<_> = cand.iter().copied().filter(|&h| m.matches(h, &c)).collect();
            assert_eq!(full, fast, "{s}");
            if cand.len() < els.len() { narrowed += 1 }
        }
    }
    assert!(narrowed >= 15, "the index should narrow most of these: {narrowed}");
}
